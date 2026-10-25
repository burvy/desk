//! OpenCode: `opencode run --format json`. Serves many providers (GLM, free models...).

use super::{Adapter, Event, RunSpec, clip, program, summarize};
use crate::agent::AgentConfig;
use serde_json::Value;
use std::process::Command;

pub struct OpenCode;

impl Adapter for OpenCode {
    fn command(&self, run: &RunSpec) -> Command {
        let mut c = program("opencode");
        c.args(["run", "--format", "json", "--model", &run.cfg.model]);
        if run.cfg.permissions == "bypass" {
            c.arg("--auto");
        }
        if let Some(s) = run.session {
            c.args(["--session", s]);
        }
        c.current_dir(&run.cfg.dir);
        c
    }

    fn system_prompt(&self) -> bool {
        false
    }

    fn interactive(&self, cfg: &AgentConfig, session: Option<&str>) -> Vec<String> {
        let mut v = vec!["opencode".to_string()];
        if cfg.permissions == "bypass" {
            v.push("--auto".into());
        }
        if let Some(s) = session {
            v.extend(["--session".into(), s.into()]);
        }
        v
    }

    fn parse(&self, line: &str) -> Vec<Event> {
        let Ok(v) = serde_json::from_str::<Value>(line) else { return Vec::new() };
        let part = &v["part"];
        let mut out = Vec::new();
        match v["type"].as_str() {
            Some("step_start") => {
                if let Some(s) = v["sessionID"].as_str() {
                    out.push(Event::Session(s.into()));
                }
            }
            Some("text") => out.push(Event::Text(part["text"].as_str().unwrap_or("").into())),
            Some("tool_use") => {
                let state = &part["state"];
                out.push(Event::Tool {
                    name: part["tool"].as_str().unwrap_or("?").into(),
                    detail: summarize(&state["input"]),
                });
                if let Some(status) = state["status"].as_str() {
                    let detail = state["output"].as_str().or(state["error"].as_str()).unwrap_or("");
                    out.push(Event::ToolResult { ok: status == "completed", detail: clip(detail, 160) });
                }
            }
            Some("step_finish") => {
                let t = &part["tokens"];
                let context = [&t["input"], &t["cache"]["read"], &t["cache"]["write"]]
                    .iter()
                    .filter_map(|n| n.as_u64())
                    .sum();
                if context > 0 {
                    out.push(Event::Usage { context });
                }
            }
            Some("error") => {
                let e = &v["error"];
                let msg = e["data"]["message"].as_str().or(e["message"].as_str()).or(e.as_str());
                out.push(Event::Error(msg.map(str::to_string).unwrap_or_else(|| e.to_string())));
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
        let o = OpenCode;
        let start = r#"{"type":"step_start","sessionID":"ses_1","part":{}}"#;
        assert_eq!(o.parse(start), [Event::Session("ses_1".into())]);

        let tool = r#"{"type":"tool_use","part":{"tool":"shell","state":{"status":"completed","input":{"command":"echo hi"},"output":"hi\r\n"}}}"#;
        assert_eq!(
            o.parse(tool),
            [
                Event::Tool { name: "shell".into(), detail: "echo hi".into() },
                Event::ToolResult { ok: true, detail: "hi".into() }
            ]
        );

        let fin = r#"{"type":"step_finish","part":{"tokens":{"input":4076,"output":14,"cache":{"read":4352,"write":0}}}}"#;
        assert_eq!(o.parse(fin), [Event::Usage { context: 8428 }]);
    }
}
