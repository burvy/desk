//! Add an agent, or change one's settings: the window version of `desk spawn`.

use desk_core::Workspace;
use desk_core::adapter::{self, Origin, Preset};
use desk_core::agent::{self, AgentConfig};
use eframe::egui::{self, RichText};

pub struct AgentForm {
    /// Set when editing an existing agent; its name can't change.
    editing: bool,
    name: String,
    cli: String,
    model: String,
    dir: String,
    role: String,
    bypass: bool,
    terminal: bool,
    active: bool,
    presets: Vec<Preset>,
    /// Presets whose program is on PATH, checked once when the form opens.
    installed: Vec<String>,
    error: Option<String>,
    /// "Custom command": any CLI, started by the command typed here.
    custom: bool,
    command: String,
    ready: String,
}

impl AgentForm {
    pub fn new_agent(ws: &Workspace, has_orchestrator: bool) -> Self {
        let (presets, installed) = load_presets(ws);
        // The first installed preset, so the default works out of the box.
        let cli = installed.first().or(presets.first().map(|p| &p.name)).cloned();
        let mut f = AgentForm {
            editing: false,
            name: String::new(),
            cli: cli.unwrap_or_default(),
            model: String::new(),
            dir: ws.config.projects.values().next().map(|p| p.display().to_string()).unwrap_or_default(),
            role: if has_orchestrator { "worker" } else { "orchestrator" }.into(),
            bypass: true,
            terminal: false,
            active: true,
            presets,
            installed,
            error: None,
            custom: false,
            command: String::new(),
            ready: String::new(),
        };
        f.model = f.preset().and_then(|p| p.models.first().cloned()).unwrap_or_default();
        f.terminal = f.prefers_terminal();
        f
    }

    pub fn edit(ws: &Workspace, name: &str, cfg: &AgentConfig) -> Self {
        let (presets, installed) = load_presets(ws);
        // A CLI made from a typed command opens as that command, to edit.
        let made =
            presets.iter().find(|p| p.name == cfg.cli).and_then(|p| Some((adapter::command_of(p)?, p)));
        AgentForm {
            custom: made.is_some(),
            command: made.as_ref().map(|(c, _)| c.clone()).unwrap_or_default(),
            ready: made.and_then(|(_, p)| p.ready.first().cloned()).unwrap_or_default(),
            editing: true,
            name: name.into(),
            cli: cfg.cli.clone(),
            model: cfg.model.clone(),
            dir: cfg.dir.display().to_string(),
            role: cfg.role.clone(),
            bypass: cfg.permissions == "bypass",
            terminal: cfg.terminal,
            active: cfg.active,
            presets,
            installed,
            error: None,
        }
    }

    fn installed(&self, name: &str) -> bool {
        self.installed.iter().any(|n| n == name)
    }

    /// CLIs desk can't read a stream from run better in a terminal.
    fn prefers_terminal(&self) -> bool {
        self.preset().is_some_and(|p| p.output == "text")
    }

    fn preset(&self) -> Option<&Preset> {
        self.presets.iter().find(|p| p.name == self.cli)
    }

