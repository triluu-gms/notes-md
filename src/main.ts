import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { ask } from "@tauri-apps/plugin-dialog";
import { openUrl } from "@tauri-apps/plugin-opener";
import { EditorState, Prec } from "@codemirror/state";
import { EditorView, keymap, drawSelection, highlightActiveLine, placeholder } from "@codemirror/view";
import { defaultKeymap, history, historyKeymap, indentWithTab, undo, redo } from "@codemirror/commands";
import { HighlightStyle, syntaxHighlighting } from "@codemirror/language";
import { search, searchKeymap, highlightSelectionMatches, openSearchPanel } from "@codemirror/search";
import { markdown } from "@codemirror/lang-markdown";
import { languages } from "@codemirror/language-data";
import { tags as t } from "@lezer/highlight";
import { marked, type TokensList } from "marked";
import DOMPurify from "dompurify";
import { renderCode } from "./highlight";
import { toggleWrap, toggleBlock, codeBlock, insertLink, insertTable, insertRule, type BlockKind } from "./format";
import "./styles.css";

// ---------- types ----------
type NoteMeta = { path: string; title: string; mtime: number; tags: string[]; snippet?: string };
type Folder = { name: string; path: string; count: number; children: Folder[] };
type TagCount = { tag: string; count: number };
type VaultInfo = { root: string; name: string };
type Changes = { updated: string[]; removed: string[] };
type Filter = { kind: "all" } | { kind: "folder"; path: string } | { kind: "tag"; tag: string };
type Mode = "edit" | "split" | "preview";

// ---------- dom ----------
const $ = <T extends HTMLElement = HTMLElement>(id: string) => document.getElementById(id) as T;
const el = {
  vaultBtn: $("vault-btn"),
  vaultName: $("vault-name"),
  themeBtn: $("theme-btn"),
  tree: $("tree"),
  tags: $("tags"),
  search: $<HTMLInputElement>("search"),
  newNote: $("new-note-btn"),
  newFolder: $("new-folder-btn"),
  listTitle: $("list-title"),
  list: $("note-list"),
  notePath: $("note-path"),
  saveState: $("save-state"),
  modeSeg: $("mode-seg"),
  notice: $("notice"),
  tagBar: $("tag-bar"),
  tagChips: $("tag-chips"),
  tagInput: $<HTMLInputElement>("tag-input"),
  tagOptions: $("tag-options"),
  formatBar: $("format-bar"),
  body: $("editor-body"),
  editor: $("editor"),
  preview: $("preview"),
  empty: $("empty"),
  menu: $("menu"),
};

// ---------- state ----------
const store = {
  get(key: string): string | null {
    try { return localStorage.getItem(key); } catch { return null; }
  },
  set(key: string, val: string) {
    try { localStorage.setItem(key, val); } catch { /* ignore */ }
  },
};

const S = {
  vault: null as VaultInfo | null,
  filter: { kind: "all" } as Filter,
  query: "",
  notes: [] as NoteMeta[],
  current: null as string | null,
  currentTags: [] as string[],
  allTags: [] as string[],
  saved: "",
  dirty: false,
  expanded: new Set<string>(JSON.parse(store.get("expanded") ?? "[]")),
  mode: (store.get("mode") as Mode) ?? "split",
};

// ---------- helpers ----------
const esc = (s: string) =>
  s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!);
const dirOf = (p: string) => (p.includes("/") ? p.slice(0, p.lastIndexOf("/")) : "");
const baseName = (p: string) => p.slice(p.lastIndexOf("/") + 1);
const stem = (p: string) => baseName(p).replace(/\.md$/i, "");

function debounce<A extends unknown[]>(fn: (...a: A) => void, ms: number) {
  let h: number | undefined;
  return (...a: A) => {
    clearTimeout(h);
    h = window.setTimeout(() => fn(...a), ms);
  };
}

/** Resolve `rel` (may contain ./ ../) against folder `base`; returns vault-relative path or null if it escapes. */
function joinRel(base: string, rel: string): string | null {
  const out = base ? base.split("/") : [];
  for (const part of rel.split(/[\\/]/)) {
    if (part === "" || part === ".") continue;
    if (part === "..") {
      if (!out.length) return null;
      out.pop();
    } else out.push(part);
  }
  return out.join("/");
}

/** Absolute path of a vault file, with the OS's separator (taken from the vault path: `\` on Windows). */
const absPath = (rel: string) => {
  const root = S.vault!.root.replace(/[\\/]+$/, "");
  const sep = root.includes("\\") ? "\\" : "/";
  return root + sep + rel.split("/").join(sep);
};

function fmtDate(ms: number) {
  const d = new Date(ms);
  const now = new Date();
  return d.toDateString() === now.toDateString()
    ? d.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" })
    : d.toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" });
}

function toast(msg: string) {
  el.notice.textContent = msg;
  el.notice.hidden = false;
  el.notice.className = "notice error";
  setTimeout(() => (el.notice.hidden = true), 4000);
}

async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  try {
    return await invoke<T>(cmd, args);
  } catch (e) {
    toast(String(e));
    throw e;
  }
}

// ---------- theme ----------
const THEMES = ["system", "light", "dark"] as const;
type Theme = (typeof THEMES)[number];
const THEME_ICON: Record<Theme, string> = { system: "◐", light: "☀", dark: "☾" };

function applyTheme(theme: Theme) {
  if (theme === "system") delete document.documentElement.dataset.theme;
  else document.documentElement.dataset.theme = theme;
  el.themeBtn.textContent = THEME_ICON[theme];
  el.themeBtn.title = `Theme: ${theme}`;
  getCurrentWindow().setTheme(theme === "system" ? null : theme).catch(() => {});
  store.set("theme", theme);
}
el.themeBtn.onclick = () => {
  const cur = (store.get("theme") as Theme) ?? "system";
  applyTheme(THEMES[(THEMES.indexOf(cur) + 1) % THEMES.length]);
};
applyTheme((store.get("theme") as Theme) ?? "system");

