//! Import from Evernote (`.enex`) and Joplin (`.jex`) exports into plain markdown files.
//!
//! Notebooks become folders, attachments go to `_resources/`, and tags, dates and source URL go into
//! front matter. Existing files are never overwritten: name clashes get a ` (1)` suffix.

use crate::{html2md, parse, xml};
use base64::Engine;
use md5::{Digest, Md5};
use serde::Serialize;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const RESOURCES: &str = "_resources";

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub notes: usize,
    pub notebooks: usize,
    pub attachments: usize,
    pub tags: usize,
    /// Items that could not be imported, with the reason.
    pub skipped: Vec<String>,
    /// Vault-relative folder the import went into.
    pub folder: String,
}

/// Writes into the vault without ever overwriting, and remembers names it has handed out.
struct Writer<'a> {
    root: &'a Path,
    taken: HashSet<String>,
    tags: HashSet<String>,
    report: Report,
}

impl<'a> Writer<'a> {
    fn new(root: &'a Path, folder: &str) -> Self {
        Self {
            root,
            taken: HashSet::new(),
            tags: HashSet::new(),
            report: Report { folder: folder.to_string(), ..Report::default() },
        }
    }

    fn abs(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    /// A free vault-relative path `dir/stem[ (n)]ext`.
    fn alloc(&mut self, dir: &str, stem: &str, ext: &str) -> String {
        for n in 0.. {
            let name = if n == 0 { format!("{stem}{ext}") } else { format!("{stem} ({n}){ext}") };
            let rel = join(dir, &name);
            if !self.taken.contains(&rel.to_lowercase()) && !self.abs(&rel).exists() {
                self.taken.insert(rel.to_lowercase());
                return rel;
            }
        }
        unreachable!()
    }

    fn mkdir(&self, rel: &str) -> io::Result<()> {
        fs::create_dir_all(self.abs(rel))
    }

    fn write(&mut self, rel: &str, bytes: &[u8], modified: Option<SystemTime>) -> io::Result<()> {
        let path = self.abs(rel);
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(&path, bytes)?;
        if let Some(t) = modified {
            if let Ok(f) = fs::File::options().write(true).open(&path) {
                let _ = f.set_modified(t);
            }
        }
        Ok(())
    }

    fn note(&mut self, rel: &str, meta: &NoteMeta, body: &str) -> io::Result<()> {
        let mut tags: Vec<String> = Vec::new();
        for t in meta.tags.iter().filter_map(|t| parse::sanitize_tag(t)) {
            if !tags.contains(&t) {
                tags.push(t);
            }
        }
        self.tags.extend(tags.iter().cloned());
        let mut fm = format!("---\ntitle: {}\n", yaml_str(&meta.title));
        for (key, val) in [("created", &meta.created), ("updated", &meta.updated)] {
            if let Some((iso, _)) = val {
                fm.push_str(&format!("{key}: {iso}\n"));
            }
        }
        for (key, val) in [("source", &meta.source), ("author", &meta.author)] {
            if let Some(v) = val.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
                fm.push_str(&format!("{key}: {}\n", yaml_str(v)));
            }
        }
        if !tags.is_empty() {
            fm.push_str(&format!("tags: [{}]\n", tags.join(", ")));
        }
        fm.push_str("---\n\n");
        let modified = meta.updated.as_ref().or(meta.created.as_ref()).map(|(_, t)| *t);
        self.write(rel, (fm + body.trim_start_matches('\n')).as_bytes(), modified)?;
        self.report.notes += 1;
        Ok(())
    }

    fn finish(mut self) -> Report {
        self.report.tags = self.tags.len();
        self.report
    }
}

#[derive(Default)]
struct NoteMeta {
    title: String,
    created: Option<(String, SystemTime)>,
    updated: Option<(String, SystemTime)>,
    source: Option<String>,
    author: Option<String>,
    tags: Vec<String>,
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") }
}

fn yaml_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace(['\n', '\r'], " "))
}

/// A file/folder name that is valid on Windows.
fn safe_name(s: &str, fallback: &str) -> String {
    let cleaned: String =
        s.chars().map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { '-' } else { c }).collect();
    let mut t: String = cleaned.trim().chars().take(120).collect();
    while t.ends_with(['.', ' ']) {
        t.pop();
    }
    let t = t.trim().to_string();
    let reserved = ["CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "LPT1", "LPT2", "LPT3"];
    if t.is_empty() {
        fallback.to_string()
    } else if reserved.contains(&t.to_uppercase().as_str()) {
        format!("{t}_")
    } else {
        t
    }
}