    /// Draws the window. Returns `Some(message)` once the agent is saved,
    /// and `open` goes false when the form should close.
    pub fn show(&mut self, ctx: &egui::Context, ws: &Workspace, open: &mut bool) -> Option<String> {
        let mut saved = None;
        let title = if self.editing { format!("{} settings", self.name) } else { "Add agent".into() };
        let mut keep = true;
        egui::Window::new(title)
            .id(egui::Id::unique("agent_form"))
            .collapsible(false)
            .resizable(false)
            .open(&mut keep)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                saved = self.body(ui, ws);
            });
        *open = keep && saved.is_none();
        saved
    }

    fn body(&mut self, ui: &mut egui::Ui, ws: &Workspace) -> Option<String> {
        egui::Grid::new("agent_grid").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
            ui.label("Name");
            ui.add_enabled(
                !self.editing,
                egui::TextEdit::singleline(&mut self.name).hint_text("e.g. mike").desired_width(200.0),
            );
            ui.end_row();

            ui.label("CLI");
            let before = self.cli.clone();
            let shown = match self.preset() {
                _ if self.custom => "Custom command".to_string(),
                Some(p) => p.description.clone(),
                None => self.cli.clone(),
            };
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("cli").selected_text(shown).width(300.0).show_ui(ui, |ui| {
                    for p in &self.presets {
                        let installed = self.installed(&p.name);
                        let mut text = RichText::new(format!("{} — {}", p.name, p.description));
                        if !installed {
                            text = text.weak();
                        }
                        let picked = !self.custom && self.cli == p.name;
                        let r = ui.selectable_label(picked, text).on_hover_text(if installed {
                            "installed"
                        } else {
                            "not found on PATH"
                        });
                        if r.clicked() {
                            self.cli = p.name.clone();
                            self.custom = false;
                        }
                    }
                    ui.separator();
                    if ui
                        .selectable_label(self.custom, "Custom command…")
                        .on_hover_text("Any CLI: type the command that starts it")
                        .clicked()
                    {
                        self.custom = true;
                    }
                });
                if let Some(msg) = self.remove_button(ui, ws) {
                    self.error = Some(msg);
                }
                if !self.custom
                    && ui
                        .button("Type a command…")
                        .on_hover_text("Any CLI, like freebuff or free-pi-cli: type the command that starts it")
                        .clicked()
                {
                    self.custom = true;
                }
            });
            if self.cli != before {
                self.model = self.preset().and_then(|p| p.models.first().cloned()).unwrap_or_default();
                if !self.editing {
                    self.terminal = self.prefers_terminal();
                }
            }
            ui.end_row();

            if self.custom {
                ui.label("Command");
                ui.vertical(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.command)
                            .hint_text("e.g. freebuff, or free-pi-cli --yolo")
                            .desired_width(300.0),
                    );
                    ui.label(
                        RichText::new(
                            "Runs in a terminal in this window, and desk types messages in. \
                             Put any flags (model, permissions) in the command.",
                        )
                        .weak()
                        .small(),
                    );
                });
                ui.end_row();
                ui.label("Ready text");
                ui.add(
                    egui::TextEdit::singleline(&mut self.ready)
                        .hint_text("optional")
                        .desired_width(300.0),
                )
                .on_hover_text(
                    "Text its screen shows only when it's waiting for you to type, like a prompt hint. \
                     desk then types only at that point; without it, desk types once the screen is still for 3s.",
                );
                ui.end_row();
            } else {
            ui.label("Model");
            ui.vertical(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.model)
                        .hint_text("any model the CLI takes; empty: its default")
                        .desired_width(300.0),
                );
                let suggestions = self.preset().map(|p| p.models.clone()).unwrap_or_default();
                if !suggestions.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        for m in suggestions {
                            if ui.small_button(&m).clicked() {
                                self.model = m;
                            }
                        }
                    });
                }
            });
            ui.end_row();
            }

            ui.label("Project");
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.dir)
                            .hint_text("the folder with the code")
                            .desired_width(300.0),
                    );
                    if ui.button("Browse…").clicked()
                        && let Some(d) = rfd::FileDialog::new().set_title("Project folder").pick_folder()
                    {
                        self.dir = d.display().to_string();
                    }
                });
                if !ws.config.projects.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        for (name, path) in &ws.config.projects {
                            if ui.small_button(name).on_hover_text(path.display().to_string()).clicked() {
                                self.dir = path.display().to_string();
                            }
                        }
                    });
                }
            });
            ui.end_row();

            ui.label("Role");
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.role, "orchestrator".into(), "orchestrator")
                    .on_hover_text("Plans, writes tasks, assigns and reviews");
                ui.radio_value(&mut self.role, "worker".into(), "worker")
                    .on_hover_text("Does the tasks it's given");
                if !matches!(self.role.as_str(), "orchestrator" | "worker") {
                    ui.label(RichText::new(&self.role).weak());
                }
            });
            ui.end_row();

            if !self.custom {
            ui.label("Runs");
            ui.vertical(|ui| {
                ui.radio_value(&mut self.terminal, true, "In a terminal, in this window")
                    .on_hover_text("Its CLI's own interface, live. desk types messages in; you can type too.");
                ui.radio_value(&mut self.terminal, false, "Headless")
                    .on_hover_text("One command per turn, with a parsed transcript (Claude, OpenCode). Works with `desk run`.");
            });
            ui.end_row();

            ui.label("Permissions");
            ui.checkbox(&mut self.bypass, "Full: no permission prompts").on_hover_text(
                "Headless runs can't answer prompts. Without this, tools that need approval fail.",
            );
            ui.end_row();
            }
        });

        if let Some(p) = self.preset().filter(|_| !self.custom)
            && !self.installed(&p.name)
        {
            ui.label(RichText::new(format!("`{}` isn't on PATH yet.", p.program)).color(crate::app::YELLOW));
        }
        if let Some(e) = &self.error {
            ui.label(RichText::new(e).color(crate::app::RED));
        }
        ui.add_space(4.0);
        let label = if self.editing { "Save" } else { "Add" };
        let ready = !self.name.trim().is_empty() && !self.dir.trim().is_empty();
        if ui.add_enabled(ready, egui::Button::new(label)).clicked() {
            match self.save(ws) {
                Ok(msg) => return Some(msg),
                Err(e) => self.error = Some(format!("{e:#}")),
            }
        }
        None
    }

    /// For a CLI you added: remove it here, unless another agent uses it.
    fn remove_button(&mut self, ui: &mut egui::Ui, ws: &Workspace) -> Option<String> {
        if self.custom || adapter::origin(ws, &self.cli) != Origin::Custom {
            return None;
        }
        let users: Vec<String> = agent::names(ws)
            .into_iter()
            .filter(|n| *n != self.name.trim())
            .filter(|n| AgentConfig::load(ws, n).is_ok_and(|c| c.cli == self.cli))
            .collect();
        let hint = if users.is_empty() {
            format!("Delete presets/{}.toml", self.cli)
        } else {
            format!("Used by {}", users.join(", "))
        };
        let button = ui.add_enabled(users.is_empty(), egui::Button::new("Remove CLI").small());
        if !button
            .on_hover_text(hint)
            .on_disabled_hover_text(format!("Used by {}", users.join(", ")))
            .clicked()
        {
            return None;
        }
        if let Err(e) = adapter::remove(ws, &self.cli) {
            return Some(format!("{e:#}"));
        }
        (self.presets, self.installed) = load_presets(ws);
        self.cli =
            self.installed.first().or(self.presets.first().map(|p| &p.name)).cloned().unwrap_or_default();
        None
    }

    fn save(&self, ws: &Workspace) -> anyhow::Result<String> {
        let name = self.name.trim();
        let (cli, model, terminal) = if self.custom {
            let (cli, text) = adapter::from_command(&self.command, &self.ready)?;
            let preset = Preset::parse(&cli, &text)?;
            if !preset.installed() {
                anyhow::bail!("`{}` isn't on PATH. Install it, or type its full path.", preset.program);
            }
            adapter::save(ws, &cli, &text)?;
            (cli, String::new(), true)
        } else {
            (self.cli.clone(), self.model.trim().to_string(), self.terminal)
        };
        let cfg = AgentConfig {
            cli,
            model,
            dir: ws.resolve_dir(self.dir.trim())?,
            role: self.role.clone(),
            permissions: if self.bypass { "bypass" } else { "default" }.into(),
            active: self.active,
            terminal,
        };
        agent::spawn(ws, name, cfg)?;
        Ok(if self.editing {
            format!("{name} updated; it applies from its next run")
        } else {
            format!("{name} is ready. It starts when it gets a message.")
        })
    }
}

fn load_presets(ws: &Workspace) -> (Vec<Preset>, Vec<String>) {
    let presets: Vec<Preset> = adapter::names(ws).iter().filter_map(|n| Preset::load(ws, n).ok()).collect();
    let installed = presets.iter().filter(|p| p.installed()).map(|p| p.name.clone()).collect();
    (presets, installed)
}
