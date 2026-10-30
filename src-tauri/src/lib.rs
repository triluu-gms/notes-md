mod html2md;
mod import;
mod index;
mod parse;
mod xml;

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

// The UI never passes a vault path: a vault is only the folder the user picked in the dialog (or the last one
// they picked), so script in the webview cannot widen file access to an arbitrary folder.

#[tauri::command]
async fn open_last_vault(app: AppHandle, state: State<'_, AppState>) -> Res<Option<VaultInfo>> {
    let mut cfg = load_config(&app);
    match cfg.last_vault.clone().filter(|p| Path::new(p).is_dir()) {
        Some(path) => load_vault(&app, &state, path).map(Some).inspect_err(|_| {
            // Don't fail the same way on every start.
            cfg.last_vault = None;
            save_config(&app, &cfg);
        }),
        None => Ok(None),
    }
}

/// Shows the folder dialog and opens the chosen folder. None when the user cancels.
#[tauri::command]
async fn pick_vault(app: AppHandle, state: State<'_, AppState>) -> Res<Option<VaultInfo>> {
    use tauri_plugin_dialog::DialogExt;
    let mut dialog = app.dialog().file().set_title("Choose a folder for your notes");
    if let Some(w) = app.get_webview_window("main") {
        dialog = dialog.set_parent(&w);
    }
    let Some(dir) = dialog.blocking_pick_folder() else { return Ok(None) };
    let path = dir.into_path().map_err(err)?.to_string_lossy().into_owned();
    load_vault(&app, &state, path).map(Some)
}

/// Most files + folders a vault may contain. Real note collections are far below this; a drive or a
/// system folder is far above it, and scanning one would freeze the app.
const MAX_VAULT_ENTRIES: usize = 100_000;

/// Refuse folders that cannot sensibly be a vault, before anything is changed.
fn check_vault_dir(root: &Path) -> Res<()> {
    let canon = fs::canonicalize(root).map_err(err)?;
    let shown = root.display();
    if canon.parent().is_none() {
        return Err(format!(
            "\"{shown}\" is a whole drive. Choose (or create) a folder that holds just your notes, e.g. {}Notes.",
            root.to_string_lossy().trim_end_matches(['\\', '/']).to_string() + "\\"
        ));
    }
    let system: Vec<PathBuf> = ["WINDIR", "ProgramFiles", "ProgramFiles(x86)", "ProgramW6432", "ProgramData"]
        .iter()
        .filter_map(|v| std::env::var_os(v))
        .filter_map(|p| fs::canonicalize(p).ok())
        .collect();
    if system.iter().any(|s| canon.starts_with(s)) {
        return Err(format!("\"{shown}\" is a Windows system folder and can't be used for notes."));
    }
    let home = std::env::var_os("USERPROFILE").and_then(|p| fs::canonicalize(p).ok());
    if home.as_deref() == Some(canon.as_path()) || canon.parent().and_then(Path::parent).is_none() && canon.ends_with("Users") {
        return Err(format!(
            "\"{shown}\" contains your whole user profile. Choose a folder inside it, e.g. Documents\\Notes."
        ));
    }
    if !index::within_size(root, MAX_VAULT_ENTRIES) {
        return Err(format!(
            "\"{shown}\" has more than {MAX_VAULT_ENTRIES} files and folders, which is too many for a notes vault. \
             Choose a folder that holds just your notes."
        ));
    }
    Ok(())
}

fn load_vault(app: &AppHandle, state: &AppState, path: String) -> Res<VaultInfo> {
    let root = PathBuf::from(&path);
    if !root.is_dir() {
        return Err(format!("Folder not found: {path}"));
    }
    check_vault_dir(&root)?;
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
}

#[tauri::command]
fn vault_settings(state: State<AppState>) -> Res<VaultSettings> {
    with_vault(&state, |v| Ok(VaultSettings { inline_tags: v.inline }))
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

/// Ask for Evernote (`.enex`, several allowed) or Joplin (`.jex`) export files and import them into
/// `dest_folder`. Returns None when the file dialog is cancelled.
#[tauri::command]
async fn import_notes(app: AppHandle, kind: String, dest_folder: String) -> Res<Option<import::Report>> {
    use tauri_plugin_dialog::DialogExt;
    let mut dialog = app.dialog().file().set_title("Choose export file to import");
    if let Some(w) = app.get_webview_window("main") {
        dialog = dialog.set_parent(&w);
    }
    let files: Vec<PathBuf> = match kind.as_str() {
        "evernote" => dialog
            .add_filter("Evernote export (.enex)", &["enex"])
            .blocking_pick_files()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|f| f.into_path().ok())
            .collect(),
        "joplin" => dialog
            .add_filter("Joplin export (.jex)", &["jex"])
            .blocking_pick_file()
            .and_then(|f| f.into_path().ok())
            .into_iter()
            .collect(),
        _ => return Err(format!("Unknown import type: {kind}")),
    };
    if files.is_empty() {
        return Ok(None);
    }
    let app2 = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app2.state::<AppState>();
        with_vault(&state, |v| {
            let dest = resolve(&v.root, &dest_folder)?;
            fs::create_dir_all(&dest).map_err(err)?;
            let report = match kind.as_str() {
                "evernote" => import::evernote(&v.root, &dest_folder, &files)?,
                _ => import::joplin(&v.root, &dest_folder, &files[0])?,
            };
            sync(v)?;
            Ok(Some(report))
        })
    })
    .await
    .map_err(err)?
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

/// Shows the item selected in the system file manager (no command line is built from the path).
#[tauri::command]
fn reveal(state: State<AppState>, path: String) -> Res<()> {
    let full = with_vault(&state, |v| resolve(&v.root, &path))?;
    if !full.exists() {
        return Err(format!("Not found: {path}"));
    }
    tauri_plugin_opener::reveal_item_in_dir(full).map_err(err)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            open_last_vault,
            pick_vault,
            get_tree,
            get_tags,
            list_notes,
            search,
            note_meta,
            edit_tags,
            vault_settings,
            set_inline_tags,
            import_notes,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_drives_and_system_folders() {
        let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into()) + "\\";
        let e = check_vault_dir(Path::new(&drive)).unwrap_err();
        assert!(e.contains("whole drive"), "{e}");
        let windir = std::env::var("WINDIR").unwrap();
        assert!(check_vault_dir(&Path::new(&windir).join("System32")).unwrap_err().contains("system folder"));
        let home = std::env::var("USERPROFILE").unwrap();
        assert!(check_vault_dir(Path::new(&home)).unwrap_err().contains("user profile"));
        let ok = std::env::temp_dir().join(format!("notesmd-vault-{}", std::process::id()));
        fs::create_dir_all(ok.join("sub")).unwrap();
        assert!(check_vault_dir(&ok).is_ok());
        assert!(!index::within_size(&ok, 1)); // root + sub = 2 entries
        let _ = fs::remove_dir_all(&ok);
    }
}
