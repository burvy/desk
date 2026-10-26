//! An agent is a folder: `agent.toml` (config, yours to edit) and
//! `state.toml` (runtime state, written only by the supervisor).

use crate::fsutil;
use crate::workspace::Workspace;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// The CLI preset: `claude`, `opencode`, or one of yours (see `desk presets`).
    pub cli: String,
    /// Whatever the CLI accepts: `sonnet`, `opencode/glm-5.3-flash-free`.
    pub model: String,
    /// The folder the agent works in.
    pub dir: PathBuf,
    /// `orchestrator`, `worker`, or a path to a prompt file (relative to the workspace).
    pub role: String,
    /// `bypass` (no permission prompts) or `default`.
    #[serde(default = "bypass")]
    pub permissions: String,
    /// false after `desk kill`. The agent keeps its memory and can be respawned.
    #[serde(default = "yes")]
    pub active: bool,
    /// Run the CLI interactively in a terminal (inside desk-gui) instead of
    /// headless. desk types its messages in.
    #[serde(default)]
    pub terminal: bool,
}

fn bypass() -> String {
    "bypass".into()
}
fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AgentState {
    /// The CLI's session id. Cleared by reset.
    pub session: Option<String>,
    /// idle | running | error | inactive
    pub state: String,
    pub context_tokens: u64,
    pub failures: u32,
    pub last_error: Option<String>,
    pub updated: String,
}

impl AgentConfig {
    pub fn load(ws: &Workspace, name: &str) -> Result<Self> {
        let path = ws.agent_dir(name).join("agent.toml");
        let text = fsutil::read(&path)?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self, ws: &Workspace, name: &str) -> Result<()> {
        fsutil::write_atomic(&ws.agent_dir(name).join("agent.toml"), &toml::to_string(self)?)
    }

    pub fn is_orchestrator(&self) -> bool {
        self.role == "orchestrator"
    }

    /// The prompt file for this role.
    pub fn role_file(&self, ws: &Workspace) -> PathBuf {
        match self.role.as_str() {
            "orchestrator" | "worker" => ws.root.join("prompts").join(format!("{}.md", self.role)),
            path => ws.root.join(path),
        }
    }

    /// Key for `models/<key>.md`: `opencode-opencode-glm-5.3-flash-free`.
    pub fn model_key(&self) -> String {
        format!("{}-{}", self.cli, self.model.replace(['/', '\\', ':'], "-"))
    }
}

impl AgentState {
    pub fn load(ws: &Workspace, name: &str) -> Self {
        std::fs::read_to_string(ws.agent_dir(name).join("state.toml"))
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&mut self, ws: &Workspace, name: &str) -> Result<()> {
        self.updated = fsutil::now_rfc3339();
        fsutil::write_atomic(&ws.agent_dir(name).join("state.toml"), &toml::to_string(self)?)
    }
}

/// Every agent with an `agent.toml` (so not the human), sorted by name.
pub fn names(ws: &Workspace) -> Vec<String> {
    ws.participants().into_iter().filter(|n| ws.agent_dir(n).join("agent.toml").is_file()).collect()
}

/// Every agent in the order you put them in (`agents/order.txt`, one name a
/// line). Anyone not listed comes after: orchestrators first, then by name.
pub fn ordered(ws: &Workspace) -> Vec<String> {
    let order = read_order(ws);
    let mut names = names(ws);
    names.sort_by_cached_key(|n| {
        let orchestrator = AgentConfig::load(ws, n).is_ok_and(|c| c.is_orchestrator());
        (order.iter().position(|o| o == n).unwrap_or(usize::MAX), !orchestrator, n.clone())
    });
    names
}

/// Saves the order agents are listed in.
pub fn set_order(ws: &Workspace, names: &[String]) -> Result<()> {
    fsutil::write_atomic(&order_path(ws), &names.iter().map(|n| format!("{n}\n")).collect::<String>())
}

fn read_order(ws: &Workspace) -> Vec<String> {
    let text = std::fs::read_to_string(order_path(ws)).unwrap_or_default();
    text.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect()
}

fn order_path(ws: &Workspace) -> PathBuf {
    ws.agents_dir().join("order.txt")
}