// ---------- editor ----------
const mdHighlight = HighlightStyle.define([
  { tag: t.heading1, fontSize: "1.45em", fontWeight: "700" },
  { tag: t.heading2, fontSize: "1.25em", fontWeight: "700" },
  { tag: t.heading3, fontSize: "1.1em", fontWeight: "700" },
  { tag: [t.heading4, t.heading5, t.heading6], fontWeight: "700" },
  { tag: t.strong, fontWeight: "700" },
  { tag: t.emphasis, fontStyle: "italic" },
  { tag: t.strikethrough, textDecoration: "line-through" },
  { tag: [t.link, t.url], color: "var(--accent)" },
  { tag: t.monospace, fontFamily: "var(--mono)", color: "var(--code)" },
  { tag: t.quote, color: "var(--muted)", fontStyle: "italic" },
  { tag: [t.processingInstruction, t.meta, t.contentSeparator, t.labelName], color: "var(--muted)" },
  // code inside fenced blocks (```sql …)
  { tag: [t.keyword, t.operatorKeyword, t.modifier, t.controlKeyword], color: "var(--syn-keyword)" },
  { tag: [t.string, t.special(t.string), t.regexp], color: "var(--syn-string)" },
  { tag: [t.comment, t.lineComment, t.blockComment], color: "var(--syn-comment)", fontStyle: "italic" },
  { tag: [t.number, t.bool, t.null, t.atom], color: "var(--syn-number)" },
  { tag: [t.function(t.variableName), t.function(t.propertyName)], color: "var(--syn-func)" },
  { tag: [t.typeName, t.className, t.namespace], color: "var(--syn-type)" },
  { tag: [t.propertyName, t.attributeName, t.tagName], color: "var(--syn-attr)" },
]);

const editorExtensions = [
  history(),
  drawSelection(),
  highlightActiveLine(),
  highlightSelectionMatches(),
  EditorView.lineWrapping,
  markdown({ codeLanguages: languages }), // grammars load on demand, only for languages a note uses
  syntaxHighlighting(mdHighlight),
  placeholder("Start writing…"),
  search({ top: true }),
  Prec.high(
    keymap.of([
      { key: "Mod-b", run: (v) => (toggleWrap(v, "**"), true) },
      { key: "Mod-i", run: (v) => (toggleWrap(v, "_"), true) },
      { key: "Mod-h", run: openSearchPanel },
    ]),
  ),
  keymap.of([...defaultKeymap, ...historyKeymap, ...searchKeymap, indentWithTab]),
  EditorView.updateListener.of((u) => {
    if (!u.docChanged) return;
    S.dirty = u.state.doc.toString() !== S.saved;
    setSaveState(S.dirty ? "Edited" : "Saved");
    if (S.dirty) autosave();
    if (S.mode !== "edit") renderPreviewSoon();
  }),
];

const view = new EditorView({ parent: el.editor, state: EditorState.create({ extensions: editorExtensions }) });

function setDoc(content: string, keepCursor = false) {
  const sel = keepCursor ? Math.min(view.state.selection.main.head, content.length) : 0;
  view.setState(EditorState.create({ doc: content, selection: { anchor: sel }, extensions: editorExtensions }));
}

function setSaveState(s: string) {
  el.saveState.textContent = s;
}

// ---------- preview ----------
marked.use({
  gfm: true,
  renderer: {
    code: ({ text, lang }) => renderCode(text, lang),
  },
  extensions: [
    {
      name: "wikilink",
      level: "inline",
      start: (src: string) => src.indexOf("[["),
      tokenizer(src: string) {
        const m = /^\[\[([^\]|\n]+)(?:\|([^\]\n]+))?\]\]/.exec(src);
        if (m) return { type: "wikilink", raw: m[0], target: m[1].trim(), label: (m[2] ?? m[1]).trim() };
      },
      renderer: (tok) => `<a href="#" class="wikilink" data-wiki="${esc(tok.target)}">${esc(tok.label)}</a>`,
    },
  ],
});

const stripFrontMatter = (s: string) => s.replace(/^﻿?---\r?\n[\s\S]*?\r?\n(?:---|\.\.\.)[ \t]*(?:\r?\n|$)/, "");

const countLines = (s: string) => s.split("\n").length - 1;

function renderPreview() {
  if (!S.current) return;
  const src = view.state.doc.toString();
  const body = stripFrontMatter(src);
  // Render block by block, putting an invisible marker with the source line before each block.
  // The markers let the editor and preview scroll in step.
  const tokens = marked.lexer(body);
  let line = countLines(src.slice(0, src.length - body.length)) + 1;
  let html = "";
  for (const tok of tokens) {
    if (tok.type !== "space") html += `<div class="src-line" data-line="${line}"></div>`;
    html += marked.parser(Object.assign([tok], { links: tokens.links }) as TokensList);
    line += countLines(tok.raw);
  }
  el.preview.innerHTML = DOMPurify.sanitize(html);
  const dir = dirOf(S.current);
  el.preview.querySelectorAll("img").forEach((img) => {
    const s = img.getAttribute("src");
    if (!s || /^(https?:|data:|blob:)/i.test(s)) return;
    const rel = joinRel(dir, decodeURI(s));
    if (rel !== null) img.src = convertFileSrc(absPath(rel));
  });
  if (S.mode === "split") syncScroll("editor");
}
const renderPreviewSoon = debounce(renderPreview, 120);

// ---------- scroll sync (split mode) ----------
// The pane under the mouse (or holding focus) drives; the other follows by source line.
let scrollDriver: "editor" | "preview" = "editor";
let syncFrame = 0;

