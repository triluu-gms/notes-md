# Notes MD

A fast, lightweight Windows app for managing markdown notes with folders, sub-folders and tags.
Built with [Tauri 2](https://tauri.app) (Rust + WebView2): ~5–10 MB installer, low memory use, instant startup.

## Features

- **Vault = any folder.** Your notes stay plain `.md` files; real sub-folders are the folder tree.
- **Tags** from YAML front matter and inline `#tags` (nested tags like `#project/alpha` supported):
  ```markdown
  ---
  title: Optional title
  tags: [work, ideas/app]
  ---
  # Heading
  Text with an inline #todo tag.
  ```
- **Tag bar** under the note title: type in `+ Add tag` (autocompletes existing tags, Enter or comma to add),
  `×` to remove, right-click a chip to rename it on this note. Front-matter `tags:` is updated in its existing
  style (or created); inline `#tags` are renamed in place or removed. Undo with `Ctrl+Z`.
- **Tag options** (`⋯` next to *Tags* in the sidebar):
  - *Detect #tags in note text* — per-vault switch. Turn it off when notes contain shell sessions
    (`#cd /etc` root prompts) so only front-matter tags count.
  - *Import QOwnNotes tags* — QOwnNotes keeps tags in `notes.sqlite`, not in the `.md` files. This copies each
    note's tags into its front matter (names with spaces become `cisco-vpn-client`), backs up every changed
    file to `%APPDATA%\com.notesmd.app\backups\`, keeps file modified dates, and never writes `notes.sqlite`.
- **Full-text search** (SQLite FTS5, prefix matching, diacritics-insensitive). Mix in `#tag` to filter:
  `meeting #work`.
- **Editor + live preview** (CodeMirror 6 + GFM): Edit / Split / Preview, autosave, `[[wikilinks]]`,
  relative links and images.
- **File watching**: changes made by other apps (VS Code, Obsidian, git, sync tools) show up automatically.
- **Light / dark theme** following Windows, or forced from the sidebar toggle.
- Drag notes or folders onto a folder to move them; right-click for New / Rename / Delete (Recycle Bin) /
  Show in Explorer.

| Shortcut | Action |
| --- | --- |
| `Ctrl+N` | New note in current folder |
| `Ctrl+S` | Save now (autosave is on) |
| `Ctrl+P` / `Ctrl+K` | Focus search (`Enter` opens first result, `Esc` clears) |
| `Ctrl+T` | Add a tag to the open note |
| `Ctrl+E` | Cycle Edit → Split → Preview |
| `Ctrl+F` | Find in note |

## How it works

```
src/                 Frontend (vanilla TypeScript, no framework)
  main.ts            UI: sidebar, note list, editor, preview
  styles.css         Theme tokens (light/dark) and layout
src-tauri/src/
  lib.rs             Tauri commands (vault, notes CRUD) + file watcher
  index.rs           SQLite cache: notes, tags, FTS5 full-text index, folder tree
  parse.rs           Title / tag extraction from markdown
```

The SQLite index lives in `%APPDATA%\com.notesmd.app\index-<hash>.db`, never inside your vault. It is only a
cache: on open and on every file-system change, files whose size/mtime changed are re-parsed. Delete it any time
to force a full rebuild.

## Development

Prerequisites: Node 18+, Rust (via `rustup`), MSVC C++ Build Tools, WebView2 (preinstalled on Windows 10/11).

```powershell
npm install
npm run tauri dev      # run with hot reload
npm run tauri build    # release installer -> src-tauri/target/release/bundle/nsis/
cd src-tauri; cargo test   # parser tests
```
