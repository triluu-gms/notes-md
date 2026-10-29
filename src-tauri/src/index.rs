//! SQLite cache of the vault: note metadata, tags and an FTS5 full-text index.
//! The markdown files on disk are always the source of truth; this can be deleted at any time.

use crate::parse;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS notes (
    id    INTEGER PRIMARY KEY,
    path  TEXT NOT NULL UNIQUE,
    title TEXT NOT NULL,
    mtime INTEGER NOT NULL,
    size  INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS tags (
    note_id INTEGER NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
    tag     TEXT NOT NULL,
    PRIMARY KEY (note_id, tag)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS tags_by_tag ON tags(tag);
CREATE VIRTUAL TABLE IF NOT EXISTS fts USING fts5(title, body, tokenize = 'unicode61 remove_diacritics 2');
CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;
";

/// Bump whenever parse.rs changes what it extracts, to force a full re-index.
const INDEX_VERSION: i64 = 3;

/// Separator used by group_concat for tag lists (cannot appear in a tag).
const SEP: char = '\u{1f}';

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct NoteMeta {
    pub path: String,
    pub title: String,
    pub mtime: i64,
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

#[derive(Serialize)]
pub struct TagCount {
    pub tag: String,
    pub count: i64,
}

#[derive(Serialize)]
pub struct Folder {
    pub name: String,
    pub path: String,
    pub count: usize,
    pub children: Vec<Folder>,
}

#[derive(Serialize, Default, Clone)]
pub struct Changes {
    pub updated: Vec<String>,
    pub removed: Vec<String>,
}

pub struct Index {
    conn: Connection,
    /// Read inline `#tags` from note text (per-vault setting).
    inline: bool,
}

impl Index {
    pub fn open(db: &Path, inline: bool) -> rusqlite::Result<Self> {
        let conn = Connection::open(db)?;
        conn.execute_batch(SCHEMA)?;
        // Parser or tag setting changed since this cache was built: drop everything so the next sync
        // re-reads all files.
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        let stored: Option<String> = conn
            .query_row("SELECT value FROM settings WHERE key = 'inline_tags'", [], |r| r.get(0))
            .optional()?;
        let want = if inline { "1" } else { "0" };
        if version != INDEX_VERSION || stored.as_deref() != Some(want) {
            conn.execute_batch(&format!("DELETE FROM fts; DELETE FROM notes; PRAGMA user_version = {INDEX_VERSION};"))?;
            conn.execute("INSERT OR REPLACE INTO settings (key, value) VALUES ('inline_tags', ?1)", [want])?;
        }
        Ok(Self { conn, inline })
    }

    /// Current metadata of one note (used to show its tags in the preview).
    pub fn get(&self, rel: &str) -> rusqlite::Result<Option<NoteMeta>> {
        let mut st = self.conn.prepare_cached(
            "SELECT n.path, n.title, n.mtime,
                    (SELECT group_concat(tag, char(31)) FROM tags WHERE note_id = n.id)
             FROM notes n WHERE n.path = ?1",
        )?;
        let mut rows = st.query([rel])?;
        rows.next()?
            .map(|r| {
                Ok(NoteMeta { path: r.get(0)?, title: r.get(1)?, mtime: r.get(2)?, tags: split_tags(r.get(3)?), snippet: None })
            })
            .transpose()
    }

    /// Bring the index in line with the files on disk. Only files whose mtime/size changed are re-read.
    pub fn sync(&mut self, root: &Path) -> rusqlite::Result<Changes> {
        let files = scan(root);
        let tx = self.conn.transaction()?;
        let existing: HashMap<String, (i64, i64, i64)> = {
            let mut st = tx.prepare("SELECT path, id, mtime, size FROM notes")?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?, r.get(3)?))))?;
            rows.collect::<Result<_, _>>()?
        };

        let mut ch = Changes::default();
        for (path, &(id, _, _)) in &existing {
            if !files.contains_key(path) {
                delete_note(&tx, id)?;
                ch.removed.push(path.clone());
            }
        }
        for (path, &(mtime, size)) in &files {
            if matches!(existing.get(path), Some(&(_, m, s)) if m == mtime && s == size) {
                continue;
            }
            let Ok(bytes) = fs::read(root.join(path)) else { continue };
            upsert(&tx, path, mtime, size, &String::from_utf8_lossy(&bytes), self.inline)?;
            ch.updated.push(path.clone());
        }
        tx.commit()?;
        Ok(ch)
    }

    /// Re-index a single file right after the app wrote it.
    pub fn update_file(&mut self, root: &Path, rel: &str) -> rusqlite::Result<()> {
        let full = root.join(rel);
        let (Ok(meta), Ok(bytes)) = (fs::metadata(&full), fs::read(&full)) else { return Ok(()) };
        let tx = self.conn.transaction()?;
        upsert(&tx, rel, mtime_ms(&meta), meta.len() as i64, &String::from_utf8_lossy(&bytes), self.inline)?;
        tx.commit()
    }

    /// Notes in `folder` (recursively) and/or carrying `tag` (or a nested `tag/...`), newest first.
    pub fn list(&self, folder: Option<&str>, tag: Option<&str>) -> rusqlite::Result<Vec<NoteMeta>> {
        let folder_like = folder.filter(|f| !f.is_empty()).map(|f| format!("{}/%", escape_like(f)));
        let tag = tag.map(str::to_lowercase);
        let tag_like = tag.as_deref().map(|t| format!("{}/%", escape_like(t)));
        let mut st = self.conn.prepare_cached(
            "SELECT n.path, n.title, n.mtime,
                    (SELECT group_concat(tag, char(31)) FROM tags WHERE note_id = n.id)
             FROM notes n
             WHERE (?1 IS NULL OR n.path LIKE ?1 ESCAPE '\\')
               AND (?2 IS NULL OR EXISTS (SELECT 1 FROM tags t WHERE t.note_id = n.id
                                          AND (t.tag = ?2 OR t.tag LIKE ?3 ESCAPE '\\')))
             ORDER BY n.mtime DESC",
        )?;
        let rows = st.query_map(params![folder_like, tag, tag_like], |r| {
            Ok(NoteMeta {
                path: r.get(0)?,
                title: r.get(1)?,
                mtime: r.get(2)?,
                tags: split_tags(r.get(3)?),
                snippet: None,
            })
        })?;
        rows.collect()
    }

    pub fn tags(&self) -> rusqlite::Result<Vec<TagCount>> {
        let mut st = self.conn.prepare_cached("SELECT tag, count(*) FROM tags GROUP BY tag ORDER BY tag")?;
        let rows = st.query_map([], |r| Ok(TagCount { tag: r.get(0)?, count: r.get(1)? }))?;
        rows.collect()
    }

    /// Full-text search. `#tag` words act as tag filters; other words are prefix-matched (all must match).
    pub fn search(&self, query: &str) -> rusqlite::Result<Vec<NoteMeta>> {
        let mut tag_filters = Vec::new();
        let mut terms = Vec::new();
        for w in query.split_whitespace() {
            match w.strip_prefix('#') {
                Some(t) if !t.is_empty() => tag_filters.push(t.to_lowercase()),
                _ => terms.push(format!("\"{}\"*", w.replace('"', "\"\""))),
            }
        }

        let mut hits = if terms.is_empty() {
            match tag_filters.first() {
                Some(t) => self.list(None, Some(t))?,
                None => return Ok(Vec::new()),
            }
        } else {
            let mut st = self.conn.prepare_cached(
                "SELECT n.path, n.title, n.mtime,
                        (SELECT group_concat(tag, char(31)) FROM tags WHERE note_id = n.id),
                        snippet(fts, 1, char(2), char(3), '…', 14)
                 FROM fts JOIN notes n ON n.id = fts.rowid
                 WHERE fts MATCH ?1
                 ORDER BY bm25(fts, 6.0, 1.0)
                 LIMIT 300",
            )?;
            let rows = st.query_map([terms.join(" ")], |r| {
                Ok(NoteMeta {
                    path: r.get(0)?,
                    title: r.get(1)?,
                    mtime: r.get(2)?,
                    tags: split_tags(r.get(3)?),
                    snippet: r.get(4)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };

        hits.retain(|h| {
            tag_filters.iter().all(|f| {
                h.tags.iter().any(|t| t == f || t.strip_prefix(f.as_str()).is_some_and(|r| r.starts_with('/')))
            })
        });
        Ok(hits)
    }

    /// Folder tree from disk (so empty folders show up), with recursive note counts from the index.
    pub fn tree(&self, root: &Path) -> rusqlite::Result<Folder> {
        let mut counts: HashMap<String, usize> = HashMap::new();
        let mut st = self.conn.prepare_cached("SELECT path FROM notes")?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let path: String = r.get(0)?;
            *counts.entry(String::new()).or_default() += 1;
            let mut end = 0;
            while let Some(i) = path[end..].find('/') {
                end += i;
                *counts.entry(path[..end].to_string()).or_default() += 1;
                end += 1;
            }
        }
        let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        Ok(build_tree(root, String::new(), name, &counts))
    }
}

fn upsert(c: &Connection, path: &str, mtime: i64, size: i64, content: &str, inline: bool) -> rusqlite::Result<()> {
    let stem = Path::new(path).file_stem().map(|s| s.to_string_lossy()).unwrap_or_default();
    let p = parse::parse(&stem, content, inline);
    let id: i64 = c.prepare_cached(
        "INSERT INTO notes (path, title, mtime, size) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(path) DO UPDATE SET title = excluded.title, mtime = excluded.mtime, size = excluded.size
         RETURNING id",
    )?
    .query_row(params![path, p.title, mtime, size], |r| r.get(0))?;
    c.prepare_cached("DELETE FROM tags WHERE note_id = ?1")?.execute([id])?;
    let mut ins = c.prepare_cached("INSERT OR IGNORE INTO tags (note_id, tag) VALUES (?1, ?2)")?;
    for t in &p.tags {
        ins.execute(params![id, t])?;
    }
    c.prepare_cached("DELETE FROM fts WHERE rowid = ?1")?.execute([id])?;
    c.prepare_cached("INSERT INTO fts (rowid, title, body) VALUES (?1, ?2, ?3)")?
        .execute(params![id, p.title, p.body])?;
    Ok(())
}

fn delete_note(c: &Connection, id: i64) -> rusqlite::Result<()> {
    c.prepare_cached("DELETE FROM fts WHERE rowid = ?1")?.execute([id])?;
    c.prepare_cached("DELETE FROM notes WHERE id = ?1")?.execute([id])?;
    Ok(())
}

fn split_tags(s: Option<String>) -> Vec<String> {
    let mut v: Vec<String> = s.map(|s| s.split(SEP).map(String::from).collect()).unwrap_or_default();
    v.sort();
    v
}

fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

pub fn is_hidden(name: &str) -> bool {
    name.starts_with('.') || name == "node_modules"
}

fn mtime_ms(m: &fs::Metadata) -> i64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as i64)
}

