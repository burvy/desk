//! Asks: questions for the human, `asks/<id>.md`. A blocking ask blocks
//! its task until it's answered.

use crate::frontmatter::Doc;
use crate::fsutil::{self, FileLock};
use crate::message::{self, Kind};
use crate::task::{self, Task};
use crate::workspace::Workspace;
use anyhow::{Result, bail};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Ask {
    pub path: PathBuf,
    pub id: String,
    pub doc: Doc,
}

impl Ask {
    pub fn from(&self) -> String {
        self.doc.get("from").unwrap_or_default()
    }
    pub fn task(&self) -> Option<String> {
        self.doc.get("task")
    }
    pub fn blocking(&self) -> bool {
        self.doc.get("blocking").as_deref() == Some("true")
    }
    pub fn is_open(&self) -> bool {
        self.doc.get("status").as_deref() != Some("answered")
    }
    /// The question, without any answer below it.
    pub fn question(&self) -> &str {
        self.doc.body.split("\n## Answer").next().unwrap_or("").trim()
    }
}

pub fn list(ws: &Workspace) -> Vec<Ask> {
    fsutil::files_with_ext(&ws.asks_dir(), "md")
        .into_iter()
        .filter_map(|path| {
            let doc = Doc::parse(&fsutil::read(&path).ok()?).ok()?;
            let id = path.file_stem()?.to_string_lossy().into_owned();
            Some(Ask { path, id, doc })
        })
        .collect()
}

/// Open asks, blocking first.
pub fn open(ws: &Workspace) -> Vec<Ask> {
    let mut v: Vec<Ask> = list(ws).into_iter().filter(Ask::is_open).collect();
    v.sort_by_key(|a| !a.blocking());
    v
}

pub fn find(ws: &Workspace, id: &str) -> Result<Ask> {
    let id = id.parse::<u32>().map(fsutil::fmt_id).unwrap_or_else(|_| id.to_string());
    list(ws).into_iter().find(|a| a.id == id).ok_or_else(|| anyhow::anyhow!("no ask {id}"))
}

pub fn new(ws: &Workspace, from: &str, task: Option<&str>, blocking: bool, question: &str) -> Result<Ask> {
    let task = task.map(|t| Task::find(ws, t)).transpose()?;
    std::fs::create_dir_all(ws.asks_dir())?;
    let path = {
        let _guard = FileLock::acquire(ws.asks_dir().join(".lock"))?;
        let id = fsutil::fmt_id(fsutil::next_id(&ws.asks_dir())?);
        let mut fields = vec![("from", from.to_string())];
        if let Some(t) = &task {
            fields.push(("task", t.id.clone()));
        }
        fields.push(("blocking", blocking.to_string()));
        fields.push(("status", "open".into()));
        let path = ws.asks_dir().join(format!("{id}.md"));
        fsutil::write_atomic(&path, &Doc::new(&fields, format!("{}\n", question.trim())).render())?;
        path
    };
    // Already blocked: leave it, so unblocking can still find the status before.
    if let (true, Some(t)) = (blocking, &task)
        && t.status() != "blocked"
    {
        task::set(ws, &t.id, "status", "blocked")?;
    }
    let ask = find(ws, &path.file_stem().unwrap_or_default().to_string_lossy())?;
    ws.log(
        "ask",
        serde_json::json!({ "ask": ask.id, "from": from, "task": ask.task(), "blocking": blocking }),
    );
    Ok(ask)
}

/// Writes the answer into the ask, messages the asker, and unblocks the task
/// if nothing else is blocking it.
pub fn answer(ws: &Workspace, id: &str, text: &str) -> Result<Ask> {
    let mut ask = find(ws, id)?;
    if !ask.is_open() {
        bail!("ask {} is already answered", ask.id);
    }
    let by = ws.caller();
    ask.doc.set("status", "answered");
    ask.doc.body = format!("{}\n\n## Answer ({by})\n{}\n", ask.question(), text.trim());
    fsutil::write_atomic(&ask.path, &ask.doc.render())?;
    ws.log("answer", serde_json::json!({ "ask": ask.id, "by": by }));

    let task = ask.task();
    if let Some(t) = &task {
        let still_blocked = open(ws).iter().any(|a| a.blocking() && a.task().as_ref() == Some(t));
        if !still_blocked && Task::find(ws, t)?.status() == "blocked" {
            task::set(ws, t, "status", &task::status_before_block(ws, t))?;
        }
    }
    // Asks raised by desk itself (budget) go back to the task's owner.
    let asker = match ask.from() {
        a if ws.participants().contains(&a) => Some(a),
        _ => task.as_ref().and_then(|t| Task::find(ws, t).ok()?.owner()),
    };
    if let Some(asker) = asker.filter(|a| *a != by) {
        let body = format!(
            "Answer to ask {}:\n\n> {}\n\n{}",
            ask.id,
            ask.question().replace('\n', "\n> "),
            text.trim()
        );
        message::send(ws, &by, &asker, task.as_deref(), Kind::Note, &body)?;
    }
    Ok(ask)
}
