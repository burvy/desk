//! `models/<cli>-<model>.md`: the orchestrator's notes on a model, plus a
//! track record appended automatically from review verdicts.

use crate::agent::AgentConfig;
use crate::fsutil;
use crate::workspace::Workspace;
use anyhow::Result;
use std::path::PathBuf;

pub fn path(ws: &Workspace, cfg: &AgentConfig) -> PathBuf {
    ws.models_dir().join(format!("{}.md", cfg.model_key()))
}

/// Creates the model's file if it's new, so notes and track record share one file.
pub fn ensure(ws: &Workspace, cfg: &AgentConfig) -> Result<PathBuf> {
    let path = path(ws, cfg);
    let template = format!(
        "# {}/{}\n\n## Notes\nStrengths, weaknesses, what to give it.\n\n## Track record\n",
        cfg.cli, cfg.model
    );
    fsutil::create_new(&path, &template)?;
    Ok(path)
}

pub fn record(ws: &Workspace, cfg: &AgentConfig, task: &str, passed: bool, reviewer: &str) -> Result<()> {
    let path = ensure(ws, cfg)?;
    let verdict = if passed { "passed" } else { "sent back" };
    let date = chrono::Local::now().format("%Y-%m-%d");
    fsutil::append_line(&path, &format!("- {date} task {task}: {verdict} (reviewed by {reviewer})"))
}

/// (passed, sent back) counts from the track record.
pub fn tally(ws: &Workspace, cfg: &AgentConfig) -> (usize, usize) {
    let text = std::fs::read_to_string(path(ws, cfg)).unwrap_or_default();
    let passed = text.lines().filter(|l| l.contains(": passed")).count();
    let back = text.lines().filter(|l| l.contains(": sent back")).count();
    (passed, back)
}
