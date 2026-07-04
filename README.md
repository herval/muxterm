# muxterm

An iTerm-style terminal emulator for macOS, written in Rust — with one twist:
**every pane is backed by its own tmux session**. Quit the app and your
shells, running processes, and scrollback stay alive; relaunch and the exact
tab/split layout reattaches to them. Closing a pane (cmd+w, or the shell
exiting) is what kills its session.

## Run

```sh
cargo run --release
```

Requires `tmux` (`brew install tmux`).

## Shortcuts

| Chord | Action |
| --- | --- |
| cmd+t | new tab |
| cmd+w | close focused pane (kills its tmux session; closes tab when last pane) |
| cmd+d / cmd+shift+d | split side-by-side / stacked |
| cmd+shift+[ / cmd+shift+] | previous / next tab |
| cmd+1 … cmd+9 | jump to tab |
| cmd+opt+arrows | move focus between panes directionally¹ |
| cmd+[ / cmd+] | cycle focus through panes |
| cmd+c / cmd+v | copy / paste |
| shift+PageUp or mouse wheel | tmux copy-mode scrollback |

¹ Window managers like Rectangle bind cmd+opt+arrows globally; free those
hotkeys or use cmd+[ / cmd+].

## How persistence works

- Panes run `tmux new-session -A -D -s mux-<id>` on a dedicated socket
  (`tmux -L muxterm`), isolated from your own tmux server. `-A` makes
  attach-or-create idempotent, so fresh spawn and post-relaunch restore are
  the same code path (after a reboot you get the same layout with fresh
  shells).
- Quitting the app just drops the tmux clients — the daemonized server and
  its sessions survive.
- Layout (windows → tabs → split tree → session names) is saved to
  `~/Library/Application Support/muxterm/state.json` on every mutation, so
  even a crash restores.
- On startup, `mux-*` sessions not referenced by the saved state are killed
  (skipped entirely if the state file is corrupt).

## Code layout

- `src/` — the app: split tree (`layout.rs`), tmux lifecycle (`tmux.rs`),
  persistence (`state.rs`), shortcuts (`keys.rs`), render loop (`app.rs`).
- `crates/egui_term/` — vendored [egui_term](https://github.com/Harzu/egui_term)
  0.1.0 (terminal widget on alacritty_terminal) with five local patches;
  see `crates/egui_term/VENDOR.md`.
