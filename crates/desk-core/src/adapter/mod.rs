//! One adapter per agent CLI: how to start a headless run, and how to read
//! its JSON stream. Everything CLI-specific lives here.

mod claude;
mod opencode;

use crate::agent::AgentConfig;
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct RunSpec<'a> {
    pub cfg: &'a AgentConfig,
    pub session: Option<&'a str>,
    pub role_file: &'a Path,
    pub workspace: &'a Path,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The CLI's session id, for resuming next run.
    Session(String),
    Text(String),
    Tool {
        name: String,
        detail: String,
    },
    ToolResult {
        ok: bool,
        detail: String,
    },
    /// Tokens in context after the latest model call.
    Usage {
        context: u64,
    },
    /// The run failed (API error, bad model...). Delivery will retry.
    Error(String),
}

pub trait Adapter: Sync {
    /// The command for one run, in `cfg.dir`. The prompt is written to stdin.
    fn command(&self, run: &RunSpec) -> Command;
    /// Whether `command` passes the role prompt as a system prompt. If not, the
    /// supervisor puts it at the top of each new session's first message.
    fn system_prompt(&self) -> bool;
    /// One line of stdout → what happened.
    fn parse(&self, line: &str) -> Vec<Event>;
    /// The program and args that open this session interactively, for take-over.
    fn interactive(&self, cfg: &AgentConfig, session: Option<&str>) -> Vec<String>;
}

pub fn for_cli(cli: &str) -> Option<&'static dyn Adapter> {
    match cli {
        "claude" => Some(&claude::Claude),
        "opencode" => Some(&opencode::OpenCode),
        _ => None,
    }
}

/// `Command::new` that also finds npm's `.cmd` shims on Windows.
fn program(name: &str) -> Command {
    Command::new(which(name).unwrap_or_else(|| PathBuf::from(name)))
}

fn which(name: &str) -> Option<PathBuf> {
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT".into())
            .split(';')
            .map(str::to_string)
            .collect()
    } else {
        vec![String::new()]
    };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .find_map(|dir| exts.iter().map(|ext| dir.join(format!("{name}{ext}"))).find(|p| p.is_file()))
}

/// A one-line summary of a tool call's input: the command, the path, or the JSON.
fn summarize(input: &serde_json::Value) -> String {
    for key in ["command", "file_path", "filePath", "path", "pattern", "url", "description"] {
        if let Some(s) = input.get(key).and_then(|v| v.as_str()) {
            return clip(s, 160);
        }
    }
    clip(&input.to_string(), 160)
}

pub fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    let first = s.lines().next().unwrap_or("");
    let mut out: String = first.chars().take(max).collect();
    if out.len() < s.len() {
        out.push('…');
    }
    out
}
