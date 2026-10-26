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
use serde::{Deserialize, Deserializer};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Built in: the two desk reads a stream from. Any other CLI is a command you
/// type in the window (`from_command`), saved in the workspace.
const BUILTIN: [(&str, &str); 2] = [
    ("claude", include_str!("../../presets/claude.toml")),
    ("opencode", include_str!("../../presets/opencode.toml")),
];

/// Built in before custom commands, and the command each ran in a terminal.
/// An agent still using one gets it back as a custom command in its workspace.
const RETIRED: [(&str, &str); 7] = [
    ("codex", "codex"),
    ("gemini", "gemini"),
    ("qwen", "qwen"),
    ("goose", "goose session"),
    ("cline", "cline"),
    ("zcode", "zcode"),
    ("junie", "junie --skip-update-check"),
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
    /// The interactive command, for take-over and terminal agents; `bypass`,
    /// `system` and (for take-over) `resume` are added as for a run.
    pub interactive: Vec<String>,
    /// Keys that stop a terminal agent's current turn. Default: Escape.
    pub interrupt: Option<String>,
    /// Text a terminal agent's screen shows only when its CLI is waiting for
    /// a message (not in a startup or permission dialog). desk types only then.
    /// One string, or a list where any one will do.
    #[serde(deserialize_with = "one_or_many")]
    pub ready: Vec<String>,
    /// Environment for the CLI. An empty value removes the variable.
    pub env: BTreeMap<String, String>,
    /// Environment set only when the agent sets a model (placeholders filled),
    /// for CLIs whose interactive mode has no model flag.
    pub model_env: BTreeMap<String, String>,
    /// Model suggestions for the GUI.
    pub models: Vec<String>,
    /// Added once for each of the workspace's other project folders, with
    /// `{project}` filled in: how a CLI gets access to them (Claude: `--add-dir`).
    pub per_project: Vec<String>,
}

pub struct RunSpec<'a> {
    pub cfg: &'a AgentConfig,
    pub session: Option<&'a str>,
    /// Whether `session` is being continued (false on its first run).
    pub resuming: bool,
    pub role_file: &'a Path,
    pub prompt_file: &'a Path,
    pub workspace: &'a Path,
    /// The workspace's other project folders.
    pub projects: &'a [PathBuf],
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
        let path = preset_file(ws, name);
        if path.is_file() {
            return Self::parse(name, &std::fs::read_to_string(&path)?);
        }
        if let Some((_, text)) = BUILTIN.iter().find(|(n, _)| *n == name) {
            return Self::parse(name, text);
        }
        if let Some((_, command)) = RETIRED.iter().find(|(n, _)| *n == name) {
            let (_, text) = from_command(command, "")?;
            save(ws, name, &text)?;
            return Self::parse(name, &text);
        }
        bail!("no preset `{name}`. Known: {}", names(ws).join(", "))
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
        c.args(self.project_args(run.projects));
        for (k, v) in &self.env {
            if v.is_empty() {
                c.env_remove(k);
            } else {
                c.env(k, v);
            }
        }
        if !run.cfg.model.is_empty() {
            for (k, v) in &self.model_env {
                c.env(k, self.fill(v, run));
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

    fn project_args(&self, projects: &[PathBuf]) -> Vec<String> {
        let mut v = Vec::new();
        for p in projects {
            v.extend(self.per_project.iter().map(|a| a.replace("{project}", &p.display().to_string())));
        }
        v
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
    pub fn interactive(
        &self,
        cfg: &AgentConfig,
        session: Option<&str>,
        projects: &[PathBuf],
    ) -> Result<Vec<String>> {
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
        let v = drop_empty(&v, &cfg.model, session);
        let session = session.unwrap_or("");
        let mut v: Vec<String> =
            v.into_iter().map(|a| a.replace("{model}", &cfg.model).replace("{session}", session)).collect();
        v.extend(self.project_args(projects));
        Ok(v)
    }

    /// The interactive command for a terminal agent: a fresh session each time
    /// it starts, with the role prompt as a system prompt where the CLI takes one.
    pub fn terminal_command(&self, run: &RunSpec) -> Result<(PathBuf, Vec<String>)> {
        let Some((program, rest)) = self.interactive.split_first() else {
            bail!("preset `{}` has no interactive command", self.name);
        };
        let mut args = drop_empty(rest, &run.cfg.model, run.session);
        args = args.iter().map(|a| self.fill(a, run)).collect();
        if run.cfg.permissions == "bypass" {
            args.extend(self.bypass.iter().map(|a| self.fill(a, run)));
        }
        args.extend(self.system.iter().map(|a| self.fill(a, run)));
        args.extend(self.project_args(run.projects));
        Ok((which(program).unwrap_or_else(|| PathBuf::from(program)), args))
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

/// What a new preset starts as.
pub const TEMPLATE: &str = r#"# See presets/README.md for every field.
description = "My CLI"
program = "mycli"
# The prompt goes to stdin. For a CLI that wants it as an argument, use
# args = ["-p", "Read {prompt_file} and do what it says."]
args = []
model_args = ["--model", "{model}"]
bypass = []
session = "none"
output = "text"
interactive = ["mycli"]
"#;

/// Where a preset comes from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Origin {
    Builtin,
    /// A workspace file replaces the built-in one.
    Overridden,
    /// Only in the workspace.
    Custom,
}

pub fn origin(ws: &Workspace, name: &str) -> Origin {
    match (preset_file(ws, name).is_file(), BUILTIN.iter().any(|(n, _)| *n == name)) {
        (true, true) => Origin::Overridden,
        (true, false) => Origin::Custom,
        _ => Origin::Builtin,
    }
}

fn preset_file(ws: &Workspace, name: &str) -> PathBuf {
    ws.root.join("presets").join(format!("{name}.toml"))
}

/// The preset's TOML: the workspace's file, else the built-in one.
pub fn source(ws: &Workspace, name: &str) -> Option<String> {
    std::fs::read_to_string(preset_file(ws, name))
        .ok()
        .or_else(|| BUILTIN.iter().find(|(n, _)| *n == name).map(|(_, t)| t.to_string()))
}

/// The built-in text, for "revert".
pub fn builtin(name: &str) -> Option<&'static str> {
    BUILTIN.iter().find(|(n, _)| *n == name).map(|(_, t)| *t)
}

/// Checks and writes `presets/<name>.toml`. Agents pick it up on their next run.
pub fn save(ws: &Workspace, name: &str, text: &str) -> Result<()> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("preset names are letters, digits, - and _");
    }
    Preset::parse(name, text)?;
    std::fs::create_dir_all(ws.root.join("presets"))?;
    crate::fsutil::write_atomic(&preset_file(ws, name), text)?;
    ws.log("preset_saved", serde_json::json!({ "preset": name }));
    Ok(())
}