/** (source line, preview y) pairs, from the markers written by renderPreview, plus both ends. */
function previewAnchors() {
  const pts = [{ line: 1, y: 0 }];
  el.preview.querySelectorAll<HTMLElement>(".src-line").forEach((a) => {
    const line = Number(a.dataset.line);
    if (line > pts[pts.length - 1].line && a.offsetTop >= pts[pts.length - 1].y) pts.push({ line, y: a.offsetTop });
  });
  pts.push({ line: view.state.doc.lines + 1, y: el.preview.scrollHeight - el.preview.clientHeight });
  return pts;
}

/** Linear interpolation between anchor points, from one coordinate to the other. */
function interpolate(pts: { line: number; y: number }[], value: number, from: "line" | "y", to: "line" | "y") {
  for (let i = 0; i < pts.length - 1; i++) {
    const a = pts[i];
    const b = pts[i + 1];
    if (value < b[from] || i === pts.length - 2) {
      const span = b[from] - a[from];
      const frac = span > 0 ? Math.min(1, Math.max(0, (value - a[from]) / span)) : 0;
      return a[to] + frac * (b[to] - a[to]);
    }
  }
  return 0;
}

/** Offset of the editor document's top inside its scroller (CSS padding). */
const editorDocOffset = () =>
  view.documentTop - view.scrollDOM.getBoundingClientRect().top + view.scrollDOM.scrollTop;

/** Fractional source line at the top of the editor viewport. */
function editorTopLine() {
  const h = view.scrollDOM.scrollTop - editorDocOffset();
  if (h <= 0) return 1;
  const blk = view.lineBlockAtHeight(h);
  return view.state.doc.lineAt(blk.from).number + Math.min(1, (h - blk.top) / Math.max(1, blk.height));
}

function scrollEditorToLine(line: number) {
  const doc = view.state.doc;
  const n = Math.min(Math.max(1, Math.floor(line)), doc.lines);
  const blk = view.lineBlockAt(doc.line(n).from);
  view.scrollDOM.scrollTop = editorDocOffset() + blk.top + (line - n) * blk.height;
}

const atBottom = (e: HTMLElement) => e.scrollTop > 0 && e.scrollTop >= e.scrollHeight - e.clientHeight - 2;

function syncScroll(from: "editor" | "preview") {
  if (S.mode !== "split" || !S.current) return;
  const pts = previewAnchors();
  const ed = view.scrollDOM;
  const pv = el.preview;
  if (from === "editor") {
    pv.scrollTop = atBottom(ed) ? pv.scrollHeight : interpolate(pts, editorTopLine(), "line", "y");
  } else if (atBottom(pv)) {
    ed.scrollTop = ed.scrollHeight;
  } else {
    scrollEditorToLine(interpolate(pts, pv.scrollTop, "y", "line"));
  }
}

function onPaneScroll(pane: "editor" | "preview") {
  if (pane !== scrollDriver || S.mode !== "split") return;
  cancelAnimationFrame(syncFrame);
  syncFrame = requestAnimationFrame(() => syncScroll(pane));
}

for (const [pane, node] of [["editor", el.editor], ["preview", el.preview]] as const) {
  node.addEventListener("pointerenter", () => (scrollDriver = pane));
  node.addEventListener("wheel", () => (scrollDriver = pane), { passive: true });
  node.addEventListener("focusin", () => (scrollDriver = pane));
}
view.scrollDOM.addEventListener("scroll", () => onPaneScroll("editor"), { passive: true });
el.preview.addEventListener("scroll", () => onPaneScroll("preview"), { passive: true });
// Images change the preview's height once loaded; re-align to the editor.
el.preview.addEventListener("load", () => scrollDriver === "editor" && syncScroll("editor"), true);

/** Reload the open note's tags from the index (they change when the note is saved). */
async function loadCurrentTags() {
  const path = S.current;
  if (!path) return;
  const meta = await invoke<NoteMeta | null>("note_meta", { path }).catch(() => null);
  if (S.current !== path) return;
  const tags = meta?.tags ?? [];
  if (tags.join("\n") === S.currentTags.join("\n")) return;
  S.currentTags = tags;
  renderTagBar();
}

// ---------- tag bar (add / rename / remove tags on the open note) ----------
function renderTagBar() {
  el.tagChips.innerHTML = S.currentTags
    .map(
      (tg) =>
        `<span class="tag-chip" data-tag="${esc(tg)}" title="Click: notes with #${esc(tg)} · Right-click: rename"><span class="tag-name">#${esc(tg)}</span><button class="tag-x" title="Remove tag" aria-label="Remove tag ${esc(tg)}">×</button></span>`,
    )
    .join("");
  renderTagOptions();
}

function renderTagOptions() {
  el.tagOptions.innerHTML = S.allTags
    .filter((tg) => !S.currentTags.includes(tg))
    .map((tg) => `<option value="${esc(tg)}"></option>`)
    .join("");
}

type TagEdit = { content: string; tags: string[] };

async function applyTagOp(op: "add" | "remove" | "rename", tag: string, newTag?: string) {
  if (!S.current) return;
  const before = view.state.doc.toString();
  let res: TagEdit;
  try {
    res = await invoke<TagEdit>("edit_tags", { content: before, op, tag, newTag: newTag ?? null });
  } catch (e) {
    toast(String(e));
    return;
  }
  if (view.state.doc.toString() !== before) return; // text changed while we were busy; drop this edit
  const after = res.content;
  if (after !== before) {
    // Replace only the changed middle part so the cursor stays put and Ctrl+Z undoes the tag change.
    let a = 0;
    while (a < before.length && a < after.length && before[a] === after[a]) a++;
    let b = 0;
    while (b < before.length - a && b < after.length - a && before[before.length - 1 - b] === after[after.length - 1 - b]) b++;
    view.dispatch({ changes: { from: a, to: before.length - b, insert: after.slice(a, after.length - b) } });
  }
  S.currentTags = res.tags;
  renderTagBar();
  await save();
}

async function addTagsFromInput() {
  const parts = el.tagInput.value.split(/[\s,]+/).filter(Boolean);
  el.tagInput.value = "";
  for (const p of parts) await applyTagOp("add", p);
}

