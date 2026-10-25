//! Tasks: `tasks/<id>-<slug>/task.md`, a header plus Intent / Proposed
//! approach / Done when / Non-goals.

use crate::frontmatter::{Doc, list_value};
use crate::fsutil::{self, FileLock};
use crate::workspace::Workspace;
use crate::{agent, models};
use anyhow::{Result, bail};
use std::path::PathBuf;

pub const STATUSES: [&str; 8] =
    ["proposed", "open", "assigned", "working", "review", "blocked", "done", "cancelled"];

const LIST_FIELDS: [&str; 2] = ["files", "depends_on"];

#[derive(Debug, Clone)]
pub struct Task {
    pub dir: PathBuf,
    pub id: String,
    pub doc: Doc,
}

#[derive(Debug, Clone, Default)]
pub struct NewTask {
    pub title: String,
    pub intent: String,
    pub done_when: String,
    pub approach: String,
    pub non_goals: String,
    pub status: Option<String>,
    pub owner: Option<String>,
    pub group: Option<String>,
    pub tier: Option<String>,
    pub files: Vec<String>,
    pub depends_on: Vec<String>,
    pub budget: Option<u32>,
}

impl Task {
    pub fn path(&self) -> PathBuf {
        self.dir.join("task.md")
    }

    pub fn load(dir: PathBuf) -> Result<Self> {
        let doc = Doc::parse(&fsutil::read(&dir.join("task.md"))?)?;
        let name = dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let id = doc.get("id").unwrap_or_else(|| name.split('-').next().unwrap_or("").to_string());
        Ok(Task { dir, id, doc })
    }

    /// `14`, `014` and `014-explosion-sand` all find task 014.
    pub fn find(ws: &Workspace, id: &str) -> Result<Self> {
        let key = id.split('-').next().unwrap_or(id);
        let key = key.parse::<u32>().map(fsutil::fmt_id).unwrap_or_else(|_| key.to_string());
        for dir in dirs(ws) {
            let name = dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
            if name == key || name.starts_with(&format!("{key}-")) {
                return Task::load(dir);
            }
        }
        bail!("no task {id}")
    }

    pub fn title(&self) -> String {
        self.doc.get("title").unwrap_or_else(|| self.slug())
    }
    pub fn slug(&self) -> String {
        let name = self.dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
        name.split_once('-').map(|(_, s)| s.to_string()).unwrap_or(name)
    }
    pub fn status(&self) -> String {
        self.doc.get("status").unwrap_or_else(|| "proposed".into())
    }
    pub fn owner(&self) -> Option<String> {
        self.doc.get("owner")
    }
    pub fn group(&self) -> Option<String> {
        self.doc.get("group")
    }
    pub fn budget(&self, ws: &Workspace) -> u32 {
        self.doc.get("budget").and_then(|b| b.parse().ok()).unwrap_or(ws.config.default_budget)
    }

    /// Required sections that are missing or empty.
    pub fn missing_sections(&self) -> Vec<&'static str> {
        ["Intent", "Done when"]
            .into_iter()
            .filter(|s| section(&self.doc.body, s).is_none_or(|t| t.trim().is_empty()))
            .collect()
    }
}

/// The text under `## <name>`, up to the next `## `.
pub fn section(body: &str, name: &str) -> Option<String> {
    let head = format!("## {name}");
    let mut lines = body.lines().skip_while(|l| !l.trim().eq_ignore_ascii_case(&head));
    lines.next()?;
    Some(lines.take_while(|l| !l.starts_with("## ")).collect::<Vec<_>>().join("\n"))
}

fn dirs(ws: &Workspace) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(ws.tasks_dir())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("task.md").is_file())
        .collect();
    v.sort();
    v
}

pub fn list(ws: &Workspace) -> Vec<Task> {
    dirs(ws).into_iter().filter_map(|d| Task::load(d).ok()).collect()
}

