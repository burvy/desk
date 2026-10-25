//! The supervisor owns every agent process. Each tick it looks at the
//! workspace and decides: deliver mail, interrupt, reset, stop, retry.
//!
//! It polls instead of watching files: a workspace is a few dozen small
//! files, polling works the same on every drive, and missed events can't
//! wedge it. One supervisor per workspace, enforced by a heartbeat file.

use crate::adapter::{self, Event, RunSpec};
use crate::agent::{self, AgentConfig, AgentState};
use crate::message::{self, Kind, Message};
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
    ws: Workspace,
    runs: HashMap<String, Run>,
    tx: Sender<Update>,
    rx: Receiver<Update>,
    retry_at: HashMap<String, Instant>,
    warned: HashSet<String>,
}

struct Run {
    pid: u32,
    delivered: Vec<PathBuf>,
    handoff: bool,
    /// Set when desk stops the run on purpose; the mail stays unread.
    stopping: Option<&'static str>,
    error: Option<String>,
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
        let s =
            Supervisor { ws, runs: HashMap::new(), tx, rx, retry_at: HashMap::new(), warned: HashSet::new() };
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
            if let Err(e) = self.step(&name, paused, &mut notes) {
                notes.push(format!("{name}: {e:#}"));
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
        let _ = std::fs::remove_file(heartbeat_path(&self.ws));
        self.ws.log("supervisor_stop", serde_json::json!({}));
    }

    fn step(&mut self, name: &str, paused: bool, notes: &mut Vec<String>) -> Result<()> {
        let cfg = AgentConfig::load(&self.ws, name)?;
        let dir = self.ws.agent_dir(name);
        let mut st = AgentState::load(&self.ws, name);

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
        if st.state == "inactive" {
            st.state = "idle".into(); // respawned
            st.save(&self.ws, name)?;
        }
        if paused || st.state == "error" || agent::is_held(&self.ws, name) {
            return Ok(());
        }
        if self.retry_at.get(name).is_some_and(|t| Instant::now() < *t) {
            return Ok(());
        }
        let cap = self.ws.config.max_running;
        if cap > 0 && self.runs.len() >= cap {
            return Ok(());
        }

        if dir.join("RESET").exists() {
            if st.session.is_some() {
                let text = prompt::handoff(&self.ws, name);
                self.launch(name, &cfg, &mut st, text, Vec::new(), true)?;
                notes.push(format!("{name}: writing handoff before reset"));
            } else {
                let _ = std::fs::remove_file(dir.join("RESET"));
                lock::release_all(&self.ws, name);
                notes.push(format!("{name}: reset (no session to hand off)"));
            }
            return Ok(());
        }

        let unread = message::unread(&self.ws, name);
        if unread.is_empty() {
            return Ok(());
        }
        let (batch, task) = pick_batch(&cfg, unread);
        let task = task.and_then(|id| Task::find(&self.ws, &id).ok());
        let adapter = adapter::for_cli(&cfg.cli).context("unknown cli")?;
        let role = if adapter.system_prompt() {
            None
        } else {
            std::fs::read_to_string(cfg.role_file(&self.ws)).ok()
        };
        let text = prompt::delivery(
            &self.ws,
            &prompt::Delivery {
                name,
                dir: &cfg.dir,
                new_session: st.session.is_none(),
                role: role.as_deref(),
                task: task.as_ref(),
                messages: &batch,
            },
        );
        let delivered: Vec<PathBuf> = batch.iter().map(|m| m.path.clone()).collect();
        let n = delivered.len();
        self.launch(name, &cfg, &mut st, text, delivered, false)?;
        let about = task.map(|t| format!(" about task {}", t.id)).unwrap_or_default();
        notes.push(format!("{name}: started with {n} message{}{about}", if n == 1 { "" } else { "s" }));
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
        let adapter = adapter::for_cli(&cfg.cli).context("unknown cli")?;
        let role_file = cfg.role_file(&self.ws);
        let spec =
            RunSpec { cfg, session: st.session.as_deref(), role_file: &role_file, workspace: &self.ws.root };
        let mut cmd = adapter.command(&spec);

        let tdir = self.ws.transcripts_dir(name);
        std::fs::create_dir_all(&tdir)?;
        let stderr = std::fs::OpenOptions::new().create(true).append(true).open(tdir.join("stderr.log"))?;
        cmd.env("DESK_WORKSPACE", &self.ws.root)
            .env("DESK_AGENT", name)
            // Some CLIs (OpenCode) trust an inherited $PWD over the real cwd.
            .env("PWD", &cfg.dir)
            .env("PATH", path_with_desk()?)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr);
        hide_window(&mut cmd);
        let mut child = cmd.spawn().with_context(|| format!("starting `{}`", cfg.cli))?;
        let pid = child.id();