el.tagInput.addEventListener("keydown", (e) => {
  if (e.key === "Enter" || e.key === ",") {
    e.preventDefault();
    addTagsFromInput();
  } else if (e.key === "Escape") {
    el.tagInput.value = "";
    view.focus();
  } else if (e.key === "Backspace" && !el.tagInput.value && S.currentTags.length) {
    el.tagChips.querySelector<HTMLElement>(".tag-chip:last-child .tag-x")?.focus();
  }
});
// Picking a suggestion from the autocomplete list adds it right away.
el.tagInput.addEventListener("input", (e) => {
  if ((e as InputEvent).inputType === "insertReplacementText" || !(e as InputEvent).inputType) addTagsFromInput();
});

el.tagChips.addEventListener("click", (e) => {
  const chip = (e.target as HTMLElement).closest<HTMLElement>(".tag-chip");
  if (!chip) return;
  const tag = chip.dataset.tag!;
  if ((e.target as HTMLElement).closest(".tag-x")) applyTagOp("remove", tag);
  else setFilter({ kind: "tag", tag });
});

// ---------- tag options: inline #tags switch ----------
type VaultSettings = { inlineTags: boolean };

$("tags-menu-btn").addEventListener("click", async (e) => {
  if (!S.vault) return;
  const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
  const settings = await call<VaultSettings>("vault_settings");
  showMenu(r.left, r.bottom + 4, [
    [`${settings.inlineTags ? "✓" : "  "}  Detect #tags in note text`, () => setInlineTags(!settings.inlineTags)],
  ]);
});

async function setInlineTags(enabled: boolean) {
  await save();
  el.vaultName.textContent = "Re-indexing…";
  try {
    await call("set_inline_tags", { enabled });
  } finally {
    el.vaultName.textContent = S.vault!.name;
  }
  await refresh();
  await loadCurrentTags();
}

// ---------- vault menu: open vault, import from Evernote / Joplin ----------
type ImportReport = {
  notes: number;
  notebooks: number;
  attachments: number;
  tags: number;
  skipped: string[];
  folder: string;
};
type ImportKind = "evernote" | "joplin";

const importItems = (folder: string): [string, () => void][] => [
  ["Import from Evernote (.enex)…", () => importNotes("evernote", folder)],
  ["Import from Joplin (.jex)…", () => importNotes("joplin", folder)],
];

$("vault-menu-btn").addEventListener("click", (e) => {
  const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
  const items: MenuItem[] = [["Open vault folder…", pickVault]];
  if (S.vault) items.push("-", ...importItems(currentFolder()));
  showMenu(r.left, r.bottom + 4, items);
});

async function importNotes(kind: ImportKind, folder: string) {
  if (!S.vault) return;
  await save();
  el.vaultName.textContent = "Importing…";
  let rep: ImportReport | null;
  try {
    rep = await call<ImportReport | null>("import_notes", { kind, destFolder: folder });
  } finally {
    el.vaultName.textContent = S.vault.name;
  }
  if (!rep) return; // dialog cancelled
  await refresh();
  setFilter(rep.folder ? { kind: "folder", path: rep.folder } : { kind: "all" });
  const where = rep.folder || S.vault.name;
  const parts = [
    `Imported ${rep.notes} notes from ${rep.notebooks} notebook${rep.notebooks === 1 ? "" : "s"} into "${where}"`,
    `with ${rep.attachments} attachment${rep.attachments === 1 ? "" : "s"} and ${rep.tags} tag${rep.tags === 1 ? "" : "s"}.`,
  ];
  if (rep.skipped.length) parts.push(`Skipped ${rep.skipped.length}: ${rep.skipped.slice(0, 3).join("; ")}${rep.skipped.length > 3 ? "…" : ""}`);
  el.notice.className = "notice";
  el.notice.textContent = parts.join(" ");
  el.notice.hidden = false;
}

async function renameTagOnNote(tag: string) {
  const name = await promptName(`Rename #${tag} on this note`, tag);
  if (name && name !== tag) await applyTagOp("rename", tag, name);
}

el.preview.addEventListener("click", async (e) => {
  const copy = (e.target as HTMLElement).closest<HTMLButtonElement>(".code-copy");
  if (copy) {
    const code = copy.closest(".code-block")?.querySelector("code")?.textContent ?? "";
    await navigator.clipboard.writeText(code).then(
      () => (copy.textContent = "Copied"),
      () => (copy.textContent = "Failed"),
    );
    setTimeout(() => (copy.textContent = "Copy"), 1200);
    return;
  }
  const a = (e.target as HTMLElement).closest("a");
  if (!a) return;
  e.preventDefault();
  if (a.dataset.wiki) return openWiki(a.dataset.wiki);
  const href = a.getAttribute("href") ?? "";
  if (/^(https?:|mailto:)/i.test(href)) return openUrl(href);
  if (href.startsWith("#")) return;
  const rel = joinRel(dirOf(S.current ?? ""), decodeURI(href.split("#")[0]));
  if (rel && /\.md$/i.test(rel)) openNote(rel);
});

async function openWiki(name: string) {
  const want = name.split("#")[0].toLowerCase().replace(/\.md$/, "");
  const all = await call<NoteMeta[]>("list_notes", { folder: null, tag: null });
  const hit =
    all.find((n) => n.path.toLowerCase().replace(/\.md$/, "") === want) ??
    all.find((n) => stem(n.path).toLowerCase() === want);
  if (hit) openNote(hit.path);
  else toast(`Note not found: ${name}`);
}