/// Creates the agent, or reactivates it with new settings. Memory is kept either way.
pub fn spawn(ws: &Workspace, name: &str, config: AgentConfig) -> Result<()> {
    if name == ws.config.human {
        bail!("`{name}` is the human");
    }
    crate::workspace::check_name(name)?;
    crate::adapter::Preset::load(ws, &config.cli)?;
    if !config.role_file(ws).is_file() {
        bail!("no prompt file at {}", config.role_file(ws).display());
    }
    let dir = ws.agent_dir(name);
    std::fs::create_dir_all(dir.join("inbox/read"))?;
    std::fs::create_dir_all(dir.join("scratch"))?;
    fsutil::create_new(&dir.join("memory.md"), &format!("# {name}'s memory\n\n"))?;
    config.save(ws, name)?;
    crate::models::ensure(ws, &config)?;
    ws.log("spawn", serde_json::json!({ "agent": name, "cli": config.cli, "model": config.model }));
    Ok(())
}

/// Deactivates the agent. The supervisor stops its run and releases its locks.
pub fn kill(ws: &Workspace, name: &str) -> Result<()> {
    let mut c = AgentConfig::load(ws, name)?;
    c.active = false;
    c.save(ws, name)?;
    ws.log("kill", serde_json::json!({ "agent": name }));
    Ok(())
}

/// Removes the agent for good. A running supervisor stops it first and then
/// deletes it (`true` comes back once it's gone). Works on an agent whose
/// agent.toml is broken.
pub fn remove(ws: &Workspace, name: &str) -> Result<bool> {
    if name == ws.config.human {
        bail!("`{name}` is the human");
    }
    crate::workspace::check_name(name)?;
    if !ws.agent_dir(name).is_dir() {
        bail!("no agent named `{name}`");
    }
    if crate::supervisor::is_alive(ws) {
        fsutil::write_atomic(&ws.agent_dir(name).join("REMOVE"), "")?;
        return Ok(false);
    }
    purge(ws, name)?;
    Ok(true)
}

