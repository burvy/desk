//! How to run an agent CLI headless. Each CLI is a preset: a small TOML
//! file naming the command, how sessions continue, and how to read the
//! output. desk ships presets for common CLIs (`presets/*.toml` in this
//! crate); a workspace's `presets/<name>.toml` adds one or overrides one.
//! The only CLI-specific code is the output parsers in this folder.

mod claude;
mod opencode;

use crate::agent::AgentConfig;
use crate::workspace::Workspace;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

const BUILTIN: [(&str, &str); 7] = [
    ("claude", include_str!("../../presets/claude.toml")),
    ("opencode", include_str!("../../presets/opencode.toml")),
    ("codex", include_str!("../../presets/codex.toml")),
    ("gemini", include_str!("../../presets/gemini.toml")),
    ("qwen", include_str!("../../presets/qwen.toml")),
    ("goose", include_str!("../../presets/goose.toml")),
    ("cline", include_str!("../../presets/cline.toml")),
];

/// One preset. In `args`, `bypass`, `resume` and the rest, these are filled in:
/// `{model}` `{session}` `{dir}` `{workspace}` `{role_file}` `{prompt_file}`.
/// The prompt goes to stdin, unless `args` mention `{prompt_file}`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Preset {
    #[serde(skip)]
    pub name: String,
    /// One line for the GUI.
    pub description: String,
    /// The program. npm `.cmd` shims are found on Windows.
    pub program: String,
    /// Arguments for every headless run.
    pub args: Vec<String>,
    /// Added when the agent sets a model.
    pub model_args: Vec<String>,
    /// Added when the agent has `permissions = "bypass"`.
    pub bypass: Vec<String>,
    /// Passes the role prompt as a system prompt. If empty, the role prompt
    /// goes at the top of each new session's first message instead.
    pub system: Vec<String>,
    /// `stream`: the CLI reports a session id. `named`: desk names the
    /// session. `none`: every run is fresh; memory.md carries over.
    pub session: String,
    /// Added when continuing a session.
    pub resume: Vec<String>,
    /// How to read stdout: `claude` or `opencode` (their JSON streams), or `text`.
    pub output: String,
    /// The command for take-over; `bypass` and `resume` are added as for a run.
    pub interactive: Vec<String>,
    /// Environment for the CLI. An empty value removes the variable.
    pub env: BTreeMap<String, String>,
    /// Model suggestions for the GUI.
    pub models: Vec<String>,
}

pub struct RunSpec<'a> {
    pub cfg: &'a AgentConfig,
    pub session: Option<&'a str>,
    /// Whether `session` is being continued (false on its first run).
    pub resuming: bool,
    pub role_file: &'a Path,
    pub prompt_file: &'a Path,
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

impl Preset {
    pub fn parse(name: &str, text: &str) -> Result<Self> {
        let mut p: Preset = toml::from_str(text).with_context(|| format!("parsing preset `{name}`"))?;
        p.name = name.to_string();
        if p.program.is_empty() {
            bail!("preset `{name}` has no program");
        }
        if !matches!(p.session.as_str(), "stream" | "named" | "none") {
            bail!("preset `{name}`: session is stream, named or none, not `{}`", p.session);
        }
        if !matches!(p.output.as_str(), "claude" | "opencode" | "text") {
            bail!("preset `{name}`: output is claude, opencode or text, not `{}`", p.output);
        }
        if p.session == "stream" && p.output == "text" {
            bail!("preset `{name}`: plain text output can't report a session id; use named or none");
        }
        Ok(p)
    }

    /// The workspace's `presets/<name>.toml`, else the built-in one.
    pub fn load(ws: &Workspace, name: &str) -> Result<Self> {
        let path = ws.root.join("presets").join(format!("{name}.toml"));
        if path.is_file() {
            return Self::parse(name, &std::fs::read_to_string(&path)?);
        }
        match BUILTIN.iter().find(|(n, _)| *n == name) {
            Some((_, text)) => Self::parse(name, text),
            None => bail!("no preset `{name}`. Known: {}", names(ws).join(", ")),
        }
    }

