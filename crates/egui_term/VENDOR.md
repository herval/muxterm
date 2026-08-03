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
- **P20** (`src/backend/mod.rs`, `link_match_at`, `runs_of`,
  `set_link_opener`): rejoin tokens a TUI hard-wrapped across rows. P19 only
  stitches rows that are *visually* continuous (`WRAPLINE` or a full last
  column); a TUI that wraps inside its own layout box - Claude Code breaks a
  long path short of the right edge and indents the continuation - leaves
  neither signal, and the wrap whitespace splits the token anyway. Now, when
  the clicked non-whitespace run starts its logical line, the previous line's
  trailing run may be its head; when it ends its line, the next line's
  leading run may be its tail (chained through single-run middle lines,
  capped at `JOIN_CAP` runs, whitespace between runs dropped). A run is only
  taken as a continuation when it is *indented* - boxed TUIs indent the rows
  they wrap onto, while flush-left runs are distinct items (a find/ls column
  of paths must not glue, in the hover highlight or anywhere). The emitter's
  wrap point is invisible, so every joining is a guess: `link_match_at` now
  returns *candidate* texts - every sub-chain's match around the clicked
  run, longest first, the plain unjoined match last - and the `link_opener`
  callback takes `&[String]`. The app opener existence-checks candidates in
  order, which is what discards bad guesses (prose gluing `src/app.rs` +
  `and` falls back to `src/app.rs`); the no-opener fallback tries
  `open::that` per candidate until one succeeds. URLs never match across a
  guessed join (multi-run sub-chains are path-only): URLs open without an
  existence check, and gluing the next line's word onto one would open a
  wrong address - within a P19 logical line the text is genuinely
  contiguous, so URLs still span real soft wraps. Hover originally
  highlighted the *longest* candidate's span, which overreached into the
  next line's first word for prose (the same wart family as P10's `and/or`:
  highlights, opens nothing) - **P28 fixes both**, by asking the app which
  candidate actually resolves.
- **P21** (`src/backend/mod.rs`, `src/lib.rs`, pty event subscription
  thread): visibility-aware repaint gating. Upstream requested an
  unconditional `request_repaint()` per PTY event, so output on any pane -
  a background tab, an unfocused window - forced full-window frames at an
  uncapped rate. New `RepaintPolicy` (`Live`/`Throttled`/`Background`) on
  an `Arc<AtomicU8>` shared with the subscription thread; the host app
  publishes it per pane (`set_repaint_policy`, `&self`, one atomic store)
  and the thread reads it per event. Flood-class events (`Wakeup`,
  `MouseCursorDirty`, `CursorBlinkingChange`) honor the policy: immediate
  under `Live`, else coalesced via `request_repaint_after` (250ms
  throttled / 500ms background - egui keeps only the smallest pending
  delay, and the call is thread-safe). Every other event (`PtyWrite` query
  replies that a hidden program blocks on, `Exit`/`ChildExit`, `Bell`,
  `Title`, OSC 52 clipboard, ...) repaints immediately regardless. The
  dirty flag is still stored before any wake - delayed ones included - so
  a repaint can never observe a clean flag; a stale policy read can only
  misclassify one wake's delay (<=500ms, around a tab/focus flip), never
  lose an update. Default `Live`: a host that never calls
  `set_repaint_policy` keeps upstream behavior.