// ---------- format toolbar ----------
const FORMAT: Record<string, (v: EditorView) => unknown> = {
  bold: (v) => toggleWrap(v, "**"),
  italic: (v) => toggleWrap(v, "_"),
  strike: (v) => toggleWrap(v, "~~"),
  code: (v) => toggleWrap(v, "`", "code"),
  codeblock: codeBlock,
  link: (v) => insertLink(v),
  image: (v) => insertLink(v, true),
  table: insertTable,
  rule: insertRule,
  find: (v) => (openSearchPanel(v), v.focus()),
  undo: (v) => (undo(v), v.focus()),
  redo: (v) => (redo(v), v.focus()),
};
// Keep the editor's selection: toolbar buttons must not take focus.
el.formatBar.addEventListener("mousedown", (e) => e.preventDefault());
el.formatBar.addEventListener("click", (e) => {
  const cmd = (e.target as HTMLElement).closest<HTMLElement>("[data-cmd]")?.dataset.cmd;
  if (!cmd || !S.current) return;
  if (cmd in FORMAT) FORMAT[cmd](view);
  else toggleBlock(view, cmd as BlockKind);
});

// ---------- modes ----------
function setMode(mode: Mode) {
  S.mode = mode;
  store.set("mode", mode);
  el.body.className = `mode-${mode}`;
  el.formatBar.hidden = mode === "preview" || !S.current;
  el.modeSeg.querySelectorAll("button").forEach((b) => b.classList.toggle("active", b.dataset.mode === mode));
  if (mode !== "edit") renderPreview();
  if (mode !== "preview") view.focus();
}
el.modeSeg.onclick = (e) => {
  const m = (e.target as HTMLElement).dataset.mode as Mode | undefined;
  if (m) setMode(m);
};

// ---------- notes ----------
async function save() {
  if (!S.current || !S.dirty) return;
  const path = S.current;
  const content = view.state.doc.toString();
  setSaveState("Saving…");
  try {
    await invoke("save_note", { path, content });
    if (S.current === path) {
      S.saved = content;
      S.dirty = view.state.doc.toString() !== content;
      setSaveState(S.dirty ? "Edited" : "Saved");
      loadCurrentTags();
    }
    refreshSoon();
  } catch (e) {
    setSaveState("Save failed");
    toast(String(e));
  }
}
const autosave = debounce(save, 700);

