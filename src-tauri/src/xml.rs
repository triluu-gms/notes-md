//! Tolerant XML/HTML reader for note exports (Evernote ENEX/ENML, Joplin HTML notes): a forgiving
//! tokenizer that builds a tiny DOM. Unknown entities are kept as text and stray end tags are ignored,
//! so real-world exports never fail to parse.

pub enum Node {
    Text(String),
    Elem(Elem),
}

pub struct Elem {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
}

impl Elem {
    fn new(name: &str) -> Self {
        Self { name: name.to_string(), attrs: Vec::new(), children: Vec::new() }
    }

    pub fn attr(&self, key: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    pub fn elems(&self) -> impl Iterator<Item = &Elem> {
        self.children.iter().filter_map(|n| match n {
            Node::Elem(e) => Some(e),
            Node::Text(_) => None,
        })
    }

    pub fn child(&self, name: &str) -> Option<&Elem> {
        self.elems().find(|e| e.name == name)
    }

    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Elem> {
        self.elems().filter(move |e| e.name == name)
    }

    /// First descendant (depth-first) with this name, including self.
    pub fn find(&self, name: &str) -> Option<&Elem> {
        if self.name == name {
            return Some(self);
        }
        self.elems().find_map(|e| e.find(name))
    }

    /// All text below this element, concatenated as-is.
    pub fn text(&self) -> String {
        let mut s = String::new();
        collect_text(self, &mut s);
        s
    }
}

fn collect_text(e: &Elem, out: &mut String) {
    for n in &e.children {
        match n {
            Node::Text(t) => out.push_str(t),
            Node::Elem(c) => collect_text(c, out),
        }
    }
}

const VOID: &[&str] = &[
    "br", "hr", "img", "en-media", "en-todo", "meta", "link", "input", "col", "area", "base", "wbr", "source",
];

/// Parse a document into a synthetic `#root` element.
pub fn parse(src: &str) -> Elem {
    let mut stack = vec![Elem::new("#root")];
    let len = src.len();
    let mut i = 0;
    while i < len {
        let rest = &src[i..];
        if !rest.starts_with('<') {
            let end = rest.find('<').map_or(len, |j| i + j);
            push_text(&mut stack, decode_entities(&src[i..end]));
            i = end;
            continue;
        }
        if rest.starts_with("<!--") {
            i = rest[4..].find("-->").map_or(len, |j| i + 4 + j + 3);
        } else if let Some(cdata) = rest.strip_prefix("<![CDATA[") {
            let end = cdata.find("]]>").unwrap_or(cdata.len());
            push_text(&mut stack, cdata[..end].to_string());
            i = (i + 9 + end + 3).min(len);
        } else if rest.starts_with("<!") || rest.starts_with("<?") {
            i = skip_declaration(src, i);
        } else if let Some(close) = rest.strip_prefix("</") {
            let end = close.find('>').unwrap_or(close.len());
            close_elem(&mut stack, &close[..end].trim().to_ascii_lowercase());
            i = (i + 2 + end + 1).min(len);
        } else if let Some((elem, self_closing, next)) = parse_start_tag(src, i) {
            if self_closing || VOID.contains(&elem.name.as_str()) {
                stack.last_mut().unwrap().children.push(Node::Elem(elem));
            } else {
                stack.push(elem);
            }
            i = next;
        } else {
            push_text(&mut stack, "<".to_string());
            i += 1;
        }
    }
    while stack.len() > 1 {
        let e = stack.pop().unwrap();
        stack.last_mut().unwrap().children.push(Node::Elem(e));
    }
    stack.pop().unwrap()
}

fn push_text(stack: &mut [Elem], text: String) {
    if text.is_empty() {
        return;
    }
    let children = &mut stack.last_mut().unwrap().children;
    if let Some(Node::Text(prev)) = children.last_mut() {
        prev.push_str(&text);
    } else {
        children.push(Node::Text(text));
    }
}

/// Pop up to (and including) the nearest open element with this name; ignore unmatched end tags.
fn close_elem(stack: &mut Vec<Elem>, name: &str) {
    let Some(pos) = stack.iter().rposition(|e| e.name == name) else { return };
    if pos == 0 {
        return;
    }
    while stack.len() > pos {
        let e = stack.pop().unwrap();
        stack.last_mut().unwrap().children.push(Node::Elem(e));
    }
}

/// Skip `<!DOCTYPE ...>` (with an optional `[ ... ]` internal subset) or `<? ... ?>`.
fn skip_declaration(src: &str, start: usize) -> usize {
    let bytes = src.as_bytes();
    let mut depth = 0;
    let mut i = start + 2;
    while i < bytes.len() {
        match bytes[i] {
            b'[' => depth += 1,
            b']' => depth -= 1,
            b'>' if depth <= 0 => return i + 1,
            _ => {}
        }
        i += 1;
    }
    bytes.len()
}

fn is_name_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b':' | b'_' | b'-' | b'.')
}