/// Vault-relative path with `/` separators.
pub fn rel_path(root: &Path, p: &Path) -> Option<String> {
    let rel = p.strip_prefix(root).ok()?;
    let parts: Vec<_> = rel.components().map(|c| c.as_os_str().to_string_lossy()).collect();
    Some(parts.join("/"))
}

/// All `.md` files under root: rel path -> (mtime ms, size).
fn scan(root: &Path) -> HashMap<String, (i64, i64)> {
    WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !is_hidden(&e.file_name().to_string_lossy()))
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_type().is_file()
                && e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("md"))
        })
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            Some((rel_path(root, e.path())?, (mtime_ms(&meta), meta.len() as i64)))
        })
        .collect()
}

fn build_tree(dir: &Path, rel: String, name: String, counts: &HashMap<String, usize>) -> Folder {
    let mut children: Vec<Folder> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            if is_hidden(&n) {
                return None;
            }
            let r = if rel.is_empty() { n.clone() } else { format!("{rel}/{n}") };
            Some(build_tree(&e.path(), r, n, counts))
        })
        .collect();
    children.sort_by_key(|f| f.name.to_lowercase());
    Folder { count: counts.get(&rel).copied().unwrap_or(0), name, path: rel, children }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_list_search_tree() {
        let root = std::env::temp_dir().join(format!("notesmd-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("Work/Projects")).unwrap();
        fs::create_dir_all(root.join(".hidden")).unwrap();
        fs::write(root.join("Work/Projects/alpha.md"), "---\ntags: [project/alpha]\n---\n# Alpha plan\nKickoff meeting notes\n").unwrap();
        fs::write(root.join("Work/todo.md"), "# Todo\nCall Huyền about the #urgent thing\n").unwrap();
        fs::write(root.join("home.md"), "Groceries list\n").unwrap();
        fs::write(root.join(".hidden/x.md"), "ignored").unwrap();

        let mut idx = Index::open(&root.join(".hidden/index.db"), true).unwrap();
        let ch = idx.sync(&root).unwrap();
        assert_eq!(ch.updated.len(), 3);
        assert!(idx.sync(&root).unwrap().updated.is_empty(), "unchanged files are not re-read");

        assert_eq!(idx.list(Some("Work"), None).unwrap().len(), 2);
        assert_eq!(idx.list(None, Some("project")).unwrap()[0].title, "Alpha plan");

        let hits = idx.search("kick").unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.as_deref().unwrap().contains('\u{2}'));
        assert_eq!(idx.search("huyen").unwrap().len(), 1, "diacritics-insensitive");
        assert_eq!(idx.search("#urgent").unwrap().len(), 1);
        assert!(idx.search("notes #urgent").unwrap().is_empty());

        let tree = idx.tree(&root).unwrap();
        assert_eq!(tree.count, 3);
        assert_eq!(tree.children.len(), 1);
        assert_eq!(tree.children[0].count, 2);
        assert_eq!(tree.children[0].children[0].path, "Work/Projects");

        fs::remove_file(root.join("home.md")).unwrap();
        assert_eq!(idx.sync(&root).unwrap().removed, vec!["home.md"]);
        drop(idx);
        let _ = fs::remove_dir_all(&root);
    }
}