/// Split a file name into a safe stem and its extension (with dot).
fn split_name(name: &str, fallback: &str) -> (String, String) {
    let name = safe_name(name, fallback);
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() && ext.len() <= 8 && !ext.contains(' ') => {
            (stem.to_string(), format!(".{ext}"))
        }
        _ => (name, String::new()),
    }
}

/// Relative link from a note in `from_dir` to vault path `to`, URL-encoded for Markdown.
fn rel_link(from_dir: &str, to: &str) -> String {
    let from: Vec<&str> = from_dir.split('/').filter(|s| !s.is_empty()).collect();
    let to_parts: Vec<&str> = to.split('/').collect();
    let common = from.iter().zip(&to_parts).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<&str> = vec![".."; from.len() - common];
    parts.extend(&to_parts[common..]);
    parts.join("/").replace('%', "%25").replace(' ', "%20").replace('(', "%28").replace(')', "%29")
}

/// `20130623T132036Z` (Evernote) or `2012-12-13T03:07:08.000Z` (Joplin) -> ISO string and time.
fn parse_time(s: &str) -> Option<(String, SystemTime)> {
    let digits: Vec<u32> = s.chars().filter_map(|c| c.to_digit(10)).take(14).collect();
    if digits.len() < 14 {
        return None;
    }
    let num = |a: usize, b: usize| digits[a..b].iter().fold(0i64, |acc, d| acc * 10 + *d as i64);
    let (y, mo, d, h, mi, sec) = (num(0, 4), num(4, 6), num(6, 8), num(8, 10), num(10, 12), num(12, 14));
    if y < 1971 || !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    // Days since 1970-01-01 (Howard Hinnant's days_from_civil).
    let (yy, mm) = if mo <= 2 { (y - 1, mo + 9) } else { (y, mo - 3) };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let doy = (153 * mm + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + h * 3600 + mi * 60 + sec;
    let iso = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{sec:02}Z");
    Some((iso, UNIX_EPOCH + Duration::from_secs(secs as u64)))
}

// ---------------------------------------------------------------- Evernote

/// Import `.enex` files into `folder`; each file is one notebook (folder named after the file).
pub fn evernote(root: &Path, folder: &str, files: &[PathBuf]) -> Result<Report, String> {
    let mut w = Writer::new(root, folder);
    let res_dir = join(folder, RESOURCES);
    for file in files {
        let bytes = fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
        let doc = xml::parse(&String::from_utf8_lossy(&bytes));
        let notebook = safe_name(&file.file_stem().unwrap_or_default().to_string_lossy(), "Evernote");
        let dir = join(folder, &notebook);
        w.mkdir(&dir).map_err(|e| e.to_string())?;
        w.report.notebooks += 1;
        let export = doc.find("en-export").unwrap_or(&doc);
        for note in export.children_named("note") {
            if let Err(e) = evernote_note(&mut w, note, &dir, &res_dir) {
                let title = note.child("title").map(|t| t.text()).unwrap_or_default();
                w.report.skipped.push(format!("{notebook}/{title}: {e}"));
            }
        }
    }
    Ok(w.finish())
}

fn evernote_note(w: &mut Writer, note: &xml::Elem, dir: &str, res_dir: &str) -> io::Result<()> {
    let text = |name: &str| note.child(name).map(|e| e.text().trim().to_string()).filter(|s| !s.is_empty());
    let attrs = note.child("note-attributes");
    let attr = |name: &str| attrs.and_then(|a| a.child(name)).map(|e| e.text().trim().to_string());
    let meta = NoteMeta {
        title: text("title").unwrap_or_else(|| "Untitled".into()),
        created: text("created").and_then(|s| parse_time(&s)),
        updated: text("updated").and_then(|s| parse_time(&s)),
        source: attr("source-url"),
        author: attr("author"),
        tags: note.children_named("tag").map(|t| t.text()).collect(),
    };

    // Attachments: save each one and remember it by the MD5 hash <en-media> refers to.
    struct Media {
        rel: String,
        name: String,
        image: bool,
    }
    let mut media: HashMap<String, Media> = HashMap::new();
    let mut order = Vec::new();
    for r in note.children_named("resource") {
        let Some(data) = r.child("data") else { continue };
        let b64: String = data.text().chars().filter(|c| !c.is_whitespace()).collect();
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64.as_bytes()) else { continue };
        let mime = r.child("mime").map(|m| m.text().trim().to_string()).unwrap_or_default();
        let file_name = r
            .child("resource-attributes")
            .and_then(|a| a.child("file-name"))
            .map(|f| f.text())
            .filter(|f| !f.trim().is_empty())
            .unwrap_or_else(|| format!("attachment.{}", mime.rsplit('/').next().unwrap_or("bin")));
        let (stem, ext) = split_name(&file_name, "attachment");
        let rel = w.alloc(res_dir, &stem, &ext);
        w.write(&rel, &bytes, None)?;
        w.report.attachments += 1;
        let hash = format!("{:x}", Md5::digest(&bytes));
        order.push(hash.clone());
        media.insert(hash, Media { rel, name: file_name.trim().to_string(), image: mime.starts_with("image/") });
    }

    let used = RefCell::new(HashSet::new());
    let link = |m: &Media| {
        let target = rel_link(dir, &m.rel);
        let label = m.name.replace(['[', ']'], "");
        if m.image { format!("![{label}]({target})") } else { format!("[{label}]({target})") }
    };
    let render_media = |e: &xml::Elem| {
        let m = media.get(e.attr("hash")?)?;
        used.borrow_mut().insert(e.attr("hash")?.to_string());
        Some(link(m))
    };
    let content = note.child("content").map(|c| c.text()).unwrap_or_default();
    let enml = xml::parse(&content);
    let mut body = html2md::convert(enml.find("en-note").unwrap_or(&enml), &render_media);

    // Attachments that the note text never shows.
    let unused: Vec<String> =
        order.iter().filter(|h| !used.borrow().contains(*h)).filter_map(|h| media.get(h)).map(link).collect();
    if !unused.is_empty() {
        body.push_str("\n## Attachments\n\n");
        for l in unused {
            body.push_str(&format!("- {l}\n"));
        }
    }

    let rel = w.alloc(dir, &safe_name(&meta.title, "Untitled"), ".md");
    w.note(&rel, &meta, &body)
}

