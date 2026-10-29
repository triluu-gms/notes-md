//! Extracts title, tags and searchable body from a markdown note.
//!
//! Tags come from YAML front matter (`tags: [a, b]`, `tags: a, b` or a `- a` list)
//! and from inline `#tags` outside code. Tags are lower-cased; `a/b` nests under `a`.

use std::ops::Range;

pub struct Parsed<'a> {
    pub title: String,
    pub tags: Vec<String>,
    /// Content with front matter removed (what gets full-text indexed).
    pub body: &'a str,
}

/// `inline`: also read `#tags` written in the note text (can be switched off per vault).
pub fn parse<'a>(stem: &str, content: &'a str, inline: bool) -> Parsed<'a> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let (fm, body) = split_front_matter(content);
    let mut title = None;
    let mut tags = Vec::new();
    if let Some(fm) = fm {
        parse_front_matter(fm, &mut title, &mut tags);
    }

    let mut code = CodeTracker::default();
    for line in body.lines() {
        if code.skip(line) {
            continue;
        }
        if title.is_none() {
            if let Some(h) = line.trim_start().strip_prefix("# ") {
                let h = h.trim();
                if !h.is_empty() {
                    title = Some(h.to_string());
                }
            }
        }
        if inline {
            tags.extend(inline_tag_spans(line).into_iter().map(|(_, t)| t));
        }
    }

    tags.sort();
    tags.dedup();
    Parsed { title: title.unwrap_or_else(|| stem.to_string()), tags, body }
}

/// Byte range of the front matter's inner text; the closing `---` line starts at the range end.
fn front_matter_range(s: &str) -> Option<(usize, usize)> {
    let start = if s.starts_with("---\r\n") {
        5
    } else if s.starts_with("---\n") {
        4
    } else {
        return None;
    };
    let mut pos = start;
    for line in s[start..].split_inclusive('\n') {
        let l = line.trim_end();
        if l == "---" || l == "..." {
            return Some((start, pos));
        }
        pos += line.len();
    }
    None
}

/// (front matter, body after the closing line).
fn split_front_matter(s: &str) -> (Option<&str>, &str) {
    match front_matter_range(s) {
        Some((a, b)) => {
            let rest = &s[b..];
            let body_at = rest.find('\n').map_or(rest.len(), |i| i + 1);
            (Some(&s[a..b]), &rest[body_at..])
        }
        None => (None, s),
    }
}

fn parse_front_matter(fm: &str, title: &mut Option<String>, tags: &mut Vec<String>) {
    let mut in_list = false;
    for line in fm.lines() {
        if in_list {
            let t = line.trim();
            if let Some(item) = t.strip_prefix('-') {
                push_tag(tags, item);
                continue;
            }
            if t.is_empty() {
                continue;
            }
            in_list = false;
        }
        if line.starts_with([' ', '\t']) {
            continue;
        }
        let Some((key, val)) = line.split_once(':') else { continue };
        let val = val.trim();
        match key.trim().to_ascii_lowercase().as_str() {
            "title" if !val.is_empty() => *title = Some(unquote(val).to_string()),
            "tags" | "tag" => {
                if val.is_empty() {
                    in_list = true;
                } else {
                    let v = val.trim_start_matches('[').trim_end_matches(']');
                    for t in v.split([',', ' ']) {
                        push_tag(tags, t);
                    }
                }
            }
            _ => {}
        }
    }
}

fn unquote(s: &str) -> &str {
    let s = s.trim();
    for q in ['"', '\''] {
        if s.len() >= 2 && s.starts_with(q) && s.ends_with(q) {
            return &s[1..s.len() - 1];
        }
    }
    s
}

fn push_tag(tags: &mut Vec<String>, raw: &str) {
    let t = unquote(raw).trim().trim_start_matches('#').trim_matches('/');
    if !t.is_empty() && t.chars().all(is_fm_tag_char) {
        tags.push(t.to_lowercase());
    }
}

