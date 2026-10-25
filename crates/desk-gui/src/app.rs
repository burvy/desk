//! The window. Left: agents. Center: a transcript, task, inbox or activity.
//! Right: the task board. Top: asks. Bottom: send a message.

use crate::Embedded;
use crate::agent_form::AgentForm;
use crate::clis::CliEditor;
use crate::snapshot::{AgentRow, Snapshot};
use desk_core::adapter::{self, Event, Preset};
use desk_core::agent::{self, AgentConfig};
use desk_core::message::{self, Kind};
use desk_core::task::{self, Task};
use desk_core::transcript::{self, Entry};
use desk_core::{Workspace, ask, fsutil, supervisor};
use eframe::egui::{self, Color32, RichText, Ui};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const BOARD: [&str; 6] = ["blocked", "review", "working", "assigned", "open", "proposed"];

#[derive(Clone, PartialEq)]
enum View {
    Agent(String),
    Task(String),
    Inbox,
    Activity,
    Clis,
}

#[derive(Default)]
struct Compose {
    to: String,
    task: Option<String>,
    interrupt: bool,
    raw: bool,
    text: String,
}

/// The selected agent's transcript, re-parsed only when the file grows.
#[derive(Default)]
struct TranscriptCache {
    key: Option<(PathBuf, u64)>,
    entries: Vec<Entry>,
}

pub struct App {
    ws: Workspace,
    snap: Snapshot,
    read_at: Instant,
    view: View,
    embedded: Option<Embedded>,
    transcript: TranscriptCache,
    activity: Vec<String>,
    compose: Compose,
    answers: HashMap<String, String>,
    flash: Option<(String, bool, Instant)>,
    form: Option<AgentForm>,
    clis: CliEditor,
    /// For terminal agents: the desk log instead of the live terminal.
    show_log: bool,
    /// Set by "Workspaces"; the window goes back to the start screen.
    pub close_requested: bool,
}

impl App {
    pub fn new(ws: Workspace) -> Self {
        let embedded = Embedded::start(&ws);
        let snap = Snapshot::read(&ws);
        let view = snap.agents.first().map_or(View::Activity, |a| View::Agent(a.name.clone()));
        // Requests go to the orchestrator unless you pick someone else.
        let to = snap.agents.iter().find(|a| a.cfg.is_orchestrator()).or(snap.agents.first());
        let compose = Compose { to: to.map(|a| a.name.clone()).unwrap_or_default(), ..Compose::default() };
        App {
            ws,
            snap,
            read_at: Instant::now(),
            view,
            embedded,
            transcript: TranscriptCache::default(),
            activity: Vec::new(),
            compose,
            answers: HashMap::new(),
            flash: None,
            form: None,
            clis: CliEditor::default(),
            show_log: false,
            close_requested: false,
        }
    }

    fn refresh(&mut self, force: bool) {
        if !force && self.read_at.elapsed() < Duration::from_millis(800) {
            return;
        }
        self.read_at = Instant::now();
        if let Ok(ws) = Workspace::open(&self.ws.root) {
            self.ws = ws;
        }
        self.snap = Snapshot::read(&self.ws);
        if self.view == View::Activity {
            self.activity = activity_lines(&self.ws, self.embedded.as_ref());
        }
        if let View::Agent(name) = &self.view {
            self.load_transcript(&name.clone());
        }
    }

    fn load_transcript(&mut self, name: &str) {
        let Some(row) = self.snap.agent(name) else { return };
        let Ok(preset) = Preset::load(&self.ws, &row.cfg.cli) else { return };
        let Some(path) = transcript::latest(&self.ws, name) else {
            self.transcript = TranscriptCache::default();
            return;
        };
        let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let key = Some((path.clone(), len));
        if self.transcript.key != key {
            self.transcript = TranscriptCache { key, entries: transcript::entries(&preset, &path) };
        }
    }