// ---------------------------------------------------------------- Joplin

/// One serialized Joplin item: `title`, blank line, `body`, blank line, `key: value` properties.
struct JItem {
    title: String,
    body: String,
    props: HashMap<String, String>,
}

impl JItem {
    fn prop(&self, key: &str) -> &str {
        self.props.get(key).map_or("", String::as_str)
    }
    fn kind(&self) -> u32 {
        self.prop("type_").parse().unwrap_or(0)
    }
}

fn parse_jitem(src: &str) -> Option<JItem> {
    let src = src.replace("\r\n", "\n");
    let lines: Vec<&str> = src.split('\n').collect();
    let mut end = lines.len();
    while end > 0 && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    // Properties are the lines after the last blank line.
    let start = lines[..end].iter().rposition(|l| l.trim().is_empty()).map_or(0, |i| i + 1);
    let mut props = HashMap::new();
    for l in &lines[start..end] {
        let (k, v) = l.split_once(':')?;
        props.insert(k.trim().to_string(), v.trim_start().replace("\\n", "\n"));
    }
    let mut item = JItem { title: String::new(), body: String::new(), props };
    if matches!(item.kind(), 1 | 2 | 4 | 5) && start > 0 {
        item.title = lines[0].trim().to_string();
        let body_end = start.saturating_sub(1).max(1);
        if body_end > 2 {
            item.body = lines[2..body_end].join("\n");
        }
    }
    Some(item)
}