        let mut stdin = child.stdin.take().context("no stdin")?;
        let input = prompt.clone();
        std::thread::spawn(move || {
            let _ = stdin.write_all(input.as_bytes());
        });

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
                for e in adapter.parse(&line) {
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
        self.runs.insert(name.to_string(), Run { pid, delivered, handoff, stopping: None, error: None });
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
                    self.warn_context(name, context, notes);
                }
            }
            Event::Error(msg) => {
                if let Some(run) = self.runs.get_mut(name) {
                    run.error = Some(msg);
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

        if let Some(reason) = run.stopping {
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
            }
        } else {
            st.failures += 1;
            let err = run.error.unwrap_or_else(|| "the CLI exited with an error (see stderr.log)".into());
            st.last_error = Some(err.clone());
            if st.failures >= MAX_FAILURES {
                st.state = "error".into();
                lock::release_all(&self.ws, name);
                notes.push(format!("{name}: stopped after {} failures: {err}", st.failures));
                let body = format!(
                    "{name} failed {} times in a row and has stopped.\n\nLast error: {err}\n\n\
                     Its mail is still unread. Fix the cause, then `desk resume {name}`.",
                    st.failures
                );
                let _ = message::send(&self.ws, "desk", &self.ws.config.human, None, Kind::Note, &body);
            } else {
                let wait = Duration::from_secs(30 * 2u64.pow(st.failures - 1));
                self.retry_at.insert(name.to_string(), Instant::now() + wait);
                notes.push(format!("{name}: failed ({err}); retrying in {}s", wait.as_secs()));
            }
        }
        let _ = st.save(&self.ws, name);
        self.ws.log(
            "run_end",
            serde_json::json!({ "agent": name, "ok": ok, "stopped": run.stopping, "error": st.last_error }),
        );
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

    /// Tells the orchestrator (or the human, if the orchestrator is the one
    /// that's full) that a reset is due.
    fn warn_context(&self, name: &str, tokens: u64, notes: &mut Vec<String>) {
        let orchestrator = agent::names(&self.ws).into_iter().find(|n| {
            n != name && AgentConfig::load(&self.ws, n).is_ok_and(|c| c.is_orchestrator() && c.active)
        });
        let to = orchestrator.unwrap_or_else(|| self.ws.config.human.clone());
        let body = format!(
            "{name}'s context is at {tokens} tokens. Consider `desk reset {name}` once it reaches a \
             good stopping point; it writes a handoff first."
        );
        let _ = message::send(&self.ws, "desk", &to, None, Kind::Note, &body);
        notes.push(format!("{name}: context at {tokens} tokens, told {to}"));
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

/// What an agent gets in one run. Interrupts go first. An orchestrator gets
/// one task's mail per turn, so it never juggles unrelated work.
fn pick_batch(cfg: &AgentConfig, mut unread: Vec<Message>) -> (Vec<Message>, Option<String>) {
    unread.sort_by_key(|m| m.kind != Kind::Interrupt); // stable: oldest first otherwise
    if !cfg.is_orchestrator() {
        return (unread, None);
    }
    let task = unread[0].task.clone();
    let batch = unread.into_iter().filter(|m| m.task == task).collect();
    (batch, task)
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

#[cfg(windows)]
fn hide_window(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(unix)]
fn hide_window(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0); // so kill_tree can take the whole group
}

/// Agent CLIs start their own children (shells, servers); take them all down.
fn kill_tree(pid: u32) {
    let mut cmd = if cfg!(windows) {
        let mut c = std::process::Command::new("taskkill");
        c.args(["/T", "/F", "/PID", &pid.to_string()]);
        c
    } else {
        let mut c = std::process::Command::new("kill");
        c.args(["-TERM", &format!("-{pid}")]);
        c
    };
    hide_window(&mut cmd);
    let _ = cmd.stdout(Stdio::null()).stderr(Stdio::null()).status();
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
