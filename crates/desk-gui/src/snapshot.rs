//! Everything the window shows, read from the workspace in one go.
//! Re-read about once a second; the files are small.

use desk_core::agent::{self, AgentConfig, AgentState};
use desk_core::ask::{self, Ask};
use desk_core::lock::{self, Lock};
use desk_core::message::{self, Message};
use desk_core::task::{self, Task};
use desk_core::{Workspace, supervisor};

pub struct AgentRow {
    pub name: String,
    pub cfg: AgentConfig,
    pub state: AgentState,
    pub unread: usize,
    pub held: bool,
}

impl AgentRow {
    /// idle | running | error | inactive | held
    pub fn status(&self) -> &str {
        if self.held {
            "held"
        } else if self.state.state.is_empty() {
            "idle"
        } else {
            &self.state.state
        }
    }
}

pub struct Snapshot {
    pub agents: Vec<AgentRow>,
    /// Agents whose agent.toml doesn't parse, with the error.
    pub broken: Vec<(String, String)>,
    pub tasks: Vec<Task>,
    pub asks: Vec<Ask>,
    pub locks: Vec<Lock>,
    pub inbox: Vec<Message>,
    pub supervisor: bool,
    pub paused: bool,
}

impl Snapshot {
    pub fn read(ws: &Workspace) -> Self {
        let (mut agents, mut broken) = (Vec::new(), Vec::new());
        for name in agent::names(ws) {
            match AgentConfig::load(ws, &name) {
                Ok(cfg) => agents.push(AgentRow {
                    cfg,
                    state: AgentState::load(ws, &name),
                    unread: message::unread(ws, &name).len(),
                    held: agent::is_held(ws, &name),
                    name,
                }),
                Err(e) => broken.push((name, format!("{e:#}"))),
            }
        }
        Snapshot {
            agents,
            broken,
            tasks: task::list(ws),
            asks: ask::open(ws),
            locks: lock::list(ws),
            inbox: message::unread(ws, &ws.config.human),
            supervisor: supervisor::is_alive(ws),
            paused: ws.is_paused(),
        }
    }

    pub fn agent(&self, name: &str) -> Option<&AgentRow> {
        self.agents.iter().find(|a| a.name == name)
    }
}
