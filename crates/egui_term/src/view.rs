use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Point as TerminalGridPoint;
use alacritty_terminal::term::cell;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::{Color, NamedColor};
use std::ops::RangeInclusive;
use egui::epaint::RectShape;
use egui::{Color32, CornerRadius, FontId, Key};
use egui::Modifiers;
use egui::MouseWheelUnit;
use egui::Shape;
use egui::Widget;
use egui::{Align2, Painter, Pos2, Rect, Response, Stroke, Vec2};
use egui::{Id, PointerButton};

use crate::backend::BackendCommand;
use crate::backend::TerminalBackend;
use crate::backend::{
    LinkAction, MouseButton, RenderableContent, SelectionType,
};
use crate::bindings::Binding;
use crate::bindings::{BindingAction, BindingsLayout, InputKind};
use crate::font::TerminalFont;
use crate::theme::TerminalTheme;
use crate::types::Size;

const EGUI_TERM_WIDGET_ID_PREFIX: &str = "egui_term::instance::";

// muxterm patch P17: inset the grid a few px from the pane's top-left so
// column 0 / row 0 aren't drawn flush against (and clipped by) the pane
// edge. The grid size is computed from the inset-reduced area (see resize)
// so the right/bottom edges never overflow; the floor-division remainder
// becomes the right/bottom gutter. Draw origin, mouse->grid mapping, and
// the resize all share this offset so they stay aligned.
const GRID_INSET: Vec2 = Vec2::new(6.0, 3.0);

#[derive(Debug, Clone)]
enum InputAction {
    BackendCall(BackendCommand),
    WriteToClipboard(String),
    // muxterm patch P8: copy the live selection when the action is applied
    // (after any BackendCall queued before it, so a double-click's
    // SelectStart is already visible to it). Empty selections are ignored.
    CopySelection,
    Ignore,
}

#[derive(Clone, Default)]
pub struct TerminalViewState {
    is_dragged: bool,
    scroll_pixels: f32,
    current_mouse_position_on_grid: TerminalGridPoint,
}

pub struct TerminalView<'a> {
    widget_id: Id,
    has_focus: bool,
    size: Vec2,
    backend: &'a mut TerminalBackend,
    font: TerminalFont,
    theme: TerminalTheme,
    bindings_layout: BindingsLayout,
    // muxterm patch P8: finishing a local mouse selection copies it to the
    // clipboard (iTerm's "copy on select"). Off by default.
    copy_on_select: bool,
    // muxterm patch P18: when false, the view renders but ignores all input
    // (keyboard, pointer, link hover) - a read-only preview, e.g. a peeked
    // archived workspace. On by default.
    interactive: bool,
}

impl Widget for TerminalView<'_> {
    fn ui(self, ui: &mut egui::Ui) -> Response {
        let (layout, painter) =
            ui.allocate_painter(self.size, egui::Sense::click());

        let widget_id = self.widget_id;
        let mut state = ui.memory(|m| {
            m.data
                .get_temp::<TerminalViewState>(widget_id)
                .unwrap_or_default()
        });

        self.focus(&layout)
            .resize(&layout)
            .process_input(&layout, &mut state)
            .show(&mut state, &layout, &painter);

        ui.memory_mut(|m| m.data.insert_temp(widget_id, state));
        layout
    }
}

impl<'a> TerminalView<'a> {
    pub fn new(ui: &mut egui::Ui, backend: &'a mut TerminalBackend) -> Self {
        let widget_id = ui.make_persistent_id(format!(
            "{}{}",
            EGUI_TERM_WIDGET_ID_PREFIX, backend.id
        ));

        Self {
            widget_id,
            has_focus: false,
            size: ui.available_size(),
            backend,
            font: TerminalFont::default(),
            theme: TerminalTheme::default(),
            bindings_layout: BindingsLayout::new(),
            copy_on_select: false,
            interactive: true,
        }
    }

    #[inline]
    pub fn set_theme(mut self, theme: TerminalTheme) -> Self {
        self.theme = theme;
        self
    }

    #[inline]
    pub fn set_font(mut self, font: TerminalFont) -> Self {
        self.font = font;
        self
    }

    #[inline]
    pub fn set_focus(mut self, has_focus: bool) -> Self {
        self.has_focus = has_focus;
        self
    }

    #[inline]
    pub fn set_size(mut self, size: Vec2) -> Self {
        self.size = size;
        self
    }

