//! The supervisor owns every agent process. Each tick it looks at the
//! workspace and decides: deliver mail, interrupt, reset, stop, retry.
//!
//! It polls instead of watching files: a workspace is a few dozen small
//! files, polling works the same on every drive, and missed events can't
//! wedge it. One supervisor per workspace, enforced by a heartbeat file.

use crate::adapter::{Event, Preset, RunSpec};
use crate::agent::{self, AgentConfig, AgentState};
use crate::message::{self, Kind, Message};
use crate::process::{adopt, hide_window, kill_tree};
use crate::task::Task;
use crate::workspace::Workspace;
use crate::{fsutil, lock, prompt};
use anyhow::{Context, Result, bail};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant, SystemTime};

const MAX_FAILURES: u32 = 3;
const HEARTBEAT_STALE: Duration = Duration::from_secs(5);

pub struct Supervisor {
    pub(crate) ws: Workspace,
    pub(crate) runs: HashMap<String, Run>,
    tx: Sender<Update>,
    rx: Receiver<Update>,
    pub(crate) retry_at: HashMap<String, Instant>,
    warned: HashSet<String>,
    /// The last error each agent's step hit, so it's reported once.
    errors: HashMap<String, String>,
    #[cfg(feature = "terminal")]
    pub(crate) terms: HashMap<String, crate::term_run::TermAgent>,
    #[cfg(feature = "terminal")]
    pub(crate) registry: crate::term_run::Registry,
}

pub(crate) struct Run {
    pid: u32,
    resuming: bool,
    delivered: Vec<PathBuf>,
    handoff: bool,
    /// Set when desk stops the run on purpose; the mail stays unread.
    stopping: Option<&'static str>,
    error: Option<String>,
    /// The run's latest text, and whether it ran `desk ask` or `desk send`:
    /// a question left in the transcript is one the human never sees.
    last_text: String,
    reached_out: bool,
    /// Its prompt said to compact; still full at the end, it gets reset.
    asked_to_compact: bool,
}

enum Update {
    Event(String, Event),
    Exited(String, bool),
}

impl Supervisor {
    pub fn start(ws: Workspace) -> Result<Self> {
        let beat = heartbeat_path(&ws);
        if is_alive(&ws) {
            bail!("a supervisor is already running for this workspace ({})", beat.display());
        }
        // Anything marked running is left over from a supervisor that died.
        for name in agent::names(&ws) {
            let mut st = AgentState::load(&ws, &name);
            if st.state == "running" {
                st.state = "idle".into();
                st.save(&ws, &name)?;
            }
        }
        let (tx, rx) = channel();
        let s = Supervisor {
            ws,
            runs: HashMap::new(),
            tx,
            rx,
            retry_at: HashMap::new(),
            warned: HashSet::new(),
            errors: HashMap::new(),
            #[cfg(feature = "terminal")]
            terms: HashMap::new(),
            #[cfg(feature = "terminal")]
            registry: Default::default(),
        };
        s.beat()?;
        s.ws.log("supervisor_start", serde_json::json!({ "pid": std::process::id() }));
        Ok(s)
    }

    pub fn workspace(&self) -> &Workspace {
        &self.ws
    }

    pub fn running(&self) -> Vec<String> {
        self.runs.keys().cloned().collect()
    }

    /// The terminal agents' live terminals, for the GUI to draw.
    #[cfg(feature = "terminal")]
    pub fn terminals(&self) -> crate::term_run::Registry {
        self.registry.clone()
    }