/// Import a `.jex` (Joplin export: a tar of serialized items plus `resources/`) into `folder`.
pub fn joplin(root: &Path, folder: &str, file: &Path) -> Result<Report, String> {
    let open = || fs::File::open(file).map_err(|e| format!("{}: {e}", file.display()));
    let mut items = Vec::new();
    let mut archive = tar::Archive::new(open()?);
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path().map_err(|e| e.to_string())?.to_string_lossy().replace('\\', "/");
        if path.ends_with(".md") && !path.contains('/') {
            let mut s = String::new();
            if io::Read::read_to_string(&mut entry, &mut s).is_ok() {
                items.extend(parse_jitem(&s));
            }
        }
    }

    let mut w = Writer::new(root, folder);
    let by_kind = |k: u32| items.iter().filter(move |i| i.kind() == k);
    let folders: HashMap<&str, &JItem> = by_kind(2).map(|f| (f.prop("id"), f)).collect();
    let tag_names: HashMap<&str, &str> = by_kind(5).map(|t| (t.prop("id"), t.title.as_str())).collect();
    let mut note_tags: HashMap<&str, Vec<String>> = HashMap::new();
    for nt in by_kind(6) {
        if let Some(name) = tag_names.get(nt.prop("tag_id")) {
            note_tags.entry(nt.prop("note_id")).or_default().push(name.to_string());
        }
    }

    // Notebook folders (Joplin notebooks nest via parent_id).
    let mut folder_paths: HashMap<String, String> = HashMap::new();
    fn folder_path<'a>(
        id: &'a str,
        folders: &HashMap<&'a str, &'a JItem>,
        cache: &mut HashMap<String, String>,
        base: &str,
        depth: usize,
    ) -> String {
        if let Some(p) = cache.get(id) {
            return p.clone();
        }
        let Some(f) = folders.get(id).filter(|_| depth < 32) else { return base.to_string() };
        let parent = folder_path(f.prop("parent_id"), folders, cache, base, depth + 1);
        let p = join(&parent, &safe_name(&f.title, "Notebook"));
        cache.insert(id.to_string(), p.clone());
        p
    }
    for id in folders.keys() {
        let p = folder_path(id, &folders, &mut folder_paths, folder, 0);
        w.mkdir(&p).map_err(|e| e.to_string())?;
    }
    w.report.notebooks = folders.len();

    // Plan every file first so notes can link to each other and to attachments.
    let notes: Vec<&JItem> = by_kind(1)
        .filter(|n| n.prop("is_conflict") != "1" && n.prop("deleted_time").parse::<i64>().unwrap_or(0) == 0)
        .collect();
    let mut targets: HashMap<String, String> = HashMap::new(); // Joplin id -> vault path
    let mut note_paths = Vec::new();
    for n in &notes {
        let dir = folder_paths.get(n.prop("parent_id")).cloned().unwrap_or_else(|| folder.to_string());
        let rel = w.alloc(&dir, &safe_name(&n.title, "Untitled"), ".md");
        targets.insert(n.prop("id").to_string(), rel.clone());
        note_paths.push((dir, rel));
    }
    let res_dir = join(folder, RESOURCES);
    let mut resources: HashMap<String, String> = HashMap::new(); // resource id -> vault path
    for r in by_kind(4) {
        let ext = r.prop("file_extension");
        let mut name = if r.title.is_empty() { r.prop("id").to_string() } else { r.title.clone() };
        if !ext.is_empty() && !name.to_lowercase().ends_with(&format!(".{}", ext.to_lowercase())) {
            name = format!("{name}.{ext}");
        }
        let (stem, ext) = split_name(&name, "attachment");
        let rel = w.alloc(&res_dir, &stem, &ext);
        targets.insert(r.prop("id").to_string(), rel.clone());
        resources.insert(r.prop("id").to_string(), rel);
    }

    // Second pass over the archive: copy attachment files.
    let mut archive = tar::Archive::new(open()?);
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path().map_err(|e| e.to_string())?.to_string_lossy().replace('\\', "/");
        let Some(name) = path.strip_prefix("resources/") else { continue };
        let id = name.split('.').next().unwrap_or("");
        let Some(rel) = resources.get(id) else { continue };
        let dest = w.abs(rel);
        let copied = dest.parent().map_or(Ok(()), fs::create_dir_all).and_then(|_| {
            let mut out = fs::File::create(&dest)?;
            io::copy(&mut entry, &mut out).map(|_| ())
        });
        match copied {
            Ok(()) => w.report.attachments += 1,
            Err(e) => w.report.skipped.push(format!("attachment {name}: {e}")),
        }
    }

    for (n, (dir, rel)) in notes.iter().zip(note_paths) {
        let body = if n.prop("markup_language") == "2" {
            let dom = xml::parse(&n.body);
            html2md::convert(dom.find("body").unwrap_or(&dom), &|_| None)
        } else {
            n.body.clone()
        };
        let body = rewrite_joplin_links(&body, &dir, &targets);
        let time = |k: &str| Some(n.prop(k)).filter(|s| !s.is_empty()).and_then(parse_time);
        let meta = NoteMeta {
            title: n.title.clone(),
            created: time("user_created_time").or_else(|| time("created_time")),
            updated: time("user_updated_time").or_else(|| time("updated_time")),
            source: Some(n.prop("source_url").to_string()),
            author: Some(n.prop("author").to_string()),
            tags: note_tags.remove(n.prop("id")).unwrap_or_default(),
        };
        if let Err(e) = w.note(&rel, &meta, &body) {
            w.report.skipped.push(format!("{}: {e}", n.title));
        }
    }
    Ok(w.finish())
}

