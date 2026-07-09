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
- **P9** (`src/view.rs`, `src/backend/mod.rs`): render performance. Local
  scrollback is capped at 200 lines (tmux owns real history) and `sync()`
  skips the whole-grid clone on clean frames via a dirty flag set by the
  PTY event thread and grid-mutating commands. `show()` merges contiguous
  same-bg cells into single rects and contiguous same-fg ASCII into single
  galleys (painted bg → decorations → text) instead of one shape per cell,
  and cell metrics are exact f32 rather than truncated u16 so batched runs
  align with the grid to the sub-pixel.
- **P10** (`src/view.rs`, `src/backend/mod.rs`): cmd+click opens links and
  file paths. Upstream's LinkOpen release path was unreachable under tmux
  (`mouse on` keeps `MOUSE_MODE` set, so every unshifted press became a
  mouse report), only knew URL schemes, opened from possibly-stale hover
  state, and panicked on a failed `open::that`. Now: a press whose binding
  resolves to LinkOpen with a link-shaped token under the pointer
  (`TerminalBackend::has_link_at`) bypasses mouse reporting so the release
  can open it; a new `path_regex` matches absolute/`~/`/dot-relative/bare
  relative paths with optional `:line[:col]` suffixes alongside the URL
  regex (URLs win ties); Open re-resolves the match at the clicked point on
  the live `Term` (`bounds_to_string`, so wrapped lines join) and hands the
  text to an app-provided `set_link_opener` callback - the app resolves
  relative paths against the pane's cwd and existence-checks before
  opening; without a callback, `open::that` with errors ignored. Hover is
  frame-synced while cmd is held (underline appears without mouse motion,
  clears on cmd release or pointer exit, hand cursor over matches), a
  link-opening release skips copy-on-select, and the match helpers are free
  fns generic over `EventListener` so `term::test::mock_term` can drive
  unit tests.
- **P11** (`src/view.rs`): honor DECTCEM cursor visibility. The renderer
  drew the block cursor whenever it passed `grid.cursor.point`, ignoring
  `TermMode::SHOW_CURSOR`, so TUI repaints (which hide the cursor, rewrite
  lines by cursor-addressing, then show it) flashed the cursor at every
  intermediate position - a fast "scanning" flicker across the pane. The
  cursor rect, its IME anchor, and the cursor-cell fg/bg swap now only
  apply while the mode contains `SHOW_CURSOR`.
- **P12** (`src/font.rs`, `font_measure`): quantize the cell width to the
  physical pixel grid. epaint's text layout rounds the pen x to a whole
  pixel after every glyph, so a P9 batched galley advances by
  round(advance*ppp)/ppp per char - fractionally less than the raw
  `glyph_width` P9 used as the cell width. Long same-color runs drifted
  left of the grid (~0.2pt/cell at 12pt on retina) while everything drawn
  per-cell (cursor, the next colored run) snapped back to it, which read
  as phantom extra spaces before every color change and a gap that grew
  ahead of the cursor while typing. `font_measure` now returns
  round(width*ppp)/ppp, which the per-glyph pen rounding then matches
  exactly at every column.
- **P13** (`src/bindings.rs`, `platform_keyboard_bindings`): standard macOS
  line-editing chords, matching iTerm2's default key maps. option+left/right
  send `ESC b`/`ESC f` (readline backward/forward-word) instead of the
  cross-platform `CSI 1;3D`/`1;3C`; cmd+left/right send `Ctrl-A`/`Ctrl-E`
  (line start/end); cmd+delete sends `Ctrl-U` (kill to line start).
  option+delete already sent `ESC DEL` (backward-kill-word). The cmd entries
  reuse the `Modifiers::COMMAND` arrow/backspace Binding keys so they *replace*
  the cross-platform defaults on macOS only (where `command` == ⌘), leaving
  Linux/Windows Ctrl+arrow word-jumps untouched.
- **P14** (`src/backend/mod.rs`, `term::Config`): double-click selects a whole
  non-whitespace run. Double-click already maps to `SelectionType::Semantic`,
  but alacritty's default `semantic_escape_chars` (",│`|:\"' ()[]{}<>\t")
  treats quotes/brackets/colons/etc. as word boundaries, so a double-click on
  `foo(bar)` or `a/b:c` only grabbed a fragment. Setting the boundary set to
  just whitespace (`" \t"`) makes Semantic selection cover every contiguous
  non-whitespace character, matching iTerm/macOS word selection.