    /// One pass. Returns human-readable notes on what changed.
    pub fn tick(&mut self) -> Vec<String> {
        let mut notes = Vec::new();
        let _ = self.beat();
        if let Ok(ws) = Workspace::open(&self.ws.root) {
            self.ws = ws; // pick up desk.toml edits
        }
        while let Ok(u) = self.rx.try_recv() {
            match u {
                Update::Event(name, e) => self.on_event(&name, e, &mut notes),
                Update::Exited(name, ok) => self.on_exit(&name, ok, &mut notes),
            }
        }

        let paused = self.ws.is_paused();
        if paused && std::fs::read_to_string(self.ws.paused_flag()).unwrap_or_default().trim() == "now" {
            for name in self.running() {
                self.stop(&name, "paused", &mut notes);
            }
        }

        for name in agent::names(&self.ws) {
            // A broken agent.toml fails every tick; say so once, not twice a second.
            let result = if self.ws.agent_dir(&name).join("REMOVE").exists() {
                self.remove(&name, &mut notes)
            } else {
                self.step(&name, paused, &mut notes)
            };
            match result {
                Ok(()) => {
                    self.errors.remove(&name);
                }
                Err(e) => {
                    let e = format!("{e:#}");
                    if self.errors.get(&name) != Some(&e) {
                        notes.push(format!("{name}: {e}"));
                        self.ws.log("agent_error", serde_json::json!({ "agent": name, "error": e }));
                        self.errors.insert(name, e);
                    }
                }
            }
        }
        notes
    }

