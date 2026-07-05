# Vendored egui_term

Vendored from https://github.com/Harzu/egui_term at tag `0.1.0` (commit 84555c7).
Upstream is MIT-licensed; see LICENSE.

Vendored because upstream 0.1.0 has behaviors that break muxterm's split-pane +
tmux-backed design. Local patches:

- **P1** (`src/view.rs`, `process_input`): upstream early-returns unless the
  widget has focus AND contains the pointer. Patched to gate keyboard-ish
  events (Text/Key/Copy/Paste) on focus only, and pointer events
  (MouseWheel/PointerButton/PointerMoved) on hover (or an active drag) only.
  Without this, typing into a focused pane is dead whenever the mouse hovers a
  different pane.
- **P2** (`src/view.rs`, `process_mouse_wheel`): when the terminal is in
  `TermMode::MOUSE_MODE` (tmux `mouse on`), emit SGR mouse-wheel reports
  (buttons 64/65) instead of local `Scroll`, so the wheel drives tmux
  copy-mode scrollback. Non-mouse-mode behavior unchanged.
- **P3** (`src/view.rs`, macOS `Event::Copy` arm): don't write an empty
  selection to the clipboard (the normal case under tmux, where real copies
  arrive via OSC 52).
- **P4** (`src/backend/mod.rs`, pty event subscription thread): exit the loop
  when the channel disconnects instead of busy-looping on `Err`, and stop
  panicking when the app-side receiver is gone.
- **P5** (`src/view.rs`, macOS `Event::Paste` arm): honor
  `TermMode::BRACKETED_PASTE` by wrapping pasted text in `ESC[200~`/`ESC[201~`
  so multi-line pastes don't execute line by line.
- **P6** (`src/view.rs`): IME support. Request platform IME while the widget
  is focused (`PlatformOutput::ime` anchored at the terminal cursor) and write
  `Event::Ime(Commit(text))` to the PTY. Without this, dead keys on layouts
  like US-International (~ ' ` ^ ") and CJK input methods produce nothing or
  a bare base letter, because winit only delivers composition when a widget
  enables IME.
- **P7** (`src/view.rs`): mouse drag under tmux mouse mode. Track the drag
  started at press time (`is_dragged`/`mouse_reporting_drag`) so motion and
  release are routed the same way as the press: as mouse reports whenever the
  application enabled `MOUSE_DRAG`/`MOUSE_MOTION` (tmux `mouse on` is mode
  1002), or as a local selection when Shift bypasses reporting. Upstream
  never set `is_dragged` on a reported press and only forwarded motion under
  mode 1003, so tmux saw press+release with no drag in between and mouse
  selection was impossible.
- **P8** (`src/view.rs`, `src/backend/mod.rs`): copy on select. New
  `TerminalView::set_copy_on_select(bool)` (default off): finishing a local
  mouse selection - drag release, double- or triple-click - emits
  `InputAction::CopySelection`, which copies the selection to the clipboard.
  It reads the new `TerminalBackend::selection_content()` (the live `Term`'s
  `selection_to_string()`), so a SelectStart issued earlier in the same
  frame is included, line breaks survive, and an empty selection is `None` -
  a bare click never touches the clipboard. The macOS `Event::Copy` arm now
  reads the same live selection instead of the flattened render-grid walk,
  so cmd+c copies preserve newlines too.