    #[inline]
    pub fn set_copy_on_select(mut self, copy_on_select: bool) -> Self {
        self.copy_on_select = copy_on_select;
        self
    }

    /// muxterm patch P18: render the terminal but ignore all input - a
    /// read-only preview. On by default.
    #[inline]
    pub fn set_interactive(mut self, interactive: bool) -> Self {
        self.interactive = interactive;
        self
    }

    #[inline]
    pub fn add_bindings(
        mut self,
        bindings: Vec<(Binding<InputKind>, BindingAction)>,
    ) -> Self {
        self.bindings_layout.add_bindings(bindings);
        self
    }

    fn focus(self, layout: &Response) -> Self {
        if self.has_focus {
            layout.request_focus();
        } else {
            layout.surrender_focus();
        }

        self
    }

    fn resize(self, layout: &Response) -> Self {
        // P17: size the grid to the area left of the inset so the last
        // column/row lands inside the pane rather than under its edge.
        let usable = (layout.rect.size() - GRID_INSET).max(Vec2::ZERO);
        self.backend.process_command(BackendCommand::Resize(
            Size::from(usable),
            self.font.font_measure(&layout.ctx),
        ));

        self
    }

    fn process_input(
        self,
        layout: &Response,
        state: &mut TerminalViewState,
    ) -> Self {
        // muxterm patch P18: a non-interactive view is a read-only preview -
        // skip keyboard, pointer, and link-hover entirely (it still renders).
        if !self.interactive {
            return self;
        }
        // muxterm patch P1: upstream required focus AND pointer-over to process
        // any input, which kills typing in split panes whenever the mouse
        // rests over another pane. Gate keyboard events on focus and pointer
        // events on hover (or an in-progress drag) independently instead.
        let accepts_keyboard = layout.has_focus();
        let accepts_pointer = layout.contains_pointer() || state.is_dragged;

        // muxterm patch P10: cmd-held link hover, synced every frame rather
        // than only on mouse-move events, so pressing cmd in place lights
        // the link up, releasing cmd (or leaving the pane) clears it, and
        // the hand cursor advertises the click. Runs before the early
        // return so a pane the pointer just left still clears its
        // underline.
        let modifiers = layout.ctx.input(|i| i.modifiers);
        if layout.contains_pointer() && modifiers.command_only() {
            self.backend.process_command(BackendCommand::ProcessLink(
                LinkAction::Hover,
                state.current_mouse_position_on_grid,
            ));
            if self
                .backend
                .last_content()
                .hovered_hyperlink
                .as_ref()
                .is_some_and(|r| {
                    r.contains(&state.current_mouse_position_on_grid)
                })
            {
                layout.ctx.output_mut(|o| {
                    o.cursor_icon = egui::CursorIcon::PointingHand;
                });
            }
        } else if self.backend.last_content().hovered_hyperlink.is_some() {
            self.backend.process_command(BackendCommand::ProcessLink(
                LinkAction::Clear,
                state.current_mouse_position_on_grid,
            ));
        }

        if !accepts_keyboard && !accepts_pointer {
            return self;
        }

        let events = layout.ctx.input(|i| i.events.clone());
        for event in events {
            let mut input_actions = vec![];

            match event {
                // muxterm patch P6: dead keys (e.g. ~ ' ` ^ on US-International
                // layouts) and CJK input methods deliver composed text as IME
                // events; without this the composition is silently dropped.
                egui::Event::Ime(ime) => {
                    if !accepts_keyboard {
                        continue;
                    }
                    if let egui::ImeEvent::Commit(text) = ime {
                        input_actions.push(InputAction::BackendCall(
                            BackendCommand::Write(text.as_bytes().to_vec()),
                        ));
                    }
                },
                egui::Event::Text(_)
                | egui::Event::Key { .. }
                | egui::Event::Copy
                | egui::Event::Paste(_) => {
                    if !accepts_keyboard {
                        continue;
                    }
                    input_actions.push(process_keyboard_event(
                        event,
                        self.backend,
                        &self.bindings_layout,
                        modifiers,
                    ))
                },
                egui::Event::MouseWheel { unit, delta, .. } => {
                    if !accepts_pointer {
                        continue;
                    }
                    input_actions.push(process_mouse_wheel(
                        state,
                        self.backend,
                        &modifiers,
                        self.font.font_type().size,
                        unit,
                        delta,
                    ))
                },
                egui::Event::PointerButton {
                    button,
                    pressed,
                    modifiers,
                    pos,
                    ..
                } => {
                    if !accepts_pointer {
                        continue;
                    }
                    input_actions = process_button_click(
                        state,
                        layout,
                        self.backend,
                        &self.bindings_layout,
                        button,
                        pos,
                        &modifiers,
                        pressed,
                        self.copy_on_select,
                    )
                },
                egui::Event::PointerMoved(pos) => {
                    if !accepts_pointer {
                        continue;
                    }
                    input_actions = process_mouse_move(
                        state,
                        layout,
                        self.backend,
                        pos,
                        &modifiers,
                    )
                },
                _ => {},
            };

            for action in input_actions {
                match action {
                    InputAction::BackendCall(cmd) => {
                        self.backend.process_command(cmd);
                    },
                    InputAction::WriteToClipboard(data) => {
                        layout.ctx.copy_text(data);
                    },
                    InputAction::CopySelection => {
                        if let Some(data) = self.backend.selection_content() {
                            layout.ctx.copy_text(data);
                        }
                    },
                    InputAction::Ignore => {},
                }
            }
        }

        self
    }