    /// Shows the outcome of an action in the top bar for a few seconds.
    fn report<T: std::fmt::Display>(&mut self, r: anyhow::Result<T>) {
        self.flash = Some(match r {
            Ok(v) => (v.to_string(), false, Instant::now()),
            Err(e) => (format!("{e:#}"), true, Instant::now()),
        });
        self.refresh(true);
    }

    fn top_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.heading("desk");
            if ui
                .small_button("Workspaces")
                .on_hover_text("Back to the start screen; stops agents this window runs")
                .clicked()
            {
                self.close_requested = true;
            }
            ui.separator();
            if self.embedded.is_some() {
                ui.label(RichText::new("● supervising").color(GREEN));
            } else if self.snap.supervisor {
                ui.label(RichText::new("● supervisor running elsewhere").color(GREEN));
            } else {
                ui.label(RichText::new("○ no supervisor").color(RED));
                if ui.button("Start").clicked() {
                    self.embedded = Embedded::start(&self.ws);
                }
            }
            ui.separator();
            if self.snap.paused {
                ui.label(RichText::new("PAUSED").color(YELLOW).strong());
                if ui.button("Resume").clicked() {
                    let r = std::fs::remove_file(self.ws.paused_flag()).map(|_| "resumed");
                    self.report(r.map_err(Into::into));
                }
            } else {
                if ui.button("Pause").on_hover_text("Running agents finish their turn").clicked() {
                    let r = std::fs::write(self.ws.paused_flag(), "").map(|_| "paused");
                    self.report(r.map_err(Into::into));
                }
                if ui
                    .button("Stop all")
                    .on_hover_text("Pause and stop runs now; their mail stays unread")
                    .clicked()
                {
                    let r = std::fs::write(self.ws.paused_flag(), "now").map(|_| "stopping all runs");
                    self.report(r.map_err(Into::into));
                }
            }
            if let Some((msg, err, at)) = &self.flash
                && at.elapsed() < Duration::from_secs(6)
            {
                ui.separator();
                ui.label(RichText::new(msg).color(if *err { RED } else { ui.visuals().weak_text_color() }));
            }
        });
    }

    fn asks(&mut self, ui: &mut Ui) {
        ui.add_space(4.0);
        let asks: Vec<_> = self
            .snap
            .asks
            .iter()
            .map(|a| (a.id.clone(), a.blocking(), a.from(), a.task(), a.question().to_string()))
            .collect();
        egui::ScrollArea::vertical().id_salt("asks").max_height(220.0).show(ui, |ui| {
            for (id, blocking, from, task, question) in asks {
                ui.horizontal(|ui| {
                    if blocking {
                        ui.label(RichText::new("BLOCKING").color(RED).strong());
                    }
                    let task = task.map(|t| format!(" · task {t}")).unwrap_or_default();
                    ui.label(RichText::new(format!("ask {id} · from {from}{task}")).weak());
                });
                ui.label(question);
                ui.horizontal(|ui| {
                    let answer = self.answers.entry(id.clone()).or_default();
                    let field =
                        ui.add(egui::TextEdit::singleline(answer).hint_text("answer…").desired_width(500.0));
                    let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if (ui.button("Answer").clicked() || enter) && !answer.trim().is_empty() {
                        let text = std::mem::take(answer);
                        let r = ask::answer(&self.ws, &id, &text).map(|a| format!("answered ask {}", a.id));
                        self.report(r);
                    }
                });
                ui.separator();
            }
        });
    }

    fn agents(&mut self, ui: &mut Ui) {
        ui.add_space(4.0);
        let inbox = format!("Inbox ({})", self.snap.inbox.len());
        let inbox_text =
            if self.snap.inbox.is_empty() { RichText::new(inbox) } else { RichText::new(inbox).strong() };
        if ui.selectable_label(self.view == View::Inbox, inbox_text).clicked() {
            self.view = View::Inbox;
        }
        if ui.selectable_label(self.view == View::Activity, "Activity").clicked() {
            self.view = View::Activity;
            self.refresh(true);
        }
        if ui
            .selectable_label(self.view == View::Clis, "CLIs")
            .on_hover_text("Edit how each CLI is run")
            .clicked()
        {
            self.view = View::Clis;
        }
        ui.separator();
        ui.label(RichText::new("AGENTS").weak().small());
        let mut pick = None;
        for a in &self.snap.agents {
            let selected = self.view == View::Agent(a.name.clone());
            let r = ui.selectable_label(selected, agent_label(a));
            if r.clicked() {
                pick = Some(a.name.clone());
            }
        }
        if let Some(name) = pick {
            self.view = View::Agent(name.clone());
            self.compose.to = name;
            self.refresh(true);
        }
        if self.snap.agents.is_empty() {
            ui.label(RichText::new("No agents yet. Start with an orchestrator.").weak());
        }
        if ui.button("+ Add agent").clicked() {
            let has_orchestrator = self.snap.agents.iter().any(|a| a.cfg.is_orchestrator());
            self.form = Some(AgentForm::new_agent(&self.ws, has_orchestrator));
        }
        if !self.snap.locks.is_empty() {
            ui.separator();
            ui.label(RichText::new("LOCKS").weak().small());
            for l in &self.snap.locks {
                ui.label(format!("🔒 {} — {}", l.name, l.holder))
                    .on_hover_text(format!("since {} {}", l.since, l.note));
            }
        }
    }

    fn tasks(&mut self, ui: &mut Ui) {
        ui.add_space(4.0);
        egui::ScrollArea::vertical().id_salt("board").show(ui, |ui| {
            let mut pick = None;
            for status in BOARD {
                let tasks: Vec<&Task> = self.snap.tasks.iter().filter(|t| t.status() == status).collect();
                if tasks.is_empty() {
                    continue;
                }
                ui.label(
                    RichText::new(format!("{} ({})", status.to_uppercase(), tasks.len()))
                        .small()
                        .color(status_color(status)),
                );
                for t in tasks {
                    if task_row(ui, t, self.view == View::Task(t.id.clone())) {
                        pick = Some(t.id.clone());
                    }
                }
                ui.add_space(6.0);
            }
            let finished: Vec<&Task> = self
                .snap
                .tasks
                .iter()
                .filter(|t| matches!(t.status().as_str(), "done" | "cancelled"))
                .collect();
            if !finished.is_empty() {
                egui::CollapsingHeader::new(RichText::new(format!("DONE ({})", finished.len())).small())
                    .id_salt("done")
                    .show(ui, |ui| {
                        for t in finished.into_iter().rev() {
                            if task_row(ui, t, self.view == View::Task(t.id.clone())) {
                                pick = Some(t.id.clone());
                            }
                        }
                    });
            }
            if self.snap.tasks.is_empty() {
                ui.label(RichText::new("No tasks yet.").weak());
            }
            if let Some(id) = pick {
                self.view = View::Task(id.clone());
                self.compose.task = Some(id);
            }
        });
    }

    fn compose(&mut self, ui: &mut Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("To");
            egui::ComboBox::from_id_salt("to").selected_text(&self.compose.to).show_ui(ui, |ui| {
                for a in &self.snap.agents {
                    ui.selectable_value(&mut self.compose.to, a.name.clone(), &a.name);
                }
            });
            ui.label("Task");
            let shown = self.compose.task.clone().unwrap_or_else(|| "none".into());
            egui::ComboBox::from_id_salt("task").selected_text(shown).show_ui(ui, |ui| {
                ui.selectable_value(&mut self.compose.task, None, "none");
                for t in
                    self.snap.tasks.iter().filter(|t| !matches!(t.status().as_str(), "done" | "cancelled"))
                {
                    ui.selectable_value(
                        &mut self.compose.task,
                        Some(t.id.clone()),
                        format!("{} {}", t.id, t.title()),
                    );
                }
            });
            ui.checkbox(&mut self.compose.interrupt, "Interrupt")
                .on_hover_text("Stop their current run and deliver this first");
            ui.checkbox(&mut self.compose.raw, "Raw").on_hover_text(
                "Send the text alone and verbatim, as its own run: for CLI commands like /compact",
            );
        });
        ui.horizontal(|ui| {
            let field = ui.add(
                egui::TextEdit::multiline(&mut self.compose.text)
                    .hint_text("Message… (Ctrl+Enter to send)")
                    .desired_rows(2)
                    .desired_width(ui.available_width() - 70.0),
            );
            let hotkey =
                field.has_focus() && ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Enter));
            if (ui.button("Send").clicked() || hotkey) && !self.compose.text.trim().is_empty() {
                let kind = match (self.compose.raw, self.compose.interrupt) {
                    (true, _) => Kind::Raw,
                    (_, true) => Kind::Interrupt,
                    _ => Kind::Note,
                };
                let me = self.ws.config.human.clone();
                let r = message::send(
                    &self.ws,
                    &me,
                    &self.compose.to,
                    self.compose.task.as_deref(),
                    kind,
                    &self.compose.text,
                );
                if r.is_ok() {
                    self.compose.text.clear();
                    self.compose.interrupt = false;
                    self.compose.raw = false;
                }
                let to = self.compose.to.clone();
                self.report(r.map(|_| format!("sent to {to}")));
            }
        });
        ui.add_space(4.0);
    }

    fn agent_view(&mut self, ui: &mut Ui, name: &str) {
        let Some(a) = self.snap.agent(name) else {
            ui.label("This agent no longer exists.");
            return;
        };
        let (cfg, held, status) = (a.cfg.clone(), a.held, a.status().to_string());
        ui.horizontal(|ui| {
            ui.heading(name);
            ui.label(RichText::new(&status).color(status_color(&status)));
            ui.label(RichText::new(format!("{} · {}/{}", cfg.role, cfg.cli, cfg.model)).weak());
        });
        let detail = if cfg.terminal {
            format!("{} · runs in a terminal", cfg.dir.display())
        } else {
            format!(
                "{} · {}k tokens in context · session {}",
                cfg.dir.display(),
                a.state.context_tokens / 1000,
                a.state.session.as_deref().unwrap_or("none")
            )
        };
        ui.label(RichText::new(detail).weak().small());
        if status == "waiting" {
            ui.label(
                RichText::new(
                    "Its CLI is asking something, so desk isn't typing. Answer it in the terminal.",
                )
                .color(YELLOW),
            );
        }
        if let Some(e) = &a.state.last_error {
            ui.label(RichText::new(format!("last error: {e}")).color(RED));
        }
        ui.horizontal(|ui| self.agent_buttons(ui, name, &cfg, held, &status));
        if cfg.terminal {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.show_log, false, "Terminal");
                ui.selectable_value(&mut self.show_log, true, "Log")
                    .on_hover_text("What desk typed in, and the screen after each turn");
            });
        }
        ui.separator();
        if cfg.terminal && !self.show_log {
            let term = self.embedded.as_ref().and_then(|e| e.terminals.lock().unwrap().get(name).cloned());
            match term {
                Some(t) => crate::term_view::show(ui, &t),
                None if self.embedded.is_none() => {
                    ui.label(
                        RichText::new(
                            "Terminal agents run inside the window that supervises. A supervisor is running                              elsewhere (`desk run`), which can't host terminals: stop it, then press Start.",
                        )
                        .color(YELLOW),
                    );
                }
                None if !cfg.active => {
                    ui.label(RichText::new("Inactive. Reactivate to start its terminal.").weak());
                }
                None => {
                    ui.label(RichText::new("Starting its terminal…").weak());
                }
            }
            return;
        }

        let short = Shortener::new(&cfg.dir, &self.ws.root);
        egui::ScrollArea::vertical().id_salt("transcript").stick_to_bottom(true).auto_shrink(false).show(
            ui,
            |ui| {
                if self.transcript.entries.is_empty() {
                    ui.label(
                        RichText::new("No transcript yet. The agent runs when it gets a message.").weak(),
                    );
                }
                for (i, e) in self.transcript.entries.iter().enumerate() {
                    entry(ui, i, e, &short);
                }
            },
        );
    }

    fn agent_buttons(&mut self, ui: &mut Ui, name: &str, cfg: &AgentConfig, held: bool, status: &str) {
        if held {
            if ui.button("Hand back").on_hover_text("Resume delivery; held mail is delivered").clicked() {
                let r = agent::hand_back(&self.ws, name).map(|_| format!("{name} is back with desk"));
                self.report(r);
            }
        } else if cfg.terminal {
            if ui
                .button("Take over")
                .on_hover_text("desk stops typing into it; type in its terminal yourself")
                .clicked()
            {
                let r =
                    agent::take_over(&self.ws, name).map(|_| format!("{name}'s mail is held; it's yours"));
                self.report(r);
            }
        } else if ui
            .button("Take over")
            .on_hover_text("Hold its mail and open its session in a terminal")
            .clicked()
        {
            let r = agent::take_over(&self.ws, name).and_then(|cmd| open_terminal(ui.ctx(), &cfg.dir, &cmd));
            self.report(r);
        }
        if !cfg.terminal
            && ui
                .button("Terminal")
                .on_hover_text("A terminal in its project with desk's setup, showing the command desk runs")
                .clicked()
        {
            let r = self.agent_terminal(ui.ctx(), name, cfg);
            self.report(r);
        }
        if ui.button("Reset").on_hover_text("Write a handoff, then start a fresh session").clicked() {
            let r = agent::request_reset(&self.ws, name).map(|_| format!("{name} will hand off and reset"));
            self.report(r);
        }
        if status == "error" && ui.button("Retry").clicked() {
            let r = agent::request_retry(&self.ws, name).map(|_| format!("{name} will retry"));
            self.report(r);
        }
        if cfg.active {
            if ui.button("Kill").on_hover_text("Stop it and release its locks; files are kept").clicked() {
                let r = agent::kill(&self.ws, name).map(|_| format!("{name} deactivated"));
                self.report(r);
            }
        } else if ui.button("Reactivate").clicked() {
            let mut c = cfg.clone();
            c.active = true;
            let r = agent::spawn(&self.ws, name, c).map(|_| format!("{name} is back"));
            self.report(r);
        }
        if ui.button("Settings").on_hover_text("CLI, model, project, role").clicked() {
            self.form = Some(AgentForm::edit(&self.ws, name, cfg));
        }
        if ui.button("Memory").clicked() {
            let r = open_path(&self.ws.agent_dir(name).join("memory.md"));
            self.report(r);
        }
        if ui.button("Folder").clicked() {
            let r = open_path(&cfg.dir);
            self.report(r);
        }
    }

    /// Writes `scratch/terminal.cmd` (desk's environment plus the agent's
    /// next command) and opens a terminal that runs it.
    fn agent_terminal(&self, ctx: &egui::Context, name: &str, cfg: &AgentConfig) -> anyhow::Result<String> {
        let line = supervisor::command_line(&self.ws, name)?;
        if !cfg!(windows) {
            ctx.copy_text(format!("cd {:?} && {}", cfg.dir, line.display()));
            return Ok("the command is on your clipboard".into());
        }
        let script = self.ws.agent_dir(name).join("scratch/terminal.cmd");
        fsutil::write_atomic(&script, &line.script(&self.ws, name)?)?;
        open_terminal(ctx, &cfg.dir, &[script.display().to_string()])
            .map(|_| format!("opened a terminal for {name}"))
    }

    fn task_view(&mut self, ui: &mut Ui, id: &str) {
        let Some(t) = self.snap.tasks.iter().find(|t| t.id == id).cloned() else {
            ui.label("This task no longer exists.");
            return;
        };
        ui.horizontal(|ui| {
            ui.heading(format!("{} {}", t.id, t.title()));
        });
        ui.horizontal(|ui| {
            let mut status = t.status();
            egui::ComboBox::from_id_salt("status").selected_text(&status).show_ui(ui, |ui| {
                for s in task::STATUSES {
                    ui.selectable_value(&mut status, s.to_string(), s);
                }
            });
            if status != t.status() {
                let r = task::set(&self.ws, id, "status", &status).map(|_| format!("task {id} → {status}"));
                self.report(r);
            }
            let mut owner = t.owner().unwrap_or_default();
            egui::ComboBox::from_id_salt("owner")
                .selected_text(if owner.is_empty() { "no owner" } else { &owner })
                .show_ui(ui, |ui| {
                    for a in &self.snap.agents {
                        ui.selectable_value(&mut owner, a.name.clone(), &a.name);
                    }
                });
            if owner != t.owner().unwrap_or_default() {
                let r =
                    task::set(&self.ws, id, "owner", &owner).map(|_| format!("task {id} owner → {owner}"));
                self.report(r);
            }
            for key in ["group", "tier", "budget", "files", "depends_on"] {
                if let Some(v) = t.doc.get(key).filter(|v| v != "[]") {
                    ui.label(RichText::new(format!("{key}: {v}")).weak());
                }
            }
            if ui.button("Edit").on_hover_text("Open task.md in your editor").clicked() {
                let r = open_path(&t.path());
                self.report(r);
            }
        });
        let missing = t.missing_sections();
        if !missing.is_empty() {
            ui.label(RichText::new(format!("⚠ missing {}", missing.join(", "))).color(YELLOW));
        }
        ui.separator();
        egui::ScrollArea::vertical().id_salt("task").auto_shrink(false).show(ui, |ui| {
            markdown(ui, &t.doc.body);
            for report in fsutil::files_with_ext(&t.dir.join("reports"), "md") {
                let who = report.file_stem().unwrap_or_default().to_string_lossy().into_owned();
                egui::CollapsingHeader::new(format!("Report: {who}"))
                    .id_salt(&report)
                    .default_open(true)
                    .show(ui, |ui| {
                        markdown(ui, &std::fs::read_to_string(&report).unwrap_or_default());
                    });
            }
        });
    }

    fn inbox_view(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.heading("Inbox");
            if !self.snap.inbox.is_empty() && ui.button("Mark all read").clicked() {
                let paths: Vec<PathBuf> = self.snap.inbox.iter().map(|m| m.path.clone()).collect();
                let human = self.ws.config.human.clone();
                let r = message::mark_read(&self.ws, &human, &paths).map(|_| "inbox cleared");
                self.report(r);
            }
        });
        ui.separator();
        if self.snap.inbox.is_empty() {
            ui.label(RichText::new("Nothing unread. Read messages are in agents/<you>/inbox/read/.").weak());
        }
        let mut reply = None;
        egui::ScrollArea::vertical().id_salt("inbox").auto_shrink(false).show(ui, |ui| {
            for m in &self.snap.inbox {
                ui.horizontal(|ui| {
                    let task = m.task.as_ref().map(|t| format!(" · task {t}")).unwrap_or_default();
                    ui.label(RichText::new(format!("from {}{task}", m.from)).strong());
                    if m.from != "desk" && ui.small_button("Reply").clicked() {
                        reply = Some((m.from.clone(), m.task.clone()));
                    }
                });
                ui.label(m.body.trim_end());
                ui.separator();
            }
        });
        if let Some((to, task)) = reply {
            self.compose.to = to;
            self.compose.task = task;
        }
    }

    fn activity_view(&mut self, ui: &mut Ui) {
        ui.heading("Activity");
        ui.label(RichText::new("From log/events.jsonl, newest last.").weak().small());
        ui.separator();
        egui::ScrollArea::vertical().id_salt("activity").stick_to_bottom(true).auto_shrink(false).show(
            ui,
            |ui| {
                for line in &self.activity {
                    ui.label(RichText::new(line).monospace().small());
                }
            },
        );
    }
}

