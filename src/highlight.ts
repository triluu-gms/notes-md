// Syntax highlighting for code blocks in the preview. Only popular languages are bundled to stay light.
import hljs from "highlight.js/lib/core";
import bash from "highlight.js/lib/languages/bash";
import shell from "highlight.js/lib/languages/shell";
import powershell from "highlight.js/lib/languages/powershell";
import dos from "highlight.js/lib/languages/dos";
import sql from "highlight.js/lib/languages/sql";
import pgsql from "highlight.js/lib/languages/pgsql";
import xml from "highlight.js/lib/languages/xml";
import json from "highlight.js/lib/languages/json";
import yaml from "highlight.js/lib/languages/yaml";
import ini from "highlight.js/lib/languages/ini";
import properties from "highlight.js/lib/languages/properties";
import python from "highlight.js/lib/languages/python";
import javascript from "highlight.js/lib/languages/javascript";
import typescript from "highlight.js/lib/languages/typescript";
import java from "highlight.js/lib/languages/java";
import kotlin from "highlight.js/lib/languages/kotlin";
import csharp from "highlight.js/lib/languages/csharp";
import vbnet from "highlight.js/lib/languages/vbnet";
import c from "highlight.js/lib/languages/c";
import cpp from "highlight.js/lib/languages/cpp";
import go from "highlight.js/lib/languages/go";
import rust from "highlight.js/lib/languages/rust";
import php from "highlight.js/lib/languages/php";
import ruby from "highlight.js/lib/languages/ruby";
import perl from "highlight.js/lib/languages/perl";
import css from "highlight.js/lib/languages/css";
import scss from "highlight.js/lib/languages/scss";
import dockerfile from "highlight.js/lib/languages/dockerfile";
import nginx from "highlight.js/lib/languages/nginx";
import apache from "highlight.js/lib/languages/apache";
import makefile from "highlight.js/lib/languages/makefile";
import diff from "highlight.js/lib/languages/diff";
import markdown from "highlight.js/lib/languages/markdown";
import plaintext from "highlight.js/lib/languages/plaintext";

const LANGS = {
  bash, shell, powershell, dos, sql, pgsql, xml, json, yaml, ini, properties, python, javascript, typescript,
  java, kotlin, csharp, vbnet, c, cpp, go, rust, php, ruby, perl, css, scss, dockerfile, nginx, apache,
  makefile, diff, markdown, plaintext,
};
for (const [name, lang] of Object.entries(LANGS)) hljs.registerLanguage(name, lang);
hljs.registerAliases(["plsql", "oracle", "sqlplus", "mysql", "tsql", "mssql"], { languageName: "sql" });
hljs.registerAliases(["cmd", "batch"], { languageName: "dos" });
hljs.registerAliases(["conf", "cfg", "toml"], { languageName: "ini" });
hljs.registerAliases(["text", "txt"], { languageName: "plaintext" });

/** Languages guessed for code blocks without a language tag. */
const AUTO = ["bash", "sql", "xml", "json", "yaml", "ini", "powershell", "python", "javascript", "java", "css"];
const AUTO_MAX_CHARS = 20_000;
const AUTO_MIN_RELEVANCE = 6;

const esc = (s: string) => s.replace(/[&<>"]/g, (ch) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[ch]!);

// Re-rendering the preview on every keystroke would otherwise re-highlight every block.
const cache = new Map<string, string>();

/** Highlighted `<pre>` block with a language label and a copy button. */
export function renderCode(text: string, lang?: string): string {
  const key = `${lang ?? ""}\u0000${text}`;
  const hit = cache.get(key);
  if (hit) return hit;

  const want = (lang ?? "").trim().split(/\s+/)[0].toLowerCase();
  let html = esc(text);
  let label = want;
  try {
    if (want && hljs.getLanguage(want)) {
      html = hljs.highlight(text, { language: want, ignoreIllegals: true }).value;
    } else if (!want && text.length <= AUTO_MAX_CHARS) {
      const r = hljs.highlightAuto(text, AUTO);
      if (r.language && r.relevance >= AUTO_MIN_RELEVANCE) {
        html = r.value;
        label = r.language;
      }
    }
  } catch {
    // fall back to plain, escaped text
  }

  const out =
    `<div class="code-block"><div class="code-head"><span class="code-lang">${esc(label)}</span>` +
    `<button class="code-copy" type="button" title="Copy code">Copy</button></div>` +
    `<pre><code class="hljs">${html}</code></pre></div>`;
  if (cache.size > 400) cache.clear();
  cache.set(key, out);
  return out;
}
