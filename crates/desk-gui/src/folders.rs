//! The project's folders (desk.toml's `[projects]`): every agent is told
//! about all of them, so work can span folders that belong together.

use desk_core::Workspace;
use desk_core::agent::{self, AgentConfig};
use eframe::egui::{self, RichText, Ui};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Default)]
pub struct Folders {
    name: String,
    path: String,
}

impl Folders {
    /// Returns a note for the status line when something changed.
    pub fn ui(&mut self, ui: &mut Ui, ws: &Workspace) -> Option<anyhow::Result<String>> {
        ui.heading("Folders");
        ui.label(
            RichText::new(
                "Every agent is told about all of these in each message, and Claude can work in each. \
                 An agent's own folder is still where it starts.",
            )
            .weak(),
        );
        ui.separator();
        let mut projects = ws.config.projects.clone();
        let mut changed = None;

        egui::Grid::new("folders").num_columns(3).spacing([12.0, 6.0]).show(ui, |ui| {
            for (name, path) in &ws.config.projects {
                ui.label(RichText::new(name).strong());
                ui.label(path.display().to_string());
                if ui.small_button("Remove").clicked() {
                    projects.remove(name);
                    changed = Some(format!("removed {name}"));
                }
                ui.end_row();
            }
        });
        if ws.config.projects.is_empty() {
            ui.label(RichText::new("No folders yet.").weak());
        }

        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.name).hint_text("name, e.g. site").desired_width(120.0),
            );
            ui.add(egui::TextEdit::singleline(&mut self.path).hint_text("folder").desired_width(260.0));
            if ui.button("Browse…").clicked()
                && let Some(d) = rfd::FileDialog::new().set_title("Project folder").pick_folder()
            {
                if self.name.trim().is_empty() {
                    self.name = name_for(&d);
                }
                self.path = d.display().to_string();
            }
            let ready = !self.name.trim().is_empty() && !self.path.trim().is_empty();
            if ui.add_enabled(ready, egui::Button::new("Add")).clicked() {
                projects.insert(self.name.trim().to_string(), PathBuf::from(self.path.trim()));
                changed = Some(format!("added {}", self.name.trim()));
                *self = Folders::default();
            }
        });

        // Folders agents already work in, one click away.
        let listed: Vec<PathBuf> = ws.config.projects.values().cloned().collect();
        let mut used: Vec<PathBuf> = agent::names(ws)
            .iter()
            .filter_map(|n| AgentConfig::load(ws, n).ok().map(|c| c.dir))
            .filter(|d| ws.other_projects(d).len() == listed.len())
            .collect();
        used.sort();
        used.dedup();
        if !used.is_empty() {
            ui.add_space(8.0);
            ui.label(RichText::new("Your agents' folders, not listed yet:").weak().small());
            ui.horizontal_wrapped(|ui| {
                for d in used {
                    if ui.small_button(format!("+ {}", d.display())).clicked() {
                        let name = unique(&projects, name_for(&d));
                        changed = Some(format!("added {name}"));
                        projects.insert(name, d);
                    }
                }
            });
        }

        let note = changed?;
        Some(save(ws, &projects).map(|_| note))
    }
}

fn save(ws: &Workspace, projects: &BTreeMap<String, PathBuf>) -> anyhow::Result<()> {
    for (name, path) in projects {
        if !path.is_dir() {
            anyhow::bail!("{name}: {} isn't a folder", path.display());
        }
    }
    ws.set_projects(projects)
}

/// A folder's name as a project name: letters, digits, - and _.
fn name_for(dir: &std::path::Path) -> String {
    let base = dir.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
    let name: String =
        base.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '-' }).collect();
    let name = name.trim_matches('-').to_string();
    if name.is_empty() { "project".into() } else { name }
}

fn unique(projects: &BTreeMap<String, PathBuf>, name: String) -> String {
    (1..)
        .map(|i| if i == 1 { name.clone() } else { format!("{name}-{i}") })
        .find(|n| !projects.contains_key(n))
        .unwrap()
}
