//! The workspace folder: where it is, what's in `desk.toml`, and where
//! everything lives inside it.

use crate::fsutil;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// The human's name. They get an inbox like any agent.
    pub human: String,
    /// How many agents may run at once. 0 = no cap.
    pub max_running: usize,
    /// Warn when an agent's context passes this many tokens.
    pub context_warn: u64,
    /// Message budget for tasks that don't set one.
    pub default_budget: u32,
    /// Project name → path, usable as `desk spawn --dir <name>`.
    pub projects: BTreeMap<String, PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            human: "human".into(),
            max_running: 0,
            context_warn: 150_000,
            default_budget: 30,
            projects: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Workspace {
    pub root: PathBuf,
    pub config: Config,
}

impl Workspace {
    pub fn open(root: &Path) -> Result<Self> {
        let root = std::path::absolute(root)?;
        let text = fsutil::read(&root.join("desk.toml"))?;
        let config = toml::from_str(&text).context("parsing desk.toml")?;
        Ok(Workspace { root, config })
    }

    /// `$DESK_WORKSPACE`, else the nearest folder above `start` with a desk.toml.
    pub fn find(start: &Path) -> Result<Self> {
        if let Ok(p) = std::env::var("DESK_WORKSPACE") {
            return Self::open(Path::new(&p));
        }
        let start = std::path::absolute(start)?;
        for dir in start.ancestors() {
            if dir.join("desk.toml").is_file() {
                return Self::open(dir);
            }
        }
        bail!("no desk.toml here or above; run `desk init` or set DESK_WORKSPACE")
    }

    /// Lays out a new workspace. Never overwrites existing files.
    pub fn init(root: &Path, human: &str) -> Result<Self> {
        check_name(human)?;
        if root.join("desk.toml").exists() {
            bail!("{} is already a workspace", root.display());
        }
        let config = Config { human: human.into(), ..Config::default() };
        let toml = format!(
            "# desk workspace. See DESIGN.md in the desk repo for what each file is.\n\
             {}\n\
             # [projects]\n# game = \"Z:/path/to/game\"\n",
            toml::to_string(&Config { projects: BTreeMap::new(), ..config.clone() })?
        );
        let files: [(&str, &str); 8] = [
            ("desk.toml", &toml),
            ("DESIGN.md", "# Design\n\nThe orchestrator drafts this on onboarding.\n"),
            ("DECISIONS.md", "# Decisions\n\nOne line per decision, with the reason.\n\n"),
            ("PLAN.md", "# Plan\n\n## Focus\n\n## Next groups\n"),
            ("CONVENTIONS.md", DEFAULT_CONVENTIONS),
            ("prompts/orchestrator.md", include_str!("../prompts/orchestrator.md")),
            ("prompts/worker.md", include_str!("../prompts/worker.md")),
            ("presets/README.md", include_str!("../presets/README.md")),
        ];
        for (rel, text) in files {
            fsutil::create_new(&root.join(rel), text)?;
        }
        for dir in ["tasks", "asks", "locks", "models", "log/transcripts"] {
            std::fs::create_dir_all(root.join(dir))?;
        }
        let ws = Self::open(root)?;
        std::fs::create_dir_all(ws.inbox(&ws.config.human).join("read"))?;
        Ok(ws)
    }

    pub fn agents_dir(&self) -> PathBuf {
        self.root.join("agents")
    }
    pub fn agent_dir(&self, name: &str) -> PathBuf {
        self.agents_dir().join(name)
    }
    pub fn inbox(&self, name: &str) -> PathBuf {
        self.agent_dir(name).join("inbox")
    }
    pub fn tasks_dir(&self) -> PathBuf {
        self.root.join("tasks")
    }
    pub fn asks_dir(&self) -> PathBuf {
        self.root.join("asks")
    }
    pub fn locks_dir(&self) -> PathBuf {
        self.root.join("locks")
    }
    pub fn models_dir(&self) -> PathBuf {
        self.root.join("models")
    }
    pub fn transcripts_dir(&self, agent: &str) -> PathBuf {
        self.root.join("log/transcripts").join(agent)
    }
    pub fn paused_flag(&self) -> PathBuf {
        self.root.join("PAUSED")
    }

    /// Everyone who can receive messages: agents and the human.
    pub fn participants(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(self.agents_dir())
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().join("inbox").is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    /// Appends one line to `log/events.jsonl`.
    pub fn log(&self, event: &str, fields: serde_json::Value) {
        let mut obj = serde_json::json!({ "ts": fsutil::now_rfc3339(), "event": event });
        if let (Some(o), serde_json::Value::Object(f)) = (obj.as_object_mut(), fields) {
            o.extend(f);
        }
        // Logging must never take the core down with it.
        let _ = fsutil::append_line(&self.root.join("log/events.jsonl"), &obj.to_string());
    }

    pub fn events(&self) -> Vec<serde_json::Value> {
        std::fs::read_to_string(self.root.join("log/events.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    pub fn is_paused(&self) -> bool {
        self.paused_flag().exists()
    }

    /// The name of whoever is calling: `$DESK_AGENT` inside an agent, else the human.
    pub fn caller(&self) -> String {
        std::env::var("DESK_AGENT").unwrap_or_else(|_| self.config.human.clone())
    }

    /// `--dir` accepts a project name from desk.toml or a path.
    pub fn resolve_dir(&self, dir: &str) -> Result<PathBuf> {
        let p = match self.config.projects.get(dir) {
            Some(p) => p.clone(),
            None => PathBuf::from(dir),
        };
        let p = std::path::absolute(&p)?;
        if !p.is_dir() {
            bail!("{} is not a directory (or a project in desk.toml)", p.display());
        }
        Ok(p)
    }
}

/// Names become folder names and `desk send` arguments: letters, digits, - and _.
pub fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("`{name}`: names are letters, digits, - and _");
    }
    Ok(())
}

const DEFAULT_CONVENTIONS: &str = "# Conventions

Project taste. Agents read this before writing code. Edit freely.

- Commit small, focused changes with messages that say why.
- Commits are the human's: use their git identity and no AI co-author lines.
- Run the tests before handing work to review.
";