fn is_tag_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '/')
}

/// Front matter is unambiguous, so it may also hold dotted tags like `ebs12.2`.
fn is_fm_tag_char(c: char) -> bool {
    is_tag_char(c) || c == '.'
}

/// Tracks fenced code blocks (``` / ~~~) while walking lines; tags are never read inside code.
#[derive(Default)]
struct CodeTracker {
    fence: Option<&'static str>,
}

impl CodeTracker {
    /// True if `line` is a fence line or inside a fenced block.
    fn skip(&mut self, line: &str) -> bool {
        let t = line.trim_start();
        if let Some(marker) = self.fence {
            if t.starts_with(marker) {
                self.fence = None;
            }
            return true;
        }
        for marker in ["```", "~~~"] {
            if t.starts_with(marker) {
                self.fence = Some(marker);
                return true;
            }
        }
        false
    }
}

/// Inline `#tags` in one line: byte range of the `#tag` text and the lower-cased tag.
fn inline_tag_spans(line: &str) -> Vec<(Range<usize>, String)> {
    let mut out = Vec::new();
    // A line starting with `#word` is usually a shell root prompt (`#cd /etc`) or a script comment,
    // not tags. Only trust it when every word on the line is a tag (`#work #ideas`).
    let t = line.trim_start();
    let starts_with_tag = t.starts_with('#') && !t[1..].starts_with(['#', ' ', '\t']) && t.len() > 1;
    if starts_with_tag && !t.split_whitespace().all(|w| w.starts_with('#')) {
        return out;
    }

    let mut in_code = false;
    let mut prev = ' ';
    let mut chars = line.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '`' {
            in_code = !in_code;
        } else if c == '#' && !in_code && prev.is_whitespace() {
            let start = i + 1;
            let mut end = start;
            while let Some(&(j, d)) = chars.peek() {
                if !is_tag_char(d) {
                    break;
                }
                end = j + d.len_utf8();
                chars.next();
            }
            let tag = line[start..end].trim_end_matches('/');
            // Must end at a word boundary (skips `#ORACLE_SID=+ASM2`, `#X="1"`), contain a letter
            // (skips `#2024`, `#-----`) and not look like a path (`#/usr/bin`).
            let ends_cleanly = line[end..].chars().next().is_none_or(|n| n.is_whitespace() || ",.;:!?)]".contains(n));
            if ends_cleanly && !tag.starts_with('/') && tag.chars().any(char::is_alphabetic) {
                out.push((i..end, tag.to_lowercase()));
            }
            prev = line[..end].chars().next_back().unwrap_or(' ');
            continue;
        }
        prev = c;
    }
    out
}

// ---------------- tag editing ----------------

pub enum TagOp {
    Add(String),
    Remove(String),
    Rename(String, String),
}

/// Normalise user input into a tag (`#Work/Ideas ` -> `work/ideas`), or None if it is not a valid tag.
pub fn clean_tag(raw: &str) -> Option<String> {
    let t = raw.trim().trim_start_matches('#').trim_matches('/');
    (!t.is_empty() && t.chars().all(is_fm_tag_char)).then(|| t.to_lowercase())
}

/// Turn a free-form tag name from another app into a valid tag:
/// `cisco vpn client` -> `cisco-vpn-client`, `Form & Reports 10g` -> `form-and-reports-10g`.
pub fn sanitize_tag(name: &str) -> Option<String> {
    let mut s = String::new();
    for c in name.trim().replace('&', " and ").chars() {
        if c.is_whitespace() || c == '-' {
            if !s.is_empty() && !s.ends_with('-') {
                s.push('-');
            }
        } else if is_fm_tag_char(c) {
            s.push(c);
        }
    }
    clean_tag(s.trim_end_matches('-'))
}

