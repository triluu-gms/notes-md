//! Converts an HTML/ENML DOM (from [`crate::xml`]) to Markdown.

use crate::xml::{Elem, Node};

/// Renders an Evernote `<en-media>` element (attachment) as Markdown, if it is known.
pub type MediaFn<'a> = &'a dyn Fn(&Elem) -> Option<String>;

struct Ctx<'a> {
    media: MediaFn<'a>,
    /// Open lists, innermost last.
    lists: usize,
}

pub fn convert(root: &Elem, media: MediaFn) -> String {
    let mut cx = Ctx { media, lists: 0 };
    let mut out = String::new();
    blocks(&root.children, &mut cx, &mut out);
    tidy(&out)
}

fn is_block(name: &str) -> bool {
    matches!(
        name,
        "div" | "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "ul" | "ol" | "li" | "blockquote" | "pre"
            | "table" | "hr" | "en-note" | "html" | "body" | "section" | "article" | "header" | "footer"
            | "center" | "dl" | "dt" | "dd" | "figure" | "form" | "nav" | "aside" | "main"
    )
}

fn style_has(e: &Elem, needle: &str) -> bool {
    e.attr("style").is_some_and(|s| s.replace(' ', "").to_ascii_lowercase().contains(needle))
}

/// Append `block` to `out` as its own paragraph.
fn push_block(out: &mut String, block: &str) {
    let block = block.trim_matches('\n');
    if block.trim().is_empty() {
        return;
    }
    if !out.is_empty() {
        while out.ends_with('\n') {
            out.pop();
        }
        out.push_str("\n\n");
    }
    out.push_str(block);
}

/// Render a mix of block and inline children; runs of inline content become paragraphs.
fn blocks(children: &[Node], cx: &mut Ctx, out: &mut String) {
    let mut para = String::new();
    for n in children {
        match n {
            Node::Elem(e) if is_block(&e.name) => {
                flush(&mut para, cx, out);
                block(e, cx, out);
            }
            _ => inline(n, cx, &mut para),
        }
    }
    flush(&mut para, cx, out);
}

fn flush(para: &mut String, cx: &Ctx, out: &mut String) {
    let t = para.trim_matches(|c: char| c.is_whitespace());
    if !t.is_empty() {
        let mut t = t.to_string();
        // A checkbox outside a list becomes a task-list item.
        if cx.lists == 0 && (t.starts_with("[ ] ") || t.starts_with("[x] ")) {
            t.insert_str(0, "- ");
        } else if t.starts_with('#') {
            t.insert(0, '\\');
        }
        push_block(out, &t);
    }
    para.clear();
}

fn block(e: &Elem, cx: &mut Ctx, out: &mut String) {
    match e.name.as_str() {
        h @ ("h1" | "h2" | "h3" | "h4" | "h5" | "h6") => {
            let mut s = String::new();
            for n in &e.children {
                inline(n, cx, &mut s);
            }
            let s = s.replace("  \n", " ").replace('\n', " ");
            if !s.trim().is_empty() {
                let level = h[1..].parse::<usize>().unwrap_or(1);
                push_block(out, &format!("{} {}", "#".repeat(level), s.trim()));
            }
        }
        "ul" | "ol" => list(e, cx, out),
        "li" => {
            let mut s = String::new();
            list_item(e, "- ", cx, &mut s);
            push_block(out, &s);
        }
        "blockquote" => {
            let mut s = String::new();
            blocks(&e.children, cx, &mut s);
            let quoted: Vec<String> =
                s.lines().map(|l| if l.is_empty() { ">".to_string() } else { format!("> {l}") }).collect();
            push_block(out, &quoted.join("\n"));
        }
        "pre" => push_block(out, &fence(&code_text(e))),
        "hr" => push_block(out, "---"),
        "table" => table(e, cx, out),
        _ if style_has(e, "-en-codeblock:true") => push_block(out, &fence(&code_text(e))),
        _ => {
            let mut s = String::new();
            blocks(&e.children, cx, &mut s);
            push_block(out, &s);
        }
    }
}

