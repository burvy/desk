//! Desktop notifications while desk's window isn't in front: a question or
//! mail for you, an agent stopped by failures or waiting at its CLI's
//! prompt, and everyone going idle after work. A thread watches the
//! workspace, so this works with the window minimized too.

use crate::snapshot::Snapshot;
use desk_core::Workspace;
use eframe::egui;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub struct Notifier {
    stop: Arc<AtomicBool>,
    /// Whether the window is in front; the GUI sets it every frame.
    pub focused: Arc<AtomicBool>,
    /// The bell in the top bar.
    pub enabled: Arc<AtomicBool>,
}

impl Notifier {
    pub fn start(ws: &Workspace, ctx: &egui::Context) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let focused = Arc::new(AtomicBool::new(true));
        let enabled =
            Arc::new(AtomicBool::new(!crate::home::config_file("quiet").is_some_and(|f| f.exists())));
        let (s, f, e, root, ctx) =
            (stop.clone(), focused.clone(), enabled.clone(), ws.root.clone(), ctx.clone());
        std::thread::spawn(move || {
            let mut seen = None;
            while !s.load(Ordering::SeqCst) {
                if let Ok(ws) = Workspace::open(&root) {
                    let snap = Snapshot::read(&ws);
                    let news = match &mut seen {
                        None => {
                            seen = Some(Seen::of(&snap)); // what's already there isn't news
                            Vec::new()
                        }
                        Some(seen) => seen.update(&snap),
                    };
                    if !news.is_empty() && e.load(Ordering::SeqCst) && !f.load(Ordering::SeqCst) {
                        show(&ws, &news);
                        ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
                            egui::UserAttentionType::Informational,
                        ));
                    }
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        });
        Notifier { stop, focused, enabled }
    }

    /// Flips the bell, remembering it for next time.
    pub fn toggle(&self) {
        let on = !self.enabled.load(Ordering::SeqCst);
        self.enabled.store(on, Ordering::SeqCst);
        if let Some(f) = crate::home::config_file("quiet") {
            let _ = if on { std::fs::remove_file(f) } else { std::fs::write(f, "") };
        }
    }
}

impl Drop for Notifier {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// What has already been notified.
#[derive(Default)]
struct Seen {
    asks: HashSet<String>,
    inbox: HashSet<PathBuf>,
    /// Agents in error or waiting at a prompt.
    stuck: HashSet<String>,
    busy: bool,
}

impl Seen {
    fn of(snap: &Snapshot) -> Self {
        let mut s = Seen::default();
        s.update(snap);
        s
    }

    /// The news since last time, one line each.
    fn update(&mut self, snap: &Snapshot) -> Vec<String> {
        let mut news = Vec::new();
        for a in &snap.asks {
            if self.asks.insert(a.id.clone()) {
                let blocking = if a.blocking() { " (blocking)" } else { "" };
                news.push(format!("{} asks{blocking}: {}", a.from(), first_line(a.question())));
            }
        }
        for m in &snap.inbox {
            if self.inbox.insert(m.path.clone()) {
                news.push(format!("{}: {}", m.from, first_line(&m.body)));
            }
        }
        let mut stuck = HashSet::new();
        for a in &snap.agents {
            let why = match a.status() {
                "error" => "stopped after repeated failures",
                "waiting" => "is waiting at its CLI; it may be asking something",
                _ => continue,
            };
            if !self.stuck.contains(&a.name) {
                news.push(format!("{} {why}", a.name));
            }
            stuck.insert(a.name.clone());
        }
        self.stuck = stuck;
        let busy = snap.agents.iter().any(|a| a.status() == "running");
        if self.busy && !busy {
            news.push("Everyone's idle".into());
        }
        self.busy = busy;
        news
    }
}

fn first_line(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let line = line.trim_start_matches('#').trim();
    match line.char_indices().nth(120) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None => line.to_string(),
    }
}

fn show(ws: &Workspace, news: &[String]) {
    let name = ws.root.file_name().unwrap_or_default().to_string_lossy();
    let mut body = news.iter().take(4).cloned().collect::<Vec<_>>().join("\n");
    if news.len() > 4 {
        body.push_str(&format!("\n…and {} more", news.len() - 4));
    }
    let _ = notify_rust::Notification::new()
        .summary(&format!("desk · {name}"))
        .body(&body)
        .sound_name("Default")
        .show();
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_core::{agent::AgentState, ask, message};

    #[test]
    fn only_new_things_are_news() {
        let dir = std::env::temp_dir().join(format!("desk-notify-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ws = Workspace::init(&dir, "me").unwrap();
        std::fs::create_dir_all(ws.inbox("bot").join("read")).unwrap();
        let cfg = "cli = \"opencode\"\nmodel = \"\"\ndir = \".\"\nrole = \"worker\"\n";
        std::fs::write(ws.agent_dir("bot").join("agent.toml"), cfg).unwrap();
        let set = |state: &str| {
            AgentState { state: state.into(), ..AgentState::default() }.save(&ws, "bot").unwrap()
        };
        set("running");
        message::send(&ws, "bot", "me", None, message::Kind::Note, "old news").unwrap();

        let mut seen = Seen::of(&Snapshot::read(&ws));
        assert!(seen.update(&Snapshot::read(&ws)).is_empty(), "what was there at the start isn't news");

        ask::new(&ws, "bot", None, true, "## Which DB?\n\nSQLite or Postgres").unwrap();
        message::send(&ws, "bot", "me", None, message::Kind::Note, "done with 004").unwrap();
        set("idle");
        assert_eq!(
            seen.update(&Snapshot::read(&ws)),
            ["bot asks (blocking): Which DB?", "bot: done with 004", "Everyone's idle"]
        );
        set("waiting");
        assert_eq!(
            seen.update(&Snapshot::read(&ws)),
            ["bot is waiting at its CLI; it may be asking something"]
        );
        assert!(seen.update(&Snapshot::read(&ws)).is_empty(), "said once, not every poll");
        let _ = std::fs::remove_dir_all(&ws.root);
    }
}