    /// Stops every run. Unread mail stays unread and is delivered next time.
    pub fn shutdown(&mut self) {
        let mut notes = Vec::new();
        for name in self.running() {
            self.stop(&name, "supervisor stopped", &mut notes);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.runs.is_empty() && Instant::now() < deadline {
            if let Ok(Update::Exited(name, ok)) = self.rx.recv_timeout(Duration::from_millis(100)) {
                self.on_exit(&name, ok, &mut notes);
            }
        }
        #[cfg(feature = "terminal")]
        for name in self.terms.keys().cloned().collect::<Vec<_>>() {
            self.close_terminal(&name);
        }
        let _ = std::fs::remove_file(heartbeat_path(&self.ws));
        self.ws.log("supervisor_stop", serde_json::json!({}));
    }

    fn step(&mut self, name: &str, paused: bool, notes: &mut Vec<String>) -> Result<()> {
        let cfg = AgentConfig::load(&self.ws, name)?;
        let dir = self.ws.agent_dir(name);
        let mut st = AgentState::load(&self.ws, name);

        // Switched between headless and terminal: end the old kind first, so
        // one agent never has two CLIs reading the same mail.
        if cfg.terminal && self.runs.contains_key(name) {
            self.stop(name, "switched to a terminal", notes);
            return Ok(());
        }
        #[cfg(feature = "terminal")]
        if !cfg.terminal && self.terms.contains_key(name) {
            self.close_terminal(name);
            notes.push(format!("{name}: terminal closed (switched to headless)"));
        }
        if cfg.terminal {
            #[cfg(feature = "terminal")]
            return self.step_terminal(name, &cfg, &mut st, paused, notes);
            #[cfg(not(feature = "terminal"))]
            {
                if self.warned.insert(format!("terminal:{name}")) {
                    notes.push(format!("{name} runs in a terminal; open desk-gui to run it"));
                }
                return Ok(());
            }
        }

        if !cfg.active {
            if self.runs.contains_key(name) {
                self.stop(name, "killed", notes);
            } else if st.state != "inactive" {
                lock::release_all(&self.ws, name);
                st.state = "inactive".into();
                st.save(&self.ws, name)?;
            }
            return Ok(());
        }

        if let Some(run) = self.runs.get(name) {
            // An interrupt that arrived mid-run stops it; the next run leads with it.
            let interrupted = message::unread(&self.ws, name)
                .iter()
                .any(|m| m.kind == Kind::Interrupt && !run.delivered.contains(&m.path));
            if agent::is_held(&self.ws, name) {
                self.stop(name, "taken over", notes); // two writers on one session would corrupt it
            } else if interrupted && run.stopping.is_none() && !run.handoff {
                self.stop(name, "interrupted", notes);
            }
            return Ok(());
        }

        if dir.join("RETRY").exists() {
            let _ = std::fs::remove_file(dir.join("RETRY"));
            self.retry_at.remove(name);
            st.failures = 0;
            st.state = "idle".into();
            st.save(&self.ws, name)?;
            notes.push(format!("{name}: retrying"));
        }
        // The agent wrote its handoff and asked for a fresh session.
        if dir.join("COMPACT").exists() {
            let _ = std::fs::remove_file(dir.join("COMPACT"));
            st.session = None;
            st.context_tokens = 0;
            st.save(&self.ws, name)?;
            self.warned.remove(name);
            notes.push(format!("{name}: compacted; its next turn starts fresh"));
        }
        if st.state == "inactive" {
            st.state = "idle".into(); // respawned
            st.save(&self.ws, name)?;
        }
        if paused || st.state == "error" || agent::is_held(&self.ws, name) {
            return Ok(());
        }
        if self.backing_off(name) {
            return Ok(());
        }
        let cap = self.ws.config.max_running;
        if cap > 0 && self.runs.len() >= cap {
            return Ok(());
        }

        let (text, delivered, handoff, note) = if dir.join("RESET").exists() {
            if st.session.is_none() {
                let _ = std::fs::remove_file(dir.join("RESET"));
                lock::release_all(&self.ws, name);
                notes.push(format!("{name}: reset (no session to hand off)"));
                return Ok(());
            }
            (
                prompt::handoff(&self.ws, name),
                Vec::new(),
                true,
                format!("{name}: writing handoff before reset"),
            )
        } else {
            let unread = message::unread(&self.ws, name);
            if unread.is_empty() {
                return Ok(());
            }
            let (batch, task) = pick_batch(&cfg, unread);
            let task = task.and_then(|id| Task::find(&self.ws, &id).ok());
            let preset = Preset::load(&self.ws, &cfg.cli)?;
            let role = if preset.system_prompt() {
                None
            } else {
                std::fs::read_to_string(cfg.role_file(&self.ws)).ok()
            };
            let text = if batch[0].kind == Kind::Raw {
                batch[0].body.trim_end().to_string()
            } else {
                prompt::delivery(
                    &self.ws,
                    &prompt::Delivery {
                        name,
                        dir: &cfg.dir,
                        new_session: st.session.is_none(),
                        full: (st.context_tokens >= self.ws.config.context_warn).then_some(st.context_tokens),
                        role: role.as_deref(),
                        task: task.as_ref(),
                        messages: &batch,
                    },
                )
            };
            let delivered: Vec<PathBuf> = batch.iter().map(|m| m.path.clone()).collect();
            let n = delivered.len();
            let about = task.map(|t| format!(" about task {}", t.id)).unwrap_or_default();
            let note = format!("{name}: started with {n} message{}{about}", if n == 1 { "" } else { "s" });
            (text, delivered, false, note)
        };
        // A CLI that won't start (not installed, bad folder) backs off like a failed run.
        // A raw message carries no desk text, so it doesn't count as asking.
        let asked = !handoff && st.context_tokens >= self.ws.config.context_warn && !text.starts_with('/');
        if let Err(e) = self.launch(name, &cfg, &mut st, text, delivered, handoff) {
            self.fail(name, &mut st, format!("couldn't start the CLI: {e:#}"), notes);
            st.save(&self.ws, name)?;
            return Ok(());
        }
        if let Some(run) = self.runs.get_mut(name) {
            run.asked_to_compact = asked;
        }
        notes.push(note);
        Ok(())
    }

    fn launch(
        &mut self,
        name: &str,
        cfg: &AgentConfig,
        st: &mut AgentState,
        prompt: String,
        delivered: Vec<PathBuf>,
        handoff: bool,
    ) -> Result<()> {
        let preset = Preset::load(&self.ws, &cfg.cli)?;
        let resuming = st.session.is_some();
        if preset.session == "named" && st.session.is_none() {
            st.session = Some(format!("desk-{name}-{}", chrono::Local::now().format("%Y%m%d-%H%M%S")));
        }
        // Always saved, so the run can be replayed by hand (see `command_line`).
        fsutil::write_atomic(&prompt_file(&self.ws, name), &prompt)?;
        let mut cmd = agent_command(&self.ws, name, cfg, &preset, st.session.as_deref(), resuming)?;

        let tdir = self.ws.transcripts_dir(name);
        std::fs::create_dir_all(&tdir)?;
        let stderr = std::fs::OpenOptions::new().create(true).append(true).open(tdir.join("stderr.log"))?;
        cmd.stdin(if preset.uses_prompt_file() { Stdio::null() } else { Stdio::piped() })
            .stdout(Stdio::piped())
            .stderr(stderr);
        hide_window(&mut cmd);
        let mut child = cmd.spawn().with_context(|| format!("starting `{}`", cfg.cli))?;
        let pid = child.id();
        adopt(pid);

        if let Some(mut stdin) = child.stdin.take() {
            let input = prompt.clone();
            std::thread::spawn(move || {
                let _ = stdin.write_all(input.as_bytes());
            });
        }

        let stdout = child.stdout.take().context("no stdout")?;
        let tx = self.tx.clone();
        let agent = name.to_string();
        let session = st.session.clone();
        std::thread::spawn(move || {
            let mut log = TranscriptWriter::new(tdir, session);
            log.write(
                &serde_json::json!({ "desk": "prompt", "ts": fsutil::now_rfc3339(), "text": prompt })
                    .to_string(),
            );
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                for e in preset.parse_line(&line) {
                    if let Event::Session(s) = &e {
                        log.set_session(s);
                    }
                    let _ = tx.send(Update::Event(agent.clone(), e));
                }
                log.write(&line);
            }
            log.finish();
            let ok = child.wait().is_ok_and(|s| s.success());
            let _ = tx.send(Update::Exited(agent, ok));
        });

        st.state = "running".into();
        st.save(&self.ws, name)?;
        self.ws.log(
            "run_start",
            serde_json::json!({ "agent": name, "pid": pid, "messages": delivered, "handoff": handoff }),
        );
        self.runs.insert(
            name.to_string(),
            Run {
                pid,
                resuming,
                delivered,
                handoff,
                stopping: None,
                error: None,
                last_text: String::new(),
                reached_out: false,
                asked_to_compact: false,
            },
        );
        Ok(())
    }

