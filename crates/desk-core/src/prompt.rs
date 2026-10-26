//! The text desk itself writes into agents' prompts. Kept here, in one place,
//! so it's easy to read and tune.

use crate::message::{Kind, Message};
use crate::task::Task;
use crate::workspace::Workspace;
use std::fmt::Write;

pub struct Delivery<'a> {
    pub name: &'a str,
    /// The agent's project folder.
    pub dir: &'a std::path::Path,
    pub new_session: bool,
    /// Role prompt text, for CLIs that can't take a system prompt.
    pub role: Option<&'a str>,
    /// Set for an orchestrator's one-task turn.
    pub task: Option<&'a Task>,
    pub messages: &'a [Message],
}

pub fn delivery(ws: &Workspace, d: &Delivery) -> String {
    let dir = ws.agent_dir(d.name);
    let mut p = String::new();
    if let Some(role) = d.role.filter(|_| d.new_session) {
        p.push_str(role.trim());
        p.push_str("\n\n---\n\n");
    }
    let _ = writeln!(
        p,
        "[desk] To {}. Your project (where the code is, and your cwd): {}\n\
         The desk workspace (tasks, messages, memory; not code): {}\n\
         The human is `{}`; they read their inbox and asks.",
        d.name,
        d.dir.display(),
        ws.root.display(),
        ws.config.human
    );
    // A project spread over several folders: every agent knows them all.
    if !ws.config.projects.is_empty() {
        let list: Vec<String> =
            ws.config.projects.iter().map(|(n, p)| format!("{n} ({})", p.display())).collect();
        let _ = writeln!(
            p,
            "The project's folders, which work together; read and change across them as the work needs: {}.",
            list.join(", ")
        );
    }
    if d.new_session {
        let _ = writeln!(
            p,
            "This is a new session. Before anything else, read your memory ({}) and, if it exists, \
             your handoff note ({}).",
            dir.join("memory.md").display(),
            dir.join("handoff.md").display()
        );
    }
    let n = d.messages.len();
    let _ = writeln!(
        p,
        "{n} new message{}. Delivery is at-least-once: if something looks familiar, check your memory \
         before redoing it. Your text output only reaches your transcript, which the human doesn't read: \
         use `desk send` to reach anyone, and put every question for the human in `desk ask`.",
        if n == 1 { "" } else { "s" }
    );
    if let Some(t) = d.task {
        let _ = writeln!(
            p,
            "This turn is about task {} ({}). Read {} first. Mail about other tasks arrives in its own turn.",
            t.id,
            t.title(),
            t.path().display()
        );
    }
    for (i, m) in d.messages.iter().enumerate() {
        let _ = write!(p, "\n--- message {} · from {}", i + 1, m.from);
        if let Some(t) = &m.task {
            let _ = write!(p, " · task {t}");
        }
        if m.kind == Kind::Interrupt {
            p.push_str(" · INTERRUPT");
        }
        let _ = writeln!(p, " ---\n{}", m.body.trim_end());
    }
    p
}

pub fn handoff(ws: &Workspace, name: &str) -> String {
    let dir = ws.agent_dir(name);
    format!(
        "[desk] Your session is about to be reset. Don't start new work.\n\
         1. Update {} with what you'll need later. Keep it current and concise, not a log.\n\
         2. Write {}: what you were doing, what's next, and open questions.\n\
         A fresh session starts with both.",
        dir.join("memory.md").display(),
        dir.join("handoff.md").display()
    )
}