/// Joplin links to notes and attachments as `:/<32 hex id>`; point them at the imported files.
fn rewrite_joplin_links(body: &str, note_dir: &str, targets: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(i) = rest.find(":/") {
        let cand = &rest[i + 2..];
        if cand.len() >= 32 && cand.as_bytes()[..32].iter().all(u8::is_ascii_hexdigit) {
            if let Some(target) = targets.get(&cand[..32]) {
                out.push_str(&rest[..i]);
                out.push_str(&rel_link(note_dir, target));
                rest = &cand[32..];
                continue;
            }
        }
        out.push_str(&rest[..i + 2]);
        rest = cand;
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_and_names() {
        let (iso, t) = parse_time("20130623T132036Z").unwrap();
        assert_eq!(iso, "2013-06-23T13:20:36Z");
        assert_eq!(t.duration_since(UNIX_EPOCH).unwrap().as_secs(), 1_371_993_636);
        assert_eq!(parse_time("2012-12-13T03:07:08.000Z").unwrap().0, "2012-12-13T03:07:08Z");
        assert_eq!(safe_name(" a/b: c?. ", "x"), "a-b- c-");
        assert_eq!(safe_name("con", "x"), "con_");
        assert_eq!(rel_link("A/B", "A/_resources/my img.png"), "../_resources/my%20img.png");
        assert_eq!(rel_link("", "_resources/x.png"), "_resources/x.png");
    }

    #[test]
    fn joplin_item_and_links() {
        let item = parse_jitem(
            "My note\n\nBody line\n\n![img](:/0123456789abcdef0123456789abcdef)\n\nid: aa\nparent_id: bb\ntype_: 1",
        )
        .unwrap();
        assert_eq!(item.title, "My note");
        assert_eq!(item.body, "Body line\n\n![img](:/0123456789abcdef0123456789abcdef)");
        assert_eq!(item.prop("parent_id"), "bb");
        let targets = HashMap::from([("0123456789abcdef0123456789abcdef".to_string(), "_resources/a b.png".to_string())]);
        assert_eq!(rewrite_joplin_links(&item.body, "Notebook", &targets), "Body line\n\n![img](../_resources/a%20b.png)");
    }

    #[test]
    fn joplin_end_to_end() {
        let root = std::env::temp_dir().join(format!("notesmd-jex-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let (f1, f2, n1, n2, r1, t1) = (
            "f1000000000000000000000000000001",
            "f2000000000000000000000000000002",
            "a1000000000000000000000000000001",
            "a2000000000000000000000000000002",
            "c1000000000000000000000000000001",
            "e1000000000000000000000000000001",
        );
        let items = [
            format!("Oracle\n\nid: {f1}\nparent_id: \ntype_: 2"),
            format!("RAC\n\nid: {f2}\nparent_id: {f1}\ntype_: 2"),
            format!(
                "Start CRS\n\nRun this:\n\n![diagram](:/{r1})\n\nSee [other](:/{n2})\n\nid: {n1}\nparent_id: {f2}\n\
                 created_time: 2012-12-13T03:07:08.000Z\nupdated_time: 2024-08-04T04:21:46.036Z\n\
                 user_updated_time: 2020-01-02T03:04:05.000Z\nsource_url: http://x.com\nis_conflict: 0\nmarkup_language: 1\ntype_: 1"
            ),
            format!("Other note\n\n<p>Hello <b>html</b></p>\n\nid: {n2}\nparent_id: {f1}\nmarkup_language: 2\ntype_: 1"),
            format!("rac\n\nid: {t1}\ntype_: 5"),
            format!("id: x1\nnote_id: {n1}\ntag_id: {t1}\ntype_: 6"),
            format!("crs diagram.png\n\nid: {r1}\nmime: image/png\nfile_extension: png\ntype_: 4"),
        ];
        let jex = root.join("export.jex");
        {
            let mut b = tar::Builder::new(fs::File::create(&jex).unwrap());
            let mut add = |name: &str, data: &[u8]| {
                let mut h = tar::Header::new_gnu();
                h.set_size(data.len() as u64);
                h.set_mode(0o644);
                h.set_cksum();
                b.append_data(&mut h, name, data).unwrap();
            };
            for (i, item) in items.iter().enumerate() {
                add(&format!("item{i}.md"), item.as_bytes());
            }
            add(&format!("resources/{r1}.png"), b"PNGDATA");
            b.finish().unwrap();
        }
        let rep = joplin(&root, "Imported", &jex).unwrap();
        assert_eq!((rep.notes, rep.notebooks, rep.attachments, rep.tags), (2, 2, 1, 1));
        let note = fs::read_to_string(root.join("Imported/Oracle/RAC/Start CRS.md")).unwrap();
        assert_eq!(
            note,
            "---\ntitle: \"Start CRS\"\ncreated: 2012-12-13T03:07:08Z\nupdated: 2020-01-02T03:04:05Z\n\
             source: \"http://x.com\"\ntags: [rac]\n---\n\nRun this:\n\n\
             ![diagram](../../_resources/crs%20diagram.png)\n\nSee [other](../Other%20note.md)"
        );
        let other = fs::read_to_string(root.join("Imported/Oracle/Other note.md")).unwrap();
        assert!(other.ends_with("---\n\nHello **html**\n"), "{other}");
        assert_eq!(fs::read(root.join("Imported/_resources/crs diagram.png")).unwrap(), b"PNGDATA");
        let modified = fs::metadata(root.join("Imported/Oracle/RAC/Start CRS.md")).unwrap().modified().unwrap();
        assert_eq!(modified, parse_time("2020-01-02T03:04:05Z").unwrap().1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn evernote_end_to_end() {
        let root = std::env::temp_dir().join(format!("notesmd-enex-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let png = b"\x89PNG fake";
        let b64 = base64::engine::general_purpose::STANDARD.encode(png);
        let hash = format!("{:x}", Md5::digest(png));
        let enex = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><!DOCTYPE en-export SYSTEM "x.dtd">
<en-export><note><title>Hello: World</title><content><![CDATA[<?xml version="1.0"?><!DOCTYPE en-note SYSTEM "y.dtd">
<en-note><div>Hi <b>there</b>&nbsp;!</div><en-media type="image/png" hash="{hash}"/></en-note>]]></content>
<created>20130623T132036Z</created><updated>20140101T000000Z</updated><tag>Work Stuff</tag><tag>AWS</tag>
<note-attributes><source-url>https://example.com</source-url></note-attributes>
<resource><data encoding="base64">{b64}</data><mime>image/png</mime><resource-attributes><file-name>shot 1.png</file-name></resource-attributes></resource>
</note></en-export>"#
        );
        let file = root.join("My Notebook.enex");
        fs::write(&file, enex).unwrap();
        let rep = evernote(&root, "", &[file]).unwrap();
        assert_eq!((rep.notes, rep.notebooks, rep.attachments, rep.tags), (1, 1, 1, 2));
        let md = fs::read_to_string(root.join("My Notebook/Hello- World.md")).unwrap();
        assert_eq!(
            md,
            "---\ntitle: \"Hello: World\"\ncreated: 2013-06-23T13:20:36Z\nupdated: 2014-01-01T00:00:00Z\n\
             source: \"https://example.com\"\ntags: [work-stuff, aws]\n---\n\n\
             Hi **there** !\n\n![shot 1.png](../_resources/shot%201.png)\n"
        );
        assert_eq!(fs::read(root.join("_resources/shot 1.png")).unwrap(), png);
        let _ = fs::remove_dir_all(&root);
    }
}