    fn on_event(&mut self, name: &str, e: Event, notes: &mut Vec<String>) {
        let mut st = AgentState::load(&self.ws, name);
        match e {
            Event::Session(s) if st.session.as_deref() != Some(s.as_str()) => {
                st.session = Some(s);
                let _ = st.save(&self.ws, name);
            }
            Event::Usage { context } => {
                st.context_tokens = context;
                let _ = st.save(&self.ws, name);
                let limit = self.ws.config.context_warn;
                if context < limit {
                    self.warned.remove(name);
                } else if self.warned.insert(name.to_string()) {
                    notes.push(format!("{name}: context at {context} tokens; it's asked to compact"));
                }
            }
            Event::Error(msg) => {
                if let Some(run) = self.runs.get_mut(name) {
                    run.error = Some(msg);
                }
            }
            Event::Text(t) if !t.trim().is_empty() => {
                if let Some(run) = self.runs.get_mut(name) {
                    run.last_text = t;
                }
            }
            Event::Tool { detail, .. } if detail.contains("desk ask") || detail.contains("desk send") => {
                if let Some(run) = self.runs.get_mut(name) {
                    run.reached_out = true;
                }
            }
            _ => {}
        }
    }

    fn on_exit(&mut self, name: &str, ok: bool, notes: &mut Vec<String>) {
        let Some(run) = self.runs.remove(name) else { return };
        let mut st = AgentState::load(&self.ws, name);
        st.state = "idle".into();
        let dir = self.ws.agent_dir(name);

        // A run that finished just as desk stopped it still finished: keep its mail read.
        if let Some(reason) = run.stopping.filter(|_| !(ok && run.error.is_none())) {
            notes.push(format!("{name}: stopped ({reason})"));
        } else if ok && run.error.is_none() {
            let _ = message::mark_read(&self.ws, name, &run.delivered);
            st.failures = 0;
            st.last_error = None;
            self.retry_at.remove(name);
            if run.handoff {
                st.session = None;
                st.context_tokens = 0;
                let _ = std::fs::remove_file(dir.join("RESET"));
                lock::release_all(&self.ws, name);
                notes.push(format!("{name}: handoff written, session reset"));
            } else {
                notes.push(format!("{name}: done"));
                // Told to compact and didn't: desk resets it, handoff first.
                let hard = self.ws.config.context_warn.saturating_mul(2);
                if run.asked_to_compact
                    && hard > 0
                    && st.context_tokens >= hard
                    && !dir.join("COMPACT").exists()
                {
                    let _ = fsutil::write_atomic(&dir.join("RESET"), "");
                    notes.push(format!("{name}: context at {} tokens; resetting it", st.context_tokens));
                }
                if !run.reached_out && ends_in_question(&run.last_text) {
                    let q = format!(
                        "{}\n\n*desk raised this: {name} ended its turn with this question in its transcript, \
                         which you don't see, instead of using `desk ask`.*",
                        run.last_text.trim()
                    );
                    if crate::ask::new(&self.ws, name, None, false, &q).is_ok() {
                        notes.push(format!("{name}: its question is now an ask"));
                    }
                }
            }
        } else {
            // A session that never got going (a named one, say) isn't worth resuming.
            if !run.resuming {
                st.session = None;
            }
            let err = run.error.unwrap_or_else(|| "the CLI exited with an error (see stderr.log)".into());
            self.fail(name, &mut st, err, notes);
        }
        let _ = st.save(&self.ws, name);
        self.ws.log(
            "run_end",
            serde_json::json!({ "agent": name, "ok": ok, "stopped": run.stopping, "error": st.last_error }),
        );
    }

