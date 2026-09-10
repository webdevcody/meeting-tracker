//! The shapes `claude -p --output-format stream-json --verbose` prints, reduced to what the
//! TUI shows (what the agent is doing right now) and what the runner needs (how it ended).

use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum StreamItem {
    /// The session started; `session_id` is what `claude --resume` takes later.
    Init { session_id: Option<String> },
    /// The assistant said something (text blocks only).
    Text(String),
    /// The assistant called a tool; `detail` is the command / path / pattern when there is one.
    ToolUse { name: String, detail: String },
    /// A tool answered; a short excerpt.
    ToolResult { excerpt: String, is_error: bool },
    /// The run ended.
    Result {
        is_error: bool,
        text: String,
        cost_usd: Option<f64>,
        duration_ms: Option<u64>,
    },
    /// Anything else (rate-limit notices, hooks).
    Other,
}

pub fn parse_line(line: &str) -> Option<StreamItem> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    let kind = v.get("type")?.as_str()?;
    Some(match kind {
        "system" if v.get("subtype").and_then(Value::as_str) == Some("init") => StreamItem::Init {
            session_id: v
                .get("session_id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        },
        "assistant" => {
            let content = v.pointer("/message/content")?.as_array()?;
            // One line per block would be noise; the tool call is the interesting part.
            if let Some(tool) = content
                .iter()
                .find(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
            {
                let name = tool
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string();
                let input = tool.get("input").cloned().unwrap_or(Value::Null);
                StreamItem::ToolUse {
                    detail: tool_detail(&name, &input),
                    name,
                }
            } else {
                let text: Vec<&str> = content
                    .iter()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|b| b.get("text").and_then(Value::as_str))
                    .collect();
                if text.is_empty() {
                    StreamItem::Other
                } else {
                    StreamItem::Text(text.join("\n"))
                }
            }
        }
        "user" => {
            let content = v.pointer("/message/content")?.as_array()?;
            let result = content
                .iter()
                .find(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))?;
            let is_error = result
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let text = match result.get("content") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(parts)) => parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join(" "),
                _ => String::new(),
            };
            StreamItem::ToolResult {
                excerpt: excerpt(&text, 160),
                is_error,
            }
        }
        "result" => StreamItem::Result {
            is_error: v.get("is_error").and_then(Value::as_bool).unwrap_or(false)
                || v.get("subtype")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.starts_with("error")),
            text: v
                .get("result")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            cost_usd: v.get("total_cost_usd").and_then(Value::as_f64),
            duration_ms: v.get("duration_ms").and_then(Value::as_u64),
        },
        _ => StreamItem::Other,
    })
}

/// The one input field worth showing per tool, on one line.
fn tool_detail(name: &str, input: &Value) -> String {
    let pick = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| input.get(*k).and_then(Value::as_str))
            .map(str::to_string)
    };
    let detail = match name {
        "Bash" => pick(&["command"]),
        "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => {
            pick(&["file_path", "notebook_path"])
        }
        "Glob" | "Grep" => pick(&["pattern"]),
        "Agent" | "Task" => pick(&["description", "prompt"]),
        "WebFetch" => pick(&["url"]),
        _ => pick(&["description", "command", "file_path", "pattern", "query"]),
    };
    excerpt(&detail.unwrap_or_default(), 120)
}

/// First line, at most `max` chars, `…` when cut.
pub fn excerpt(s: &str, max: usize) -> String {
    let first = s
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let mut out: String = first.chars().take(max).collect();
    if first.chars().count() > max || s.lines().filter(|l| !l.trim().is_empty()).count() > 1 {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_shapes_claude_prints() {
        let init =
            r#"{"type":"system","subtype":"init","cwd":"/x","session_id":"s","tools":["Bash"]}"#;
        assert_eq!(
            parse_line(init),
            Some(StreamItem::Init {
                session_id: Some("s".into())
            })
        );

        let tool = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"echo hello","description":"Print hello"}}]}}"#;
        assert_eq!(
            parse_line(tool),
            Some(StreamItem::ToolUse {
                name: "Bash".into(),
                detail: "echo hello".into()
            })
        );

        let result = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"t1","type":"tool_result","content":"hello\nworld","is_error":false}]}}"#;
        assert_eq!(
            parse_line(result),
            Some(StreamItem::ToolResult {
                excerpt: "hello…".into(),
                is_error: false
            })
        );

        let text = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"done"}]}}"#;
        assert_eq!(parse_line(text), Some(StreamItem::Text("done".into())));

        let end = r#"{"type":"result","subtype":"success","is_error":false,"result":"done","total_cost_usd":0.15,"duration_ms":4855}"#;
        assert_eq!(
            parse_line(end),
            Some(StreamItem::Result {
                is_error: false,
                text: "done".into(),
                cost_usd: Some(0.15),
                duration_ms: Some(4855)
            })
        );
        let err = r#"{"type":"result","subtype":"error_max_turns","is_error":false,"result":""}"#;
        assert!(matches!(
            parse_line(err),
            Some(StreamItem::Result { is_error: true, .. })
        ));

        assert_eq!(
            parse_line(r#"{"type":"rate_limit_event"}"#),
            Some(StreamItem::Other)
        );
        assert_eq!(parse_line("not json"), None);
    }

    #[test]
    fn excerpt_cuts_and_marks() {
        assert_eq!(excerpt("short", 10), "short");
        assert_eq!(excerpt("a very long line indeed", 6), "a very…");
        assert_eq!(excerpt("\n\nfirst\nsecond", 20), "first…");
    }
}