impl App {
    pub fn ui(&mut self, ui: &mut Ui) {
        self.refresh(false);
        ui.ctx().request_repaint_after(Duration::from_millis(500));

        egui::Panel::top("top").show(ui, |ui| self.top_bar(ui));
        if !self.snap.asks.is_empty() {
            egui::Panel::top("asks").show(ui, |ui| self.asks(ui));
        }
        egui::Panel::bottom("compose").show(ui, |ui| self.compose(ui));
        egui::Panel::left("agents").resizable(true).default_size(240.0).show(ui, |ui| self.agents(ui));
        egui::Panel::right("tasks").resizable(true).default_size(300.0).show(ui, |ui| self.tasks(ui));
        egui::CentralPanel::default_margins().show(ui, |ui| match self.view.clone() {
            View::Agent(name) => self.agent_view(ui, &name),
            View::Task(id) => self.task_view(ui, &id),
            View::Inbox => self.inbox_view(ui),
            View::Activity => self.activity_view(ui),
            View::Clis => {
                if let Some(msg) = self.clis.ui(ui, &self.ws) {
                    self.report(Ok(msg));
                }
            }
        });
        if let Some(form) = &mut self.form {
            let mut open = true;
            let saved = form.show(ui.ctx(), &self.ws, &mut open);
            if !open {
                self.form = None;
            }
            if let Some(msg) = saved {
                self.report(Ok(msg));
            }
        }
    }
}