/// Apply a tag change to a note's text. `tags:` in front matter is updated in its existing style
/// (or the front matter is created); inline `#tags` are renamed in place or deleted.
/// All tags in `op` must already be normalised with [`clean_tag`]. With `inline` off, note text is left alone.
pub fn edit_tags(content: &str, op: &TagOp, inline: bool) -> String {
    let (bom, content) = match content.strip_prefix('\u{feff}') {
        Some(c) => ("\u{feff}", c),
        None => ("", content),
    };
    let eol = if content.contains("\r\n") { "\r\n" } else { "\n" };
    let fm_range = front_matter_range(content);
    let (_, body) = split_front_matter(content);

    let new_body = match op {
        _ if !inline => body.to_string(),
        TagOp::Add(_) => body.to_string(),
        TagOp::Remove(t) => edit_inline(body, t, None),
        TagOp::Rename(old, new) => edit_inline(body, old, Some(new)),
    };
    let same = |a: &str, b: &str| a.to_lowercase() == b;
    let apply = |tags: &mut Vec<String>| match op {
        TagOp::Add(t) => {
            let in_text = inline && inline_tags_of(body).contains(t);
            if !tags.iter().any(|x| same(x, t)) && !in_text {
                tags.push(t.clone());
            }
        }
        TagOp::Remove(t) => tags.retain(|x| !same(x, t)),
        TagOp::Rename(old, new) => {
            let mut seen = Vec::new();
            tags.retain_mut(|x| {
                if same(x, old) {
                    *x = new.clone();
                }
                let key = x.to_lowercase();
                let first = !seen.contains(&key);
                seen.push(key);
                first
            });
        }
    };

    let mut out = String::from(bom);
    match fm_range {
        Some((start, end)) => {
            let new_fm = rewrite_fm_tags(&content[start..end], eol, apply);
            // A front matter that only held tags disappears together with its last tag.
            if !new_fm.is_empty() {
                let closing_len = content.len() - end - body.len();
                out.push_str(&content[..start]);
                out.push_str(&new_fm);
                out.push_str(&content[end..end + closing_len]);
            }
        }
        None => {
            let new_fm = rewrite_fm_tags("", eol, apply);
            if !new_fm.is_empty() {
                out.push_str(&format!("---{eol}{new_fm}---{eol}"));
            }
        }
    }
    out.push_str(&new_body);
    out
}

fn inline_tags_of(body: &str) -> Vec<String> {
    let mut code = CodeTracker::default();
    body.lines()
        .filter(|l| !code.skip(l))
        .flat_map(|l| inline_tag_spans(l).into_iter().map(|(_, t)| t))
        .collect()
}

/// Rename (`new = Some`) or delete (`new = None`) every inline `#old` outside code.
fn edit_inline(body: &str, old: &str, new: Option<&str>) -> String {
    let mut out = String::with_capacity(body.len());
    let mut code = CodeTracker::default();
    for line in body.split_inclusive('\n') {
        let text = line.trim_end_matches(['\r', '\n']);
        if code.skip(text) {
            out.push_str(line);
            continue;
        }
        let spans: Vec<_> = inline_tag_spans(text).into_iter().filter(|(_, t)| t == old).collect();
        if spans.is_empty() {
            out.push_str(line);
            continue;
        }
        let mut s = text.to_string();
        for (r, _) in spans.into_iter().rev() {
            match new {
                Some(n) => s.replace_range(r, &format!("#{n}")),
                None => {
                    // Take one neighbouring space with it so no double spaces are left behind.
                    let mut r = r;
                    if s[..r.start].ends_with(' ') {
                        r.start -= 1;
                    } else if s[r.end..].starts_with(' ') {
                        r.end += 1;
                    }
                    s.replace_range(r, "");
                }
            }
        }
        if new.is_none() && s.trim().is_empty() {
            continue; // the line held only tags
        }
        out.push_str(&s);
        out.push_str(&line[text.len()..]);
    }
    out
}

