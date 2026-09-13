//! The shapes `claude -p --output-format stream-json --verbose` prints, reduced to what the
//! TUI shows (what the agent is doing right now) and what the runner needs (how it ended).

use serde_json::Value;

/// Tokens a call read and wrote, and what claude said it cost. Adds up across calls.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Usage {
    /// Fresh input tokens.
    pub input: u64,
    /// Input tokens served from the prompt cache.
    pub cache_read: u64,
    /// Input tokens written to the prompt cache.
    pub cache_write: u64,
    pub output: u64,
    /// `total_cost_usd`; only the final `result` carries one.
    pub cost_usd: f64,
}

impl Usage {
    /// The `usage` object claude prints on `assistant` messages and on the `result`.
    pub fn from_value(v: &Value) -> Usage {
        let n = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
        Usage {
            input: n("input_tokens"),
            cache_read: n("cache_read_input_tokens"),
            cache_write: n("cache_creation_input_tokens"),
            output: n("output_tokens"),
            cost_usd: 0.0,
        }
    }

    /// Everything the model read: fresh, cached and cache-written input.
    pub fn input_total(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }

    pub fn is_zero(&self) -> bool {
        self.input_total() == 0 && self.output == 0 && self.cost_usd == 0.0
    }
}

impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, o: Usage) {
        self.input += o.input;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
        self.output += o.output;
        self.cost_usd += o.cost_usd;
    }
}

impl std::ops::Add for Usage {
    type Output = Usage;
    fn add(mut self, o: Usage) -> Usage {
        self += o;
        self
    }
}

/// `1234` → `1.2k`, `45678` → `46k`, `1234567` → `1.2M`.
pub fn tokens_short(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=9_999 => format!("{:.1}k", n as f64 / 1_000.0),
        10_000..=999_999 => format!("{}k", n / 1_000),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

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
        /// The whole run's tokens, cost included, when the result carried them.
        usage: Option<Usage>,
    },
    /// Anything else (rate-limit notices, hooks).
    Other,
}

#[cfg(test)]
pub fn parse_line(line: &str) -> Option<StreamItem> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    parse_value(&v)
}

/// An `assistant` line's API turn: the message id and what that turn read and wrote.
/// One turn streams as several `assistant` lines (thinking, text, tool_use) that share
/// the id and the usage, so a tally must count each id once. The `output_tokens` on
/// these is a placeholder (`1`) while the turn streams; only the `result` has the real one.
pub fn turn_usage(v: &Value) -> Option<(String, Usage)> {
    if v.get("type").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let id = v.pointer("/message/id")?.as_str()?.to_string();
    let usage = Usage::from_value(v.pointer("/message/usage")?);
    Some((id, usage))
}

pub fn parse_value(v: &Value) -> Option<StreamItem> {
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
            usage: result_usage(v),
        },
        _ => StreamItem::Other,
    })
}

/// The `usage` of a `result` envelope (`claude -p --output-format json` or the last
/// stream-json line), with its `total_cost_usd` folded in.
pub fn result_usage(v: &Value) -> Option<Usage> {
    let mut u = Usage::from_value(v.get("usage")?);
    u.cost_usd = v.get("total_cost_usd").and_then(Value::as_f64).unwrap_or(0.0);
    Some(u)
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

        let end = r#"{"type":"result","subtype":"success","is_error":false,"result":"done","total_cost_usd":0.15,"duration_ms":4855,"usage":{"input_tokens":18,"cache_creation_input_tokens":26729,"cache_read_input_tokens":26503,"output_tokens":171}}"#;
        assert_eq!(
            parse_line(end),
            Some(StreamItem::Result {
                is_error: false,
                text: "done".into(),
                cost_usd: Some(0.15),
                duration_ms: Some(4855),
                usage: Some(Usage {
                    input: 18,
                    cache_write: 26729,
                    cache_read: 26503,
                    output: 171,
                    cost_usd: 0.15,
                }),
            })
        );
        let bare = r#"{"type":"result","subtype":"success","is_error":false,"result":"done"}"#;
        assert!(matches!(
            parse_line(bare),
            Some(StreamItem::Result { usage: None, .. })
        ));
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
    fn a_turn_is_counted_by_its_message_id_and_usage_adds_up() {
        let v: Value = serde_json::from_str(r#"{"type":"assistant","message":{"id":"msg_1","role":"assistant","content":[{"type":"text","text":"hi"}],"usage":{"input_tokens":10,"cache_creation_input_tokens":26503,"cache_read_input_tokens":0,"output_tokens":1}}}"#).unwrap();
        let (id, u) = turn_usage(&v).unwrap();
        assert_eq!(id, "msg_1");
        assert_eq!(u.input_total(), 26513);
        assert_eq!(u.output, 1);
        let user: Value = serde_json::from_str(r#"{"type":"user","message":{"content":[]}}"#).unwrap();
        assert!(turn_usage(&user).is_none());
        let no_usage: Value =
            serde_json::from_str(r#"{"type":"assistant","message":{"id":"m","content":[]}}"#).unwrap();
        assert!(turn_usage(&no_usage).is_none());

        let mut total = Usage::default();
        assert!(total.is_zero());
        total += u;
        total += Usage {
            input: 5,
            output: 40,
            cost_usd: 0.02,
            ..Default::default()
        };
        assert_eq!(total.input, 15);
        assert_eq!(total.input_total(), 26518);
        assert_eq!(total.output, 41);
        assert_eq!(total.cost_usd, 0.02);
        assert!(!total.is_zero());
        assert_eq!((total + total).output, 82);
    }

    #[test]
    fn tokens_read_short() {
        assert_eq!(tokens_short(0), "0");
        assert_eq!(tokens_short(999), "999");
        assert_eq!(tokens_short(1234), "1.2k");
        assert_eq!(tokens_short(9960), "10.0k");
        assert_eq!(tokens_short(45678), "45k");
        assert_eq!(tokens_short(999_999), "999k");
        assert_eq!(tokens_short(1_234_567), "1.2M");
    }

    #[test]
    fn excerpt_cuts_and_marks() {
        assert_eq!(excerpt("short", 10), "short");
        assert_eq!(excerpt("a very long line indeed", 6), "a very…");
        assert_eq!(excerpt("\n\nfirst\nsecond", 20), "first…");
    }
}
