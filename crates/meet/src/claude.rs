//! Headless Claude Code (`claude -p`): a one-shot structured answer, for the summary
//! writer. It drops `CLAUDECODE` from the environment so `meet` also works
//! when launched from inside a Claude Code session, like the bundled summary hook does, and
//! starts `claude` through the user's login shell (see [`crate::shell`]), so it is the
//! `claude` their terminal would run — rc files, PATH, alias and all — as nebula starts
//! every session.

use crate::stream_json::{result_usage, Usage};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::path::Path;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;

/// `bin args…` in `cwd`, through the login shell, with its stdio piped.
fn command(bin: &str, args: &[String], cwd: &Path) -> tokio::process::Command {
    let (program, args) = crate::shell::claude_launch(bin, args);
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Its own session: the shell must not touch the terminal `meet` draws on.
    crate::shell::own_session(&mut cmd);
    cmd
}

/// A one-shot `claude -p` answering with JSON that matches `schema`. `tools` is the
/// built-in tool allowlist (empty: no tools). The prompt goes in on stdin so its size and
/// quoting never matter.
pub struct Structured<'a> {
    pub bin: &'a str,
    pub cwd: &'a Path,
    pub model: Option<&'a str>,
    /// `--effort`: how hard the model thinks (`None`: claude's own).
    pub effort: Option<&'a str>,
    pub system_prompt: &'a str,
    pub prompt: &'a str,
    pub schema: &'a str,
    pub tools: &'a [&'a str],
    pub max_budget_usd: f64,
}

/// What a one-shot call came back with: the answer, and what it spent. The spend is
/// counted even when the answer is unusable (a schema mismatch, a budget error): the
/// tokens went either way. Zero when claude never printed its envelope.
pub struct Answer {
    pub usage: Usage,
    pub value: Result<Value>,
}

pub async fn structured(req: Structured<'_>) -> Answer {
    let stdout = match run_structured(req).await {
        Ok(s) => s,
        Err(e) => {
            return Answer {
                usage: Usage::default(),
                value: Err(e),
            }
        }
    };
    let env = envelope(&stdout);
    let usage = env
        .as_ref()
        .ok()
        .and_then(result_usage)
        .unwrap_or_default();
    let value = env
        .and_then(|e| structured_of(&e))
        .with_context(|| format!("claude output: {}", excerpt(&stdout, 300)));
    Answer { usage, value }
}

/// Runs the call; claude's stdout, once it exited.
async fn run_structured(req: Structured<'_>) -> Result<String> {
    let budget = format!("{:.2}", req.max_budget_usd);
    let mut args: Vec<String> = [
        "-p",
        "--output-format",
        "json",
        "--no-session-persistence",
        "--permission-mode",
        "dontAsk",
        "--json-schema",
        req.schema,
        "--append-system-prompt",
        req.system_prompt,
        "--max-budget-usd",
        &budget,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if req.tools.is_empty() {
        args.extend(["--tools".to_string(), String::new()]);
    } else {
        args.extend(["--tools".to_string(), req.tools.join(",")]);
        args.push("--allowedTools".into());
        args.extend(req.tools.iter().map(|t| t.to_string()));
    }
    if let Some(m) = req.model {
        args.extend(["--model".to_string(), m.to_string()]);
    }
    if let Some(e) = req.effort {
        args.extend(["--effort".to_string(), e.to_string()]);
    }
    let mut cmd = command(req.bin, &args, req.cwd);
    let mut child = cmd.spawn().with_context(|| format!("start {}", req.bin))?;
    let mut group = crate::shell::GroupKill::new(child.id());
    let mut stdin = child.stdin.take().context("claude stdin")?;
    let prompt = req.prompt.to_string();
    tokio::spawn(async move {
        let _ = stdin.write_all(prompt.as_bytes()).await;
        let _ = stdin.shutdown().await;
    });
    let output = child.wait_with_output().await;
    group.disarm();
    let output = output.context("wait for claude")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() && stdout.trim().is_empty() {
        bail!("claude exited {}: {}", output.status, stderr.trim());
    }
    Ok(stdout.into_owned())
}

/// `structured_output` when the schema was honoured; otherwise the JSON object inside the
/// `result` text (some runs answer in prose with a fenced block).
#[cfg(test)]
pub fn parse_structured(stdout: &str) -> Result<Value> {
    structured_of(&envelope(stdout)?)
}

/// The `result` envelope in claude's stdout. A rate-limit notice or a stray log line can
/// precede it.
fn envelope(stdout: &str) -> Result<Value> {
    stdout
        .lines()
        .rev()
        .find_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .ok_or_else(|| anyhow!("no JSON envelope in claude's output"))
}

fn structured_of(envelope: &Value) -> Result<Value> {
    if envelope.get("is_error").and_then(Value::as_bool) == Some(true) {
        bail!(
            "claude reported an error: {}",
            envelope
                .get("result")
                .and_then(Value::as_str)
                .unwrap_or("(no detail)")
        );
    }
    if let Some(v) = envelope.get("structured_output") {
        if !v.is_null() {
            return Ok(v.clone());
        }
    }
    let text = envelope
        .get("result")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("claude's result carried no text"))?;
    lenient_json(text).ok_or_else(|| anyhow!("no JSON object in claude's answer"))
}

