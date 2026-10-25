//! The CLIs view: edit how desk runs each CLI. A preset is a small TOML
//! file; saving writes the workspace's `presets/<name>.toml`.

use desk_core::Workspace;
use desk_core::adapter::{self, Origin, Preset};
use eframe::egui::{self, RichText, Ui};

#[derive(Default)]
pub struct CliEditor {
    selected: Option<String>,
    text: String,
    /// The text as last loaded or saved, to tell whether there are edits.
    saved: String,
    new_name: String,
    error: Option<String>,
}

impl CliEditor {
    fn select(&mut self, ws: &Workspace, name: &str) {
        self.selected = Some(name.to_string());
        self.text = adapter::source(ws, name).unwrap_or_default();
        self.saved = self.text.clone();
        self.error = None;
    }

    /// Draws the view; returns a message for the top bar after an action.
    pub fn ui(&mut self, ui: &mut Ui, ws: &Workspace) -> Option<String> {
        let mut flash = None;
        ui.horizontal(|ui| {
            ui.heading("CLIs");
            ui.label(RichText::new("How desk runs each agent CLI. Changes apply from the next run.").weak());
        });
        ui.separator();
        let names = adapter::names(ws);
        if self.selected.is_none()
            && let Some(first) = names.first()
        {
            self.select(ws, &first.clone());
        }
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(170.0);
                for n in &names {
                    let tag = match adapter::origin(ws, n) {
                        Origin::Builtin => "",
                        Origin::Overridden => " · edited",
                        Origin::Custom => " · yours",
                    };
                    let picked = self.selected.as_deref() == Some(n.as_str());
                    if ui.selectable_label(picked, format!("{n}{tag}")).clicked() && !picked {
                        self.select(ws, n);
                    }
                }
                ui.add_space(8.0);
                ui.label(RichText::new("NEW CLI").weak().small());
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.new_name).hint_text("name").desired_width(100.0),
                    );
                    if ui.button("Add").clicked() {
                        let name = self.new_name.trim().to_string();
                        match adapter::save(ws, &name, adapter::TEMPLATE) {
                            Ok(()) => {
                                self.select(ws, &name);
                                self.new_name.clear();
                                flash = Some(format!("added {name}; fill in its command"));
                            }
                            Err(e) => self.error = Some(format!("{e:#}")),
                        }
                    }
                });
            });
            ui.separator();
            ui.vertical(|ui| flash = self.editor(ui, ws).or(flash.take()));
        });
        flash
    }

    fn editor(&mut self, ui: &mut Ui, ws: &Workspace) -> Option<String> {
        let name = self.selected.clone()?;
        let origin = adapter::origin(ws, &name);
        let mut flash = None;
        ui.horizontal(|ui| {
            ui.label(RichText::new(&name).strong());
            if let Ok(p) = Preset::parse(&name, &self.text) {
                if p.installed() {
                    ui.label(RichText::new(format!("`{}` found", p.program)).weak());
                } else {
                    ui.label(
                        RichText::new(format!("`{}` isn't on PATH", p.program)).color(crate::app::YELLOW),
                    );
                }
            }
            let dirty = self.text != self.saved;
            if ui.add_enabled(dirty, egui::Button::new("Save")).clicked() {
                match adapter::save(ws, &name, &self.text) {
                    Ok(()) => {
                        self.saved = self.text.clone();
                        self.error = None;
                        flash = Some(format!("saved presets/{name}.toml"));
                    }
                    Err(e) => self.error = Some(format!("{e:#}")),
                }
            }
            if dirty && ui.button("Discard").clicked() {
                self.text = self.saved.clone();
                self.error = None;
            }
            let undo = match origin {
                Origin::Overridden => {
                    Some(("Revert to built-in", "Delete presets/{name}.toml; the default comes back"))
                }
                Origin::Custom => Some(("Delete", "Delete presets/{name}.toml")),
                Origin::Builtin => None,
            };
            if let Some((label, hover)) = undo
                && ui.button(label).on_hover_text(hover.replace("{name}", &name)).clicked()
            {
                match adapter::remove(ws, &name) {
                    Ok(()) if origin == Origin::Custom => {
                        self.selected = None;
                        flash = Some(format!("deleted {name}"));
                    }
                    Ok(()) => {
                        self.select(ws, &name);
                        flash = Some(format!("{name} is back to the built-in"));
                    }
                    Err(e) => self.error = Some(format!("{e:#}")),
                }
            }
        });
        if origin == Origin::Builtin {
            ui.label(
                RichText::new("Built in. Saving keeps your copy in this workspace's presets/.")
                    .weak()
                    .small(),
            );
        }
        // Check as you type, so mistakes show before saving.
        let problem =
            self.error.clone().or_else(|| Preset::parse(&name, &self.text).err().map(|e| format!("{e:#}")));
        if let Some(e) = problem {
            ui.label(RichText::new(e).color(crate::app::RED));
        }
        egui::ScrollArea::vertical().id_salt("preset_text").auto_shrink(false).show(ui, |ui| {
            ui.add(
                egui::TextEdit::multiline(&mut self.text)
                    .code_editor()
                    .desired_width(f32::INFINITY)
                    .desired_rows(20),
            );
        });
        ui.label(
            RichText::new(
                "Placeholders: {model} {session} {dir} {workspace} {role_file} {prompt_file}. \
                 Fields: see presets/README.md in the workspace.",
            )
            .weak()
            .small(),
        );
        flash
    }
}
