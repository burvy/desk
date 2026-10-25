//! Transcripts: `log/transcripts/<agent>/<session>.jsonl`, the CLI's raw
//! stream plus desk's own `{"desk": ...}` lines (the prompt it delivered).

use crate::adapter::{Event, Preset, clip};
use crate::workspace::Workspace;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    /// What desk sent the agent at the start of a run.
    Prompt {
        ts: String,
        text: String,
    },
    Event(Event),
}

/// The agent's most recently written transcript.
pub fn latest(ws: &Workspace, agent: &str) -> Option<PathBuf> {
    std::fs::read_dir(ws.transcripts_dir(agent))
        .ok()?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .map(|e| e.path())
}

pub fn entries(preset: &Preset, path: &Path) -> Vec<Entry> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut out = Vec::new();
    for line in text.lines() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line)
            && v["desk"] == "prompt"
        {
            out.push(Entry::Prompt {
                ts: v["ts"].as_str().unwrap_or("").into(),
                text: v["text"].as_str().unwrap_or("").into(),
            });
            continue;
        }
        out.extend(
            preset.parse_line(line).into_iter().filter(|e| !matches!(e, Event::Session(_))).map(Entry::Event),
        );
    }
    out
}

/// One readable line per entry, for `desk peek`.
pub fn render(e: &Entry) -> String {
    match e {
        Entry::Prompt { ts, text } => {
            // Skip the role prompt OpenCode sessions start with; it's the same every time.
            let text = text.find("[desk]").map_or(text.as_str(), |i| &text[i..]);
            format!("── prompt {ts} ──\n{}", text.trim_end())
        }
        Entry::Event(Event::Text(t)) => t.trim().to_string(),
        Entry::Event(Event::Tool { name, detail }) => format!("  ⚙ {name}: {detail}"),
        Entry::Event(Event::ToolResult { ok, detail }) => {
            format!("    {} {}", if *ok { "→" } else { "✗" }, clip(detail, 120))
        }
        Entry::Event(Event::Usage { context }) => format!("  ({context} tokens in context)"),
        Entry::Event(Event::Error(e)) => format!("✗ error: {e}"),
        Entry::Event(Event::Session(s)) => format!("(session {s})"),
    }
}