- **P15** (`src/bindings.rs`, `default_keyboard_bindings`): Shift+Enter as a
  soft line break. It bound to a bare `\x0d`, byte-identical to Enter, so an
  app couldn't distinguish the two and always submitted. Now, when the
  terminal mode carries `DISAMBIGUATE_ESC_CODES` (set when an app enables the
  kitty keyboard protocol - Claude Code and other TUIs do), Shift+Enter
  reports the kitty CSI-u sequence `ESC [ 13 ; 2 u` (Enter keycode 13, Shift
  = modifier 2), which the app decodes as a `return` key carrying the shift
  flag and inserts a newline instead of submitting. A second binding keeps
  the bare CR when that mode is off, so a plain shell (which can't decode
  CSI-u) is unaffected.
- **P16** (`src/view.rs`, `process_left_button`, `process_mouse_move`):
  the left mouse button is never reported to the application - clicks and
  drags always drive the widget's local selection, shift or not (supersedes
  P7's left-button forwarding; P7's drag tracking remains for the local
  path). Forwarding was unwinnable under tmux: `mouse on` keeps the client
  in MOUSE_MODE for its whole life, so every click became a mouse report,
  and tmux hardcodes passing the second press of a double-click through to
  a pane whose app enabled mouse tracking (the agent CLIs do) - the app's
  cursor moved on clicks and no binding could stop it. Local selection
  covers what the mouse is for: click anchors quietly, drag selects (P8
  copy-on-select), double/triple selects word/line (P14). The wheel is
  still reported (P2) - that is how tmux scrollback works.
- **P17** (`src/view.rs`, `resize`, `show`, `process_mouse_move`,
  `build_start_select_command`): inset the grid from the pane's top-left
  corner (`GRID_INSET`). Upstream drew column 0 / row 0 at exactly
  `rect.min`, so the first cell's glyphs rendered flush against - and were
  clipped by - the pane edge (the floor-division remainder already left a
  gutter on the right/bottom, so only the top-left touched). Three call
  sites share one offset: `resize` computes cols/rows from the
  inset-reduced area so the far edges still land inside the pane, `show`
  hangs glyphs/cursor/underlines off `layout_min + GRID_INSET` (the
  background rect still fills the whole pane so the gutter is painted), and
  the mouse->grid mapping subtracts the same inset before locating a cell
  (`selection_point` clamps a click in the gutter to cell 0).
- **P18** (`src/view.rs`, `TerminalView::interactive` + `set_interactive` +
  `process_input`): a read-only mode. When `interactive` is false the view
  still renders (resize + show run) but `process_input` early-returns, so
  keyboard, pointer, and cmd-link-hover are all ignored. muxterm uses it to
  make a peeked *archived* workspace a look-but-don't-touch preview; the pane
  is also washed with `archived_overlay` and denied focus (the
  `PaneId(u64::MAX)` sentinel) at the call site. On by default.
- **P19** (`src/backend/mod.rs`, `Cargo.toml`): multi-row link detection. A
  URL or path that soft-wraps onto the next row was only clickable up to the
  wrap point. Alacritty's grid regex search breaks a match at any row boundary
  whose last cell lacks the `WRAPLINE` flag (`regex_search_internal`'s
  linebreak handling), and tmux repaints soft-wrapped output as discrete
  cursor-addressed rows that carry no `WRAPLINE` - so the match always
  truncated at the edge. Link detection no longer uses alacritty's search
  (`RegexIter`/`RegexSearch`/`Match`, all `WRAPLINE`-gated). `link_match_at`
  reconstructs the clicked point's *logical line* - the run of visually
  continuous rows, joined when a row is `WRAPLINE`-flagged *or* its last column
  holds a glyph (a full row, the tmux case) - into a string with a parallel
  grid `Point` per char (wide-char spacer cells skipped), matches the URL/path
  regexes over it, and maps the hit back to a `Point` span for hover + open.
  The regexes moved from alacritty's `RegexSearch` to the `regex` crate (the
  same patterns, `\u{..}` rewritten `\x{..}`; already in the lock tree). This
  subsumes the old single-row and native-`WRAPLINE` paths (both still tested).