/// The first `{ … }` object in `text`, fenced or not.
pub fn lenient_json(text: &str) -> Option<Value> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str(&text[start..=end]).ok()
}

fn excerpt(s: &str, max: usize) -> String {
    let mut out: String = s.trim().chars().take(max).collect();
    if s.trim().chars().count() > max {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_output_wins_over_result_text() {
        let out = r#"{"type":"result","is_error":false,"result":"{\"a\":1}","structured_output":{"a":2}}"#;
        assert_eq!(parse_structured(out).unwrap()["a"], 2);
        let out = "some notice\n{\"type\":\"result\",\"is_error\":false,\"result\":\"Here you go:\\n```json\\n{\\\"a\\\": 3}\\n```\"}";
        assert_eq!(parse_structured(out).unwrap()["a"], 3);
        let err = r#"{"type":"result","is_error":true,"result":"boom"}"#;
        assert!(parse_structured(err)
            .unwrap_err()
            .to_string()
            .contains("boom"));
        assert!(parse_structured("").is_err());
    }

    /// A stand-in that answers the schema with an envelope carrying `usage`: the answer
    /// and the spend both come back. With `is_error`, the spend still does.
    #[tokio::test]
    async fn a_structured_call_reports_what_it_spent_even_when_the_answer_is_unusable() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("claude");
        std::fs::write(
            &fake,
            "#!/bin/sh\nif [ \"$(cat)\" = err ]; then\necho '{\"type\":\"result\",\"is_error\":true,\"result\":\"over budget\",\"total_cost_usd\":0.5,\"usage\":{\"input_tokens\":7,\"output_tokens\":3}}'\nelse\necho '{\"type\":\"result\",\"is_error\":false,\"result\":\"ok\",\"structured_output\":{\"a\":1},\"total_cost_usd\":0.25,\"usage\":{\"input_tokens\":10,\"cache_creation_input_tokens\":20,\"cache_read_input_tokens\":30,\"output_tokens\":4}}'\nfi\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let req = |prompt: &'static str| Structured {
            bin: fake.to_str().unwrap(),
            cwd: dir.path(),
            model: None,
            effort: None,
            system_prompt: "s",
            prompt,
            schema: "{}",
            tools: &[],
            max_budget_usd: 1.0,
        };
        let ans = structured(req("p")).await;
        assert_eq!(ans.value.unwrap()["a"], 1);
        assert_eq!(
            ans.usage,
            Usage {
                input: 10,
                cache_write: 20,
                cache_read: 30,
                output: 4,
                cost_usd: 0.25
            }
        );
        let ans = structured(req("err")).await;
        assert!(ans.value.unwrap_err().to_string().contains("over budget"));
        assert_eq!(ans.usage.input, 7);
        assert_eq!(ans.usage.cost_usd, 0.5);
        let ans = structured(Structured {
            bin: "/nonexistent/claude",
            ..req("p")
        })
        .await;
        assert!(ans.value.is_err());
        assert!(ans.usage.is_zero());
    }
}
