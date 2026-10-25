//! `---`-fenced `key: value` headers on markdown files.
//!
//! Deliberately not YAML: headers are flat, and edits go line by line so a
//! human's comments and key order survive `desk task set`.

use anyhow::{Result, bail};

#[derive(Debug, Clone, Default)]
pub struct Doc {
    lines: Vec<String>,
    pub body: String,
}

impl Doc {
    pub fn new(fields: &[(&str, String)], body: impl Into<String>) -> Self {
        let lines = fields.iter().map(|(k, v)| format!("{k}: {v}")).collect();
        Doc { lines, body: body.into() }
    }

    pub fn parse(text: &str) -> Result<Self> {
        // Notepad and friends may start the file with a byte-order mark.
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut it = text.lines();
        if it.next().map(str::trim_end) != Some("---") {
            bail!("missing `---` header");
        }
        let mut lines = Vec::new();
        for line in it.by_ref() {
            if line.trim_end() == "---" {
                let body: Vec<&str> = it.collect();
                return Ok(Doc { lines, body: join_body(&body) });
            }
            lines.push(line.to_string());
        }
        bail!("unterminated `---` header")
    }

    pub fn render(&self) -> String {
        let mut out = String::from("---\n");
        for line in &self.lines {
            out.push_str(line);
            out.push('\n');
        }
        out.push_str("---\n");
        out.push_str(&self.body);
        out
    }

    /// The value for `key`, unquoted and without its trailing comment.
    /// Empty values count as absent.
    pub fn get(&self, key: &str) -> Option<String> {
        let i = self.find(key)?;
        let (value, _) = split_value(&self.lines[i], key);
        let v = unquote(value.trim());
        (!v.is_empty()).then(|| v.to_string())
    }

    /// `[a, b]` → `["a", "b"]`. A bare value is a one-item list.
    pub fn list(&self, key: &str) -> Vec<String> {
        let Some(v) = self.get(key) else { return Vec::new() };
        let inner = v.strip_prefix('[').and_then(|s| s.strip_suffix(']')).unwrap_or(&v);
        inner.split(',').map(|s| unquote(s.trim()).to_string()).filter(|s| !s.is_empty()).collect()
    }

    /// Replaces the value in place, keeping any trailing comment, or appends.
    pub fn set(&mut self, key: &str, value: &str) {
        match self.find(key) {
            Some(i) => {
                let (_, comment) = split_value(&self.lines[i], key);
                self.lines[i] = match comment {
                    Some(c) => format!("{key}: {value}  {c}"),
                    None => format!("{key}: {value}"),
                };
            }
            None => self.lines.push(format!("{key}: {value}")),
        }
    }

    pub fn has(&self, key: &str) -> bool {
        self.find(key).is_some()
    }

    /// The header's keys, in order.
    pub fn keys(&self) -> Vec<String> {
        self.lines.iter().filter_map(|l| l.split_once(':').map(|(k, _)| k.trim().to_string())).collect()
    }

    fn find(&self, key: &str) -> Option<usize> {
        self.lines
            .iter()
            .position(|l| l.strip_prefix(key).is_some_and(|rest| rest.trim_start().starts_with(':')))
    }
}

pub fn list_value(items: &[String]) -> String {
    format!("[{}]", items.join(", "))
}

fn join_body(lines: &[&str]) -> String {
    let mut s = lines.join("\n");
    if !lines.is_empty() {
        s.push('\n');
    }
    s
}

/// Splits `key: value  # comment` into the value and the comment.
fn split_value<'a>(line: &'a str, key: &str) -> (&'a str, Option<&'a str>) {
    let rest = line[key.len()..].trim_start();
    let rest = &rest[1..]; // the ':'
    let mut in_quote = false;
    let bytes = rest.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'"' => in_quote = !in_quote,
            b'#' if !in_quote && (i == 0 || bytes[i - 1].is_ascii_whitespace()) => {
                return (&rest[..i], Some(&rest[i..]));
            }
            _ => {}
        }
    }
    (rest, None)
}

fn unquote(s: &str) -> &str {
    s.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TASK: &str = "---\nid: 014\nstatus: open\ngroup: explosions  # related tasks\nfiles: [src/a.rs, src/b.rs]\nowner:\n---\n## Intent\nNo craters.\n";

    #[test]
    fn reads_fields() {
        let d = Doc::parse(TASK).unwrap();
        assert_eq!(d.get("id").as_deref(), Some("014"));
        assert_eq!(d.get("group").as_deref(), Some("explosions"));
        assert_eq!(d.get("owner"), None);
        assert_eq!(d.list("files"), ["src/a.rs", "src/b.rs"]);
        assert_eq!(d.body, "## Intent\nNo craters.\n");
    }

    #[test]
    fn set_keeps_comments_and_order() {
        let mut d = Doc::parse(TASK).unwrap();
        d.set("group", "fire");
        d.set("owner", "rook");
        d.set("budget", "20");
        let out = d.render();
        assert!(out.contains("group: fire  # related tasks\n"));
        assert!(out.contains("owner: rook\nbudget: 20\n---\n"));
        assert_eq!(Doc::parse(&out).unwrap().get("group").as_deref(), Some("fire"));
    }

    #[test]
    fn similar_keys_dont_collide() {
        let d = Doc::parse("---\nto_do: x\nto: rook\n---\n").unwrap();
        assert_eq!(d.get("to").as_deref(), Some("rook"));
    }

    #[test]
    fn byte_order_mark() {
        let d = Doc::parse("\u{feff}---\r\nfrom: me\r\n---\r\nhi\r\n").unwrap();
        assert_eq!(d.get("from").as_deref(), Some("me"));
    }

    #[test]
    fn crlf_and_quotes() {
        let d = Doc::parse("---\r\nfrom: \"a # b\"\r\n---\r\nhi\r\n").unwrap();
        assert_eq!(d.get("from").as_deref(), Some("a # b"));
        assert_eq!(d.body, "hi\n");
    }
}
