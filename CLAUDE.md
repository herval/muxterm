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
make install                 # bundle muxterm.app (make app) + ship to /Applications + refresh ~/.cargo/bin/mux
```

The bundle recipe lives in the Makefile and `packaging/` (Info.plist template, icon generator); `assets/muxterm.icns` is checked in — regenerate with `make icon` only when the icon design changes.

There is no test binary/harness beyond inline unit tests, and no rustfmt/clippy config — match the existing hand-formatted style (notably `},` closing match arms).

## Architecture

Cargo workspace: the root `muxterm` crate plus `crates/egui_term`, a **vendored** terminal widget (egui + alacritty_terminal) carrying local patches (input gating, SGR mouse-wheel under tmux mouse mode, bracketed paste, IME, OSC 52 copy). Any change under `crates/egui_term/` must be recorded as a patch entry in `crates/egui_term/VENDOR.md`.

Two binaries share one library. `src/lib.rs` exposes only `agent`, `ask`, `layout`, `mesh`, and `state` — the modules used by both the GUI (`src/main.rs` + private modules) and the agent-mesh CLI (`src/bin/mux.rs`). Code needed by `mux` must live in one of those modules.

### The tmux trick (persistence)

- `tmux.rs` — each pane's PTY runs a tmux *client*: `tmux -L muxterm new-session -A -D -s mux-<8hex>`. `-A` makes fresh-spawn and restore-after-relaunch the same code path. Killing a session is an explicit step (cmd+w / shell exit); dropping a `Pane` merely detaches. The tmux.conf is regenerated at every launch.
- `state.rs` — layout (windows → tabs → split tree → session names) is saved to `state.json` on every mutation. On startup, `mux-*` sessions not referenced by the state are GC'd — **never when the state file failed to parse** (a corrupt state must not cost live sessions).
- Everything lives under `~/.muxterm/`: `config.toml`, `state.json`, `tmux.conf`, mesh state (`agents.json`, `inbox/`, `ctx/`), and workspace git worktrees (`worktrees/`). `state::config_dir()` is the single source of truth (mesh's delegates to it); `state::migrate_config_dir()` (called first thing in both binaries' `main`) moves the dir over from the old `~/Library/Application Support/muxterm/` once, idempotently. Because the socket is dedicated, you can inspect the app from outside: `tmux -L muxterm list-sessions`, `capture-pane`, etc.

### GUI (private modules)

- `app.rs` — the eframe App: owns `Vec<Tab>`, routes PTY events from a shared mpsc channel keyed by pane id, applies keyboard `Action`s (`keys.rs`), persists state, and polls `config.toml` mtime for live reload (`config.rs`).
- `layout.rs` — binary split tree per tab (`Node`, leaves are `PaneId`s), rect splitting, and directional focus (`neighbor`) computed from last-frame screen rects.
- `theme.rs` / `tabbar.rs` — chrome colors are *derived* from the terminal palette; themes are curated presets with a small `[colors]` override surface.
- `links.rs` — cmd+click opener (egui_term P10 detects URL/path tokens and calls the pane's `set_link_opener`): URLs open directly; paths get `:line:col`/punctuation stripped, `~` expanded, relative resolved against the pane's live cwd (tmux `pane_current_path`), and open only if they exist — the existence check is what filters regex false positives like `and/or`.
- `search.rs` — the cmd+f scrollback search (`SearchBar`): the local grid holds no history (egui_term P9 caps it; tmux owns scrollback), so the bar only owns the keyboard and the query — every edit drives tmux copy-mode server-side in one invocation (`copy-mode ; history-bottom ; search-backward-text -- <q> ; display-message` for the counter), and highlights/scrolling come back through the PTY. Match styles in tmux.conf derive from the theme (`theme::search_highlight`). Esc closes the bar with zero tmux calls, leaving copy-mode at the match.
- `pr_status.rs` — GitHub PR badges (config `pr_status`, default on): a poller thread maps pane cwds → (repo, branch) locally every few seconds, asks `gh pr view` per unique key at most once a minute, and streams `session -> Badge` snapshots to the App over an mpsc channel; the JSON→Badge rollup is a pure fixture-tested function. Chips render in `tabbar.rs` and next to pane-title badges; clicking opens the PR.
- `git_status.rs` — git branch chips (config `git_status`, default on): a poller thread runs `git status --porcelain=v2 --branch` once per unique pane cwd every few seconds and streams `session -> Git` snapshots to the App over an mpsc channel — all local, no network, no TTL cache (mirrors `pr_status`'s local scan but skips the `gh` fetch). The porcelain→`Git` parse (branch, dirty count, ahead/behind) is a pure fixture-tested function. Chips render in `tabbar.rs` (display-only, left of the PR chip) and next to pane-title badges; dot is green when clean, yellow when dirty.
- `attention.rs` — activity/attention tab badges (`Cell`, an Instant-injected egui-free state machine like the "?" prompt's): `PtyEvent::Wakeup`/`Bell` — already delivered per-pane to `drain_pty_events`, no tmux hooks — mark background panes (dim dot for output, warn dot for bell or `mux notify`); a per-frame sweep clears the active tab's cells while the window is focused, so every tab-switch path acknowledges badges for free. Attention rises also bounce the dock and post an osascript banner (text via env vars — no AppleScript quoting surface) when the window is unfocused, gated by config `notifications` (default on). A `STARTUP_GRACE` per cell absorbs the tmux attach redraw on restore/split.
- `workspace.rs` / `workspace_popup.rs` / `sidebar.rs` — **every tab is a workspace** (`Tab.workspace: Workspace`): a bare shell for cmd+t, or a rich one for cmd+n (a project folder, an optional git worktree, the task prompt, agent+model, and an AI-generated title). The cmd+n popup gathers those; `App::create_workspace` makes the worktree (`git worktree add ~/.muxterm/worktrees/<slug> -b <branch>`, outside the repo so `git status` stays clean), opens a pane in it, types the agent's interactive launch command into the pane (`agent::launch_command`, same `BackendCommand::Write` path the "?" prompt uses), and kicks off `workspace::spawn_title` — a one-shot haiku call that streams a short title back over an mpsc channel (mirrors the `pr_status`/`git_status` poller wiring). The branch slug is prompt-derived (synchronous); the AI title arrives async and is display-only. Metadata rides inline in `state.json`'s `TabState.workspace` (serde-`default`, no VERSION bump); the collapsible left sidebar (cmd+\, `SidePanel::left`) lists workspaces newest-first — flat, no status groups in v1.

### The "?" AI prompt

`ai_prompt.rs` (`PromptMachine`, `LineTracker`) intercepts egui input events **before** `TerminalView` sees them: a `?` typed as the first char at an idle shell prompt opens a compose line. It's a deliberately egui-Context-free state machine so transitions unit-test with bare `Event` values. `LineTracker` heuristically models the shell's input line and must err toward `Dirty` — a missed trigger is harmless, a false one intercepts real typing. Submit types `mux ask '<query>'` into the pane (`agent.rs` builds the command), with the last N scrollback lines captured to a temp file and piped to stdin. `ask.rs` (behind `mux ask`) resolves agent + `agent_model` from config.toml, spawns the CLI — `claude -p` with stream-json, or `codex exec` which streams natively — and renders answer text live with tool calls as dim `»` one-liners. The agent **acts, not just answers**: claude runs with `--dangerously-skip-permissions` (headless `-p` has no prompt to approve through), codex with `--sandbox workspace-write`. Mutating tools are gated behind an interactive **y/N** — a claude PreToolUse hook (`mux approve`, wired via `--settings`) that, because claude runs hooks with no controlling terminal, *relays* each request over a unix socket to the `mux ask` parent, which owns the pane's `/dev/tty` and does the asking (fail-closed: no socket → deny). Read-only tools (Read/Grep/Glob) run ungated.

### Agent mesh

`mesh.rs` (shared) + `src/bin/mux.rs` (CLI, ~all the command logic). Agents in panes coordinate via `mux join/peers/read/post/tell/ctx/brief`, and grow their own tab with `mux split`: the CLI pre-picks the new session name, spools a `SplitRequest` under `requests/`, and the GUI's poll loop applies it (splits must go through the GUI — an out-of-band session would never render and the startup GC would kill it); the CLI learns the outcome by polling tmux for the session (or the spool for a `.err` refusal). `mux notify [msg]` raises the caller's tab badge the same way — a fire-and-forget `NotifyRequest` spooled under `notify/` (its own directory: the split drainer eats every json in `requests/`), drained by the GUI's poll tick, stale-cleared at launch. **The tab is the team boundary**: membership is resolved by mapping the caller's `MUXTERM_SESSION` (exported into panes via tmux `-e`) through `state.json` to a stable tab id (`mux-tab-<8hex>`). Isolation is cooperative — enforced at the mux command layer, not by tmux. Registry/inboxes/ctx are plain JSON/JSONL files; `mux read` works on any program because panes are real tmux sessions.
