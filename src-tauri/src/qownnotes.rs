//! Reads tags from a QOwnNotes note folder database (`notes.sqlite`), where QOwnNotes keeps tags
//! outside the markdown files. Opened read-only; QOwnNotes' own data is never modified.

use rusqlite::{Connection, OpenFlags};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

pub const DB_FILE: &str = "notes.sqlite";

/// Vault-relative note path -> QOwnNotes tag names (nested tags as `parent/child`), in link order.
pub type NoteTags = BTreeMap<String, Vec<String>>;

/// None when the vault has no QOwnNotes database with tags.
pub fn read(root: &Path) -> rusqlite::Result<Option<NoteTags>> {
    let db = root.join(DB_FILE);
    if !db.is_file() {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let has_tables: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name IN ('tag', 'noteTagLink')",
        [],
        |r| r.get(0),
    )?;
    if has_tables < 2 {
        return Ok(None);
    }

    let mut tags: HashMap<i64, (String, i64)> = HashMap::new();
    let mut st = conn.prepare("SELECT id, name, coalesce(parent_id, 0) FROM tag")?;
    for row in st.query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))? {
        let (id, v) = row?;
        tags.insert(id, v);
    }
    let full_name = |mut id: i64| {
        let mut parts = Vec::new();
        // Depth limit guards against parent cycles in a damaged database.
        while let Some((name, parent)) = tags.get(&id).filter(|_| parts.len() < 16) {
            parts.push(name.trim().to_string());
            id = *parent;
        }
        parts.reverse();
        parts.join("/")
    };

    let has_stale: bool = conn
        .prepare("SELECT stale_date FROM noteTagLink LIMIT 0")
        .is_ok();
    let sql = format!(
        "SELECT tag_id, note_file_name, coalesce(note_sub_folder_path, '') FROM noteTagLink {} ORDER BY id",
        if has_stale { "WHERE stale_date IS NULL" } else { "" }
    );
    let mut out = NoteTags::new();
    let mut st = conn.prepare(&sql)?;
    let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
    for row in rows {
        let (tag_id, file, folder) = row?;
        let name = full_name(tag_id);
        if name.is_empty() || file.is_empty() {
            continue;
        }
        let file = if file.to_lowercase().ends_with(".md") { file } else { format!("{file}.md") };
        let folder = folder.replace('\\', "/");
        let folder = folder.trim_matches('/');
        let rel = if folder.is_empty() { file } else { format!("{folder}/{file}") };
        let list = out.entry(rel).or_default();
        if !list.contains(&name) {
            list.push(name);
        }
    }
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    /// Dry run against a real folder (read-only): `NOTES_DIR=D:\Notes cargo test qownnotes -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn dry_run_import() {
        let root = std::path::PathBuf::from(std::env::var("NOTES_DIR").expect("set NOTES_DIR"));
        let links = read(&root).unwrap().expect("no QOwnNotes database");
        let (mut found, mut changed, mut shown) = (0, 0, 0);
        for (rel, names) in &links {
            let Ok(original) = std::fs::read_to_string(root.join(rel)) else {
                println!("MISSING {rel}");
                continue;
            };
            found += 1;
            let mut content = original.clone();
            for n in names {
                if let Some(t) = parse::sanitize_tag(n) {
                    content = parse::edit_tags(&content, &parse::TagOp::Add(t), false);
                }
            }
            if content != original {
                changed += 1;
            }
            let got = parse::parse("", &content, false).tags;
            let want: Vec<_> = names.iter().filter_map(|n| parse::sanitize_tag(n)).collect();
            assert!(want.iter().all(|t| got.contains(t)), "{rel}: want {want:?} got {got:?}");
            if shown < 2 {
                shown += 1;
                let head: String = content.lines().take(14).collect::<Vec<_>>().join("\n");
                println!("--- {rel} {names:?}\n{head}\n");
            }
        }
        println!("notes in db {}, found {found}, would change {changed}", links.len());
    }
}