    /// Stops the agent, then deletes it once nothing of it is running.
    fn remove(&mut self, name: &str, notes: &mut Vec<String>) -> Result<()> {
        #[cfg(feature = "terminal")]
        self.close_terminal(name);
        if self.runs.contains_key(name) {
            self.stop(name, "removed", notes);
            return Ok(());
        }
        self.retry_at.remove(name);
        agent::purge(&self.ws, name)?;
        notes.push(format!("{name}: removed"));
        Ok(())
    }

    /// Counts a failure: retry after 30s, then 60s, then stop and tell the human.
    pub(crate) fn fail(&mut self, name: &str, st: &mut AgentState, err: String, notes: &mut Vec<String>) {
        st.failures += 1;
        st.last_error = Some(err.clone());
        if st.failures >= MAX_FAILURES {
            st.state = "error".into();
            self.retry_at.remove(name);
            lock::release_all(&self.ws, name);
            notes.push(format!("{name}: stopped after {} failures: {err}", st.failures));
            let body = format!(
                "{name} failed {} times in a row and has stopped.\n\nLast error: {err}\n\n\
                 Its mail is still unread. Fix the cause, then `desk resume {name}`.",
                st.failures
            );
            let _ = message::send(&self.ws, "desk", &self.ws.config.human, None, Kind::Note, &body);
        } else {
            st.state = "idle".into();
            let wait = Duration::from_secs(30 * 2u64.pow(st.failures - 1));
            self.retry_at.insert(name.to_string(), Instant::now() + wait);
            notes.push(format!("{name}: failed ({err}); retrying in {}s", wait.as_secs()));
        }
    }

    /// Whether `name` is waiting out a failure.
    pub(crate) fn backing_off(&self, name: &str) -> bool {
        self.retry_at.get(name).is_some_and(|t| Instant::now() < *t)
    }

    fn stop(&mut self, name: &str, reason: &'static str, notes: &mut Vec<String>) {
        if let Some(run) = self.runs.get_mut(name)
            && run.stopping.is_none()
        {
            run.stopping = Some(reason);
            kill_tree(run.pid);
            notes.push(format!("{name}: stopping ({reason})"));
        }
    }

