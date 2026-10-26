//! The supervisor's side of terminal agents: keep each one's CLI running in a
//! terminal, and type its mail in when the screen has gone quiet.
//!
//! There's no event stream to say a turn is over, so "quiet" stands in for
//! it: the screen hasn't changed for `QUIET`. A spinner or streaming text
//! counts as busy. Delivery is "typed into the CLI", so mail moves to `read/`
//! as soon as it's typed.

use crate::adapter::{Preset, RunSpec};
use crate::agent::{self, AgentConfig, AgentState};
use crate::message::{self, Kind};
use crate::supervisor::{Supervisor, agent_env, pick_batch};
use crate::task::Task;
use crate::terminal::Terminal;
use crate::{fsutil, lock, prompt};
use anyhow::Result;
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long the screen must stay unchanged before desk types into it.
const QUIET: Duration = Duration::from_secs(3);
/// How long typed text may take to show up before desk counts it as dropped.
const TAKEN: Duration = Duration::from_secs(2);
/// A CLI that exits sooner than this after starting counts as a failure.
const EARLY_EXIT: Duration = Duration::from_secs(10);

/// The live terminals, shared with the GUI so it can draw them and type into them.
pub type Registry = Arc<Mutex<HashMap<String, Arc<Terminal>>>>;

pub(crate) struct TermAgent {
    term: Arc<Terminal>,
    screen_hash: u64,
    changed: Instant,
    /// Set from a delivery until the screen goes quiet again.
    busy_since: Option<Instant>,
    /// No message typed into this session yet.
    fresh: bool,
    handoff: bool,
    /// Interrupts already acted on.
    interrupted: HashSet<PathBuf>,
    /// The mail typed in this turn (now in `read/`), put back if the CLI dies mid-turn.
    typed: Vec<PathBuf>,
    /// Pasted, not yet submitted: Enter waits until the screen shows the CLI took it.
    pending: Option<Pending>,
    transcript: PathBuf,
}

pub(crate) struct Pending {
    text: String,
    /// The screen before the paste.
    before: String,
    at: Instant,
}

