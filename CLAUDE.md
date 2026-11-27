# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

@AGENTS.md

`AGENTS.md` (imported above) is upstream Zed's rule set for Rust style, GPUI usage, tests and PRs, and it applies here. This file only adds what is specific to this fork and this machine.

## This fork

- `kei-zed` is a personal fork of Zed: `origin` is `HuongBuiPhu/kei-zed` and `upstream` is `zed-industries/zed`. Nothing is sent upstream. Upstream changes are merged into `main` with `git fetch upstream && git merge upstream/main`, and feature work happens on branches that are merged into `main`.
- Commits are authored as `Kei Bui <huong.bp.97@gmail.com>`, set in the repo's local git config. Don't commit with the global git identity.
- Because there are no upstream PRs, the "Pull request hygiene" section of `AGENTS.md` and its README banner rule don't apply to work in this fork.
- Features added by this fork, each in its own crate:
  - `crates/spreadsheet_viewer` shows `.xlsx`, `.xlsm`, `.xls` and `.ods` files read-only. It parses them with calamine into `tabular_data_preview`'s `TableView`.
  - `crates/document_viewer` shows `.docx` and `.doc` files as pages. rdocx lays out the document and renders each page to PNG; rwml first converts `.doc` to `.docx`. Pages are recolored to the theme and cached in a 200 MB `PageCache` shared by all tabs.
  - `crates/binary_file_item` is shared by both viewers. It loads a binary file from a worktree, parses it on a background thread, and reloads it when the file changes on disk. A viewer only implements `FileFormat`.

## Building on this machine

- Xcode is not installed, only the Command Line Tools, so the Metal shaders can't be precompiled. The `zed` binary must be built with runtime shader compilation:
  ```sh
  cargo run -p zed --features gpui_platform/runtime_shaders
  ```
  Only the `zed` binary needs this feature, because library crates don't depend on `gpui_platform`. If a build fails because Xcode is missing, report it; don't work around it by editing `build.rs` or disabling checks.
- Disk space is limited. A debug build of `zed` fills `target/` with about 40 GB. On every cargo command set these variables, otherwise everything is rebuilt under different hashes:
  ```sh
  export CARGO_PROFILE_DEV_DEBUG=line-tables-only CARGO_INCREMENTAL=0
  ```
  Don't run several build profiles at once, and don't build `--release`. Check `df -h .` before large builds; `cargo clean` frees the space.
- `./script/clippy` builds in release mode with all features, so lint one crate at a time instead:
  ```sh
  cargo clippy -p <crate> --all-targets -- --deny warnings
  cargo fmt -p <crate>
  ```
- Tests:
  ```sh
  cargo test -p <crate>                    # one crate
  cargo test -p <crate> <test_name_filter> # one test
  cargo nextest run -p <crate>             # if `cargo test` hits OS resource limits
  ```

## Architecture

- **`crates/zed`** is the application binary. `src/main.rs` calls each feature crate's `init(cx)`, and a new feature takes effect once it is added there and to `crates/zed/Cargo.toml`.
- **`gpui`** is the UI framework: entities, contexts, executors and element rendering, described in `AGENTS.md`. Platform backends live in `gpui_macos`, `gpui_linux`, `gpui_windows` and related crates, selected through `gpui_platform`.
- **`project`** manages worktrees, buffers, language servers and git for an open project. **`worktree`** scans the files on disk and emits `UpdatedEntries` and `DeletedEntry` events.
- **`workspace`** is the window layout of panes, docks, the status bar and tabs. Anything shown in a tab implements `workspace::item::Item`.
- **Opening a file.** A crate calls `workspace::register_project_item::<View>(cx)`, and the handler registered last is tried first. Its `project::ProjectItem::try_open` decides whether it can open a given path, so `image_viewer`, `spreadsheet_viewer` and `document_viewer` take their extensions before the text editor does.
- **Restoring tabs.** An item implements `SerializableItem`, registers it with `register_serializable_item`, and stores its state in a per-crate SQLite table declared with `db::static_connection!`.
- **Keymaps and settings.** Default key bindings live in `assets/keymaps/default-{macos,linux,windows}.json`. Zed clears and reloads every binding from these files, so a binding added only in code is lost; add new action bindings to all three files. Default settings live in `assets/settings/default.json`.
- **Background threads.** On macOS, gpui's background executor runs on GCD threads with 512 KiB stacks, while test threads get 2 MiB. Code that passes in tests can still overflow the stack in the app. `document_viewer` runs rdocx on its own thread with a 16 MiB stack for this reason.

## Debugging the dev build

- The dev channel doesn't install the crash handler. A native crash, for example exit code 132 (SIGILL), leaves nothing in the Zed log. Read the macOS crash report instead, in `~/Library/Logs/DiagnosticReports/zed-*.ips`.
- The app log is `~/Library/Logs/Zed/Zed.log`. The dev build's database is `~/Library/Application Support/Zed/db/0-dev/db.sqlite`.
