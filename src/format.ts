// Markdown formatting commands for the toolbar and shortcuts. Each one is a single undoable transaction.
import { EditorSelection, type ChangeSpec, type EditorState } from "@codemirror/state";
import type { EditorView } from "@codemirror/view";

const done = (view: EditorView) => view.focus();

/** Toggle an inline marker (`**`, `_`, `~~`, `` ` ``) around each selection. */
export function toggleWrap(view: EditorView, marker: string, placeholder = "text") {
  const { state } = view;
  const m = marker.length;
  view.dispatch(
    state.changeByRange((r) => {
      const text = state.sliceDoc(r.from, r.to);
      // Already wrapped just outside the selection: unwrap.
      if (state.sliceDoc(r.from - m, r.from) === marker && state.sliceDoc(r.to, r.to + m) === marker) {
        return {
          changes: [
            { from: r.from - m, to: r.from },
            { from: r.to, to: r.to + m },
          ],
          range: EditorSelection.range(r.from - m, r.to - m),
        };
      }
      // Selection includes the markers: unwrap.
      if (text.length >= 2 * m && text.startsWith(marker) && text.endsWith(marker)) {
        return {
          changes: { from: r.from, to: r.to, insert: text.slice(m, -m) },
          range: EditorSelection.range(r.from, r.to - 2 * m),
        };
      }
      const inner = text || placeholder;
      return {
        changes: { from: r.from, to: r.to, insert: marker + inner + marker },
        range: EditorSelection.range(r.from + m, r.from + m + inner.length),
      };
    }),
    { scrollIntoView: true, userEvent: "input.format" },
  );
  done(view);
}

export type BlockKind = "h1" | "h2" | "h3" | "ul" | "ol" | "task" | "quote";

const HAS: Record<BlockKind, RegExp> = {
  h1: /^# /,
  h2: /^## /,
  h3: /^### /,
  ul: /^\s*[-*+] (?!\[[ xX]\] )/,
  ol: /^\s*\d+[.)] /,
  task: /^\s*[-*+] \[[ xX]\] /,
  quote: /^> ?/,
};
const HEADING = /^#{1,6} /;
const LIST = /^(\s*)(?:[-*+] \[[ xX]\] |[-*+] |\d+[.)] )/;
const QUOTE = /^> ?/;

function selectedLines(state: EditorState) {
  const seen = new Set<number>();
  const lines = [];
  for (const r of state.selection.ranges) {
    for (let n = state.doc.lineAt(r.from).number; n <= state.doc.lineAt(r.to).number; n++) {
      if (!seen.has(n)) {
        seen.add(n);
        lines.push(state.doc.line(n));
      }
    }
  }
  return lines.sort((a, b) => a.from - b.from);
}

/** Toggle a line prefix on every selected line; switching between list types or heading levels replaces it. */
export function toggleBlock(view: EditorView, kind: BlockKind) {
  const { state } = view;
  const isList = kind === "ul" || kind === "ol" || kind === "task";
  const family = kind.startsWith("h") ? HEADING : isList ? LIST : QUOTE;
  const all = selectedLines(state);
  const lines = all.length > 1 ? all.filter((l) => l.text.trim()) : all;
  const remove = lines.length > 0 && lines.every((l) => HAS[kind].test(l.text));
  let n = 0;
  const changes: ChangeSpec[] = lines.map((l) => {
    const m = family.exec(l.text);
    const indent = isList ? /^\s*/.exec(l.text)![0] : "";
    const oldLen = m ? m[0].length : indent.length;
    n++;
    const prefix = remove
      ? indent
      : indent + { h1: "# ", h2: "## ", h3: "### ", ul: "- ", ol: `${n}. `, task: "- [ ] ", quote: "> " }[kind];
    return { from: l.from, to: l.from + oldLen, insert: prefix };
  });
  view.dispatch({ changes, scrollIntoView: true, userEvent: "input.format" });
  done(view);
}

/** Wrap the selected lines in a fenced code block (cursor lands where the language goes). */
export function codeBlock(view: EditorView) {
  const { state } = view;
  const r = state.selection.main;
  const first = state.doc.lineAt(r.from);
  const last = state.doc.lineAt(r.to);
  if (r.empty && !first.text.trim()) {
    view.dispatch({
      changes: { from: first.from, to: first.to, insert: "```\n\n```" },
      selection: { anchor: first.from + 3 },
      userEvent: "input.format",
    });
  } else {
    view.dispatch({
      changes: [
        { from: first.from, insert: "```\n" },
        { from: last.to, insert: "\n```" },
      ],
      selection: { anchor: first.from + 3 },
      userEvent: "input.format",
    });
  }
  done(view);
}

/** `[text](url)` or `![alt](path)`: the part still to fill in is selected. */
export function insertLink(view: EditorView, image = false) {
  const { state } = view;
  const bang = image ? "!" : "";
  view.dispatch(
    state.changeByRange((r) => {
      const sel = state.sliceDoc(r.from, r.to);
      const isUrl = /^(https?:\/\/|www\.)\S+$/.test(sel.trim());
      const label = isUrl ? (image ? "alt text" : "link text") : sel || (image ? "alt text" : "link text");
      const url = isUrl ? sel.trim() : image ? "image.png" : "https://";
      const insert = `${bang}[${label}](${url})`;
      // Select whichever part the user still has to type.
      const start = isUrl || !sel ? r.from + bang.length + 1 : r.from + bang.length + label.length + 3;
      const len = isUrl || !sel ? label.length : url.length;
      return { changes: { from: r.from, to: r.to, insert }, range: EditorSelection.range(start, start + len) };
    }),
    { scrollIntoView: true, userEvent: "input.format" },
  );
  done(view);
}

/** Insert a block on its own line(s) after the cursor's line; `select` is the text to select inside it. */
function insertBlock(view: EditorView, block: string, select?: string) {
  const { state } = view;
  const line = state.doc.lineAt(state.selection.main.head);
  const empty = !line.text.trim();
  const before = empty ? "" : "\n\n";
  const from = empty ? line.from : line.to;
  const insert = before + block + "\n";
  const at = select ? from + before.length + block.indexOf(select) : from + insert.length;
  view.dispatch({
    changes: { from, to: empty ? line.to : from, insert },
    selection: EditorSelection.range(at, select ? at + select.length : at),
    scrollIntoView: true,
    userEvent: "input.format",
  });
  done(view);
}

export const insertTable = (view: EditorView) =>
  insertBlock(view, "| Column 1 | Column 2 | Column 3 |\n| --- | --- | --- |\n|  |  |  |\n|  |  |  |", "Column 1");

export const insertRule = (view: EditorView) => insertBlock(view, "---\n");