impl Supervisor {
    pub(crate) fn step_terminal(
        &mut self,
        name: &str,
        cfg: &AgentConfig,
        st: &mut AgentState,
        paused: bool,
        notes: &mut Vec<String>,
    ) -> Result<()> {
        let dir = self.ws.agent_dir(name);
        if !cfg.active {
            if self.terms.contains_key(name) {
                self.close_terminal(name);
                notes.push(format!("{name}: terminal closed (killed)"));
            }
            if st.state != "inactive" {
                lock::release_all(&self.ws, name);
                st.state = "inactive".into();
                st.save(&self.ws, name)?;
            }
            return Ok(());
        }
        if dir.join("RETRY").exists() {
            let _ = std::fs::remove_file(dir.join("RETRY"));
            self.retry_at.remove(name);
            st.failures = 0;
            st.state = "idle".into();
            st.save(&self.ws, name)?;
        }
        if st.state == "inactive" {
            st.state = "idle".into();
            st.save(&self.ws, name)?;
        }

        // The CLI exited (crashed, or someone typed /exit). Mid-turn or right
        // after starting, that's a failure, and mail it was given goes back
        // to the inbox. Otherwise it just restarts.
        if let Some(t) = self.terms.get(name)
            && t.term.exited()
        {
            let early = t.term.started.elapsed() < EARLY_EXIT;
            let busy = t.busy_since.is_some() || t.pending.is_some();
            let typed = t.typed.clone();
            self.close_terminal(name);
            if busy {
                unread_again(&self.ws, name, &typed);
            }
            if early || busy {
                let why =
                    if busy { "the CLI exited mid-turn" } else { "the CLI exited right after starting" };
                self.fail(name, st, why.into(), notes);
            } else {
                st.state = "idle".into();
                notes.push(format!("{name}: its CLI exited; restarting"));
            }
            st.save(&self.ws, name)?;
        }
        if st.state == "error" || self.backing_off(name) {
            return Ok(());
        }
        if !self.terms.contains_key(name) {
            if let Err(e) = self.open_terminal(name, cfg) {
                self.fail(name, st, format!("couldn't start the CLI: {e:#}"), notes);
                st.save(&self.ws, name)?;
            } else {
                notes.push(format!("{name}: terminal started"));
            }
            return Ok(());
        }

        let preset = Preset::load(&self.ws, &cfg.cli)?;
        let ws = self.ws.clone();
        let t = self.terms.get_mut(name).expect("checked above");
        let screen = t.term.text();
        let hash = {
            let mut h = DefaultHasher::new();
            screen.hash(&mut h);
            h.finish()
        };
        // Not at its prompt (a trust or permission dialog, say): leave it to you.
        let ready = preset.ready.is_empty() || preset.ready.iter().any(|r| screen.contains(r.as_str()));
        if hash != t.screen_hash {
            t.screen_hash = hash;
            t.changed = Instant::now();
        }
        let quiet = t.changed.elapsed() >= QUIET;

        // Some CLIs show their prompt a moment before they read input, and drop
        // what's typed meanwhile. Submit only text that showed up; otherwise
        // put the mail back and type it again once the screen is quiet.
        if let Some(p) = &t.pending {
            if took(&p.before, &screen, &p.text) {
                let p = t.pending.take().expect("checked above");
                t.term.write(b"\r".to_vec());
                t.fresh = false;
                t.busy_since = Some(Instant::now());
                t.changed = Instant::now();
                log_line(
                    &t.transcript,
                    serde_json::json!({ "desk": "prompt", "ts": fsutil::now_rfc3339(), "text": p.text }),
                );
                st.state = "running".into();
                st.save(&ws, name)?;
                ws.log("run_start", serde_json::json!({ "agent": name, "terminal": true }));
            } else if p.at.elapsed() > TAKEN {
                t.pending = None;
                unread_again(&ws, name, &std::mem::take(&mut t.typed));
                t.handoff = false;
                t.changed = Instant::now();
                notes.push(format!("{name}: its CLI didn't take the typed text; trying again"));
            }
            return Ok(());
        }

        // A turn is over once the screen has settled after a delivery.
        if let Some(since) = t.busy_since
            && quiet
            && since.elapsed() >= QUIET
        {
            t.busy_since = None;
            log_line(
                &t.transcript,
                serde_json::json!({ "desk": "screen", "ts": fsutil::now_rfc3339(), "text": screen }),
            );
            st.state = "idle".into();
            st.failures = 0;
            st.save(&ws, name)?;
            ws.log("run_end", serde_json::json!({ "agent": name, "ok": true, "terminal": true }));
            if t.handoff {
                let _ = std::fs::remove_file(dir.join("RESET"));
                lock::release_all(&ws, name);
                self.close_terminal(name); // the next tick starts a fresh session
                notes.push(format!("{name}: handoff written, session reset"));
                return Ok(());
            }
            notes.push(format!("{name}: done"));
        }
        // The agent wrote its handoff and asked for a fresh session: restart its CLI.
        if dir.join("COMPACT").exists() && t.busy_since.is_none() && t.pending.is_none() {
            let _ = std::fs::remove_file(dir.join("COMPACT"));
            st.context_tokens = 0;
            st.save(&ws, name)?;
            self.close_terminal(name);
            notes.push(format!("{name}: compacted; its CLI restarts fresh"));
            return Ok(());
        }

        let held = agent::is_held(&ws, name);
        let unread = message::unread(&ws, name);
        // An interrupt stops the current turn (the CLI's own key), then goes first.
        let stop_now =
            paused && std::fs::read_to_string(ws.paused_flag()).unwrap_or_default().trim() == "now";
        let new_interrupt =
            unread.iter().find(|m| m.kind == Kind::Interrupt && !t.interrupted.contains(&m.path));
        if t.busy_since.is_some() && (stop_now || new_interrupt.is_some()) {
            if let Some(m) = new_interrupt {
                t.interrupted.insert(m.path.clone());
            }
            t.term.write(preset.interrupt.clone().unwrap_or_else(|| "\x1b".into()).into_bytes());
            t.busy_since = None;
            t.changed = Instant::now();
            notes.push(format!("{name}: interrupted"));
            return Ok(());
        }
        if paused || held || !quiet || t.busy_since.is_some() {
            return Ok(());
        }
        if !ready {
            // Still starting up is not waiting on you.
            if !unread.is_empty() && st.state != "waiting" && t.term.started.elapsed() > EARLY_EXIT {
                st.state = "waiting".into();
                st.save(&ws, name)?;
                let looking_for = preset.ready.join("` or `");
                notes.push(format!(
                    "{name}: waiting for its CLI's prompt (`{looking_for}` on screen); if it's asking something, answer it in the terminal"
                ));
            }
            return Ok(());
        }
        if st.state == "waiting" {
            st.state = "idle".into();
            st.save(&ws, name)?;
        }

        let text = if dir.join("RESET").exists() {
            if t.fresh {
                let _ = std::fs::remove_file(dir.join("RESET"));
                lock::release_all(&ws, name);
                return Ok(());
            }
            t.handoff = true;
            notes.push(format!("{name}: writing handoff before reset"));
            prompt::handoff(&ws, name)
        } else {
            if unread.is_empty() {
                return Ok(());
            }
            let cap = ws.config.max_running;
            if cap > 0 && self.busy_count() >= cap {
                return Ok(());
            }
            let t = self.terms.get_mut(name).expect("checked above");
            let (batch, task) = pick_batch(cfg, unread);
            let task = task.and_then(|id| Task::find(&ws, &id).ok());
            let text = if batch[0].kind == Kind::Raw {
                batch[0].body.trim_end().to_string()
            } else {
                let role = if preset.system_prompt() {
                    None
                } else {
                    std::fs::read_to_string(cfg.role_file(&ws)).ok()
                };
                prompt::delivery(
                    &ws,
                    &prompt::Delivery {
                        name,
                        dir: &cfg.dir,
                        new_session: t.fresh,
                        full: (st.context_tokens >= ws.config.context_warn).then_some(st.context_tokens),
                        role: role.as_deref(),
                        task: task.as_ref(),
                        messages: &batch,
                    },
                )
            };
            let paths: Vec<PathBuf> = batch.iter().map(|m| m.path.clone()).collect();
            message::mark_read(&ws, name, &paths)?;
            t.typed = paths.clone();
            notes.push(format!(
                "{name}: typed {} message{}",
                paths.len(),
                if paths.len() == 1 { "" } else { "s" }
            ));
            text
        };

        let t = self.terms.get_mut(name).expect("checked above");
        t.term.paste(&text);
        t.pending = Some(Pending { text, before: screen, at: Instant::now() });
        Ok(())
    }