    fn show(
        self,
        state: &mut TerminalViewState,
        layout: &Response,
        painter: &Painter,
    ) {
        self.backend.sync();
        let content = self.backend.last_content();
        let layout_min = layout.rect.min;
        let layout_max = layout.rect.max;
        // P17: glyphs, cursor, and hover underlines hang off this inset
        // origin; the background rect below still fills the whole pane so
        // the gutter is painted, not left blank.
        let origin = layout_min + GRID_INSET;
        let cell_height = content.terminal_size.cell_height;
        let cell_width = content.terminal_size.cell_width;
        let font_id = self.font.font_type();

        // muxterm patch P22: when nothing the renderer reads has changed
        // since the last frame, replay that frame's shapes instead of
        // walking the grid and re-laying-out every galley. On a calm pane
        // most frames are hits - heartbeat ticks, the sidebar pulse,
        // another pane's output - and a replay is a Vec clone (galleys
        // are Arc'd).
        let hover = content
            .hovered_hyperlink
            .clone()
            .filter(|r| r.contains(&state.current_mouse_position_on_grid));
        let key = RenderCacheKey {
            generation: self.backend.generation(),
            rect: layout.rect,
            font_id: font_id.clone(),
            theme_hash: self.theme.cache_key(),
            ppp_bits: layout.ctx.pixels_per_point().to_bits(),
            hover,
        };
        let atlas_ratio = painter.fonts(|f| f.font_atlas_fill_ratio());
        if let Some(cache) = &self.backend.render_cache {
            if cache.key == key && atlas_ratio >= cache.atlas_ratio {
                painter.extend(cache.shapes.clone());
                self.emit_ime(layout, content, origin);
                return;
            }
        }

        let global_bg =
            self.theme.get_color(Color::Named(NamedColor::Background));
        let is_app_cursor_mode =
            content.terminal_mode.contains(TermMode::APP_CURSOR);
        // muxterm patch P11: honor DECTCEM (\e[?25l/h). TUI programs hide
        // the cursor while they repaint and address every rewritten line;
        // drawing it anyway makes it visibly dart across the pane.
        let cursor_visible =
            content.terminal_mode.contains(TermMode::SHOW_CURSOR);
        let display_offset = content.grid.display_offset() as i32;
        let fonts = painter.fonts(|f| f.clone());
        // Run batching assumes every ASCII glyph advances exactly one cell,
        // which only a monospace font guarantees; anything else takes the
        // per-cell path below.
        let monospace = painter.fonts(|f| {
            f.glyph_width(&font_id, 'i') == f.glyph_width(&font_id, 'W')
        });

        // Contiguous same-bg cells merge into one rect and contiguous
        // same-fg ASCII into one galley — the difference between ~10k
        // shapes per frame and ~100. Three layers keep merged backgrounds
        // from painting over a neighboring run's glyphs.
        let mut bg_shapes = vec![Shape::Rect(RectShape::filled(
            Rect::from_min_max(layout_min, layout_max),
            CornerRadius::ZERO,
            global_bg,
        ))];
        let mut deco_shapes: Vec<Shape> = Vec::new();
        let mut text_shapes: Vec<Shape> = Vec::new();

        let mut bg_run: Option<BgRun> = None;
        let mut text_run: Option<TextRun> = None;
        // (row, column) the previous cell ended at; a mismatch (row change
        // or the spacer skipped after a wide char) breaks both runs.
        let mut run_cursor: Option<(i32, usize)> = None;

        for indexed in content.grid.display_iter() {
            let flags = indexed.cell.flags;
            let is_wide_char_spacer =
                flags.contains(cell::Flags::WIDE_CHAR_SPACER);
            if is_wide_char_spacer {
                continue;
            }

            let is_wide_char = flags.contains(cell::Flags::WIDE_CHAR);
            let is_inverse = flags.contains(cell::Flags::INVERSE);
            let is_dim =
                flags.intersects(cell::Flags::DIM | cell::Flags::DIM_BOLD);
            let is_selected = content
                .selectable_range
                .is_some_and(|r| r.contains(indexed.point));
            let is_hovered_hyperling =
                content.hovered_hyperlink.as_ref().is_some_and(|r| {
                    r.contains(&indexed.point)
                        && r.contains(&state.current_mouse_position_on_grid)
                });

            let column = indexed.point.column.0;
            let line_num = indexed.point.line.0 + display_offset;
            let x = origin.x + (cell_width * column as f32);
            let y = origin.y + (cell_height * line_num as f32);

            let mut fg = self.theme.get_color(indexed.fg);
            let mut bg = self.theme.get_color(indexed.bg);
            let draw_width = if is_wide_char {
                cell_width * 2.0
            } else {
                cell_width
            };

            if is_dim {
                fg = fg.linear_multiply(0.7);
            }

            if is_inverse || is_selected {
                std::mem::swap(&mut fg, &mut bg);
            }

            if run_cursor != Some((line_num, column)) {
                flush_bg(&mut bg_run, &mut bg_shapes, cell_height);
                flush_text(&mut text_run, &mut text_shapes, painter, &font_id);
            }
            run_cursor =
                Some((line_num, column + if is_wide_char { 2 } else { 1 }));

            if global_bg != bg {
                match &mut bg_run {
                    Some(run) if run.color == bg => run.width += draw_width,
                    _ => {
                        flush_bg(&mut bg_run, &mut bg_shapes, cell_height);
                        bg_run = Some(BgRun {
                            x,
                            y,
                            width: draw_width,
                            color: bg,
                        });
                    },
                }
            } else {
                flush_bg(&mut bg_run, &mut bg_shapes, cell_height);
            }

            // Handle hovered hyperlink underline
            if is_hovered_hyperling {
                let underline_height = y + cell_height;
                deco_shapes.push(Shape::LineSegment {
                    points: [
                        Pos2::new(x, underline_height),
                        Pos2::new(x + draw_width, underline_height),
                    ],
                    stroke: Stroke::new(cell_height * 0.15, fg).into(),
                });
            }

            // Handle cursor rendering (P11: only while DECTCEM-visible)
            if cursor_visible && content.grid.cursor.point == indexed.point {
                let cursor_rect = Rect::from_min_size(
                    Pos2::new(x, y),
                    Vec2::new(draw_width, cell_height),
                );
                let cursor_color = self.theme.get_color(content.cursor.fg);
                deco_shapes.push(Shape::Rect(RectShape::filled(
                    cursor_rect,
                    CornerRadius::default(),
                    cursor_color,
                )));
            }

            // Draw text content
            if indexed.c == ' ' || indexed.c == '\t' {
                // No glyph, but an open run swallows the gap so a row of
                // prose stays a single galley.
                if let Some(run) = &mut text_run {
                    run.text.push(' ');
                }
            } else {
                if cursor_visible
                    && content.grid.cursor.point == indexed.point
                    && is_app_cursor_mode
                {
                    std::mem::swap(&mut fg, &mut bg);
                }

                if monospace && !is_wide_char && indexed.c.is_ascii_graphic() {
                    match &mut text_run {
                        Some(run) if run.color == fg => run.text.push(indexed.c),
                        _ => {
                            flush_text(
                                &mut text_run,
                                &mut text_shapes,
                                painter,
                                &font_id,
                            );
                            text_run = Some(TextRun {
                                x,
                                y,
                                text: indexed.c.to_string(),
                                color: fg,
                            });
                        },
                    }
                } else {
                    // Wide and non-ASCII glyphs keep the centered per-cell
                    // placement: their advance need not match the grid.
                    flush_text(
                        &mut text_run,
                        &mut text_shapes,
                        painter,
                        &font_id,
                    );
                    text_shapes.push(Shape::text(
                        &fonts,
                        Pos2 {
                            x: x + (draw_width / 2.0),
                            y,
                        },
                        Align2::CENTER_TOP,
                        indexed.c,
                        font_id.clone(),
                        fg,
                    ));
                }
            }
        }
        flush_bg(&mut bg_run, &mut bg_shapes, cell_height);
        flush_text(&mut text_run, &mut text_shapes, painter, &font_id);

        // P22: one Vec in paint order (bg under deco under text) so next
        // frame can replay it wholesale on a key match.
        let mut shapes = bg_shapes;
        shapes.append(&mut deco_shapes);
        shapes.append(&mut text_shapes);
        painter.extend(shapes.clone());
        self.emit_ime(layout, content, origin);
        self.backend.render_cache = Some(RenderCache {
            key,
            shapes,
            atlas_ratio,
        });
    }