/// Parse `<name attr="v" ...>` at `start`. Returns (element, self-closing, index after `>`).
fn parse_start_tag(src: &str, start: usize) -> Option<(Elem, bool, usize)> {
    let b = src.as_bytes();
    let mut i = start + 1;
    let name_start = i;
    while i < b.len() && is_name_char(b[i]) {
        i += 1;
    }
    if i == name_start || !b[name_start].is_ascii_alphabetic() {
        return None;
    }
    let mut elem = Elem::new(&src[name_start..i].to_ascii_lowercase());
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() {
            return Some((elem, false, b.len()));
        }
        match b[i] {
            b'>' => return Some((elem, false, i + 1)),
            b'/' if b.get(i + 1) == Some(&b'>') => return Some((elem, true, i + 2)),
            b'/' => {
                i += 1;
                continue;
            }
            _ => {}
        }
        let key_start = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && !matches!(b[i], b'=' | b'>' | b'/') {
            i += 1;
        }
        let key = src[key_start..i].to_ascii_lowercase();
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if b.get(i) == Some(&b'=') {
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            match b.get(i) {
                Some(&q @ (b'"' | b'\'')) => {
                    let end = src[i + 1..].find(q as char).map_or(b.len(), |j| i + 1 + j);
                    value = decode_entities(&src[i + 1..end]);
                    i = (end + 1).min(b.len());
                }
                _ => {
                    let v_start = i;
                    while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'>' {
                        i += 1;
                    }
                    value = decode_entities(&src[v_start..i]);
                }
            }
        }
        if key.is_empty() {
            i += 1; // never loop on an unexpected byte
        } else {
            elem.attrs.push((key, value));
        }
    }
}

/// Decode `&amp;`, `&#38;`, `&#x26;` and common HTML named entities; anything unknown is kept verbatim.
pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let decoded = after.find(';').filter(|&end| end <= 10).and_then(|end| {
            let name = &after[..end];
            let ch = if let Some(num) = name.strip_prefix('#') {
                let code = match num.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => num.parse().ok(),
                };
                code.and_then(char::from_u32).map(String::from)
            } else {
                named_entity(name).map(String::from)
            };
            ch.map(|c| (c, end))
        });
        match decoded {
            Some((c, end)) => {
                out.push_str(&c);
                rest = &after[end + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn named_entity(name: &str) -> Option<&'static str> {
    Some(match name {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" => "\u{a0}",
        "ensp" => "\u{2002}",
        "emsp" => "\u{2003}",
        "thinsp" => "\u{2009}",
        "zwnj" => "\u{200c}",
        "zwj" => "\u{200d}",
        "shy" => "\u{ad}",
        "ndash" => "–",
        "mdash" => "—",
        "lsquo" => "‘",
        "rsquo" => "’",
        "sbquo" => "‚",
        "ldquo" => "“",
        "rdquo" => "”",
        "bdquo" => "„",
        "hellip" => "…",
        "bull" => "•",
        "middot" => "·",
        "laquo" => "«",
        "raquo" => "»",
        "copy" => "©",
        "reg" => "®",
        "trade" => "™",
        "deg" => "°",
        "plusmn" => "±",
        "times" => "×",
        "divide" => "÷",
        "para" => "¶",
        "sect" => "§",
        "euro" => "€",
        "pound" => "£",
        "yen" => "¥",
        "cent" => "¢",
        "iexcl" => "¡",
        "iquest" => "¿",
        "larr" => "←",
        "rarr" => "→",
        "uarr" => "↑",
        "darr" => "↓",
        "harr" => "↔",
        "rArr" => "⇒",
        "le" => "≤",
        "ge" => "≥",
        "ne" => "≠",
        "frac12" => "½",
        "frac14" => "¼",
        "frac34" => "¾",
        "sup2" => "²",
        "sup3" => "³",
        "micro" => "µ",
        "check" => "✓",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tolerant_parse() {
        let root = parse(
            "<?xml version=\"1.0\"?><!DOCTYPE en-note SYSTEM \"x.dtd\" [<!ENTITY a \"b\">]>\
             <en-note><div class=x>a &nbsp;&amp; b &bogus; <b>bold</i></b><br/>\
             <![CDATA[<raw>]]></div></stray></en-note>",
        );
        let note = root.find("en-note").unwrap();
        let div = note.child("div").unwrap();
        assert_eq!(div.attr("class"), Some("x"));
        assert_eq!(div.text(), "a \u{a0}& b &bogus; bold<raw>");
        assert!(div.child("br").is_some());
    }
}