pub fn new(ws: &Workspace, t: NewTask) -> Result<Task> {
    if t.title.trim().is_empty() {
        bail!("a task needs a title");
    }
    let status = t.status.unwrap_or_else(|| "proposed".into());
    check_status(&status)?;
    std::fs::create_dir_all(ws.tasks_dir())?;
    let _guard = FileLock::acquire(ws.tasks_dir().join(".lock"))?;
    let id = fsutil::fmt_id(fsutil::next_id(&ws.tasks_dir())?);
    let dir = ws.tasks_dir().join(format!("{id}-{}", fsutil::slug(&t.title)));

    let fields = [
        ("id", id.clone()),
        ("title", t.title.trim().to_string()),
        ("status", status),
        ("owner", t.owner.unwrap_or_default()),
        ("group", t.group.unwrap_or_default()),
        ("tier", t.tier.unwrap_or_else(|| "cheap".into())),
        ("depends_on", list_value(&t.depends_on)),
        ("files", list_value(&t.files)),
        ("budget", t.budget.unwrap_or(ws.config.default_budget).to_string()),
    ];
    let body = format!(
        "## Intent\n{}\n\n## Proposed approach\n{}\n\n## Done when\n{}\n\n## Non-goals\n{}\n",
        t.intent.trim(),
        t.approach.trim(),
        t.done_when.trim(),
        t.non_goals.trim()
    );
    fsutil::write_atomic(&dir.join("task.md"), &Doc::new(&fields, body).render())?;
    std::fs::create_dir_all(dir.join("reports"))?;
    ws.log("task_new", serde_json::json!({ "task": id, "by": ws.caller() }));
    Task::load(dir)
}

/// Changes one header field. This is the only way headers should change,
/// so two writers can't overwrite each other.
pub fn set(ws: &Workspace, id: &str, field: &str, value: &str) -> Result<Task> {
    let task = Task::find(ws, id)?;
    let _guard = FileLock::acquire(task.dir.join(".lock"))?;
    let mut task = Task::load(task.dir)?;
    if field == "id" {
        bail!("ids don't change");
    }
    if field == "status" {
        check_status(value)?;
    }
    let value = if LIST_FIELDS.contains(&field) && !value.starts_with('[') {
        let items: Vec<String> = value.split(',').map(|s| s.trim().to_string()).collect();
        list_value(&items)
    } else {
        value.to_string()
    };
    let old = task.doc.get(field).unwrap_or_default();
    task.doc.set(field, &value);
    fsutil::write_atomic(&task.path(), &task.doc.render())?;
    ws.log(
        "task_set",
        serde_json::json!({ "task": task.id, "field": field, "from": old, "to": value, "by": ws.caller() }),
    );

    // A review verdict feeds the owner's model track record.
    if field == "status"
        && old == "review"
        && (value == "done" || value == "working")
        && let Some(owner) = task.owner()
        && let Ok(cfg) = agent::AgentConfig::load(ws, &owner)
    {
        let passed = value == "done";
        models::record(ws, &cfg, &task.id, passed, &ws.caller())?;
    }
    Ok(task)
}

/// What the task was before it was last blocked, from the event log.
pub fn status_before_block(ws: &Workspace, id: &str) -> String {
    ws.events()
        .iter()
        .rev()
        .find(|e| {
            e["event"] == "task_set" && e["task"] == id && e["field"] == "status" && e["to"] == "blocked"
        })
        .and_then(|e| e["from"].as_str())
        .filter(|s| !s.is_empty() && *s != "blocked")
        .unwrap_or("working")
        .to_string()
}

fn check_status(s: &str) -> Result<()> {
    if !STATUSES.contains(&s) {
        bail!("unknown status `{s}` ({})", STATUSES.join(", "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections() {
        let body = "## Intent\nNo craters.\n\n## Proposed approach\n\n## Done when\nSand.\n";
        assert_eq!(section(body, "Intent").unwrap().trim(), "No craters.");
        assert_eq!(section(body, "Proposed approach").unwrap().trim(), "");
        assert_eq!(section(body, "Done when").unwrap().trim(), "Sand.");
        assert!(section(body, "Non-goals").is_none());
    }
}
