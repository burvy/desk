//! Claude Code's `--output-format stream-json`.

use super::{Event, clip, summarize};
use serde_json::Value;

pub fn parse(line: &str) -> Vec<Event> {
    let Ok(v) = serde_json::from_str::<Value>(line) else { return Vec::new() };
    let mut out = Vec::new();
    match v["type"].as_str() {
        Some("system") if v["subtype"] == "init" => {
            if let Some(s) = v["session_id"].as_str() {
                out.push(Event::Session(s.into()));
            }
        }
        // `/compact` (or Claude's own auto-compaction) shrank the context.
        Some("system") if v["subtype"] == "compact_boundary" => {
            let m = &v["compact_metadata"];
            if let (Some(pre), Some(post)) = (m["pre_tokens"].as_u64(), m["post_tokens"].as_u64()) {
                out.push(Event::Text(format!("(compacted: {pre} → {post} tokens)")));
                out.push(Event::Usage { context: post });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stream() {
        let init = r#"{"type":"system","subtype":"init","session_id":"abc"}"#;
        assert_eq!(parse(init), [Event::Session("abc".into())]);

        let asst = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"},{"type":"tool_use","name":"Bash","input":{"command":"ls"}}],"usage":{"input_tokens":10,"cache_read_input_tokens":90}}}"#;
        assert_eq!(
            parse(asst),
            [
                Event::Text("hi".into()),
                Event::Tool { name: "Bash".into(), detail: "ls".into() },
                Event::Usage { context: 100 }
            ]
        );

        let compact = r#"{"type":"system","subtype":"compact_boundary","compact_metadata":{"trigger":"manual","pre_tokens":34644,"post_tokens":8311}}"#;
        assert_eq!(
            parse(compact),
            [Event::Text("(compacted: 34644 → 8311 tokens)".into()), Event::Usage { context: 8311 }]
        );

        let err = r#"{"type":"result","is_error":true,"result":"Credit balance is too low"}"#;
        assert_eq!(parse(err), [Event::Error("Credit balance is too low".into())]);
    }
}