fn list(e: &Elem, cx: &mut Ctx, out: &mut String) {
    let ordered = e.name == "ol";
    let mut n: usize = e.attr("start").and_then(|s| s.parse().ok()).unwrap_or(1);
    cx.lists += 1;
    let mut items = Vec::new();
    for child in &e.children {
        let marker = if ordered { format!("{n}. ") } else { "- ".to_string() };
        let mut s = String::new();
        match child {
            Node::Elem(li) if li.name == "li" => list_item(li, &marker, cx, &mut s),
            Node::Elem(inner) if inner.name == "ul" || inner.name == "ol" => {
                // Nested list placed directly in the list (invalid HTML, common in exports): indent it.
                let mut sub = String::new();
                list(inner, cx, &mut sub);
                s = indent(&sub, "  ");
                if let Some(last) = items.last_mut() {
                    *last = format!("{last}\n{s}");
                    continue;
                }
            }
            other => {
                let mut text = String::new();
                inline(other, cx, &mut text);
                if text.trim().is_empty() {
                    continue;
                }
                s = format!("{marker}{}", text.trim());
            }
        }
        if !s.trim().is_empty() {
            items.push(s);
            n += 1;
        }
    }
    cx.lists -= 1;
    push_block(out, &items.join("\n"));
}

fn list_item(li: &Elem, marker: &str, cx: &mut Ctx, out: &mut String) {
    let mut s = String::new();
    blocks(&li.children, cx, &mut s);
    // Keep list items tight.
    let mut body = s.trim().to_string();
    while body.contains("\n\n") {
        body = body.replace("\n\n", "\n");
    }
    let pad = " ".repeat(marker.len());
    let mut lines = body.lines();
    out.push_str(marker);
    out.push_str(lines.next().unwrap_or(""));
    for l in lines {
        out.push('\n');
        out.push_str(&pad);
        out.push_str(l);
    }
}

fn indent(s: &str, pad: &str) -> String {
    s.lines().map(|l| format!("{pad}{l}")).collect::<Vec<_>>().join("\n")
}

fn table(e: &Elem, cx: &mut Ctx, out: &mut String) {
    let mut rows: Vec<Vec<String>> = Vec::new();
    collect_rows(e, cx, &mut rows);
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    if width == 0 {
        return;
    }
    let line = |cells: &[String]| {
        let mut l = String::from("|");
        for i in 0..width {
            l.push(' ');
            l.push_str(cells.get(i).map_or("", String::as_str));
            l.push_str(" |");
        }
        l
    };
    let mut lines = vec![line(&rows[0]), format!("|{}", " --- |".repeat(width))];
    lines.extend(rows[1..].iter().map(|r| line(r)));
    push_block(out, &lines.join("\n"));
}

fn collect_rows(e: &Elem, cx: &mut Ctx, rows: &mut Vec<Vec<String>>) {
    for child in e.elems() {
        match child.name.as_str() {
            "tr" => {
                let cells = child
                    .elems()
                    .filter(|c| c.name == "td" || c.name == "th")
                    .map(|c| {
                        let mut s = String::new();
                        blocks(&c.children, cx, &mut s);
                        s.split_whitespace().collect::<Vec<_>>().join(" ").replace('|', "\\|")
                    })
                    .collect();
                rows.push(cells);
            }
            "table" => {} // nested tables are flattened away
            _ => collect_rows(child, cx, rows),
        }
    }
}

/// Text of a code block: `<br>` and block children become line breaks, nothing else is changed.
fn code_text(e: &Elem) -> String {
    let mut s = String::new();
    fn walk(e: &Elem, s: &mut String) {
        for n in &e.children {
            match n {
                Node::Text(t) => s.push_str(&t.replace('\u{a0}', " ")),
                Node::Elem(c) if c.name == "br" => s.push('\n'),
                Node::Elem(c) => {
                    walk(c, s);
                    if is_block(&c.name) && !s.ends_with('\n') {
                        s.push('\n');
                    }
                }
            }
        }
    }
    walk(e, &mut s);
    s
}

fn fence(code: &str) -> String {
    let mut ticks = "```".to_string();
    while code.contains(&ticks) {
        ticks.push('`');
    }
    format!("{ticks}\n{}\n{ticks}", code.trim_matches('\n'))
}

/// Wrap inline content in a marker, keeping surrounding whitespace outside it.
fn wrap(marker: &str, inner: &str) -> String {
    let t = inner.trim();
    if t.is_empty() {
        return inner.to_string();
    }
    let lead = &inner[..inner.len() - inner.trim_start().len()];
    let trail = &inner[inner.trim_end().len()..];
    format!("{lead}{marker}{t}{marker}{trail}")
}

fn inline_children(e: &Elem, cx: &mut Ctx) -> String {
    let mut s = String::new();
    for n in &e.children {
        inline(n, cx, &mut s);
    }
    s
}

fn encode_url(u: &str) -> String {
    u.trim().replace(' ', "%20").replace('(', "%28").replace(')', "%29")
}