/// A CLI you start by typing its command (`freebuff`, `pi --yolo`), as a
/// preset: `(name, TOML)`. It's meant for a terminal agent, where desk types
/// messages in, so desk needs nothing else from the CLI. `ready`, if given,
/// is text its screen shows only when it's waiting for a message. Named after
/// the program; a built-in's name gets `-custom` so the built-in stays.
pub fn from_command(command: &str, ready: &str) -> Result<(String, String)> {
    let argv = split_command(command)?;
    let Some(program) = argv.first() else { bail!("type the command that starts the CLI") };
    let stem = Path::new(program).file_stem().unwrap_or_default().to_string_lossy().to_lowercase();
    let mut name: String =
        stem.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '-' }).collect();
    name = name.trim_matches('-').to_string();
    if name.is_empty() {
        name = "custom".into();
    }
    if BUILTIN.iter().any(|(n, _)| *n == name) {
        name.push_str("-custom");
    }
    let mut t = toml::Table::new();
    t.insert("description".into(), format!("Custom: {command}").trim().to_string().into());
    t.insert("program".into(), program.clone().into());
    t.insert("session".into(), "none".into());
    t.insert("output".into(), "text".into());
    t.insert("interactive".into(), argv.clone().into());
    if !ready.trim().is_empty() {
        t.insert("ready".into(), vec![ready.trim().to_string()].into());
    }
    let text = format!(
        "# Made from the command `{command}` in desk's window. It runs in a\n\
         # terminal: desk types messages in. See README.md for the other fields.\n{}",
        toml::to_string(&t)?
    );
    Preset::parse(&name, &text)?;
    Ok((name, text))
}

/// The command a preset made by `from_command` was made from, for editing.
pub fn command_of(p: &Preset) -> Option<String> {
    p.description.strip_prefix("Custom: ").map(str::to_string)
}

/// Splits a command line into words. Quotes ("..." or '...') keep spaces in;
/// backslashes are ordinary, so Windows paths work as typed.
fn split_command(line: &str) -> Result<Vec<String>> {
    let (mut words, mut word, mut quote, mut started) = (Vec::new(), String::new(), None, false);
    for c in line.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => word.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            (None, c) => {
                word.push(c);
                started = true;
            }
        }
    }
    if quote.is_some() {
        bail!("a quote in the command isn't closed");
    }
    if started {
        words.push(word);
    }
    Ok(words)
}

