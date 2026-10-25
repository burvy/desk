//! OpenCode's `run --format json`.

use super::{Event, clip, summarize};
use serde_json::Value;

pub fn parse(line: &str) -> Vec<Event> {
    let Ok(v) = serde_json::from_str::<Value>(line) else { return Vec::new() };
    let part = &v["part"];
    let mut out = Vec::new();
    // Every line carries the session id; `step_start` alone isn't always sent.
    if let Some(s) = v["sessionID"].as_str() {
        out.push(Event::Session(s.into()));
    }
    match v["type"].as_str() {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stream() {
        let start = r#"{"type":"step_start","sessionID":"ses_1","part":{}}"#;
        assert_eq!(parse(start), [Event::Session("ses_1".into())]);

        let tool = r#"{"type":"tool_use","part":{"tool":"shell","state":{"status":"completed","input":{"command":"echo hi"},"output":"hi\r\n"}}}"#;
        assert_eq!(
            parse(tool),
            [
                Event::Tool { name: "shell".into(), detail: "echo hi".into() },
                Event::ToolResult { ok: true, detail: "hi".into() }
            ]
        );

        let text = r#"{"type":"text","sessionID":"ses_2","part":{"text":"hi"}}"#;
        assert_eq!(parse(text), [Event::Session("ses_2".into()), Event::Text("hi".into())]);

        let fin = r#"{"type":"step_finish","part":{"tokens":{"input":4076,"output":14,"cache":{"read":4352,"write":0}}}}"#;
        assert_eq!(parse(fin), [Event::Usage { context: 8428 }]);
    }
}
