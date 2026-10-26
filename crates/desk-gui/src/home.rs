//! The start screen: recent workspaces, open one, or make a new one.
//! The recent list lives in the OS config folder (`%APPDATA%\desk` on Windows).

use desk_core::Workspace;
use eframe::egui::{self, RichText, Ui};
use std::path::{Path, PathBuf};

pub struct Home {
    recent: Vec<PathBuf>,
    new_dir: String,
    new_human: String,
    error: Option<String>,
}

impl Default for Home {
    fn default() -> Self {
        Home { recent: recent(), new_dir: String::new(), new_human: default_human(), error: None }
    }
}

impl Home {
    /// Draws the screen; returns a workspace once one is opened.
    pub fn ui(&mut self, ui: &mut Ui) -> Option<Workspace> {
        let mut opened = None;
        egui::CentralPanel::default_margins().show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(40.0);
                ui.heading(RichText::new("desk").size(32.0));
                ui.label(RichText::new("A small, transparent harness for a team of coding agents.").weak());
                ui.add_space(24.0);
            });
            let width = 560f32.min(ui.available_width());
            ui.horizontal(|ui| {
                ui.add_space((ui.available_width() - width) / 2.0);
                ui.vertical(|ui| {
                    ui.set_width(width);
                    opened = self.body(ui);
                });
            });
        });
        opened
    }

    fn body(&mut self, ui: &mut Ui) -> Option<Workspace> {
        let mut opened = None;
        if !self.recent.is_empty() {
            ui.label(RichText::new("RECENT").weak().small());
            let mut forget = None;
            for p in &self.recent {
                ui.horizontal(|ui| {
                    if ui.link(p.display().to_string()).clicked() {
                        match Workspace::open(p) {
                            Ok(ws) => opened = Some(ws),
                            Err(e) => self.error = Some(format!("{e:#}")),
                        }
                    }
                    if ui.small_button("×").on_hover_text("Forget (the folder is kept)").clicked() {
                        forget = Some(p.clone());
                    }
                });
            }
            if let Some(p) = forget {
                self.recent.retain(|r| *r != p);
                save_recent(&self.recent);
            }
            ui.add_space(16.0);
        }

        ui.label(RichText::new("OPEN").weak().small());
        if ui.button("Open a workspace folder…").clicked()
            && let Some(dir) = rfd::FileDialog::new().set_title("Open a desk workspace").pick_folder()
        {
            match Workspace::open(&dir) {
                Ok(ws) => opened = Some(ws),
                Err(_) => self.error = Some(format!("{} has no desk.toml", dir.display())),
            }
        }
        ui.add_space(16.0);

        ui.label(RichText::new("NEW WORKSPACE").weak().small());
        ui.label(
            RichText::new(
                "A folder for the team's tasks, messages and memory. Keep it apart from your code; \
                 agents are pointed at projects separately.",
            )
            .weak(),
        );
        egui::Grid::new("new").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
            ui.label("Folder");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.new_dir).desired_width(360.0));
                if ui.button("Browse…").clicked()
                    && let Some(dir) = rfd::FileDialog::new().set_title("New workspace folder").pick_folder()
                {
                    self.new_dir = dir.display().to_string();
                }
            });
            ui.end_row();
            ui.label("Your name");
            ui.add(egui::TextEdit::singleline(&mut self.new_human).desired_width(200.0))
                .on_hover_text("Agents address you by this; you get an inbox under it.");
            ui.end_row();
        });
        let ready = !self.new_dir.trim().is_empty() && !self.new_human.trim().is_empty();
        if ui.add_enabled(ready, egui::Button::new("Create")).clicked() {
            let dir = PathBuf::from(self.new_dir.trim());
            match std::fs::create_dir_all(&dir).map_err(Into::into).and_then(|_| {
                if dir.join("desk.toml").exists() {
                    Workspace::open(&dir) // already a workspace: just open it
                } else {
                    Workspace::init(&dir, self.new_human.trim())
                }
            }) {
                Ok(ws) => opened = Some(ws),
                Err(e) => self.error = Some(format!("{e:#}")),
            }
        }
        if let Some(e) = &self.error {
            ui.add_space(8.0);
            ui.label(RichText::new(e).color(crate::app::RED));
        }
        opened
    }
}

/// A file in desk's settings folder, which is created if needed.
pub fn config_file(name: &str) -> Option<PathBuf> {
    let dir = config_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join(name))
}

fn config_dir() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    base.map(|b| b.join("desk"))
}

/// Recently opened workspaces, newest first, that still exist.
pub fn recent() -> Vec<PathBuf> {
    let Some(file) = config_dir().map(|d| d.join("recent.txt")) else { return Vec::new() };
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .map(PathBuf::from)
        .filter(|p| p.join("desk.toml").is_file())
        .collect()
}

pub fn remember(root: &Path) {
    let mut list = recent();
    list.retain(|p| p != root);
    list.insert(0, root.to_path_buf());
    list.truncate(10);
    save_recent(&list);
}

fn save_recent(list: &[PathBuf]) {
    let Some(dir) = config_dir() else { return };
    let text: String = list.iter().map(|p| format!("{}\n", p.display())).collect();
    let _ = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(dir.join("recent.txt"), text));
}

/// Your git name if you have one, else your login name, as a folder-safe word.
fn default_human() -> String {
    let git = crate::quiet(std::process::Command::new("git").args(["config", "user.name"])).output();
    git.ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("USERNAME").or_else(|_| std::env::var("USER")).ok())
        .map(|s| desk_core::fsutil::slug(&s))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "human".into())
}
