use egui::{Color32, CornerRadius, RichText, Vec2};

use crate::theme;

pub enum TabBarAction {
    Select(usize),
    NewTab,
}

pub fn show(
    ctx: &egui::Context,
    labels: &[String],
    active: usize,
) -> Vec<TabBarAction> {
    let mut actions = Vec::new();
    egui::TopBottomPanel::top("tab_bar")
        .exact_height(32.0)
        .frame(egui::Frame::new().fill(theme::TAB_BAR_BG))
        .show(ctx, |ui| {
            ui.horizontal_centered(|ui| {
                ui.add_space(6.0);
                ui.spacing_mut().item_spacing.x = 3.0;
                for (i, label) in labels.iter().enumerate() {
                    let is_active = i == active;
                    let text = RichText::new(format!("{}  {}", i + 1, label))
                        .size(12.0)
                        .color(if is_active {
                            theme::TEXT
                        } else {
                            theme::TEXT_DIM
                        });
                    let button = egui::Button::new(text)
                        .fill(if is_active {
                            theme::TAB_ACTIVE_BG
                        } else {
                            Color32::TRANSPARENT
                        })
                        .corner_radius(CornerRadius::same(5))
                        .min_size(Vec2::new(130.0, 24.0));
                    let resp = ui.add(button);
                    if resp.clicked() {
                        actions.push(TabBarAction::Select(i));
                    }
                    if !is_active && resp.hovered() {
                        ui.painter().rect_filled(
                            resp.rect,
                            CornerRadius::same(5),
                            theme::TAB_HOVER_BG.gamma_multiply(0.5),
                        );
                    }
                }
                let plus = egui::Button::new(
                    RichText::new("+").size(15.0).color(theme::TEXT_DIM),
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