    fn beat(&self) -> Result<()> {
        fsutil::write_atomic(&heartbeat_path(&self.ws), &std::process::id().to_string())
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        for run in self.runs.values() {
            kill_tree(run.pid);
        }
    }
}

/// Whether a turn's last text ends on a question: its last paragraph asks something.
fn ends_in_question(text: &str) -> bool {
    text.trim().rsplit("\n\n").next().is_some_and(|p| p.contains('?'))
}

/// What an agent gets in one run. Interrupts go first. An orchestrator gets
/// one task's mail per turn, so it never juggles unrelated work.
pub(crate) fn pick_batch(cfg: &AgentConfig, mut unread: Vec<Message>) -> (Vec<Message>, Option<String>) {
    unread.sort_by_key(|m| m.kind != Kind::Interrupt); // stable: oldest first otherwise
    // A raw message is a run of its own, in order: the CLI gets its text verbatim.
    match unread.iter().position(|m| m.kind == Kind::Raw) {
        Some(0) => return (vec![unread.remove(0)], None),
        Some(i) => unread.truncate(i),
        None => {}
    }
    if !cfg.is_orchestrator() {
        return (unread, None);
    }
    // Interrupts always come along, whatever their task: an interrupt left out
    // of the batch would stop the run it was left out of, again and again.
    let task = unread[0].task.clone();
    let batch = unread.into_iter().filter(|m| m.kind == Kind::Interrupt || m.task == task).collect();
    (batch, task)
}

fn prompt_file(ws: &Workspace, name: &str) -> PathBuf {
    ws.agent_dir(name).join("scratch").join("prompt.md")
}

/// The command for one run of `name`, with desk's environment.
fn agent_command(
    ws: &Workspace,
    name: &str,
    cfg: &AgentConfig,
    preset: &Preset,
    session: Option<&str>,
    resuming: bool,
) -> Result<std::process::Command> {
    let role_file = cfg.role_file(ws);
    let prompt_file = prompt_file(ws, name);
    let projects = ws.other_projects(&cfg.dir);
    let spec = RunSpec {
        cfg,
        session,
        resuming,
        role_file: &role_file,
        prompt_file: &prompt_file,
        workspace: &ws.root,
        projects: &projects,
    };
    let mut cmd = preset.command(&spec);
    cmd.env("DESK_WORKSPACE", &ws.root)
        .env("DESK_AGENT", name)
        // Some CLIs (OpenCode) trust an inherited $PWD over the real cwd.
        .env("PWD", &cfg.dir)
        .env("PATH", path_with_desk()?);
    Ok(cmd)
}

#[cfg(feature = "terminal")]
/// The environment desk gives an agent's CLI, as plain pairs (an empty
/// value clears a variable), for terminals.
pub(crate) fn agent_env(
    ws: &Workspace,
    name: &str,
    cfg: &AgentConfig,
    preset: &Preset,
) -> Result<Vec<(String, String)>> {
    let cmd = agent_command(ws, name, cfg, preset, None, false)?;
    let text = |s: &std::ffi::OsStr| s.to_string_lossy().into_owned();
    Ok(cmd.get_envs().map(|(k, v)| (text(k), v.map(text).unwrap_or_default())).collect())
}

/// What desk runs for an agent, spelled out so a human can run it by hand.
pub struct CommandLine {
    pub dir: PathBuf,
    pub program: String,
    pub args: Vec<String>,
    /// Variables desk sets (`None`: removes).
    pub env: Vec<(String, Option<String>)>,
    /// The latest prompt, fed on stdin unless the preset reads it as a file.
    pub stdin: Option<PathBuf>,
}