async function openNote(path: string) {
  if (path === S.current) return;
  await save();
  let content: string;
  try {
    content = await call<string>("read_note", { path });
  } catch {
    return;
  }
  S.current = path;
  S.currentTags = (await invoke<NoteMeta | null>("note_meta", { path }).catch(() => null))?.tags ?? [];
  S.saved = content;
  S.dirty = false;
  el.notice.hidden = true;
  el.tagInput.value = "";
  renderTagBar();
  setDoc(content);
  showEditor(true);
  el.notePath.textContent = path.replace(/\.md$/i, "").replace(/\//g, " / ");
  el.notePath.title = path;
  setSaveState("Saved");
  if (S.mode !== "edit") renderPreview();
  if (S.mode !== "preview") view.focus();
  highlightActiveNote();
}

function closeNote() {
  S.current = null;
  S.saved = "";
  S.dirty = false;
  showEditor(false);
  highlightActiveNote();
}

function showEditor(on: boolean) {
  el.body.hidden = !on;
  el.tagBar.hidden = !on;
  el.formatBar.hidden = !on || S.mode === "preview";
  el.empty.hidden = on;
  document.querySelector(".editor-head")!.classList.toggle("invisible", !on);
  if (!on) {
    el.empty.innerHTML = S.vault
      ? `<p>Select a note or create one</p><p class="hint">Ctrl+N new note · Ctrl+P search · Ctrl+E toggle preview</p>`
      : `<p>No vault open</p><button class="primary" id="empty-open">Open a folder…</button>`;
    document.getElementById("empty-open")?.addEventListener("click", pickVault);
  }
}

function currentFolder(): string {
  if (S.filter.kind === "folder") return S.filter.path;
  return S.current ? dirOf(S.current) : "";
}

async function newNote(folder = currentFolder()) {
  if (!S.vault) return;
  const path = await call<string>("create_note", { folder, name: null });
  await refresh();
  await openNote(path);
  setMode(S.mode === "preview" ? "split" : S.mode);
  // select the heading text so the user can type the title right away
  const line = view.state.doc.line(1);
  if (line.text.startsWith("# ")) view.dispatch({ selection: { anchor: line.from + 2, head: line.to } });
}

async function newFolder(parent = currentFolder()) {
  if (!S.vault) return;
  const name = await promptName("New folder name", "");
  if (!name) return;
  const path = await call<string>("create_folder", { parent, name });
  S.expanded.add(parent);
  persistExpanded();
  await refresh();
  setFilter({ kind: "folder", path });
}

async function renameItem(path: string, isFolder: boolean) {
  const old = isFolder ? baseName(path) : stem(path);
  const name = await promptName(isFolder ? "Rename folder" : "Rename note", old);
  if (!name || name === old) return;
  if (!isFolder) await save();
  const np = await call<string>("rename_item", { path, newName: name });
  remapPath(path, np);
  await refresh();
}

async function moveItem(path: string, destFolder: string) {
  if (path === S.current) await save();
  const np = await call<string>("move_item", { path, destFolder });
  remapPath(path, np);
  await refresh();
}

/** Keep the open note / selected folder pointing at the right place after a rename or move. */
function remapPath(oldP: string, newP: string) {
  const swap = (p: string) => (p === oldP ? newP : p.startsWith(oldP + "/") ? newP + p.slice(oldP.length) : p);
  if (S.current) {
    S.current = swap(S.current);
    el.notePath.textContent = S.current.replace(/\.md$/i, "").replace(/\//g, " / ");
    el.notePath.title = S.current;
  }
  if (S.filter.kind === "folder") S.filter = { kind: "folder", path: swap(S.filter.path) };
}

async function deleteItem(path: string, isFolder: boolean) {
  const ok = await ask(`Move "${baseName(path)}"${isFolder ? " and everything in it" : ""} to the Recycle Bin?`, {
    title: "Delete",
    kind: "warning",
  });
  if (!ok) return;
  await call("delete_item", { path });
  if (S.current && (S.current === path || S.current.startsWith(path + "/"))) closeNote();
  if (S.filter.kind === "folder" && (S.filter.path === path || S.filter.path.startsWith(path + "/")))
    S.filter = { kind: "all" };
  await refresh();
}

// ---------- vault ----------
const pickVault = () => openVault("pick_vault");
el.vaultBtn.onclick = pickVault;

/** The backend chooses the folder (dialog or last vault); the UI never sends a vault path. */
async function openVault(cmd: "pick_vault" | "open_last_vault") {
  await save();
  const prevName = el.vaultName.textContent;
  el.vaultName.textContent = "Indexing…";
  try {
    const info = await invoke<VaultInfo | null>(cmd);
    if (!info) {
      el.vaultName.textContent = prevName;
      if (!S.vault) showEditor(false);
      return;
    }
    S.vault = info;
  } catch (e) {
    // A refused folder is rejected before anything changes, so the open vault stays usable.
    el.vaultName.textContent = S.vault ? S.vault.name : "Open vault…";
    if (!S.vault) showEditor(false);
    toast(String(e));
    return;
  }
  el.vaultName.textContent = S.vault.name;
  el.vaultBtn.title = S.vault.root;
  S.filter = { kind: "all" };
  S.query = "";
  el.search.value = "";
  closeNote();
  await refresh();
}

// ---------- sidebar & list ----------
async function refresh() {
  if (!S.vault) return;
  const [tree, tags] = await Promise.all([call<Folder>("get_tree"), call<TagCount[]>("get_tags"), loadNotes()]);
  renderTree(tree);
  renderTags(tags);
}
const refreshSoon = debounce(refresh, 250);

async function loadNotes() {
  if (!S.vault) return;
  if (S.query.trim()) {
    S.notes = await call<NoteMeta[]>("search", { query: S.query });
    el.listTitle.textContent = `${S.notes.length} result${S.notes.length === 1 ? "" : "s"}`;
  } else {
    const args =
      S.filter.kind === "folder"
        ? { folder: S.filter.path, tag: null }
        : S.filter.kind === "tag"
          ? { folder: null, tag: S.filter.tag }
          : { folder: null, tag: null };
    S.notes = await call<NoteMeta[]>("list_notes", args);
    const label =
      S.filter.kind === "folder" ? S.filter.path : S.filter.kind === "tag" ? `#${S.filter.tag}` : "All notes";
    el.listTitle.textContent = `${label} · ${S.notes.length}`;
  }
  renderList();
}

function setFilter(f: Filter) {
  S.filter = f;
  S.query = "";
  el.search.value = "";
  loadNotes();
  highlightSidebar();
}

const ICON_CHEV = `<svg viewBox="0 0 16 16" width="12" height="12"><path d="M6 4l4 4-4 4" fill="none" stroke="currentColor" stroke-width="1.6"/></svg>`;
const ICON_FOLDER = `<svg viewBox="0 0 16 16" width="15" height="15"><path d="M1.5 3.5h4.5l1.5 1.5h7v8h-13z" fill="none" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round"/></svg>`;
const ICON_ALL = `<svg viewBox="0 0 16 16" width="15" height="15"><path d="M3 1.5h7l3 3v10H3z M10 1.5v3h3" fill="none" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round"/></svg>`;

function renderTree(root: Folder) {
  const row = (f: Folder, depth: number): string => {
    const open = S.expanded.has(f.path);
    const kids = f.children.length
      ? `<div class="tree-kids" ${open ? "" : "hidden"}>${f.children.map((c) => row(c, depth + 1)).join("")}</div>`
      : "";
    return `<div class="tree-node">
      <div class="side-item" data-folder="${esc(f.path)}" draggable="true" style="padding-left:${8 + depth * 14}px">
        <span class="chev ${f.children.length ? (open ? "open" : "") : "none"}">${ICON_CHEV}</span>${ICON_FOLDER}
        <span class="label">${esc(f.name)}</span><span class="count">${f.count || ""}</span>
      </div>${kids}</div>`;
  };
  el.tree.innerHTML =
    `<div class="side-item" data-all="1" data-folder=""><span class="chev none"></span>${ICON_ALL}<span class="label">All notes</span><span class="count">${root.count || ""}</span></div>` +
    root.children.map((c) => row(c, 0)).join("");
  highlightSidebar();
}

function renderTags(tags: TagCount[]) {
  // Build a real hierarchy so `a/b` always sits under `a` (also when `a` itself is never used as a tag).
  type TagNode = { name: string; tag: string; count: number; kids: Map<string, TagNode> };
  const root: TagNode = { name: "", tag: "", count: 0, kids: new Map() };
  for (const { tag, count } of tags) {
    let node = root;
    for (const part of tag.split("/")) {
      if (!node.kids.has(part))
        node.kids.set(part, { name: part, tag: node.tag ? `${node.tag}/${part}` : part, count: 0, kids: new Map() });
      node = node.kids.get(part)!;
    }
    node.count = count;
  }
  const rows = (n: TagNode, depth: number): string =>
    [...n.kids.values()]
      .sort((a, b) => a.name.localeCompare(b.name))
      .map(
        (k) =>
          `<div class="side-item tag-item" data-tag="${esc(k.tag)}" style="padding-left:${12 + depth * 14}px" title="#${esc(k.tag)}"><span class="hash">#</span><span class="label">${esc(k.name)}</span><span class="count">${k.count || ""}</span></div>` +
          rows(k, depth + 1),
      )
      .join("");
  S.allTags = tags.map((tg) => tg.tag);
  renderTagOptions();
  el.tags.innerHTML = tags.length
    ? rows(root, 0)
    : `<div class="side-empty">No tags yet — add <code>tags: [a, b]</code> in front matter or write #tag</div>`;
  highlightSidebar();
}

function highlightSidebar() {
  document.querySelectorAll<HTMLElement>("#sidebar .side-item").forEach((n) => {
    const f = S.filter;
    const on = S.query
      ? false
      : n.dataset.all
        ? f.kind === "all"
        : n.dataset.tag !== undefined
          ? f.kind === "tag" && f.tag === n.dataset.tag
          : f.kind === "folder" && f.path === n.dataset.folder;
    n.classList.toggle("active", on);
  });
}

function renderList() {
  if (!S.notes.length) {
    el.list.innerHTML = `<div class="list-empty">${S.query ? "No matches" : "No notes here"}</div>`;
    return;
  }
  el.list.innerHTML = S.notes
    .map((n) => {
      const folder = dirOf(n.path);
      const sub = n.snippet
        ? `<div class="snippet">${esc(n.snippet).replace(/\x02/g, "<mark>").replace(/\x03/g, "</mark>")}</div>`
        : "";
      const tags = n.tags.length
        ? `<div class="chips">${n.tags.map((tg) => `<span class="chip">#${esc(tg)}</span>`).join("")}</div>`
        : "";
      return `<div class="note-item" data-path="${esc(n.path)}" draggable="true">
        <div class="title">${esc(n.title)}</div>${sub}
        <div class="meta"><span>${fmtDate(n.mtime)}</span>${folder ? `<span class="folder">${esc(folder)}</span>` : ""}</div>${tags}
      </div>`;
    })
    .join("");
  highlightActiveNote();
}

function highlightActiveNote() {
  el.list.querySelectorAll<HTMLElement>(".note-item").forEach((n) => {
    const on = n.dataset.path === S.current;
    n.classList.toggle("active", on);
    if (on) n.scrollIntoView({ block: "nearest" });
  });
}

function persistExpanded() {
  store.set("expanded", JSON.stringify([...S.expanded]));
}

el.tree.addEventListener("click", (e) => {
  const item = (e.target as HTMLElement).closest<HTMLElement>(".side-item");
  if (!item) return;
  const path = item.dataset.folder!;
  if ((e.target as HTMLElement).closest(".chev") && !item.dataset.all) {
    const kids = item.nextElementSibling as HTMLElement | null;
    if (!kids) return;
    const open = kids.hidden === true;
    kids.hidden = !open;
    item.querySelector(".chev")!.classList.toggle("open", open);
    open ? S.expanded.add(path) : S.expanded.delete(path);
    persistExpanded();
    return;
  }
  setFilter(item.dataset.all ? { kind: "all" } : { kind: "folder", path });
});

el.tags.addEventListener("click", (e) => {
  const item = (e.target as HTMLElement).closest<HTMLElement>("[data-tag]");
  if (item) setFilter({ kind: "tag", tag: item.dataset.tag! });
});

el.list.addEventListener("click", (e) => {
  const item = (e.target as HTMLElement).closest<HTMLElement>(".note-item");
  if (item) openNote(item.dataset.path!);
});

el.search.addEventListener(
  "input",
  debounce(() => {
    S.query = el.search.value;
    loadNotes();
    highlightSidebar();
  }, 150),
);
el.search.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    el.search.value = "";
    S.query = "";
    loadNotes();
    highlightSidebar();
  } else if (e.key === "Enter" && S.notes[0]) openNote(S.notes[0].path);
});

