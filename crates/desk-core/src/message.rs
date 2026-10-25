//! Messages: one file per message in the recipient's `inbox/`, moved to
//! `inbox/read/` once a run that saw it exits cleanly.

use crate::frontmatter::Doc;
use crate::workspace::Workspace;
use crate::{ask, fsutil, task};
use anyhow::{Result, bail};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Note,
    Interrupt,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Note => "note",
            Kind::Interrupt => "interrupt",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Message {
    pub path: PathBuf,
    pub from: String,
    pub to: String,
    pub task: Option<String>,
    pub kind: Kind,
    pub body: String,
}

impl Message {
    pub fn load(path: &Path) -> Result<Self> {
        let doc = Doc::parse(&fsutil::read(path)?)?;
        Ok(Message {
            path: path.to_path_buf(),
            from: doc.get("from").unwrap_or_else(|| "unknown".into()),
            to: doc.get("to").unwrap_or_default(),
            task: doc.get("task"),
            kind: match doc.get("kind").as_deref() {
                Some("interrupt") => Kind::Interrupt,
                _ => Kind::Note,
            },
            body: doc.body,
        })
    }
}

pub fn send(
    ws: &Workspace,
    from: &str,
    to: &str,
    task: Option<&str>,
    kind: Kind,
    body: &str,
) -> Result<PathBuf> {
    let known = ws.participants();
    if !known.iter().any(|n| n == to) {
        bail!("no one named `{to}`. Known: {}", known.join(", "));
    }
    let task = match task {
        Some(id) => Some(task::Task::find(ws, id)?.id),
        None => None,
    };
    if let Some(id) = &task {
        check_budget(ws, from, to, id)?;
    }

    let mut fields = vec![("from", from.to_string()), ("to", to.to_string())];
    if let Some(id) = &task {
        fields.push(("task", id.clone()));
    }
    fields.push(("kind", kind.as_str().to_string()));
    let mut body = body.trim_end().to_string();
    body.push('\n');
    let text = Doc::new(&fields, body).render();

    // Names sort by time, so the inbox reads oldest first.
    let stamp = chrono::Local::now().format("%Y-%m-%dT%H%M%S%3f");
    let inbox = ws.inbox(to);
    let mut n = 1;
    let path = loop {
        let suffix = if n == 1 { String::new() } else { format!("-{n}") };
        let p = inbox.join(format!("{stamp}-{from}{suffix}.md"));
        if fsutil::create_new(&p, &text)? {
            break p;
        }
        n += 1;
    };
    ws.log(
        "sent",
        serde_json::json!({ "from": from, "to": to, "task": task, "kind": kind.as_str(), "file": path }),
    );
    Ok(path)
}

/// Unread messages, oldest first.
pub fn unread(ws: &Workspace, name: &str) -> Vec<Message> {
    fsutil::files_with_ext(&ws.inbox(name), "md").iter().filter_map(|p| Message::load(p).ok()).collect()
}

pub fn mark_read(ws: &Workspace, name: &str, paths: &[PathBuf]) -> Result<()> {
    let read = ws.inbox(name).join("read");
    std::fs::create_dir_all(&read)?;
    for p in paths {
        if let Some(file) = p.file_name() {
            // Already moved (e.g. by a human reading their inbox) is fine.
            let _ = std::fs::rename(p, read.join(file));
        }
    }
    Ok(())
}

/// The loop breaker. Agent-to-agent messages about a task count against its
/// budget; past it, the task is blocked and the human is asked.
fn check_budget(ws: &Workspace, from: &str, to: &str, id: &str) -> Result<()> {
    let human = &ws.config.human;
    if from == human || to == human {
        return Ok(());
    }
    let t = task::Task::find(ws, id)?;
    let budget = t.budget(ws);
    let used = ws
        .events()
        .iter()
        .filter(|e| e["event"] == "sent" && e["task"] == id && e["from"] != *human && e["to"] != *human)
        .count() as u32;
    if used < budget {
        return Ok(());
    }
    if t.status() != "blocked" {
        // A blocking ask on a task also blocks the task.
        ask::new(
            ws,
            "desk",
            Some(id),
            true,
            &format!(
                "Task {id} used its message budget ({budget}) between agents. \
                 Is it going in circles? Raise it with `desk task set {id} budget <n>`, \
                 or step in."
            ),
        )?;
    }
    bail!(
        "task {id} is over its message budget ({budget}); the human has been asked. Stop work on it for now."
    )
}
