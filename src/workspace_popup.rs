//! The cmd+n workspace-creation popup: an egui modal (styled by the app's
//! visuals, like the rest of the chrome) that gathers a starting folder, an
//! optional git worktree, the task prompt, and the agent + model. Unlike the
//! settings window (a hand-painted monospace grid) this needs real text entry,
//! so it uses ordinary egui widgets - they already pick up the theme.

use std::path::Path;

use egui::{Align2, Color32, FontId, Key, Shadow, Vec2};

use muxterm::agent::{self, Agent};

use crate::theme::UiTheme;

/// Live state of the open popup, owned by the App as `Option<NewWorkspaceForm>`.
pub struct NewWorkspaceForm {
    pub folder: String,
    pub create_worktree: bool,
    pub prompt: String,
    pub agent: &'static str,
    pub model: String,
    /// Cached `is_git_repo` for `folder`, refreshed only when the folder text
    /// settles on an existing directory (so we don't spawn `git` per keystroke).
    is_repo: bool,
    checked: String,
}

impl NewWorkspaceForm {
    pub fn new(folder: String, agent: &'static str, model: String) -> Self {
        let mut form = Self {
            folder,
            create_worktree: false,
            prompt: String::new(),
            agent,
            model,
            is_repo: false,
            checked: String::from("\0"), // force a first check
        };
        form.refresh_repo();
        // A git repo defaults the checkbox on - the common case for cmd+n.
        form.create_worktree = form.is_repo;
        form
    }

    /// Re-run the git-repo probe when the folder changed to an existing dir.
    /// Non-dirs (mid-typing paths) skip the subprocess and read as non-repos.
    fn refresh_repo(&mut self) {
        if self.folder == self.checked {
            return;
        }
        self.checked = self.folder.clone();
        let path = Path::new(self.folder.trim());
        self.is_repo = path.is_dir() && crate::workspace::is_git_repo(path);
        if !self.is_repo {
            self.create_worktree = false;
        }
    }
}

pub enum Outcome {
    None,
    Cancel,
    Create,
}

pub fn show(
    ctx: &egui::Context,
    form: &mut NewWorkspaceForm,
    th: &UiTheme,
    font: &FontId,
) -> Outcome {
    form.refresh_repo();
    let mut outcome = Outcome::None;

    egui::Window::new("New Workspace")
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .default_width(460.0)
        .frame(egui::Frame::new().fill(th.bg).inner_margin(16.0).shadow(
            Shadow {
                offset: [0, 6],
                blur: 24,
                spread: 0,
                color: Color32::from_black_alpha(100),
            },
        ))
        .show(ctx, |ui| {
            ui.set_width(430.0);
            ui.label(egui::RichText::new("New workspace").color(th.text).size(15.0));
            ui.add_space(10.0);

            ui.label(egui::RichText::new("Folder").color(th.text_dim).size(12.0));
            ui.add(
                egui::TextEdit::singleline(&mut form.folder)
                    .hint_text("~/path/to/project")
                    .desired_width(f32::INFINITY),
            );

            ui.add_space(6.0);
            ui.add_enabled_ui(form.is_repo, |ui| {
                ui.checkbox(&mut form.create_worktree, "Create git worktree");
            });
            if !form.is_repo && !form.folder.trim().is_empty() {
                ui.label(
                    egui::RichText::new("not a git repo — worktree disabled")
                        .color(th.text_dim)
                        .size(11.0),
                );
            }

            ui.add_space(10.0);
            ui.label(
                egui::RichText::new("What do you want to work on?")
                    .color(th.text_dim)
                    .size(12.0),
            );
            ui.add(
                egui::TextEdit::multiline(&mut form.prompt)
                    .desired_rows(4)
                    .desired_width(f32::INFINITY),
            );

            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Agent").color(th.text_dim).size(12.0));
                for a in agent::AGENTS {
                    if ui.selectable_label(form.agent == a.id, a.label).clicked() {
                        form.agent = a.id;
                        // Keep the model valid for the newly-picked agent.
                        if !current_agent(form.agent).models.contains(&form.model.as_str()) {
                            form.model = default_model(form.agent);
                        }
                    }
                }

                ui.add_space(12.0);
                ui.label(egui::RichText::new("Model").color(th.text_dim).size(12.0));
                let ag = current_agent(form.agent);
                egui::ComboBox::from_id_salt("ws-model")
                    .selected_text(if form.model.is_empty() {
                        "default".to_string()
                    } else {
                        form.model.clone()
                    })
                    .show_ui(ui, |ui| {
                        for m in ag.models {
                            ui.selectable_value(&mut form.model, m.to_string(), *m);
                        }
                    });
            });

            ui.add_space(14.0);
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    outcome = Outcome::Cancel;
                }
                // Right-align the primary action.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("  Create  ").clicked() {
                        outcome = Outcome::Create;
                    }
                });
            });
        });

    // cmd+Enter submits from anywhere in the form (Enter alone is a newline in
    // the prompt field). Esc is handled by the App, like the settings window.
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, Key::Enter)) {
        outcome = Outcome::Create;
    }
    let _ = font;
    outcome
}

fn current_agent(id: &str) -> &'static Agent {
    agent::by_id(id).unwrap_or_else(agent::default_agent)
}

/// The dropdown's default selection for an agent: its first curated model.
pub fn default_model(id: &str) -> String {
    current_agent(id)
        .models
        .first()
        .map(|m| m.to_string())
        .unwrap_or_default()
}