    /// muxterm patch P6/P22: emit the platform IME anchor at the cursor.
    /// It is per-frame platform output, not a shape, so the render-cache
    /// hit path must issue it too - without this, dead-key and CJK
    /// composition dies the moment a pane's content goes static. Mirrors
    /// the grid walk's cursor placement exactly: DECTCEM-visible and on
    /// screen (a scrolled-back cursor sits below the viewport and gets no
    /// anchor, just as the walk draws it no rect).
    fn emit_ime(
        &self,
        layout: &Response,
        content: &RenderableContent,
        origin: Pos2,
    ) {
        if !self.has_focus
            || !content.terminal_mode.contains(TermMode::SHOW_CURSOR)
        {
            return;
        }
        let point = content.grid.cursor.point;
        let line_num = point.line.0 + content.grid.display_offset() as i32;
        if line_num < 0
            || line_num >= content.terminal_size.screen_lines() as i32
        {
            return;
        }
        let cell_width = content.terminal_size.cell_width;
        let cell_height = content.terminal_size.cell_height;
        let width = if content.cursor.flags.contains(cell::Flags::WIDE_CHAR)
        {
            cell_width * 2.0
        } else {
            cell_width
        };
        let cursor_rect = Rect::from_min_size(
            Pos2::new(
                origin.x + cell_width * point.column.0 as f32,
                origin.y + cell_height * line_num as f32,
            ),
            Vec2::new(width, cell_height),
        );
        layout.ctx.output_mut(|o| {
            o.ime = Some(egui::output::IMEOutput {
                rect: layout.rect,
                cursor_rect,
            });
        });
    }
}