/// Deletes a stopped agent: its folder and transcripts. Its locks are
/// released, its unfinished tasks go back to `open` without an owner, and its
/// open asks are withdrawn. What it sent others and the event log stay.
pub(crate) fn purge(ws: &Workspace, name: &str) -> Result<()> {
    crate::lock::release_all(ws, name);
    for a in crate::ask::open(ws).into_iter().filter(|a| a.from() == name) {
        crate::ask::withdraw(ws, a)?;
    }
    for t in crate::task::list(ws) {
        if t.owner().as_deref() == Some(name) && !matches!(t.status().as_str(), "done" | "cancelled") {
            crate::task::set(ws, &t.id, "owner", "")?;
            crate::task::set(ws, &t.id, "status", "open")?;
        }
    }
    // Renamed out of the way first: a folder half-deleted while a file is held
    // open would otherwise look like a broken agent.
    let trash = ws.root.join("log").join(format!(".removed-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&trash);
    std::fs::rename(ws.agent_dir(name), &trash).with_context(|| format!("removing {name}'s folder"))?;
    let _ = std::fs::remove_dir_all(&trash);
    let _ = std::fs::remove_dir_all(ws.transcripts_dir(name));
    let order = read_order(ws);
    if order.iter().any(|n| n == name) {
        set_order(ws, &order.into_iter().filter(|n| n != name).collect::<Vec<_>>())?;
    }
    ws.log("remove", serde_json::json!({ "agent": name }));
    Ok(())
}

/// Asks the supervisor for a handoff turn, then a fresh session.
pub fn request_reset(ws: &Workspace, name: &str) -> Result<()> {
    AgentConfig::load(ws, name)?;
    fsutil::write_atomic(&ws.agent_dir(name).join("RESET"), "")?;
    ws.log("reset_requested", serde_json::json!({ "agent": name }));
    Ok(())
}

/// Take-over: holds delivery so a human can drive the session in a terminal.
/// Returns the command line that opens the session interactively.
pub fn take_over(ws: &Workspace, name: &str) -> Result<Vec<String>> {
    let cfg = AgentConfig::load(ws, name)?;
    let cmd = crate::adapter::Preset::load(ws, &cfg.cli)?.interactive(
        &cfg,
        AgentState::load(ws, name).session.as_deref(),
        &ws.other_projects(&cfg.dir),
    )?;
    fsutil::write_atomic(&ws.agent_dir(name).join("HELD"), "")?;
    ws.log("take_over", serde_json::json!({ "agent": name }));
    Ok(cmd)
}

/// Ends a take-over; mail that arrived meanwhile is delivered.
pub fn hand_back(ws: &Workspace, name: &str) -> Result<()> {
    let _ = std::fs::remove_file(ws.agent_dir(name).join("HELD"));
    ws.log("hand_back", serde_json::json!({ "agent": name }));
    Ok(())
}

pub fn is_held(ws: &Workspace, name: &str) -> bool {
    ws.agent_dir(name).join("HELD").exists()
}

/// Asks the supervisor to retry an agent that stopped after repeated failures.
pub fn request_retry(ws: &Workspace, name: &str) -> Result<()> {
    AgentConfig::load(ws, name)?;
    fsutil::write_atomic(&ws.agent_dir(name).join("RETRY"), "")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{self, NewTask, Task};

    #[test]
    fn remove_leaves_nothing_behind() {
        let dir = std::env::temp_dir().join(format!("desk-remove-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ws = Workspace::init(&dir, "me").unwrap();
        // Spawned straight to the folder: a broken agent.toml must not stop a remove.
        std::fs::create_dir_all(ws.inbox("zhipu").join("read")).unwrap();
        std::fs::write(ws.agent_dir("zhipu").join("agent.toml"), "cli = ").unwrap();
        std::fs::create_dir_all(ws.transcripts_dir("zhipu")).unwrap();
        let mine = |status: &str| {
            task::new(
                &ws,
                NewTask {
                    title: "x".into(),
                    status: Some(status.into()),
                    owner: Some("zhipu".into()),
                    ..NewTask::default()
                },
            )
            .unwrap()
        };
        let (working, done) = (mine("working"), mine("done"));
        crate::ask::new(&ws, "zhipu", Some(&working.id), true, "ok?").unwrap();
        crate::lock::acquire(&ws, "db", "zhipu", "").unwrap();

        assert!(remove(&ws, "zhipu").unwrap(), "no supervisor, so it's gone at once");
        assert!(!ws.agent_dir("zhipu").exists() && !ws.transcripts_dir("zhipu").exists());
        let w = Task::find(&ws, &working.id).unwrap();
        assert_eq!((w.status(), w.owner().unwrap_or_default()), ("open".into(), String::new()));
        assert_eq!(Task::find(&ws, &done.id).unwrap().owner().as_deref(), Some("zhipu"), "history stays");
        assert!(crate::ask::open(&ws).is_empty() && crate::lock::list(&ws).is_empty());
        assert!(remove(&ws, "zhipu").is_err());
        let _ = std::fs::remove_dir_all(&ws.root);
    }

    #[test]
    fn order_is_yours_then_orchestrators_then_names() {
        let dir = std::env::temp_dir().join(format!("desk-order-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ws = Workspace::init(&dir, "me").unwrap();
        for (name, role) in
            [("alpha", "worker"), ("boss", "orchestrator"), ("haiku", "worker"), ("sonnet", "worker")]
        {
            std::fs::create_dir_all(ws.inbox(name)).unwrap();
            let cfg = format!("cli = \"opencode\"\nmodel = \"\"\ndir = \".\"\nrole = \"{role}\"\n");
            std::fs::write(ws.agent_dir(name).join("agent.toml"), cfg).unwrap();
        }
        assert_eq!(ordered(&ws), ["boss", "alpha", "haiku", "sonnet"]);
        set_order(&ws, &["sonnet".into(), "haiku".into()]).unwrap();
        assert_eq!(ordered(&ws), ["sonnet", "haiku", "boss", "alpha"]);
        std::fs::write(ws.agent_dir("haiku").join("agent.toml"), "cli = ").unwrap();
        assert!(remove(&ws, "haiku").unwrap());
        assert_eq!(read_order(&ws), ["sonnet"], "a removed agent leaves the order too");
        assert!(remove(&ws, "me").is_err());
        let _ = std::fs::remove_dir_all(&ws.root);
    }
}
