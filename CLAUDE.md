# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

muxterm is an iTerm-style terminal emulator for macOS (Rust, egui/eframe) where **every pane is backed by its own tmux session** on a dedicated socket (`tmux -L muxterm`). Quitting the app only detaches clients — sessions, processes, and scrollback survive; relaunch reattaches the saved layout. Requires `tmux` on the machine (`make setup` installs it via brew).

## Commands

```sh
cargo run --release          # run the app (also: make run)
cargo build                  # compile check
cargo test                   # all tests (root crate; unit tests live in #[cfg(test)] modules)
cargo test layout            # tests in one module
cargo test split_and_leaves  # single test by name
cargo run --bin mux -- peers # the mux CLI (second binary; default-run is the GUI)
```

There is no test binary/harness beyond inline unit tests, and no rustfmt/clippy config — match the existing hand-formatted style (notably `},` closing match arms).

## Architecture

Cargo workspace: the root `muxterm` crate plus `crates/egui_term`, a **vendored** terminal widget (egui + alacritty_terminal) carrying local patches (input gating, SGR mouse-wheel under tmux mouse mode, bracketed paste, IME, OSC 52 copy). Any change under `crates/egui_term/` must be recorded as a patch entry in `crates/egui_term/VENDOR.md`.

Two binaries share one library. `src/lib.rs` exposes only `layout`, `mesh`, and `state` — the modules used by both the GUI (`src/main.rs` + private modules) and the agent-mesh CLI (`src/bin/mux.rs`). Code needed by `mux` must live in one of those three modules.

### The tmux trick (persistence)

- `tmux.rs` — each pane's PTY runs a tmux *client*: `tmux -L muxterm new-session -A -D -s mux-<8hex>`. `-A` makes fresh-spawn and restore-after-relaunch the same code path. Killing a session is an explicit step (cmd+w / shell exit); dropping a `Pane` merely detaches. The tmux.conf is regenerated at every launch.
- `state.rs` — layout (windows → tabs → split tree → session names) is saved to `state.json` on every mutation. On startup, `mux-*` sessions not referenced by the state are GC'd — **never when the state file failed to parse** (a corrupt state must not cost live sessions).
- Everything lives under `~/Library/Application Support/muxterm/`: `config.toml`, `state.json`, `tmux.conf`, and mesh state (`agents.json`, `inbox/`, `ctx/`). Because the socket is dedicated, you can inspect the app from outside: `tmux -L muxterm list-sessions`, `capture-pane`, etc.

### GUI (private modules)

- `app.rs` — the eframe App: owns `Vec<Tab>`, routes PTY events from a shared mpsc channel keyed by pane id, applies keyboard `Action`s (`keys.rs`), persists state, and polls `config.toml` mtime for live reload (`config.rs`).
- `layout.rs` — binary split tree per tab (`Node`, leaves are `PaneId`s), rect splitting, and directional focus (`neighbor`) computed from last-frame screen rects.
- `theme.rs` / `tabbar.rs` — chrome colors are *derived* from the terminal palette; themes are curated presets with a small `[colors]` override surface.

### The "? " AI prompt

`ai_prompt.rs` (`PromptMachine`, `LineTracker`) intercepts egui input events **before** `TerminalView` sees them: a `?` typed as the first char at an idle shell prompt opens a compose line. It's a deliberately egui-Context-free state machine so transitions unit-test with bare `Event` values. `LineTracker` heuristically models the shell's input line and must err toward `Dirty` — a missed trigger is harmless, a false one intercepts real typing. Submit runs a one-shot `claude -p` / `codex exec` (`agent.rs`) typed into the pane, with the last N scrollback lines captured to a temp file and piped to stdin.

### Agent mesh

`mesh.rs` (shared) + `src/bin/mux.rs` (CLI, ~all the command logic). Agents in panes coordinate via `mux join/peers/read/post/tell/ctx/brief`. **The tab is the team boundary**: membership is resolved by mapping the caller's `MUXTERM_SESSION` (exported into panes via tmux `-e`) through `state.json` to a stable tab id (`mux-tab-<8hex>`). Isolation is cooperative — enforced at the mux command layer, not by tmux. Registry/inboxes/ctx are plain JSON/JSONL files; `mux read` works on any program because panes are real tmux sessions.