el.newNote.onclick = () => newNote();
el.newFolder.onclick = () => newFolder();

// ---------- drag & drop (move notes/folders into folders) ----------
let dragPath: string | null = null;
document.addEventListener("dragstart", (e) => {
  const item = (e.target as HTMLElement).closest<HTMLElement>("[data-path], [data-folder]");
  if (!item || item.dataset.all) return;
  dragPath = item.dataset.path ?? item.dataset.folder ?? null;
  e.dataTransfer!.effectAllowed = "move";
  e.dataTransfer!.setData("text/plain", dragPath ?? "");
});
el.tree.addEventListener("dragover", (e) => {
  const item = (e.target as HTMLElement).closest<HTMLElement>("[data-folder]");
  if (!item || !dragPath) return;
  e.preventDefault();
  el.tree.querySelectorAll(".drop").forEach((n) => n.classList.remove("drop"));
  item.classList.add("drop");
});
el.tree.addEventListener("dragleave", (e) => (e.target as HTMLElement).closest(".drop")?.classList.remove("drop"));
el.tree.addEventListener("drop", (e) => {
  e.preventDefault();
  el.tree.querySelectorAll(".drop").forEach((n) => n.classList.remove("drop"));
  const item = (e.target as HTMLElement).closest<HTMLElement>("[data-folder]");
  const src = dragPath;
  dragPath = null;
  if (!item || !src) return;
  const dest = item.dataset.folder!;
  if (dirOf(src) === dest || src === dest || dest.startsWith(src + "/")) return;
  moveItem(src, dest);
});
document.addEventListener("dragend", () => (dragPath = null));

// ---------- context menu ----------
type MenuItem = [string, () => void] | "-";
function showMenu(x: number, y: number, items: MenuItem[]) {
  el.menu.innerHTML = "";
  for (const it of items) {
    if (it === "-") {
      el.menu.appendChild(document.createElement("hr"));
      continue;
    }
    const b = document.createElement("button");
    b.textContent = it[0];
    b.onclick = () => {
      hideMenu();
      it[1]();
    };
    el.menu.appendChild(b);
  }
  el.menu.hidden = false;
  const r = el.menu.getBoundingClientRect();
  el.menu.style.left = Math.min(x, innerWidth - r.width - 4) + "px";
  el.menu.style.top = Math.min(y, innerHeight - r.height - 4) + "px";
}
const hideMenu = () => (el.menu.hidden = true);
document.addEventListener("mousedown", (e) => {
  if (!el.menu.contains(e.target as Node)) hideMenu();
});