/// muxterm patch P22: the last frame's rendered shapes plus everything
/// they were built from. `show()` replays `shapes` while `key` still
/// matches and the font atlas is still the one the galleys were laid out
/// against. Owned by the backend so it dies with the pane.
pub(crate) struct RenderCache {
    key: RenderCacheKey,
    /// bg, deco, text shapes concatenated in paint order.
    shapes: Vec<Shape>,
    /// `Fonts::font_atlas_fill_ratio()` at build time. The ratio only
    /// grows within one atlas lifetime; a decrease means egui recreated
    /// the atlas (ppp change, set_fonts, or >0.8 fill) and every cached
    /// galley holds UVs into dead texture space.
    atlas_ratio: f32,
}

/// P22: every input `show()` reads that isn't already versioned by the
/// backend's generation (grid, selection, cursor, mode, and size all are:
/// they only change under the dirty flag `sync()` consumes). Focus is
/// deliberately absent - it affects no shape, only the IME side effect,
/// which `emit_ime` re-issues on hits.
#[derive(PartialEq)]
struct RenderCacheKey {
    generation: u64,
    /// Shape positions are absolute, so a moved pane can't replay.
    rect: Rect,
    font_id: FontId,
    theme_hash: u64,
    /// `pixels_per_point().to_bits()`: glyph layout rounds to physical
    /// pixels (P12), so a ppp change lays out differently at equal pt.
    ppp_bits: u32,
    /// The hover underline actually drawn - the hovered range paints only
    /// while the mouse sits inside it. In the key rather than the
    /// generation because the P10 hover re-sync rewrites it on every
    /// cmd-held frame without marking the backend dirty.
    hover: Option<RangeInclusive<TerminalGridPoint>>,
}

/// A horizontal stretch of cells sharing one background color.
struct BgRun {
    x: f32,
    y: f32,
    width: f32,
    color: Color32,
}

