//! Claude Code: `claude -p --output-format stream-json`.

use super::{Adapter, Event, RunSpec, clip, program, summarize};
use crate::agent::AgentConfig;
use serde_json::Value;
use std::process::Command;

pub struct Claude;

impl Adapter for Claude {
    fn command(&self, run: &RunSpec) -> Command {
        let mut c = program("claude");
        c.args(["-p", "--output-format", "stream-json", "--verbose", "--model", &run.cfg.model]);
        c.arg("--append-system-prompt-file").arg(run.role_file);
        c.arg("--add-dir").arg(run.workspace);
        if run.cfg.permissions == "bypass" {
            c.args(["--permission-mode", "bypassPermissions"]);
        }
        if let Some(s) = run.session {
            c.args(["--resume", s]);
        }
        c.current_dir(&run.cfg.dir);
        c
    }

    fn system_prompt(&self) -> bool {
        true
    }

    fn interactive(&self, cfg: &AgentConfig, session: Option<&str>) -> Vec<String> {
        let mut v = vec!["claude".to_string(), "--model".into(), cfg.model.clone()];
        if cfg.permissions == "bypass" {
            v.extend(["--permission-mode".into(), "bypassPermissions".into()]);
        }
        if let Some(s) = session {
            v.extend(["--resume".into(), s.into()]);
        }
        v
    }

    fn parse(&self, line: &str) -> Vec<Event> {
        let Ok(v) = serde_json::from_str::<Value>(line) else { return Vec::new() };
        let mut out = Vec::new();
        match v["type"].as_str() {
            Some("system") if v["subtype"] == "init" => {
                if let Some(s) = v["session_id"].as_str() {
                    out.push(Event::Session(s.into()));
                }
            }
            Some("assistant") => {
                let msg = &v["message"];
                for block in msg["content"].as_array().into_iter().flatten() {
                    match block["type"].as_str() {
                        Some("text") => out.push(Event::Text(block["text"].as_str().unwrap_or("").into())),
                        Some("tool_use") => out.push(Event::Tool {
                            name: block["name"].as_str().unwrap_or("?").into(),
                            detail: summarize(&block["input"]),
                        }),
                        _ => {}
                    }
                }
                let u = &msg["usage"];
                let context = ["input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"]
                    .iter()
                    .filter_map(|k| u[k].as_u64())
                    .sum();
                if context > 0 {
                    out.push(Event::Usage { context });
                }
            }
            Some("user") => {
                for block in v["message"]["content"].as_array().into_iter().flatten() {
                    if block["type"] == "tool_result" {
                        let detail = match &block["content"] {
                            Value::String(s) => clip(s, 160),
                            Value::Array(parts) => clip(
                                parts
                                    .iter()
                                    .filter_map(|p| p["text"].as_str())
                                    .collect::<Vec<_>>()
                                    .join(" ")
                                    .as_str(),
                                160,
                            ),
                            _ => String::new(),
                        };
                        out.push(Event::ToolResult { ok: block["is_error"] != true, detail });
                    }
                }
            }
            Some("result") if v["is_error"] == true => {
                out.push(Event::Error(v["result"].as_str().unwrap_or("run failed").into()));
            }
            _ => {}
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stream() {
        let c = Claude;
        let init = r#"{"type":"system","subtype":"init","session_id":"abc"}"#;
        assert_eq!(c.parse(init), [Event::Session("abc".into())]);

        let asst = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"},{"type":"tool_use","name":"Bash","input":{"command":"ls"}}],"usage":{"input_tokens":10,"cache_read_input_tokens":90}}}"#;
        assert_eq!(
            c.parse(asst),
            [
                Event::Text("hi".into()),
                Event::Tool { name: "Bash".into(), detail: "ls".into() },
                Event::Usage { context: 100 }
            ]
        );

        let err = r#"{"type":"result","is_error":true,"result":"Credit balance is too low"}"#;
        assert_eq!(c.parse(err), [Event::Error("Credit balance is too low".into())]);
    }
}
