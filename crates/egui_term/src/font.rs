use egui::{Context, FontId};

use crate::types::Size;

#[derive(Debug, Clone)]
pub struct FontSettings {
    pub font_type: FontId,
}

impl Default for FontSettings {
    fn default() -> Self {
        Self {
            font_type: FontId::monospace(14.0),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TerminalFont {
    font_type: FontId,
}

impl Default for TerminalFont {
    fn default() -> Self {
        Self {
            font_type: FontSettings::default().font_type,
        }
    }
}

impl TerminalFont {
    pub fn new(settings: FontSettings) -> Self {
        Self {
            font_type: settings.font_type,
        }
    }

    pub fn font_type(&self) -> FontId {
        self.font_type.clone()
    }

    pub fn font_measure(&self, ctx: &Context) -> Size {
        let (width, height) = ctx.fonts(|f| {
            (
                f.glyph_width(&self.font_type, 'm'),
                f.row_height(&self.font_type),
            )
        });

        // muxterm patch P12: quantize the cell width to the physical pixel
        // grid. epaint's layout rounds the pen to a whole pixel after every
        // glyph, so a batched galley advances by round(advance*ppp)/ppp per
        // char; a cell grid built on the raw advance drifts away from the
        // glyphs (~0.2pt/cell at 12pt on retina), which reads as extra
        // spaces before every run break on a long row.
        let ppp = ctx.pixels_per_point();
        Size::new((width * ppp).round() / ppp, height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::Color32;

    /// A batched row galley must land every glyph exactly on the cell grid
    /// derived from font_measure, or long same-color runs visibly drift off
    /// the columns that per-cell shapes (cursor, colored words) snap to.
    #[test]
    fn cell_width_matches_galley_advance() {
        let font_type = FontId::monospace(12.0);
        for ppp in [1.0f32, 1.25, 1.5, 1.75, 2.0] {
            let ctx = Context::default();
            ctx.set_pixels_per_point(ppp);
            let _ = ctx.run(Default::default(), |_| {});
            let font = TerminalFont::new(FontSettings {
                font_type: font_type.clone(),
            });
            let cell = font.font_measure(&ctx);
            let galley = ctx.fonts(|f| {
                f.layout_no_wrap(
                    "a".repeat(80),
                    font_type.clone(),
                    Color32::WHITE,
                )
            });
            let expected = 80.0 * cell.width;
            assert!(
                (galley.size().x - expected).abs() < 0.5,
                "ppp={ppp}: galley width {} vs 80 cells {expected}",
                galley.size().x,
            );
        }
    }
}