/// Deletes the workspace's file: a built-in goes back to its default.
pub fn remove(ws: &Workspace, name: &str) -> Result<()> {
    std::fs::remove_file(preset_file(ws, name))?;
    ws.log("preset_removed", serde_json::json!({ "preset": name }));
    Ok(())
}

/// In `interactive`, an option whose value is `{model}` or `{session}` is
/// left out when that's empty, so the CLI uses its own default.
fn drop_empty(args: &[String], model: &str, session: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for a in args {
        if (a == "{model}" && model.is_empty()) || (a == "{session}" && session.is_none()) {
            if out.last().is_some_and(|p| p.starts_with('-')) {
                out.pop();
            }
            continue;
        }
        out.push(a.clone());
    }
    out
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
    // A path, typed as one.
    if Path::new(name).is_file() {
        return Some(PathBuf::from(name));
    }
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

fn one_or_many<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::One(s) => vec![s],
        OneOrMany::Many(v) => v,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_parse() {
        for (name, text) in BUILTIN {
            Preset::parse(name, text).unwrap();
        }
        Preset::parse("template", TEMPLATE).unwrap();
    }

    #[test]
    fn presets_from_typed_commands() {
        let (name, text) = from_command(r#"free-pi-cli --yolo --system "be brief""#, "").unwrap();
        let p = Preset::parse(&name, &text).unwrap();
        assert_eq!(name, "free-pi-cli");
        assert_eq!(p.interactive, ["free-pi-cli", "--yolo", "--system", "be brief"]);
        assert!(p.ready.is_empty());
        assert_eq!(command_of(&p).as_deref(), Some(r#"free-pi-cli --yolo --system "be brief""#));

        let (name, text) = from_command(r"C:\Tools\Freebuff.EXE", "> ").unwrap();
        let p = Preset::parse(&name, &text).unwrap();
        assert_eq!((name.as_str(), p.program.as_str()), ("freebuff", r"C:\Tools\Freebuff.EXE"));
        assert_eq!(p.ready, [">"], "trimmed, like the screen text it's matched against");

        assert_eq!(from_command("opencode --yolo", "").unwrap().0, "opencode-custom", "built-ins stay");
        assert!(from_command("   ", "").is_err());
        assert!(from_command("pi \"oops", "").is_err());
    }

    #[test]
    fn retired_builtins_come_back_as_commands() {
        let dir = std::env::temp_dir().join(format!("desk-retired-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ws = Workspace::init(&dir, "me").unwrap();
        let p = Preset::load(&ws, "junie").unwrap();
        assert_eq!(p.interactive, ["junie", "--skip-update-check"]);
        assert_eq!(origin(&ws, "junie"), Origin::Custom, "saved in the workspace, so it's yours to edit");
        assert!(Preset::load(&ws, "nope").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ready_is_one_text_or_several() {
        let claude = Preset::parse("claude", builtin("claude").unwrap()).unwrap();
        assert_eq!(claude.ready, ["? for shortcuts", "shift+tab to cycle"], "bypass mode hides the first");
        assert_eq!(
            Preset::parse("opencode", builtin("opencode").unwrap()).unwrap().ready,
            ["ctrl+p commands"]
        );
    }

    #[test]
    fn save_override_revert() {
        let dir = std::env::temp_dir().join(format!("desk-preset-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ws = Workspace::init(&dir, "me").unwrap();
        assert_eq!(origin(&ws, "opencode"), Origin::Builtin);
        let yolo = builtin("opencode").unwrap().replace(r#"bypass = ["--auto"]"#, r#"bypass = ["--yolo"]"#);
        save(&ws, "opencode", &yolo).unwrap();
        assert_eq!(origin(&ws, "opencode"), Origin::Overridden);
        assert_eq!(Preset::load(&ws, "opencode").unwrap().bypass, ["--yolo"]);
        assert!(save(&ws, "opencode", "progrm = \"typo\"").is_err(), "unknown fields are refused");
        remove(&ws, "opencode").unwrap();
        assert_eq!(Preset::load(&ws, "opencode").unwrap().bypass, ["--auto"]);
        save(&ws, "mine", TEMPLATE).unwrap();
        assert_eq!(origin(&ws, "mine"), Origin::Custom);
        assert!(names(&ws).contains(&"mine".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
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
            terminal: false,
        };
        let spec = RunSpec {
            cfg: &cfg,
            session: Some("s1"),
            resuming: true,
            role_file: Path::new("role.md"),
            prompt_file: Path::new("p.md"),
            workspace: Path::new("ws"),
            projects: &[],
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