/// A horizontal stretch of same-color ASCII text, laid out as one galley.
struct TextRun {
    x: f32,
    y: f32,
    text: String,
    color: Color32,
}

fn flush_bg(
    run: &mut Option<BgRun>,
    shapes: &mut Vec<Shape>,
    cell_height: f32,
) {
    if let Some(run) = run.take() {
        shapes.push(Shape::Rect(RectShape::filled(
            Rect::from_min_size(
                Pos2::new(run.x, run.y),
                // + 1.0 is to fill grid border
                Vec2::new(run.width + 1., cell_height + 1.),
            ),
            CornerRadius::ZERO,
            run.color,
        )));
    }
}

fn flush_text(
    run: &mut Option<TextRun>,
    shapes: &mut Vec<Shape>,
    painter: &Painter,
    font_id: &FontId,
) {
    if let Some(mut run) = run.take() {
        // Spaces pushed to keep the run alive add nothing at the tail.
        while run.text.ends_with(' ') {
            run.text.pop();
        }
        if !run.text.is_empty() {
            let galley =
                painter.layout_no_wrap(run.text, font_id.clone(), run.color);
            shapes.push(Shape::galley(
                Pos2::new(run.x, run.y),
                galley,
                run.color,
            ));
        }
    }
}

fn process_keyboard_event(
    event: egui::Event,
    backend: &TerminalBackend,
    bindings_layout: &BindingsLayout,
    modifiers: Modifiers,
) -> InputAction {
    match event {
        egui::Event::Text(text) => {
            process_text_event(&text, modifiers, backend, bindings_layout)
        },
        egui::Event::Paste(text) => InputAction::BackendCall(
            #[cfg(not(any(target_os = "ios", target_os = "macos")))]
            if modifiers.contains(Modifiers::COMMAND | Modifiers::SHIFT) {
                BackendCommand::Write(text.as_bytes().to_vec())
            } else {
                // Hotfix - Send ^V when there's not selection on view.
                BackendCommand::Write([0x16].to_vec())
            },
            #[cfg(any(target_os = "ios", target_os = "macos"))]
            {
                // muxterm patch P5: honor bracketed paste so multi-line
                // pastes don't execute line by line.
                let mode = backend.last_content().terminal_mode;
                if mode.contains(TermMode::BRACKETED_PASTE) {
                    let mut buf = b"\x1b[200~".to_vec();
                    buf.extend_from_slice(text.as_bytes());
                    buf.extend_from_slice(b"\x1b[201~");
                    BackendCommand::Write(buf)
                } else {
                    BackendCommand::Write(text.as_bytes().to_vec())
                }
            },
        ),
        egui::Event::Copy => {
            #[cfg(not(any(target_os = "ios", target_os = "macos")))]
            if modifiers.contains(Modifiers::COMMAND | Modifiers::SHIFT) {
                let content = backend.selectable_content();
                InputAction::WriteToClipboard(content)
            } else {
                // Hotfix - Send ^C when there's not selection on view.
                InputAction::BackendCall(BackendCommand::Write([0x3].to_vec()))
            }
            #[cfg(any(target_os = "ios", target_os = "macos"))]
            {
                // muxterm patch P3: an empty selection must not clobber the
                // clipboard (under tmux the real copy arrives via OSC 52).
                // Patch P8 reads the live selection, which also preserves
                // line breaks (the render-grid walk flattened them).
                match backend.selection_content() {
                    Some(content) => InputAction::WriteToClipboard(content),
                    None => InputAction::Ignore,
                }
            }
        },
        egui::Event::Key {
            key,
            pressed,
            modifiers,
            ..
        } => process_keyboard_key(
            backend,
            bindings_layout,
            key,
            modifiers,
            pressed,
        ),
        _ => InputAction::Ignore,
    }
}

fn process_text_event(
    text: &str,
    modifiers: Modifiers,
    backend: &TerminalBackend,
    bindings_layout: &BindingsLayout,
) -> InputAction {
    if let Some(key) = Key::from_name(text) {
        if bindings_layout.get_action(
            InputKind::KeyCode(key),
            modifiers,
            backend.last_content().terminal_mode,
        ) == BindingAction::Ignore
        {
            InputAction::BackendCall(BackendCommand::Write(
                text.as_bytes().to_vec(),
            ))
        } else {
            InputAction::Ignore
        }
    } else {
        InputAction::BackendCall(BackendCommand::Write(
            text.as_bytes().to_vec(),
        ))
    }
}

