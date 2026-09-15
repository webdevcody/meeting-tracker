//! What `claude -p --output-format json` reports about a call — the tokens it read and
//! wrote and what it cost — and the one-line excerpts the TUI shows.

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
    /// `total_cost_usd` from the `result` envelope.
    pub cost_usd: f64,
}

impl Usage {
    /// The `usage` object claude prints on the `result`.
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

/// The `usage` of a `result` envelope, with its `total_cost_usd` folded in.
pub fn result_usage(v: &Value) -> Option<Usage> {
    let mut u = Usage::from_value(v.get("usage")?);
    u.cost_usd = v.get("total_cost_usd").and_then(Value::as_f64).unwrap_or(0.0);
    Some(u)
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
    fn the_result_envelope_carries_the_usage_and_usage_adds_up() {
        let end: Value = serde_json::from_str(r#"{"type":"result","subtype":"success","is_error":false,"result":"done","total_cost_usd":0.15,"duration_ms":4855,"usage":{"input_tokens":18,"cache_creation_input_tokens":26729,"cache_read_input_tokens":26503,"output_tokens":171}}"#).unwrap();
        let u = result_usage(&end).unwrap();
        assert_eq!(
            u,
            Usage {
                input: 18,
                cache_write: 26729,
                cache_read: 26503,
                output: 171,
                cost_usd: 0.15,
            }
        );
        let bare: Value =
            serde_json::from_str(r#"{"type":"result","is_error":false,"result":"done"}"#).unwrap();
        assert_eq!(result_usage(&bare), None);

        let mut total = Usage::default();
        assert!(total.is_zero());
        total += u;
        total += Usage {
            input: 5,
            output: 40,
            cost_usd: 0.02,
            ..Default::default()
        };
        assert_eq!(total.input, 23);
        assert_eq!(total.input_total(), 53255);
        assert_eq!(total.output, 211);
        assert!((total.cost_usd - 0.17).abs() < 1e-9);
        assert!(!total.is_zero());
        assert_eq!((total + total).output, 422);
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
