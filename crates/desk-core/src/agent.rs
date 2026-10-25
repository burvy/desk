//! An agent is a folder: `agent.toml` (config, yours to edit) and
//! `state.toml` (runtime state, written only by the supervisor).

use crate::fsutil;
use crate::workspace::Workspace;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// The CLI preset: `claude`, `opencode`, `goose`... (see `desk presets`).
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

/// Creates the agent, or reactivates it with new settings. Memory is kept either way.
pub fn spawn(ws: &Workspace, name: &str, config: AgentConfig) -> Result<()> {
    if name == ws.config.human {
        bail!("`{name}` is the human");
    }
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("agent names are letters, digits, - and _");
    }
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
    let cmd = crate::adapter::Preset::load(ws, &cfg.cli)?
        .interactive(&cfg, AgentState::load(ws, name).session.as_deref())?;
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