fn process_keyboard_key(
    backend: &TerminalBackend,
    bindings_layout: &BindingsLayout,
    key: Key,
    modifiers: Modifiers,
    pressed: bool,
) -> InputAction {
    if !pressed {
        return InputAction::Ignore;
    }

    let terminal_mode = backend.last_content().terminal_mode;
    let binding_action = bindings_layout.get_action(
        InputKind::KeyCode(key),
        modifiers,
        terminal_mode,
    );

    match binding_action {
        BindingAction::Char(c) => {
            let mut buf = [0, 0, 0, 0];
            let str = c.encode_utf8(&mut buf);
            InputAction::BackendCall(BackendCommand::Write(
                str.as_bytes().to_vec(),
            ))
        },
        BindingAction::Esc(seq) => InputAction::BackendCall(
            BackendCommand::Write(seq.as_bytes().to_vec()),
        ),
        _ => InputAction::Ignore,
    }
}

fn process_mouse_wheel(
    state: &mut TerminalViewState,
    backend: &mut TerminalBackend,
    modifiers: &Modifiers,
    font_size: f32,
    unit: MouseWheelUnit,
    delta: Vec2,
) -> InputAction {
    let lines = match unit {
        MouseWheelUnit::Line => {
            (delta.y.signum() * delta.y.abs().ceil()) as i32
        },
        MouseWheelUnit::Point => {
            state.scroll_pixels -= delta.y;
            let lines = (state.scroll_pixels / font_size).trunc();
            state.scroll_pixels %= font_size;
            -lines as i32
        },
        MouseWheelUnit::Page => 0,
    };

    if lines == 0 {
        return InputAction::Ignore;
    }

    // muxterm patch P2: when the application enabled mouse reporting (tmux
    // `mouse on`), forward the wheel as mouse button 64/65 reports so tmux
    // drives its own copy-mode scrollback. Upstream only did a local scroll,
    // which is a no-op under tmux (the local scrollback is empty).
    let terminal_mode = backend.last_content().terminal_mode;
    if terminal_mode.intersects(TermMode::MOUSE_MODE) {
        let button = if lines > 0 {
            MouseButton::ScrollUp
        } else {
            MouseButton::ScrollDown
        };
        let point = state.current_mouse_position_on_grid;
        for _ in 0..lines.abs() {
            backend.process_command(BackendCommand::MouseReport(
                button.clone(),
                *modifiers,
                point,
                true,
            ));
        }
        InputAction::Ignore
    } else {
        InputAction::BackendCall(BackendCommand::Scroll(lines))
    }
}

#[allow(clippy::too_many_arguments)]
fn process_button_click(
    state: &mut TerminalViewState,
    layout: &Response,
    backend: &TerminalBackend,
    bindings_layout: &BindingsLayout,
    button: PointerButton,
    position: Pos2,
    modifiers: &Modifiers,
    pressed: bool,
    copy_on_select: bool,
) -> Vec<InputAction> {
    match button {
        PointerButton::Primary => process_left_button(
            state,
            layout,
            backend,
            bindings_layout,
            position,
            modifiers,
            pressed,
            copy_on_select,
        ),
        _ => vec![],
    }
}

#[allow(clippy::too_many_arguments)]
fn process_left_button(
    state: &mut TerminalViewState,
    layout: &Response,
    backend: &TerminalBackend,
    bindings_layout: &BindingsLayout,
    position: Pos2,
    modifiers: &Modifiers,
    pressed: bool,
    copy_on_select: bool,
) -> Vec<InputAction> {
    // muxterm patch P16 (supersedes P7's left-button forwarding): the left
    // button is NEVER reported to the application - clicks and drags always
    // drive the widget's local selection, shift or not. Forwarding was
    // unwinnable: under tmux `mouse on` the client is in MOUSE_MODE for its
    // whole life, so every click went to tmux, and whatever the bindings,
    // tmux hardcodes passing the second press of a double-click through to a
    // pane whose app enabled mouse tracking (the agent CLIs do) - the app's
    // cursor moved on clicks and no binding could stop it. Local selection
    // already covers what the mouse is for in a terminal: click = caret
    // anchor only (no app sees it), drag = select (P8 copy-on-select),
    // double/triple = word/line (P14). The wheel is still reported (P2) -
    // that is how tmux scrollback works.
    let terminal_mode = backend.last_content().terminal_mode;
    if pressed {
        // muxterm patch P10: a cmd+click on a link is for us, not the
        // application. Only the press decides: if the LinkOpen binding
        // matches and there is a link-shaped token under the pointer,
        // swallow the press and let the release open it.
        let link_click = bindings_layout.get_action(
            InputKind::Mouse(PointerButton::Primary),
            *modifiers,
            terminal_mode,
        ) == BindingAction::LinkOpen
            && backend.has_link_at(state.current_mouse_position_on_grid);
        if link_click {
            state.is_dragged = false;
            vec![]
        } else {
            process_left_button_pressed(state, layout, position)
        }
    } else {
        process_left_button_released(
            state,
            layout,
            backend,
            bindings_layout,
            position,
            modifiers,
            copy_on_select,
        )
    }
}