    fn open_terminal(&mut self, name: &str, cfg: &AgentConfig) -> Result<()> {
        let preset = Preset::load(&self.ws, &cfg.cli)?;
        let role_file = cfg.role_file(&self.ws);
        let prompt_file = self.ws.agent_dir(name).join("scratch").join("prompt.md");
        let projects = self.ws.other_projects(&cfg.dir);
        let spec = RunSpec {
            cfg,
            session: None,
            resuming: false,
            role_file: &role_file,
            prompt_file: &prompt_file,
            workspace: &self.ws.root,
            projects: &projects,
        };
        let (program, args) = preset.terminal_command(&spec)?;
        let env = agent_env(&self.ws, name, cfg, &preset)?;
        let term = Arc::new(Terminal::spawn(&program, &args, &cfg.dir, &env)?);
        let tdir = self.ws.transcripts_dir(name);
        std::fs::create_dir_all(&tdir)?;
        let transcript = tdir.join(format!("term-{}.jsonl", chrono::Local::now().format("%Y%m%dT%H%M%S")));
        self.registry.lock().unwrap().insert(name.to_string(), term.clone());
        self.terms.insert(
            name.to_string(),
            TermAgent {
                term,
                screen_hash: 0,
                changed: Instant::now(),
                busy_since: None,
                fresh: true,
                handoff: false,
                interrupted: HashSet::new(),
                typed: Vec::new(),
                pending: None,
                transcript,
            },
        );
        self.ws.log("terminal_start", serde_json::json!({ "agent": name, "program": program, "args": args }));
        Ok(())
    }

    pub(crate) fn close_terminal(&mut self, name: &str) {
        if let Some(t) = self.terms.remove(name) {
            t.term.shutdown();
        }
        self.registry.lock().unwrap().remove(name);
    }

    pub(crate) fn busy_count(&self) -> usize {
        self.runs.len() + self.terms.values().filter(|t| t.busy_since.is_some()).count()
    }
}

/// Whether pasted `text` shows on the screen now and didn't before: its first
/// words, or the "[Pasted text …]" line CLIs show for a long paste. A screen
/// that merely changed (a spinner, a rotating banner) doesn't count.
fn took(before: &str, now: &str, text: &str) -> bool {
    let first: String =
        text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").chars().take(16).collect();
    let pastes = |s: &str| s.to_lowercase().matches("paste").count();
    (!first.is_empty() && now.contains(&first) && !before.contains(&first)) || pastes(now) > pastes(before)
}

/// Moves typed mail from `read/` back to the inbox.
fn unread_again(ws: &crate::Workspace, name: &str, paths: &[PathBuf]) {
    let read = ws.inbox(name).join("read");
    for p in paths {
        if let Some(file) = p.file_name() {
            let _ = std::fs::rename(read.join(file), p);
        }
    }
}

fn log_line(path: &std::path::Path, value: serde_json::Value) {
    let _ = fsutil::append_line(path, &value.to_string());
}

#[cfg(test)]
mod tests {
    use super::took;

    #[test]
    fn typed_text_has_to_show_up() {
        let text = "[desk] To fb. Your project: Z:/game\n\nfix the build";
        let empty = "│ ▍Enter a coding task │\n Ad: try Exa";
        assert!(took(empty, "│ [desk] To fb. Your proj▍ │\n Ad: try Exa", text));
        assert!(took(empty, "│ [Pasted text #1 +3 lines] │", text), "a long paste shown collapsed");
        assert!(
            !took(empty, "│ ▍Enter a coding task │\n Ad: Surge SMS API", text),
            "a rotating banner isn't the text"
        );
    }
}
