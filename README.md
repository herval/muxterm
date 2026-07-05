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

## Theming

`~/Library/Application Support/muxterm/config.toml` (created on first run,
**edits apply live** while the app is running):

```toml
theme = "iterm-dark"   # dracula | solarized-dark | gruvbox-dark |
                       # iterm-light | solarized-light | github-light
dim_inactive_panes = 0.12   # unfocused-split fade toward bg (0.0 - 0.8)

[font]
family = "Menlo"       # font name in the macOS font folders, or a file path
size = 14.0

[colors]               # override any color of the chosen theme
background = "#1d1e23"
accent = "#4a90d9"     # focused-pane border + active-tab underline
bright_green = "#5ffa68"
```

Tab bar, dividers, and pane-border chrome are derived from the palette
automatically, so a theme change restyles the whole window.

## Agent mesh

Run AI agents (claude, aider, any interactive CLI) in panes and let them
coordinate through the bundled `mux` CLI — **scoped per tab**: the panes of
one tab form a team; agents cannot see or message other tabs. Because every
pane is a tmux session, agents can read each other's terminals with zero
cooperation from the program running there (works for `top`, vim, anything).

```sh
mux join reviewer --role code-review   # register this pane (tab relabels)
mux peers                              # who's on the team
mux read writer -n 300                 # snapshot a teammate's terminal
mux post writer "left comments in review.md"   # inbox + one [mux] nudge
mux inbox --consume                    # read messages sent to you
mux tell writer "run the tests again"  # type into their terminal directly
mux ctx set build.status green         # shared per-tab scratchpad
mux brief                              # paste-ready team briefing for a prompt
```

Typical setup: split a tab, run an agent per pane, have each run
`mux join <name> --role <role>` (or paste `mux brief` output into their
system prompt). Agents shell out to `mux` like any other command. Prefer
`post` for anything a teammate should act on (it queues durably and injects
a single `[mux] new message from …` nudge no matter how many messages pile
up); `tell` types straight into their input — immediate, but it can
interleave with whatever they're doing. Messages up to 16 KiB (`post`) /
64 KiB (`tell`); registry, inboxes, and per-tab context live under
`~/Library/Application Support/muxterm/`, cleaned automatically when panes
close. Isolation is cooperative — `mux` enforces the tab boundary, but
anything with socket access can drive tmux directly.

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
