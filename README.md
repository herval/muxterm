```
███╗   ███╗ ██╗   ██╗ ██╗  ██╗ ████████╗ ███████╗ ██████╗  ███╗   ███╗
████╗ ████║ ██║   ██║ ╚██╗██╔╝ ╚══██╔══╝ ██╔════╝ ██╔══██╗ ████╗ ████║
██╔████╔██║ ██║   ██║  ╚███╔╝     ██║    █████╗   ██████╔╝ ██╔████╔██║
██║╚██╔╝██║ ██║   ██║  ██╔██╗     ██║    ██╔══╝   ██╔══██╗ ██║╚██╔╝██║
██║ ╚═╝ ██║ ╚██████╔╝ ██╔╝ ██╗    ██║    ███████╗ ██║  ██║ ██║ ╚═╝ ██║
╚═╝     ╚═╝  ╚═════╝  ╚═╝  ╚═╝    ╚═╝    ╚══════╝ ╚═╝  ╚═╝ ╚═╝     ╚═╝
```

An iTerm-style terminal emulator for macOS, written in Rust — with one twist:
**every pane is backed by its own tmux session**. Quit the app and your
shells, running processes, and scrollback stay alive; relaunch and the exact
tab/split layout reattaches to them. Closing a pane (cmd+w, or the shell
exiting) is what kills its session.

Because each pane is a real tmux session, panes are also *inspectable and
scriptable from outside* — which is what powers the built-in
[agent mesh](#agent-mesh) for running teams of AI agents.

## Philosophy

muxterm is an **opinionated terminal**. It ships a point of view as
defaults rather than exposing every choice as configuration:

- **Your muscle memory already works.** Keybindings are iTerm's — cmd+t,
  cmd+d, cmd+1…9 — with no prefix chords or modes to learn. tmux is the
  engine under every pane, never something you drive by hand.
- **Nothing is ever lost.** Every pane outlives the app: quit, crash, or
  relaunch, and your shells, processes, layout, and scrollback are still
  there. The end state of this opinion is *forever memory* — terminal
  output as a permanent, searchable log instead of a ring buffer (today:
  100k lines of tmux history per pane; on the roadmap: a durable on-disk
  archive).
- **AI is a first-class tenant, not a bolt-on.** Coding agents run in
  panes as peers: tabs are teams, panes are teammates, and the `mux` CLI
  gives them shared context — they read each other's terminals, message
  each other, and share a scratchpad (see [agent mesh](#agent-mesh)).
  Planned on top: `? `-prefixed commands, so typing `? why did this build
  fail` asks an AI that already sees your terminal.
- **Themed, not themeable-to-death.** A curated set of themes plus a small
  override surface (`config.toml`, edits apply live). Tab bar, dividers,
  and borders derive from the palette, so the whole window always matches.

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
mux run writer --role impl -- claude   # join + brief + launch + auto-leave
mux peers                              # who's on the team
mux read writer -n 300                 # snapshot a teammate's terminal
mux post writer "left comments in review.md"   # inbox + one [mux] nudge
mux inbox --consume                    # read messages sent to you
mux tell writer "run the tests again"  # type into their terminal directly
mux ctx set build.status green         # shared per-tab scratchpad
mux brief                              # paste-ready team briefing for a prompt
```

Typical setup: split a tab, then launch each agent with
`mux run <name> --role <role> -- <agent-command>` — it registers the pane,
prints the team briefing so it lands in the agent's first screenful, execs
the command, and deregisters when it exits. (Or register manually with
`mux join` and paste `mux brief` output into the agent's prompt.) Agents
shell out to `mux` like any other command.

Prefer `post` for anything a teammate should act on (it queues durably and
injects a single `[mux] new message from …` nudge no matter how many
messages pile up); `tell` types straight into their input — immediate, but
it can interleave with whatever they're doing. Messages up to 16 KiB
(`post`) / 64 KiB (`tell`); registry, inboxes, and per-tab context live
under `~/Library/Application Support/muxterm/`, cleaned automatically when
panes close. Isolation is cooperative — `mux` enforces the tab boundary,
but anything with socket access can drive tmux directly.

### Automatic onboarding

- Panes spawned by muxterm export `MUXTERM=1` and `MUXTERM_SESSION=<name>`,
  so scripts and hooks can detect the mesh without probing tmux (panes
  created before this feature keep their old environment until recreated).
- **Claude Code** becomes fully self-configuring with two hooks in
  `~/.claude/settings.json`: a `SessionStart` hook running
  `mux brief 2>/dev/null || true` (the briefing lands in context the moment
  a session starts inside a muxterm pane; silent no-op elsewhere) and a
  `UserPromptSubmit` hook that drains `mux inbox --consume` into context —
  teammate messages arrive mid-conversation without the agent having to
  fetch. Combined with `mux run reviewer -- claude`, a pane is a named,
  briefed, reachable agent with zero manual prompting.
- Other agents: `mux run` prints the briefing above them at launch, or wire
  `mux brief` into whatever startup-context mechanism they have (aider
  conventions file, custom system prompts).

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
  persistence (`state.rs`), shortcuts (`keys.rs`), agent-mesh state
  (`mesh.rs`), render loop (`app.rs`).
- `src/bin/mux.rs` — the `mux` CLI (agent mesh).
- `crates/egui_term/` — vendored [egui_term](https://github.com/Harzu/egui_term)
  0.1.0 (terminal widget on alacritty_terminal) with five local patches;
  see `crates/egui_term/VENDOR.md`.