fn inline(n: &Node, cx: &mut Ctx, out: &mut String) {
    let e = match n {
        Node::Text(t) => {
            push_text(out, t);
            return;
        }
        Node::Elem(e) => e,
    };
    match e.name.as_str() {
        "br" => out.push_str("  \n"),
        "b" | "strong" => out.push_str(&wrap("**", &inline_children(e, cx))),
        "i" | "em" | "cite" => out.push_str(&wrap("*", &inline_children(e, cx))),
        "s" | "strike" | "del" => out.push_str(&wrap("~~", &inline_children(e, cx))),
        "code" | "tt" | "kbd" | "samp" => {
            let t = e.text().replace('\u{a0}', " ");
            if !t.trim().is_empty() {
                let ticks = if t.contains('`') { "``" } else { "`" };
                out.push_str(&format!("{ticks}{t}{ticks}"));
            }
        }
        "a" => {
            let label = inline_children(e, cx);
            let href = e.attr("href").unwrap_or("").trim();
            if href.is_empty() || href.starts_with("evernote:") || href.starts_with("javascript:") {
                out.push_str(&label);
            } else if label.trim().is_empty() {
                out.push_str(&format!("<{href}>"));
            } else {
                out.push_str(&format!("[{}]({})", label.trim(), encode_url(href)));
            }
        }
        "img" => {
            if let Some(src) = e.attr("src").filter(|s| !s.trim().is_empty()) {
                let alt = e.attr("alt").unwrap_or("");
                out.push_str(&format!("![{alt}]({})", encode_url(src)));
            }
        }
        "en-media" => {
            if let Some(md) = (cx.media)(e) {
                out.push_str(&md);
            }
        }
        "en-todo" => out.push_str(if e.attr("checked") == Some("true") { "[x] " } else { "[ ] " }),
        "en-crypt" => out.push_str("*[encrypted content]*"),
        "script" | "style" | "head" | "title" | "meta" => {}
        name if is_block(name) => {
            // Block inside inline content (e.g. a div inside a span): keep it on its own line.
            let s = inline_children(e, cx);
            if !s.trim().is_empty() {
                out.push_str(s.trim());
                out.push_str("  \n");
            }
        }
        _ => {
            let s = inline_children(e, cx);
            let s = if style_has(e, "font-weight:bold") || style_has(e, "font-weight:700") {
                wrap("**", &s)
            } else {
                s
            };
            let s = if style_has(e, "font-style:italic") { wrap("*", &s) } else { s };
            let s = if style_has(e, "line-through") { wrap("~~", &s) } else { s };
            out.push_str(&s);
        }
    }
}

/// Append HTML text: whitespace collapses to single spaces and Markdown syntax characters are escaped.
fn push_text(out: &mut String, t: &str) {
    let mut prev_space = out.is_empty() || out.ends_with([' ', '\n']);
    for c in t.chars() {
        if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
            continue;
        }
        prev_space = false;
        if matches!(c, '*' | '`' | '<' | '[' | ']' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
}

fn tidy(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank = 0;
    for line in s.lines() {
        let l = if line.trim().is_empty() { "" } else { line.trim_end_matches(['\t']) };
        if l.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(l);
        out.push('\n');
    }
    out.trim().to_string() + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xml::parse;

    fn md(html: &str) -> String {
        convert(&parse(html), &|_| None)
    }

    #[test]
    fn evernote_note() {
        let html = r#"<en-note><div>First <b>bold</b> and <i>it</i></div><div><br/></div>
            <h2>Title</h2><ul><li>one</li><li>two<ul><li>inner</li></ul></li></ul>
            <div><en-todo checked="true"/>done</div><div><en-todo/>open</div>
            <div style="-en-codeblock: true;"><div>ls -la *</div><div>cd /u01</div></div>
            <table><tr><td>A</td><td>B</td></tr><tr><td>1</td><td>2|3</td></tr></table>
            <div><a href="https://x.com/a b">link</a> 2*3 #notag</div></en-note>"#;
        assert_eq!(
            md(html),
            "First **bold** and *it*\n\n## Title\n\n- one\n- two\n  - inner\n\n- [x] done\n\n- [ ] open\n\n\
             ```\nls -la *\ncd /u01\n```\n\n| A | B |\n| --- | --- |\n| 1 | 2\\|3 |\n\n\
             [link](https://x.com/a%20b) 2\\*3 #notag\n"
        );
    }

    #[test]
    fn line_breaks_and_quotes() {
        assert_eq!(md("<p>a<br>b</p><blockquote><p>q1</p><p>q2</p></blockquote>"), "a  \nb\n\n> q1\n>\n> q2\n");
    }
}