/// Rewrite the `tags:` entry of a front matter body, keeping inline (`[a, b]`) or list (`- a`) style.
/// Returns the new front matter text, or "" when nothing is left in it.
fn rewrite_fm_tags(fm: &str, eol: &str, apply: impl FnOnce(&mut Vec<String>)) -> String {
    let lines: Vec<&str> = fm.lines().collect();
    let mut tags = Vec::new();
    // (first line, end line exclusive, key as written, list style, list item prefix)
    let mut entry: Option<(usize, usize, &str, bool, &str)> = None;
    for (i, line) in lines.iter().enumerate() {
        if line.starts_with([' ', '\t', '-']) {
            continue;
        }
        let Some((key, val)) = line.split_once(':') else { continue };
        if !matches!(key.trim().to_ascii_lowercase().as_str(), "tags" | "tag") {
            continue;
        }
        let val = val.trim();
        let raw = |s: &str| unquote(s).trim().trim_start_matches('#').to_string();
        let mut end = i + 1;
        let mut prefix = "  ";
        if val.is_empty() {
            while end < lines.len() && (lines[end].trim_start().starts_with('-') || lines[end].trim().is_empty()) {
                let l = lines[end];
                if let Some(dash) = l.find('-').filter(|_| !l.trim().is_empty()) {
                    prefix = &l[..dash];
                    tags.push(raw(&l[dash + 1..]));
                }
                end += 1;
            }
            while end > i + 1 && lines[end - 1].trim().is_empty() {
                end -= 1; // leave trailing blank lines alone
            }
        } else {
            tags.extend(val.trim_start_matches('[').trim_end_matches(']').split([',', ' ']).map(raw));
        }
        tags.retain(|t| !t.is_empty());
        entry = Some((i, end, key, val.is_empty(), prefix));
        break;
    }

    apply(&mut tags);

    let new_entry: Vec<String> = match (tags.is_empty(), entry) {
        (true, _) => Vec::new(),
        (false, Some((_, _, key, true, prefix))) => std::iter::once(format!("{key}:"))
            .chain(tags.iter().map(|t| format!("{prefix}- {t}")))
            .collect(),
        (false, e) => vec![format!("{}: [{}]", e.map_or("tags", |e| e.2), tags.join(", "))],
    };
    let mut out: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
    match entry {
        Some((start, end, ..)) => {
            out.splice(start..end, new_entry);
        }
        None => out.extend(new_entry),
    }
    if out.iter().all(|l| l.trim().is_empty()) {
        return String::new();
    }
    out.join(eol) + eol
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn front_matter_and_inline() {
        let src = "---\ntitle: \"Hello\"\ntags: [Work, ideas/app]\n---\n# Heading\nSome #todo and #2024 `#notatag`\n```\n#nope\n```\n";
        let p = parse("file", src, true);
        assert_eq!(p.title, "Hello");
        assert_eq!(p.tags, vec!["ideas/app", "todo", "work"]);
        assert!(p.body.starts_with("# Heading"));
    }

    #[test]
    fn list_tags_and_heading_title() {
        let src = "---\ntags:\n  - alpha\n  - \"beta\"\nother: x\n---\n\n# Tiêu đề\ntext#notag [link](#anchor) #Gamma\n";
        let p = parse("file", src, true);
        assert_eq!(p.title, "Tiêu đề");
        assert_eq!(p.tags, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn shell_prompts_and_separators_are_not_tags() {
        let src = "#cd $ORACLE_HOME/bin\n   #vi /etc/profile\n#/bin/ksh\n#---------\n\
                   #For all active users\n#zfs list\n## Heading #real\n#work #ideas/app\nSee #todo here, and #done.\n  #ORACLE_SID=+ASM2\nx #LD_KERNEL=\"2.4\"\n";
        let p = parse("file", src, true);
        assert_eq!(p.tags, vec!["done", "ideas/app", "real", "todo", "work"]);
    }

    fn add(t: &str) -> TagOp {
        TagOp::Add(t.into())
    }
    fn remove(t: &str) -> TagOp {
        TagOp::Remove(t.into())
    }

    #[test]
    fn add_tag_creates_or_extends_front_matter() {
        assert_eq!(edit_tags("# T\nbody\n", &add("work"), true), "---\ntags: [work]\n---\n# T\nbody\n");
        assert_eq!(
            edit_tags("---\ntitle: X\ntags: [a, B]\n---\nbody", &add("c"), true),
            "---\ntitle: X\ntags: [a, B, c]\n---\nbody"
        );
        // list style and CRLF are preserved
        assert_eq!(
            edit_tags("---\r\ntags:\r\n    - a\r\nother: 1\r\n---\r\nx\r\n", &add("b"), true),
            "---\r\ntags:\r\n    - a\r\n    - b\r\nother: 1\r\n---\r\nx\r\n"
        );
        // front matter without tags key
        assert_eq!(edit_tags("---\ntitle: X\n---\nx", &add("a"), true), "---\ntitle: X\ntags: [a]\n---\nx");
        // already present (front matter, case-insensitive, or inline): unchanged
        let s = "---\ntags: [Work]\n---\nsee #todo\n";
        assert_eq!(edit_tags(s, &add("work"), true), s);
        assert_eq!(edit_tags(s, &add("todo"), true), s);
    }

    #[test]
    fn remove_tag_everywhere_outside_code() {
        let s = "---\ntitle: X\ntags: [a, b]\n---\nTask one #a\n#a #b\n```\n#a in code\n```\nkeep #ab\n";
        assert_eq!(
            edit_tags(s, &remove("a"), true),
            "---\ntitle: X\ntags: [b]\n---\nTask one\n#b\n```\n#a in code\n```\nkeep #ab\n"
        );
        // removing the last tag drops a tags-only front matter
        assert_eq!(edit_tags("---\ntags:\n  - a\n---\ntext\n", &remove("a"), true), "text\n");
        // a line that only had the tag disappears
        assert_eq!(edit_tags("x\n#a\ny\n", &remove("a"), true), "x\ny\n");
    }

    #[test]
    fn rename_tag_in_place() {
        let s = "---\ntags: [old, other]\n---\nText #Old, more.\n";
        assert_eq!(
            edit_tags(s, &TagOp::Rename("old".into(), "new/sub".into()), true),
            "---\ntags: [new/sub, other]\n---\nText #new/sub, more.\n"
        );
        // renaming onto an existing tag merges them
        assert_eq!(
            edit_tags("---\ntags: [a, b]\n---\n", &TagOp::Rename("a".into(), "b".into()), true),
            "---\ntags: [b]\n---\n"
        );
        assert_eq!(clean_tag(" #Work/Ideas/ "), Some("work/ideas".into()));
        assert_eq!(clean_tag("two words"), None);
    }

    #[test]
    fn inline_off_ignores_text_tags() {
        let s = "---
id: 1
tags: [ebs12.2]
---
#vda-install
see #todo
";
        assert_eq!(parse("f", s, false).tags, vec!["ebs12.2"]);
        // removing/adding only touches front matter
        assert_eq!(edit_tags(s, &remove("todo"), false), s);
        assert_eq!(
            edit_tags(s, &add("todo"), false),
            "---
id: 1
tags: [ebs12.2, todo]
---
#vda-install
see #todo
"
        );
    }

    #[test]
    fn sanitize_foreign_tag_names() {
        assert_eq!(sanitize_tag("cisco vpn client").as_deref(), Some("cisco-vpn-client"));
        assert_eq!(sanitize_tag("form & reports 10g").as_deref(), Some("form-and-reports-10g"));
        assert_eq!(sanitize_tag("AWS").as_deref(), Some("aws"));
        assert_eq!(sanitize_tag("ebs12.2").as_deref(), Some("ebs12.2"));
        assert_eq!(sanitize_tag(" ! "), None);
    }

    #[test]
    fn no_front_matter_uses_stem() {
        let p = parse("my note", "just text\n## not h1\n", true);
        assert_eq!(p.title, "my note");
        assert!(p.tags.is_empty());
    }
}