- **P22** (`src/view.rs`, `src/backend/mod.rs`, `src/theme.rs`): render
  cache. `show()` rebuilt every shape and re-laid-out every galley each
  frame even when nothing changed, so cheap wakes (the app's heartbeat,
  another pane's output) paid a full grid walk per visible pane.
  `TerminalBackend` now carries a `generation` - bumped only when `sync()`
  consumes the dirty flag, the one place fresh content enters
  `last_content` - and a `render_cache` of the last frame's shapes, keyed
  on (generation, pane rect, `FontId`, palette hash - precomputed in
  `TerminalTheme::new`, ppp bits, effective hover = hovered range while
  the mouse is inside it). On a hit the cached shapes are replayed onto
  the painter (galleys are `Arc`'d, the clone is cheap) and the walk is
  skipped. Focus is deliberately not in the key: it affects no shapes,
  only the platform IME anchor (P6), which moved out of the grid walk
  into `emit_ime` so both the hit and rebuild paths issue it every frame
  - otherwise dead-key/CJK composition dies once a pane goes static.
  Hover lives in the key rather than the generation because the P10
  cmd-held hover re-sync rewrites it every frame without marking dirty.
  Guard against egui recreating the font atlas (ppp change, `set_fonts`,
  >0.8 fill - cached galleys would hold UVs into dead texture space):
  `font_atlas_fill_ratio()` only grows within one atlas lifetime, so a
  decrease invalidates. Also: the every-frame no-op `Resize` command now
  bails before taking the terminal lock, so frames never contend with a
  streaming parser.
- **P23** (`src/backend/mod.rs`, `trim_url_punct`): URL matches shed
  trailing sentence punctuation. `.,;:!?')]` are all legal URL chars, so
  prose like "(https://ex.com/tokens/)." matched - and opened - through
  the close-paren and dot. `link_match_at` now trims the trailing
  punctuation run off a URL match before the click test, so the shed
  chars neither underline on hover nor open on click; a mid-URL `?query`
  is untouched, and a closing bracket is shed only while unbalanced
  within the match, keeping Wikipedia-style "..._(disambiguation)" URLs
  whole. Paths are exempt: the app-side opener already strips their
  punctuation candidate-by-candidate under an existence check.
- **P24** (`src/backend/mod.rs`, `pr_regex`, `set_pr_links`,
  `link_match_at`): PR-number tokens as links. A `#<digits>` token (word
  boundary after the digits, so a hex color like `#0044aa` never matches)
  is a link when its number is in an app-registered `Arc<HashSet<u64>>`
  (`set_pr_links`; `None` - the default - keeps `#N` inert, so hosts that
  never call the setter are unaffected). The grid knows nothing about
  PRs: which numbers qualify and what a click opens are both the app's
  call - the match just travels to the P10/P20 `link_opener` as its bare
  `#N` text. Ranked below URLs (the URL char class allows `#`, a
  fragment must not hijack its URL) and above paths, single-run only
  (never joined across P20's guessed wraps); hover underline, press
  swallow, and open all ride the existing P10/P19/P20 machinery.
- **P25** (`src/view.rs`, `TerminalViewState`, `process_left_button`):
  option+click relays a left-button report to the application. P16 made
  the widget silent by default, so forwarding no longer has to be
  all-or-nothing - the old unwinnable case was the client sitting in
  MOUSE_MODE full-time turning *every* click into a report. A press with
  option held (and the terminal in MOUSE_MODE) skips local selection and
  emits a plain SGR press through the wheel's P2 pipeline
  (`BackendCommand::MouseReport`); the modifier bits are stripped so the
  app sees a bare click. `relayed_click` on the view state pairs the
  release with the press even if option was let go first (a press without
  a release would leave the app's button state stuck), and the release
  never touches P8 copy-on-select - the selection was never started.
  Whether the click reaches an app is tmux's call, not the widget's:
  muxterm's tmux.conf routes MouseDown1Pane/MouseUp1Pane by
  `#{mouse_any_flag}` (`send -M` into mouse-tracking apps, consumed
  otherwise), so an option+click on a plain shell does nothing. Plain
  clicks, drags, and double/triple selection are untouched.
- **P26** (`src/backend/mod.rs`, `clear_selection`): a public one-liner
  to drop the local selection (`term.selection = None` + mark dirty).
  Under tmux the wheel is forwarded (P2) and tmux repaints the pane,
  whose in-place erases wipe any local selection - so selecting text then
  scrolling loses it. muxterm's app fixes this by recreating the
  selection in tmux copy-mode (which owns scrollback) when the wheel fires
  over a live selection; it then calls `clear_selection` so the stale
  local highlight goes away and the hand-off runs only once (the next
  wheel sees no local selection and forwards normally, tmux keeping its
  own copy-mode selection). `&self` suffices - the selection lives behind
  the term lock, like the existing selection setters.
- **P27** (`src/backend/mod.rs`, `is_gutter_char`, `content_head`,
  `link_match_at`): gutter-aware wrap joining. Agent CLIs print wrapped
  output inside a box and prefix every continuation row with a gutter glyph
  - codex `| `, claude code `⎿ `, tree-drawn `└ `/`├ `/`│ `. P20 chains
  whitespace-delimited *runs* across rows, and the gutter was a run like any
  other, so a path codex hard-wrapped broke twice over: chaining the glyph
  into the joined text truncated the match at the wrap point (no token's
  char class contains `|`), and the glyph holding run index 0 made the
  backward walk's "this run starts its line" test fail, so the continuation
  never chained back to its head at all - it stayed a bare-relative token
  that resolved to nothing and clicked to nothing. A run of pure gutter
  chars (U+2500..U+259F box drawing and block elements, plus `|` and
  `⎿`) is now chrome: `content_head` skips any leading gutter runs, so
  they are dropped from the chain rather than joined into it, and they do
  not count toward the "lone indented run" test that decides whether the
  chain keeps walking. The indent rule P20 rests on is untouched (a
  flush-left column of paths still does not glue).
- **P28** (`src/backend/mod.rs`, `link_match_at`, `link_pick`,
  `set_link_validator`, `link_hover_memo`): hover underlines what a click
  opens. P20 hands back *guesses*, and the app opener walks them in order
  and takes the first that resolves - but hover took the *longest*, so the
  underline routinely disagreed with the click (covering a path plus the
  next row's first word, or lighting up prose like `and/or` that opens
  nothing). Only the app can settle it: resolving a path needs the pane's
  cwd. `link_match_at` now returns every candidate *with its own grid span*
  (it already computed them), and a new `set_link_validator` callback -
  sibling to P10's `set_link_opener` - decides which one is real;
  `link_pick` takes the first the validator accepts, falling back to
  longest-wins when no validator is registered, so the widget still works
  standalone. `has_link_at` (P10's cmd+click press-swallow gate) uses the
  same pick, so a cmd+click on a dead token falls through to normal mouse
  handling instead of being swallowed for nothing. `LinkAction::Open` still
  hands the opener the *whole* candidate list - it re-checks against a
  freshly fetched cwd, and a stale snapshot must not cost a click. Since
  `view` re-issues Hover every frame while cmd is held, the resolution is
  memoized on `(Point, generation)` and dropped on Clear (so every fresh
  cmd-press re-resolves); this also takes P19/P20's per-frame regex sweep
  off the hover path. muxterm implements the validator with
  `links::resolve_target`, the same function the opener uses, answering
  from the App's once-a-second pane-cwd snapshot rather than a tmux call.
- **P29** (`src/view.rs`, `process_mouse_wheel`, `wheel_delta_to_lines`,
  `src/backend/mod.rs`, `TerminalSize::screen_lines`): honest wheel math.
  Trackpad (Point) deltas were divided by the font's *point* size while a
  rendered row is `font_measure(..).height` - ~1.3x taller - so every tick
  bought more lines than it had travelled; wheel (Line) deltas were
  `ceil`'d per event, turning any sub-line nudge into a whole line with no
  remainder carried across frames; Page deltas were dropped on the floor.
  One pure `pub` helper now normalizes every unit to fractional lines
  (Point / cell height, Page x viewport rows), accumulates the remainder in
  the view state across events *and* frames, and emits only whole lines - a
  direction flip forfeits the stale remainder, since a reversed gesture owes
  nothing to the old one. Each emitted line is still one SGR report under
  tmux (P2); the matching `-N`-less tmux.conf wheel bindings live in
  muxterm, because tmux's *default* copy-mode step is 5 lines per report and
  multiplied the whole gesture by five. Exported from the crate so muxterm's
  `scroll_intercept` computes its copy-mode handoff with the same function
  and the two can't disagree on what a flick is worth; `TerminalSize` grew
  an inherent `screen_lines()` so the app can pass the viewport height
  without naming alacritty's `Dimensions` trait (muxterm has no alacritty
  dependency of its own).
- **P30** (`src/view.rs`, `process_mouse_wheel`): wheel reports carry no
  modifiers. The modifier bits are *added* to the SGR button code (shift+4,
  alt+8, cmd+16), so a held modifier turned button 64/65 into 68/69 or
  80/81, which tmux doesn't route through its wheel bindings at all -
  scrolling silently stopped working with cmd held (i.e. while hovering
  links, P10) or shift held. The report now carries `Modifiers::default()`,
  the same stripping P25 does for relayed clicks, so what the wheel does
  never depends on what else is pressed.
- **P31** (`src/view.rs`, `process_input`, `process_mouse_move`): drag
  selection updates coalesce to one per frame. egui can queue several
  `PointerMoved` events in a single frame (a high-rate trackpad does), and
  each one issued a `SelectUpdate` - a term-lock acquisition and a selection
  recompute - though only the last position of a run can ever be rendered.
  Now only the move that *ends a run* of consecutive moves carries the
  `SelectUpdate`. Per-run and not per-frame on purpose: a release queued
  between two moves has to see the selection extended through the move
  before it, or P8 copy-on-select copies a stale range. Every move still
  updates the grid position and link hover, which are cheap and last-wins,
  so handlers interleaved between moves keep exact coordinates.
- **P32** (`src/view.rs`, `process_input`): un-stick a lost drag. A release
  egui never delivered - focus stolen mid-drag, a native dialog, a release
  that landed in another window - left `is_dragged` set forever, and
  `accepts_pointer` includes it, so the pane went on extending its selection
  under a pointer with no button held. The flag now also clears when egui
  itself reports the primary button up, *unless* this frame's queue carries
  the release, which the event loop handles normally (P8 copy-on-select
  needs the final motion applied first).
- **P33** (`src/backend/mod.rs`, `copy_target`, `row_stops`, `stop_at`,
  `src/view.rs`, `GRID_INSET`): pixel positions as tmux copy-mode motions.
  muxterm drives a pane's tmux copy-mode selection from the app side - the
  left button is still never reported (P16), so a drag is mirrored by
  chained `-X` commands over the control socket - because a *local*
  selection is anchored to grid rows tmux rewrites on every repaint, and
  alacritty drops any selection a rewritten row touches: selecting in a
  pane that prints anything lost the highlight within a second. The one
  thing the app cannot work out for itself is where a pointer lands in
  tmux's terms. `cursor-right -N` counts *characters*, not columns, so a
  wide CJK glyph is one press but two cells, and a column number handed
  straight to it overshoots by one press per wide glyph before the target;
  and it does not stop at the end of a row - it wraps onto the next one -
  so a count past the row's content walks the cursor *down* instead of
  clamping (measured against tmux 3.7b: press 7 lands at column 8, press
  13 at column 20, press 17 at column 24, and press 18 wraps to the next
  row). `copy_target` answers both off the rendered grid, which is the
  same copy tmux is showing: `row_stops` lists the column each character
  starts at, skipping wide-char spacer cells and dropping the trailing run
  of blanks the way tmux's own line length does (it walks back over spaces
  and padding and never looks at their colors, so a row an agent CLI
  padded with coloured spaces ends at its last glyph), plus one final
  entry for the end-of-content boundary - a legal cursor stop, one press
  past which wraps. `stop_at` snaps to the nearest *boundary* rather than
  to the cell, because tmux's selection runs `[lower, upper)` in content
  order: that makes a press and release inside one cell select nothing and
  a drag past a glyph's midpoint take it whole, the same rule
  `selection_side` already applies locally, and it spares the app from
  biasing an endpoint by drag direction. `GRID_INSET` (P17) becomes
  `pub(crate)` so the mapping subtracts the same offset the renderer and
  `process_mouse_move` do. Input handling is untouched: the widget still
  owns the local selection, which now serves as optimistic feedback for
  the first frames of a drag, and P26's `clear_selection` gains its real
  caller - the app hands the highlight over to tmux's own (`mode-style
  reverse`, which arrives as `ESC[7m` and renders through the same fg/bg
  swap, so the swap is invisible) once the first update lands.
- **P34** (`src/theme.rs`, `readable`, `contrast_ratio`, `src/view.rs`,
  `set_min_contrast`): a contrast floor for cell text. A theme only owns the
  16 ANSI slots - the 256-color cube and truecolor escapes are fixed values
  it has no say over - so a TUI that assumes a dark background paints text
  that vanishes on a light theme. Measured from a real pane: xterm index 230
  (`#ffffd7`) on `#ffffff` is a contrast ratio of 1.02, text the same color
  as the paper. `readable` bisects a blend of the foreground toward black or
  white, whichever the background is *not*, until it meets a target WCAG
  ratio, and returns it untouched when it already does - so a palette chosen
  deliberately is unaffected and only the genuinely unreadable moves (index
  230 lands on `#96967e` at exactly 3.0, while the same pane's `#af8700` at
  3.34 is left alone). Blending toward an extreme rather than desaturating
  is what keeps the hue: the text stays recognisably yellow, it just stops
  being invisible. Applied in `show` *after* the dim multiply and the
  inverse/selection swap, so what is guarded is the pair actually painted,
  and memoised per (fg, bg) pair - a grid holds a handful of them and the
  bisection is far too costly to run per cell. The ratio joins the P22 cache
  key, since changing it recolors shapes already cached. Default 1.0, which
  disables the guard: the widget standalone still renders exactly what the
  application asked for, and muxterm sets it from config `min_contrast`.