fn process_left_button_pressed(
    state: &mut TerminalViewState,
    layout: &Response,
    position: Pos2,
) -> Vec<InputAction> {
    state.is_dragged = true;
    vec![InputAction::BackendCall(build_start_select_command(
        layout, position,
    ))]
}

#[allow(clippy::too_many_arguments)]
fn process_left_button_released(
    state: &mut TerminalViewState,
    layout: &Response,
    backend: &TerminalBackend,
    bindings_layout: &BindingsLayout,
    position: Pos2,
    modifiers: &Modifiers,
    copy_on_select: bool,
) -> Vec<InputAction> {
    state.is_dragged = false;
    let mut actions = vec![];
    let mut opened_link = false;
    if layout.double_clicked() || layout.triple_clicked() {
        actions.push(InputAction::BackendCall(build_start_select_command(
            layout, position,
        )));
    } else {
        let terminal_content = backend.last_content();
        let binding_action = bindings_layout.get_action(
            InputKind::Mouse(PointerButton::Primary),
            *modifiers,
            terminal_content.terminal_mode,
        );

        if binding_action == BindingAction::LinkOpen {
            actions.push(InputAction::BackendCall(BackendCommand::ProcessLink(
                LinkAction::Open,
                state.current_mouse_position_on_grid,
            )));
            opened_link = true;
        }
    }
    // muxterm patch P8: every way a local selection can finish ends here -
    // a drag release, or the double/triple-click SelectStart pushed just
    // above (CopySelection reads the live selection, so it sees it). A
    // plain click resolves to an empty selection and copies nothing.
    // muxterm patch P10: except a link-opening click, which never touched
    // the selection - re-copying whatever older selection is still live
    // would clobber the clipboard as a side effect of following a link.
    if copy_on_select && !opened_link {
        actions.push(InputAction::CopySelection);
    }
    actions
}

fn build_start_select_command(
    layout: &Response,
    cursor_position: Pos2,
) -> BackendCommand {
    let selection_type = if layout.double_clicked() {
        SelectionType::Semantic
    } else if layout.triple_clicked() {
        SelectionType::Lines
    } else {
        SelectionType::Simple
    };

    BackendCommand::SelectStart(
        selection_type,
        cursor_position.x - layout.rect.min.x - GRID_INSET.x,
        cursor_position.y - layout.rect.min.y - GRID_INSET.y,
    )
}

fn process_mouse_move(
    state: &mut TerminalViewState,
    layout: &Response,
    backend: &TerminalBackend,
    position: Pos2,
    modifiers: &Modifiers,
) -> Vec<InputAction> {
    let terminal_content = backend.last_content();
    // P17: shift into grid space past the top-left inset before mapping to
    // a cell; selection_point clamps the negative gutter region to cell 0.
    let cursor_x = position.x - layout.rect.min.x - GRID_INSET.x;
    let cursor_y = position.y - layout.rect.min.y - GRID_INSET.y;
    state.current_mouse_position_on_grid = TerminalBackend::selection_point(
        cursor_x,
        cursor_y,
        &terminal_content.terminal_size,
        terminal_content.grid.display_offset(),
    );

    let mut actions = vec![];
    // muxterm patch P16: drags are always the local selection - left-button
    // events are never reported to the application (see process_left_button).
    if state.is_dragged {
        actions.push(InputAction::BackendCall(BackendCommand::SelectUpdate(
            cursor_x, cursor_y,
        )));
    }

    // Handle link hover if applicable
    if modifiers.command_only() {
        actions.push(InputAction::BackendCall(BackendCommand::ProcessLink(
            LinkAction::Hover,
            state.current_mouse_position_on_grid,
        )));
    }

    actions
}