/// The agent's next run: its current session, and the latest prompt desk sent it.
pub fn command_line(ws: &Workspace, name: &str) -> Result<CommandLine> {
    let cfg = AgentConfig::load(ws, name)?;
    let preset = Preset::load(ws, &cfg.cli)?;
    let session = AgentState::load(ws, name).session;
    let cmd = agent_command(ws, name, &cfg, &preset, session.as_deref(), session.is_some())?;
    let text = |s: &std::ffi::OsStr| s.to_string_lossy().into_owned();
    let prompt = prompt_file(ws, name);
    Ok(CommandLine {
        dir: cfg.dir.clone(),
        program: text(cmd.get_program()),
        args: cmd.get_args().map(text).collect(),
        env: cmd
            .get_envs()
            .filter(|(k, _)| *k != "PATH" && *k != "PWD") // PATH: set separately in scripts
            .map(|(k, v)| (text(k), v.map(text)))
            .collect(),
        stdin: (!preset.uses_prompt_file() && prompt.is_file()).then_some(prompt),
    })
}

impl CommandLine {
    /// One line for a shell: `program args < prompt`.
    pub fn display(&self) -> String {
        let mut line = quote(&self.program);
        for a in &self.args {
            line.push(' ');
            line.push_str(&quote(a));
        }
        if let Some(p) = &self.stdin {
            line.push_str(&format!(" < {}", quote(&p.display().to_string())));
        }
        line
    }

    /// A script that sets desk's environment, goes to the project, and
    /// shows the command, ready to run. A `.cmd` on Windows, `sh` elsewhere.
    pub fn script(&self, ws: &Workspace, name: &str) -> Result<String> {
        let exe = std::env::current_exe()?;
        let desk_dir = exe.parent().map(|d| d.display().to_string()).unwrap_or_default();
        let dir = quote(&self.dir.display().to_string());
        let line = self.display();
        let mut s: Vec<String> = Vec::new();
        if cfg!(windows) {
            s.push("@echo off".into());
            for (k, v) in &self.env {
                s.push(format!("set \"{k}={}\"", v.as_deref().unwrap_or("")));
            }
            s.push(format!("set \"PATH={desk_dir};%PATH%\""));
            s.push(format!("cd /d {dir}"));
            s.push(format!("echo desk's environment for {name} is set (workspace {}).", ws.root.display()));
            s.push("echo Its next run, to paste and run or tweak (the prompt goes on stdin):".into());
            s.push("echo.".into());
            s.push(format!("echo   {}", cmd_echo(&line)));
            s.push("echo.".into());
            s.push(format!(
                "echo If desk is running {name}, Take over first: two runs would share one session."
            ));
            Ok(s.join("\r\n") + "\r\n")
        } else {
            for (k, v) in &self.env {
                s.push(match v {
                    Some(v) => format!("export {k}={}", quote(v)),
                    None => format!("unset {k}"),
                });
            }
            s.push(format!("export PATH={}:\"$PATH\"", quote(&desk_dir)));
            s.push(format!("cd {dir}"));
            s.push(format!("echo {}", quote(&format!("{name}'s next run: {line}"))));
            Ok(s.join("\n") + "\n")
        }
    }
}

/// Quotes an argument for cmd.exe or sh when it needs it.
fn quote(s: &str) -> String {
    let plain = !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || r"-_./\:=,@+".contains(c));
    if plain {
        s.to_string()
    } else if cfg!(windows) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Makes text safe to `echo` in a .cmd file.
fn cmd_echo(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '%' => out.push_str("%%"),
            '<' | '>' | '|' | '&' | '^' => {
                out.push('^');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

fn heartbeat_path(ws: &Workspace) -> PathBuf {
    ws.root.join("log/supervisor.pid")
}

/// Whether some supervisor has beaten its heartbeat in the last few seconds.
pub fn is_alive(ws: &Workspace) -> bool {
    std::fs::metadata(heartbeat_path(ws))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age < HEARTBEAT_STALE)
}

/// Agents call `desk`, so the folder holding this binary goes first on PATH.
fn path_with_desk() -> Result<std::ffi::OsString> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(d) = std::env::current_exe()?.parent() {
        dirs.push(d.to_path_buf());
    }
    if let Some(p) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&p));
    }
    Ok(std::env::join_paths(dirs)?)
}