const GREEN: Color32 = Color32::from_rgb(110, 190, 120);
pub const RED: Color32 = Color32::from_rgb(225, 95, 90);
pub const YELLOW: Color32 = Color32::from_rgb(220, 180, 80);
const BLUE: Color32 = Color32::from_rgb(120, 165, 230);

fn status_color(status: &str) -> Color32 {
    match status {
        "running" | "working" => GREEN,
        "error" | "blocked" => RED,
        "held" | "review" | "waiting" => YELLOW,
        "assigned" | "open" => BLUE,
        _ => Color32::GRAY,
    }
}

fn agent_label(a: &AgentRow) -> RichText {
    let status = a.status();
    let unread = if a.unread > 0 { format!(" · {} unread", a.unread) } else { String::new() };
    RichText::new(format!("● {}  {status}{unread}\n    {}", a.name, a.cfg.model)).color(status_color(status))
}

fn task_row(ui: &mut Ui, t: &Task, selected: bool) -> bool {
    let owner = t.owner().unwrap_or_else(|| "unowned".into());
    let group = t.group().map(|g| format!(" · {g}")).unwrap_or_default();
    let text = format!("{} {}\n   {owner}{group}", t.id, t.title());
    ui.selectable_label(selected, text).clicked()
}

/// Shows paths relative to the project (`./`) or the workspace (`ws/`):
/// the absolute prefixes are the same on every line and bury what matters.
struct Shortener(Vec<(String, &'static str)>);

impl Shortener {
    fn new(project: &Path, workspace: &Path) -> Self {
        let mut v = Vec::new();
        for (root, label) in [(project, "./"), (workspace, "ws/")] {
            let s = root.display().to_string();
            v.push((format!("{s}\\"), label));
            v.push((format!("{}/", s.replace('\\', "/")), label));
        }
        v.sort_by_key(|(s, _)| std::cmp::Reverse(s.len())); // nested roots: longest first
        Shortener(v)
    }

    fn apply(&self, s: &str) -> String {
        self.0.iter().fold(s.to_string(), |acc, (root, label)| acc.replace(root.as_str(), label))
    }
}

fn entry(ui: &mut Ui, i: usize, e: &Entry, short: &Shortener) {
    match e {
        Entry::Prompt { ts, text } => {
            ui.add_space(6.0);
            let (text, first) = match text.find("[desk]") {
                Some(i) => {
                    let first = text[i..]
                        .lines()
                        .find(|l| l.starts_with("--- message"))
                        .map_or("prompt".to_string(), |l| l.trim_matches(|c| c == '-' || c == ' ').into());
                    (&text[i..], first)
                }
                // No desk framing: a raw message, delivered verbatim.
                None => (text.as_str(), format!("raw · {}", adapter::clip(text, 60))),
            };
            egui::CollapsingHeader::new(
                RichText::new(format!("→ {} · {first}", ts.get(11..19).unwrap_or(ts))).color(BLUE),
            )
            .id_salt(("prompt", i))
            .show(ui, |ui| {
                ui.label(RichText::new(text).monospace().small());
            });
        }
        Entry::Event(Event::Text(t)) if !t.trim().is_empty() => {
            ui.label(t.trim());
        }
        Entry::Event(Event::Tool { name, detail }) => {
            ui.label(
                RichText::new(format!("⚙ {name}  {}", short.apply(detail))).monospace().small().color(BLUE),
            );
        }
        Entry::Event(Event::ToolResult { ok, detail }) if !detail.is_empty() => {
            let r =
                RichText::new(format!("   {}", adapter::clip(&short.apply(detail), 140))).monospace().small();
            ui.label(if *ok { r.weak() } else { r.color(RED) });
        }
        Entry::Event(Event::Error(msg)) => {
            ui.label(RichText::new(format!("✗ {msg}")).color(RED));
        }
        _ => {}
    }
}

/// Just enough markdown for task files: `## ` headings and plain lines.
fn markdown(ui: &mut Ui, text: &str) {
    for line in text.lines() {
        if let Some(h) = line.strip_prefix("## ") {
            ui.add_space(6.0);
            ui.label(RichText::new(h).strong());
        } else if let Some(h) = line.strip_prefix("# ") {
            ui.label(RichText::new(h).heading());
        } else if !line.trim().is_empty() {
            ui.label(line);
        }
    }
}

fn activity_lines(ws: &Workspace, embedded: Option<&Embedded>) -> Vec<String> {
    let events = ws.events();
    let mut lines: Vec<String> = events[events.len().saturating_sub(300)..]
        .iter()
        .map(|e| {
            let ts = e["ts"].as_str().and_then(|t| t.get(11..19)).unwrap_or("");
            let mut s = format!("{ts} {:<12}", e["event"].as_str().unwrap_or("?"));
            if let Some(o) = e.as_object() {
                for (k, v) in o {
                    if !matches!(k.as_str(), "ts" | "event" | "file" | "messages") && !v.is_null() {
                        s.push_str(&format!(" {k}={}", v.as_str().map_or(v.to_string(), str::to_string)));
                    }
                }
            }
            s
        })
        .collect();
    if let Some(e) = embedded {
        let notes = e.notes.lock().unwrap();
        if !notes.is_empty() {
            lines.push(String::new());
            lines.push("── supervisor notes ──".into());
            lines.extend(notes.iter().cloned());
        }
    }
    lines
}

/// Opens the session in a real terminal. Elsewhere than Windows, copies the
/// command instead, since there's no one terminal to rely on.
fn open_terminal(ctx: &egui::Context, dir: &Path, cmd: &[String]) -> anyhow::Result<String> {
    if cfg!(windows) {
        // `cmd /K` so npm .cmd shims resolve, and the window stays after exit.
        let wt = std::process::Command::new("wt").arg("-d").arg(dir).arg("cmd").arg("/K").args(cmd).spawn();
        if wt.is_err() {
            std::process::Command::new("cmd")
                .args(["/C", "start", "desk"])
                .arg("/D")
                .arg(dir)
                .args(["cmd", "/K"])
                .args(cmd)
                .spawn()?;
        }
        Ok("opened a terminal; `desk hand-back` or the button when you're done".to_string())
    } else {
        let line = format!("cd {:?} && {}", dir, cmd.join(" "));
        ctx.copy_text(line);
        Ok("mail held; the command is on your clipboard".into())
    }
}

fn open_path(path: &Path) -> anyhow::Result<String> {
    let mut c = if cfg!(windows) {
        let mut c = std::process::Command::new("cmd");
        crate::quiet(c.args(["/C", "start", ""]).arg(path));
        c
    } else if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("open");
        c.arg(path);
        c
    } else {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(path);
        c
    };
    c.spawn()?;
    Ok(format!("opened {}", path.display()))
}