    /// The command for one run, in `cfg.dir`.
    pub fn command(&self, run: &RunSpec) -> Command {
        let fill = |args: &[String]| -> Vec<String> { args.iter().map(|a| self.fill(a, run)).collect() };
        let mut c = program(&self.program);
        c.args(fill(&self.args));
        if !run.cfg.model.is_empty() {
            c.args(fill(&self.model_args));
        }
        if run.cfg.permissions == "bypass" {
            c.args(fill(&self.bypass));
        }
        c.args(fill(&self.system));
        if run.resuming {
            c.args(fill(&self.resume));
        }
        for (k, v) in &self.env {
            if v.is_empty() {
                c.env_remove(k);
            } else {
                c.env(k, v);
            }
        }
        c.current_dir(&run.cfg.dir);
        c
    }

    fn fill(&self, arg: &str, run: &RunSpec) -> String {
        arg.replace("{model}", &run.cfg.model)
            .replace("{session}", run.session.unwrap_or(""))
            .replace("{dir}", &run.cfg.dir.display().to_string())
            .replace("{workspace}", &run.workspace.display().to_string())
            .replace("{role_file}", &run.role_file.display().to_string())
            .replace("{prompt_file}", &run.prompt_file.display().to_string())
    }

    /// Whether the prompt is passed as a file instead of on stdin.
    pub fn uses_prompt_file(&self) -> bool {
        self.args.iter().any(|a| a.contains("{prompt_file}"))
    }

    /// Whether the role prompt goes in as a system prompt.
    pub fn system_prompt(&self) -> bool {
        !self.system.is_empty()
    }

    /// The program and args that open this session interactively, for take-over.
    pub fn interactive(&self, cfg: &AgentConfig, session: Option<&str>) -> Result<Vec<String>> {
        if self.interactive.is_empty() {
            bail!("preset `{}` has no interactive command", self.name);
        }
        let mut v = self.interactive.clone();
        if cfg.permissions == "bypass" {
            v.extend(self.bypass.iter().cloned());
        }
        if session.is_some() {
            v.extend(self.resume.iter().cloned());
        }
        let session = session.unwrap_or("");
        Ok(v.into_iter().map(|a| a.replace("{model}", &cfg.model).replace("{session}", session)).collect())
    }

    /// One line of stdout → what happened.
    pub fn parse_line(&self, line: &str) -> Vec<Event> {
        match self.output.as_str() {
            "claude" => claude::parse(line),
            "opencode" => opencode::parse(line),
            _ if line.trim().is_empty() => Vec::new(),
            _ => vec![Event::Text(line.to_string())],
        }
    }

    /// Whether the program is on PATH.
    pub fn installed(&self) -> bool {
        which(&self.program).is_some()
    }
}

/// Every preset: built-in, plus the workspace's own.
pub fn names(ws: &Workspace) -> Vec<String> {
    let mut v: Vec<String> = BUILTIN.iter().map(|(n, _)| n.to_string()).collect();
    for p in crate::fsutil::files_with_ext(&ws.root.join("presets"), "toml") {
        let n = p.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        if !v.contains(&n) {
            v.push(n);
        }
    }
    v
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_parse() {
        for (name, text) in BUILTIN {
            Preset::parse(name, text).unwrap();
        }
    }

    #[test]
    fn builds_a_command() {
        let p = Preset::parse("claude", BUILTIN[0].1).unwrap();
        let cfg = AgentConfig {
            cli: "claude".into(),
            model: "sonnet".into(),
            dir: ".".into(),
            role: "worker".into(),
            permissions: "bypass".into(),
            active: true,
        };
        let spec = RunSpec {
            cfg: &cfg,
            session: Some("s1"),
            resuming: true,
            role_file: Path::new("role.md"),
            prompt_file: Path::new("p.md"),
            workspace: Path::new("ws"),
        };
        let c = p.command(&spec);
        let args: Vec<_> = c.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        let joined = args.join(" ");
        assert!(joined.contains("--model sonnet"), "{joined}");
        assert!(joined.contains("--append-system-prompt-file role.md"), "{joined}");
        assert!(joined.ends_with("--resume s1"), "{joined}");
        assert!(c.get_envs().any(|(k, v)| k == "ANTHROPIC_API_KEY" && v.is_none()));
    }
}
