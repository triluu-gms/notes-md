mod index;
mod parse;
mod qownnotes;

use index::{Changes, Folder, Index, NoteMeta, TagCount};
use notify_debouncer_mini::notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_mini::{new_debouncer, DebounceEventResult, Debouncer};
use serde::{Deserialize, Serialize};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State};

type Res<T> = Result<T, String>;

struct Vault {
    root: PathBuf,
    index: Index,
    /// Read inline `#tags` from note text (off for vaults where `#word` is mostly shell prompts).
    inline: bool,
}

#[derive(Default)]
struct AppState {
    vault: Mutex<Option<Vault>>,
    watcher: Mutex<Option<Debouncer<RecommendedWatcher>>>,
}

#[derive(Serialize)]
struct VaultInfo {
    root: String,
    name: String,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct Config {
    last_vault: Option<String>,
    /// Vault paths (lower-cased) where inline `#tags` are not read.
    inline_tags_off: Vec<String>,
}

impl Config {
    fn inline_tags(&self, vault: &str) -> bool {
        !self.inline_tags_off.contains(&vault.to_lowercase())
    }
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn with_vault<T>(state: &AppState, f: impl FnOnce(&mut Vault) -> Res<T>) -> Res<T> {
    let mut guard = state.vault.lock().map_err(err)?;
    f(guard.as_mut().ok_or("No vault is open")?)
}

/// Turn a vault-relative path from the UI into an absolute path, refusing anything that could escape the vault.
fn resolve(root: &Path, rel: &str) -> Res<PathBuf> {
    let p = Path::new(rel);
    if p.components().all(|c| matches!(c, Component::Normal(_))) {
        Ok(root.join(p))
    } else {
        Err(format!("Invalid path: {rel}"))
    }
}

fn join_rel(parent: &str, name: &str) -> String {
    if parent.is_empty() { name.to_string() } else { format!("{parent}/{name}") }
}

fn check_name(name: &str) -> Res<&str> {
    let name = name.trim();
    let bad = name.is_empty()
        || name == "."
        || name == ".."
        || name.ends_with('.')
        || name.chars().any(|c| c.is_control() || r#"<>:"/\|?*"#.contains(c));
    if bad { Err(format!("\"{name}\" is not a valid name")) } else { Ok(name) }
}

fn config_path(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_config_dir().ok().map(|d| d.join("config.json"))
}

fn load_config(app: &AppHandle) -> Config {
    config_path(app)
        .and_then(|p| fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_config(app: &AppHandle, cfg: &Config) {
    if let Some(p) = config_path(app) {
        let _ = fs::create_dir_all(p.parent().unwrap());
        let _ = fs::write(p, serde_json::to_vec_pretty(cfg).unwrap_or_default());
    }
}

fn start_watcher(app: AppHandle, root: &Path) -> Res<Debouncer<RecommendedWatcher>> {
    let mut debouncer = new_debouncer(Duration::from_millis(300), move |res: DebounceEventResult| {
        if res.is_err() {
            return;
        }
        let state = app.state::<AppState>();
        if let Ok(changes) = with_vault(&state, |v| v.index.sync(&v.root).map_err(err)) {
            let _ = app.emit("vault-changed", changes);
        }
    })
    .map_err(err)?;
    debouncer.watcher().watch(root, RecursiveMode::Recursive).map_err(err)?;
    Ok(debouncer)
}

fn sync(v: &mut Vault) -> Res<Changes> {
    v.index.sync(&v.root).map_err(err)
}

// ---------------- commands ----------------

#[tauri::command]
fn last_vault(app: AppHandle) -> Option<String> {
    load_config(&app).last_vault.filter(|p| Path::new(p).is_dir())
}

#[tauri::command]
async fn open_vault(app: AppHandle, state: State<'_, AppState>, path: String) -> Res<VaultInfo> {
    load_vault(&app, &state, path)
}

fn load_vault(app: &AppHandle, state: &AppState, path: String) -> Res<VaultInfo> {
    let root = PathBuf::from(&path);
    if !root.is_dir() {
        return Err(format!("Folder not found: {path}"));
    }
    // Stop the old watcher first, without holding the vault lock (its callback takes that lock).
    let old = state.watcher.lock().map_err(err)?.take();
    drop(old);

    let data_dir = app.path().app_data_dir().map_err(err)?;
    fs::create_dir_all(&data_dir).map_err(err)?;
    let mut h = std::hash::DefaultHasher::new();
    path.to_lowercase().hash(&mut h);
    let db = data_dir.join(format!("index-{:016x}.db", h.finish()));

    let mut cfg = load_config(app);
    let inline = cfg.inline_tags(&path);
    let mut index = match Index::open(&db, inline) {
        Ok(i) => i,
        Err(_) => {
            // Corrupt cache: rebuild from scratch.
            for ext in ["", "-wal", "-shm"] {
                let _ = fs::remove_file(format!("{}{ext}", db.display()));
            }
            Index::open(&db, inline).map_err(err)?
        }
    };
    index.sync(&root).map_err(err)?;

    let _ = app.asset_protocol_scope().allow_directory(&root, true);
    let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.clone());
    *state.vault.lock().map_err(err)? = Some(Vault { root: root.clone(), index, inline });
    *state.watcher.lock().map_err(err)? = Some(start_watcher(app.clone(), &root)?);
    cfg.last_vault = Some(path.clone());
    save_config(app, &cfg);
    Ok(VaultInfo { root: path, name })
}

#[tauri::command]
fn get_tree(state: State<AppState>) -> Res<Folder> {
    with_vault(&state, |v| v.index.tree(&v.root).map_err(err))
}

#[tauri::command]
fn get_tags(state: State<AppState>) -> Res<Vec<TagCount>> {
    with_vault(&state, |v| v.index.tags().map_err(err))
}

#[tauri::command]
fn list_notes(state: State<AppState>, folder: Option<String>, tag: Option<String>) -> Res<Vec<NoteMeta>> {
    with_vault(&state, |v| v.index.list(folder.as_deref(), tag.as_deref()).map_err(err))
}

#[tauri::command]
fn search(state: State<AppState>, query: String) -> Res<Vec<NoteMeta>> {
    with_vault(&state, |v| v.index.search(&query).map_err(err))
}

#[derive(Serialize)]
struct TagEdit {
    content: String,
    tags: Vec<String>,
}

/// Add / remove / rename a tag in a note's text. Pure: the UI applies the result to the editor
/// (so it can be undone) and saves it.
#[tauri::command]
fn edit_tags(state: State<AppState>, content: String, op: String, tag: String, new_tag: Option<String>) -> Res<TagEdit> {
    let inline = with_vault(&state, |v| Ok(v.inline))?;
    let clean = |raw: &str| {
        parse::clean_tag(raw).ok_or_else(|| format!("\"{}\" is not a valid tag — use letters, digits, - _ /", raw.trim()))
    };
    let op = match op.as_str() {
        "add" => parse::TagOp::Add(clean(&tag)?),
        "remove" => parse::TagOp::Remove(clean(&tag)?),
        "rename" => parse::TagOp::Rename(clean(&tag)?, clean(new_tag.as_deref().unwrap_or(""))?),
        _ => return Err(format!("Unknown tag operation: {op}")),
    };
    let content = parse::edit_tags(&content, &op, inline);
    let tags = parse::parse("", &content, inline).tags;
    Ok(TagEdit { content, tags })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct VaultSettings {
    inline_tags: bool,
    /// Present when the vault has a QOwnNotes database with tags: (notes, distinct tags).
    qownnotes: Option<(usize, usize)>,
}

#[tauri::command]
fn vault_settings(state: State<AppState>) -> Res<VaultSettings> {
    with_vault(&state, |v| {
        let qon = qownnotes::read(&v.root).ok().flatten().filter(|m| !m.is_empty()).map(|m| {
            let mut names: Vec<_> = m.values().flatten().map(|t| t.to_lowercase()).collect();
            names.sort();
            names.dedup();
            (m.len(), names.len())
        });
        Ok(VaultSettings { inline_tags: v.inline, qownnotes: qon })
    })
}

#[tauri::command]
async fn set_inline_tags(app: AppHandle, state: State<'_, AppState>, enabled: bool) -> Res<()> {
    let root = with_vault(&state, |v| Ok(v.root.to_string_lossy().into_owned()))?;
    let mut cfg = load_config(&app);
    let key = root.to_lowercase();
    cfg.inline_tags_off.retain(|p| *p != key);
    if !enabled {
        cfg.inline_tags_off.push(key);
    }
    save_config(&app, &cfg);
    load_vault(&app, &state, root).map(|_| ())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ImportReport {
    notes_updated: usize,
    tags_added: usize,
    /// Notes QOwnNotes knows about that are not in the folder.
    missing: Vec<String>,
    /// Tag names that had to change to be valid tags: (QOwnNotes name, new tag).
    renamed: Vec<(String, String)>,
    backup_dir: String,
}

/// Copy QOwnNotes tags (from notes.sqlite) into each note's front matter. Every changed file is backed up
/// first and keeps its original modified time.
#[tauri::command]
async fn import_qownnotes(app: AppHandle, state: State<'_, AppState>) -> Res<ImportReport> {
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let backup = app.path().app_data_dir().map_err(err)?.join("backups").join(format!("qownnotes-import-{stamp}"));
    with_vault(&state, |v| {
        let links = qownnotes::read(&v.root)
            .map_err(err)?
            .ok_or("This folder has no QOwnNotes database (notes.sqlite)")?;
        let mut report = ImportReport {
            notes_updated: 0,
            tags_added: 0,
            missing: Vec::new(),
            renamed: Vec::new(),
            backup_dir: backup.display().to_string(),
        };
        for (rel, names) in links {
            let full = match resolve(&v.root, &rel) {
                Ok(p) if p.is_file() => p,
                _ => {
                    report.missing.push(rel);
                    continue;
                }
            };
            let tags: Vec<String> = names
                .iter()
                .filter_map(|n| {
                    let t = parse::sanitize_tag(n)?;
                    if t != n.to_lowercase() && !report.renamed.iter().any(|(o, _)| o == n) {
                        report.renamed.push((n.clone(), t.clone()));
                    }
                    Some(t)
                })
                .collect();

            let bytes = fs::read(&full).map_err(err)?;
            let Ok(original) = String::from_utf8(bytes.clone()) else {
                report.missing.push(format!("{rel} (not UTF-8, skipped)"));
                continue;
            };
            let mut content = original.clone();
            for t in tags {
                content = parse::edit_tags(&content, &parse::TagOp::Add(t), v.inline);
            }
            if content == original {
                continue;
            }
            let modified = fs::metadata(&full).and_then(|m| m.modified()).ok();
            let bk = backup.join(&rel);
            fs::create_dir_all(bk.parent().unwrap()).map_err(err)?;
            fs::write(&bk, &bytes).map_err(err)?;
            fs::write(&full, &content).map_err(err)?;
            if let Some(m) = modified {
                if let Ok(f) = fs::File::options().write(true).open(&full) {
                    let _ = f.set_modified(m);
                }
            }
            report.notes_updated += 1;
            report.tags_added += parse::parse("", &content, v.inline)
                .tags
                .len()
                .saturating_sub(parse::parse("", &original, v.inline).tags.len());
        }
        sync(v)?;
        Ok(report)
    })
}

#[tauri::command]
fn note_meta(state: State<AppState>, path: String) -> Res<Option<NoteMeta>> {
    with_vault(&state, |v| v.index.get(&path).map_err(err))
}

#[tauri::command]
fn read_note(state: State<AppState>, path: String) -> Res<String> {
    with_vault(&state, |v| {
        let bytes = fs::read(resolve(&v.root, &path)?).map_err(err)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    })
}

#[tauri::command]
fn save_note(state: State<AppState>, path: String, content: String) -> Res<()> {
    with_vault(&state, |v| {
        let full = resolve(&v.root, &path)?;
        if let Some(dir) = full.parent() {
            fs::create_dir_all(dir).map_err(err)?;
        }
        fs::write(&full, content).map_err(err)?;
        v.index.update_file(&v.root, &path).map_err(err)
    })
}

#[tauri::command]
fn create_note(state: State<AppState>, folder: String, name: Option<String>) -> Res<String> {
    with_vault(&state, |v| {
        let base = check_name(name.as_deref().unwrap_or("Untitled"))?.to_string();
        let dir = resolve(&v.root, &folder)?;
        fs::create_dir_all(&dir).map_err(err)?;
        let mut n = 0;
        let file = loop {
            let candidate = if n == 0 { format!("{base}.md") } else { format!("{base} {n}.md") };
            if !dir.join(&candidate).exists() {
                break candidate;
            }
            n += 1;
        };
        let title = file.trim_end_matches(".md");
        fs::write(dir.join(&file), format!("# {title}\n\n")).map_err(err)?;
        let rel = join_rel(&folder, &file);
        v.index.update_file(&v.root, &rel).map_err(err)?;
        Ok(rel)
    })
}

#[tauri::command]
fn create_folder(state: State<AppState>, parent: String, name: String) -> Res<String> {
    with_vault(&state, |v| {
        let name = check_name(&name)?;
        let rel = join_rel(&parent, name);
        let full = resolve(&v.root, &rel)?;
        if full.exists() {
            return Err(format!("\"{name}\" already exists"));
        }
        fs::create_dir_all(full).map_err(err)?;
        Ok(rel)
    })
}

/// Rename a note (the `.md` extension is kept) or a folder. Returns the new relative path.
#[tauri::command]
fn rename_item(state: State<AppState>, path: String, new_name: String) -> Res<String> {
    with_vault(&state, |v| {
        let from = resolve(&v.root, &path)?;
        let mut name = check_name(&new_name)?.to_string();
        if from.is_file() && !name.to_lowercase().ends_with(".md") {
            name.push_str(".md");
        }
        let parent = path.rsplit_once('/').map_or("", |(p, _)| p);
        let rel = join_rel(parent, &name);
        let to = resolve(&v.root, &rel)?;
        // Allow case-only renames on Windows (same file, different case).
        if to.exists() && !rel.eq_ignore_ascii_case(&path) {
            return Err(format!("\"{name}\" already exists"));
        }
        fs::rename(&from, &to).map_err(err)?;
        sync(v)?;
        Ok(rel)
    })
}

#[tauri::command]
fn move_item(state: State<AppState>, path: String, dest_folder: String) -> Res<String> {
    with_vault(&state, |v| {
        if dest_folder == path || dest_folder.starts_with(&format!("{path}/")) {
            return Err("Cannot move a folder into itself".into());
        }
        let from = resolve(&v.root, &path)?;
        let name = path.rsplit('/').next().unwrap_or(&path);
        let rel = join_rel(&dest_folder, name);
        let to = resolve(&v.root, &rel)?;
        if to.exists() {
            return Err(format!("\"{name}\" already exists in the destination"));
        }
        fs::rename(&from, &to).map_err(err)?;
        sync(v)?;
        Ok(rel)
    })
}

/// Moves a note or folder to the Recycle Bin.
#[tauri::command]
fn delete_item(state: State<AppState>, path: String) -> Res<()> {
    with_vault(&state, |v| {
        if path.is_empty() {
            return Err("Cannot delete the vault root".into());
        }
        trash::delete(resolve(&v.root, &path)?).map_err(err)?;
        sync(v).map(|_| ())
    })
}

#[tauri::command]
fn reveal(state: State<AppState>, path: String) -> Res<()> {
    with_vault(&state, |v| {
        let full = resolve(&v.root, &path)?;
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            std::process::Command::new("explorer")
                .raw_arg(format!("/select,\"{}\"", full.display()))
                .spawn()
                .map_err(err)?;
        }
        #[cfg(not(windows))]
        let _ = full;
        Ok(())
    })
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            last_vault,
            open_vault,
            get_tree,
            get_tags,
            list_notes,
            search,
            note_meta,
            edit_tags,
            vault_settings,
            set_inline_tags,
            import_qownnotes,
            read_note,
            save_note,
            create_note,
            create_folder,
            rename_item,
            move_item,
            delete_item,
            reveal,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Notes MD");
}