/// Writes a run's lines to `<session>.jsonl`. Until the session id is known
/// (first run of a session) lines wait in memory.
struct TranscriptWriter {
    dir: PathBuf,
    file: Option<std::fs::File>,
    pending: Vec<String>,
}

impl TranscriptWriter {
    fn new(dir: PathBuf, session: Option<String>) -> Self {
        let mut w = TranscriptWriter { dir, file: None, pending: Vec::new() };
        if let Some(s) = session {
            w.set_session(&s);
        }
        w
    }

    fn set_session(&mut self, session: &str) {
        if self.file.is_none() {
            self.open(&format!("{session}.jsonl"));
        }
    }

    fn open(&mut self, file: &str) {
        let path = self.dir.join(file.replace(['/', '\\', ':'], "_"));
        self.file = std::fs::OpenOptions::new().create(true).append(true).open(path).ok();
        for line in std::mem::take(&mut self.pending) {
            self.write(&line);
        }
    }

    fn write(&mut self, line: &str) {
        match &mut self.file {
            Some(f) => {
                let _ = f.write_all(format!("{line}\n").as_bytes());
            }
            None => self.pending.push(line.to_string()),
        }
    }

    fn finish(&mut self) {
        if self.file.is_none() && !self.pending.is_empty() {
            self.open(&format!("nosession-{}.jsonl", chrono::Local::now().format("%Y%m%dT%H%M%S")));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(name: &str, kind: Kind) -> Message {
        Message {
            path: PathBuf::from(name),
            from: "a".into(),
            to: "b".into(),
            task: None,
            kind,
            body: name.into(),
        }
    }

    #[test]
    fn raw_messages_run_alone_and_in_order() {
        let cfg = AgentConfig {
            cli: "opencode".into(),
            model: String::new(),
            dir: ".".into(),
            role: "worker".into(),
            permissions: "bypass".into(),
            active: true,
            terminal: false,
        };
        let names = |b: (Vec<Message>, Option<String>)| b.0.into_iter().map(|m| m.body).collect::<Vec<_>>();
        let unread = vec![msg("1", Kind::Note), msg("2", Kind::Raw), msg("3", Kind::Note)];
        assert_eq!(names(pick_batch(&cfg, unread)), ["1"]);
        let unread = vec![msg("2", Kind::Raw), msg("3", Kind::Note)];
        assert_eq!(names(pick_batch(&cfg, unread)), ["2"]);
        // Interrupts still jump the queue.
        let unread = vec![msg("2", Kind::Raw), msg("4", Kind::Interrupt)];
        assert_eq!(names(pick_batch(&cfg, unread)), ["4"]);
    }

    #[test]
    fn questions_left_in_the_transcript() {
        assert!(ends_in_question("Merged 049.\n\nShould rebels keep their land, or lose it?"));
        assert!(!ends_in_question("Is it flaky? It was.\n\nFixed and merged."));
        assert!(!ends_in_question(""));
    }

    #[test]
    fn orchestrator_batches_keep_every_interrupt() {
        let cfg = AgentConfig { role: "orchestrator".into(), ..test_cfg() };
        let on = |name: &str, kind, task: &str| Message { task: Some(task.into()), ..msg(name, kind) };
        let unread = vec![
            on("a", Kind::Interrupt, "003"),
            on("b", Kind::Interrupt, "005"),
            on("c", Kind::Note, "005"),
        ];
        let (batch, task) = pick_batch(&cfg, unread);
        let names: Vec<_> = batch.into_iter().map(|m| m.body).collect();
        // One task's notes per turn, but no interrupt left out: it would stop the run forever.
        assert_eq!((names, task.as_deref()), (vec!["a".to_string(), "b".into()], Some("003")));
    }

    fn test_cfg() -> AgentConfig {
        AgentConfig {
            cli: "opencode".into(),
            model: String::new(),
            dir: ".".into(),
            role: "worker".into(),
            permissions: "bypass".into(),
            active: true,
            terminal: false,
        }
    }
}
