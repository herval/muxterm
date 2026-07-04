use egui::{Color32, CornerRadius, Pos2, Rect, RichText, Vec2};

use crate::theme::UiTheme;

pub enum TabBarAction {
    Select(usize),
    NewTab,
}

pub fn show(
    ctx: &egui::Context,
    labels: &[String],
    active: usize,
    t: &UiTheme,
) -> Vec<TabBarAction> {
    let mut actions = Vec::new();
    egui::TopBottomPanel::top("tab_bar")
        .exact_height(32.0)
        .frame(egui::Frame::new().fill(t.tab_bar_bg))
        .show(ctx, |ui| {
            ui.horizontal_centered(|ui| {
                ui.add_space(6.0);
                ui.spacing_mut().item_spacing.x = 3.0;
                for (i, label) in labels.iter().enumerate() {
                    let is_active = i == active;
                    let text = RichText::new(format!("{}  {}", i + 1, label))
                        .size(12.0)
                        .color(if is_active { t.text } else { t.text_dim });
                    let button = egui::Button::new(text)
                        .fill(if is_active {
                            t.tab_active_bg
                        } else {
                            Color32::TRANSPARENT
                        })
                        .corner_radius(CornerRadius::same(5))
                        .min_size(Vec2::new(130.0, 24.0));
                    let resp = ui.add(button);
                    if resp.clicked() {
                        actions.push(TabBarAction::Select(i));
                    }
                    if is_active {
                        ui.painter().rect_filled(
                            Rect::from_min_max(
                                Pos2::new(resp.rect.min.x + 4.0, resp.rect.max.y - 2.0),
                                Pos2::new(resp.rect.max.x - 4.0, resp.rect.max.y),
                            ),
                            CornerRadius::same(1),
                            t.accent,
                        );
                    } else if resp.hovered() {
                        ui.painter().rect_filled(
                            resp.rect,
                            CornerRadius::same(5),
                            t.tab_hover_bg.gamma_multiply(0.5),
                        );
                    }
                }
                let plus = egui::Button::new(
                    RichText::new("+").size(15.0).color(t.text_dim),
                )
                .fill(Color32::TRANSPARENT)
                .corner_radius(CornerRadius::same(5))
                .min_size(Vec2::new(26.0, 24.0));
                if ui.add(plus).clicked() {
                    actions.push(TabBarAction::NewTab);
                }
            });
        });
    actions
}