document.addEventListener("contextmenu", (e) => {
  const target = e.target as HTMLElement;
  if (target.closest("#editor, #search, input")) return; // keep native menu for text editing
  e.preventDefault();
  if (!S.vault) return;
  const tagChip = target.closest<HTMLElement>(".tag-chip");
  if (tagChip) {
    const tg = tagChip.dataset.tag!;
    showMenu(e.clientX, e.clientY, [
      ["Rename on this note…", () => renameTagOnNote(tg)],
      ["Remove from this note", () => applyTagOp("remove", tg)],
      "-",
      [`Show notes with #${tg}`, () => setFilter({ kind: "tag", tag: tg })],
    ]);
    return;
  }
  const sideTag = target.closest<HTMLElement>("#tags [data-tag]");
  if (sideTag && S.current) {
    const tg = sideTag.dataset.tag!;
    showMenu(e.clientX, e.clientY, [[`Add #${tg} to current note`, () => applyTagOp("add", tg)]]);
    return;
  }
  const folderItem = target.closest<HTMLElement>("#tree [data-folder]");
  const noteItem = target.closest<HTMLElement>(".note-item");
  if (folderItem) {
    const p = folderItem.dataset.folder!;
    const items: MenuItem[] = [
      ["New note", () => newNote(p)],
      ["New folder", () => newFolder(p)],
      "-",
      ...importItems(p).map(([label, fn]): MenuItem => [label.replace("…", " here…"), fn]),
    ];
    if (!folderItem.dataset.all)
      items.push("-", ["Rename…", () => renameItem(p, true)], ["Delete", () => deleteItem(p, true)]);
    items.push("-", ["Show in Explorer", () => call("reveal", { path: p })]);
    showMenu(e.clientX, e.clientY, items);
  } else if (noteItem) {
    const p = noteItem.dataset.path!;
    showMenu(e.clientX, e.clientY, [
      ["Open", () => openNote(p)],
      ["Rename…", () => renameItem(p, false)],
      ["Delete", () => deleteItem(p, false)],
      "-",
      ["Show in Explorer", () => call("reveal", { path: p })],
    ]);
  }
});

// ---------- name prompt ----------
function promptName(label: string, initial: string): Promise<string | null> {
  const modal = $("modal");
  const input = $<HTMLInputElement>("modal-input");
  const form = $<HTMLFormElement>("modal-form");
  $("modal-label").textContent = label;
  input.value = initial;
  modal.hidden = false;
  input.focus();
  input.select();
  return new Promise((resolve) => {
    const done = (v: string | null) => {
      modal.hidden = true;
      form.onsubmit = null;
      $("modal-cancel").onclick = null;
      input.onkeydown = null;
      resolve(v);
    };
    form.onsubmit = (e) => {
      e.preventDefault();
      done(input.value.trim() || null);
    };
    $("modal-cancel").onclick = () => done(null);
    input.onkeydown = (e) => {
      if (e.key === "Escape") done(null);
    };
  });
}

// ---------- pane resizing ----------
document.querySelectorAll<HTMLElement>(".resizer").forEach((r) => {
  const v = r.dataset.var!;
  const saved = store.get(v);
  if (saved) document.documentElement.style.setProperty(v, saved);
  r.addEventListener("pointerdown", (e) => {
    const prev = r.previousElementSibling as HTMLElement;
    const startX = e.clientX;
    const startW = prev.getBoundingClientRect().width;
    r.setPointerCapture(e.pointerId);
    const move = (ev: PointerEvent) => {
      const w = Math.max(160, Math.min(520, startW + ev.clientX - startX));
      document.documentElement.style.setProperty(v, w + "px");
    };
    const up = () => {
      r.removeEventListener("pointermove", move);
      store.set(v, getComputedStyle(document.documentElement).getPropertyValue(v));
    };
    r.addEventListener("pointermove", move);
    r.addEventListener("pointerup", up, { once: true });
  });
});

// ---------- keyboard ----------
document.addEventListener("keydown", (e) => {
  if (!(e.ctrlKey || e.metaKey) || e.altKey) return;
  const k = e.key.toLowerCase();
  if (k === "s") {
    e.preventDefault();
    save();
  } else if (k === "n") {
    e.preventDefault();
    newNote();
  } else if (k === "p" || k === "k" || (k === "f" && e.shiftKey)) {
    e.preventDefault();
    el.search.focus();
    el.search.select();
  } else if (k === "t" && S.current) {
    e.preventDefault();
    el.tagInput.focus();
  } else if (k === "e" && S.current) {
    e.preventDefault();
    setMode(S.mode === "edit" ? "split" : S.mode === "split" ? "preview" : "edit");
  }
});

// ---------- external changes ----------
listen<Changes>("vault-changed", async ({ payload }) => {
  refreshSoon();
  const cur = S.current;
  if (!cur) return;
  if (payload.removed.includes(cur) && !payload.updated.includes(cur)) {
    if (S.dirty) {
      el.notice.className = "notice";
      el.notice.textContent = "This note was deleted or moved outside the app. Your text is still here — Ctrl+S recreates it.";
      el.notice.hidden = false;
    } else closeNote();
    return;
  }
  if (!payload.updated.includes(cur)) return;
  const disk = await invoke<string>("read_note", { path: cur }).catch(() => null);
  if (disk === null || disk === S.saved || S.current !== cur) return;
  if (!S.dirty) {
    S.saved = disk;
    setDoc(disk, true);
    await loadCurrentTags();
    if (S.mode !== "edit") renderPreview();
  } else {
    el.notice.className = "notice";
    el.notice.innerHTML = `This note changed on disk. <button id="reload-disk">Load disk version</button> or keep editing to overwrite it.`;
    el.notice.hidden = false;
    $("reload-disk").onclick = () => {
      S.saved = disk;
      S.dirty = false;
      setDoc(disk, true);
      setSaveState("Saved");
      el.notice.hidden = true;
      if (S.mode !== "edit") renderPreview();
    };
  }
});

getCurrentWindow().onCloseRequested(async () => {
  await save();
});

// ---------- boot ----------
setMode(S.mode);
showEditor(false);
openVault("open_last_vault");
