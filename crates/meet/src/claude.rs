//! Headless Claude Code (`claude -p`). Two shapes: a one-shot structured answer (the
//! summarizer) and a long-running agent whose stream is logged and relayed (the runner).
//! Both drop `CLAUDECODE` from the environment so `meet` also works when launched from
//! inside a Claude Code session, like the bundled summary hook does.
//!
//! The agent keeps its session on disk (no `--no-session-persistence`), so a run that
//! `meet` took down with it — or that the user stopped — is picked up later with
//! `claude --resume <session id>` in the same worktree, conversation intact.

use crate::stream_json::{parse_line, StreamItem};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, watch};

/// How long a stopped agent gets to exit on SIGTERM before its process group is SIGKILLed.
const STOP_GRACE: Duration = Duration::from_secs(4);

fn command(bin: &str, cwd: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.current_dir(cwd)
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd
}

/// A one-shot `claude -p` answering with JSON that matches `schema`. `tools` is the
/// built-in tool allowlist (empty: no tools). The prompt goes in on stdin so its size and
/// quoting never matter.
pub struct Structured<'a> {
    pub bin: &'a str,
    pub cwd: &'a Path,
    pub model: Option<&'a str>,
    /// `--effort`: how hard the model thinks (`low` for the live lookups).
    pub effort: Option<&'a str>,
    pub system_prompt: &'a str,
    pub prompt: &'a str,
    pub schema: &'a str,
    pub tools: &'a [&'a str],
    pub max_budget_usd: f64,
}

pub async fn structured(req: Structured<'_>) -> Result<Value> {
    let mut cmd = command(req.bin, req.cwd);
    cmd.args([
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
        &format!("{:.2}", req.max_budget_usd),
    ]);
    if req.tools.is_empty() {
        cmd.args(["--tools", ""]);
    } else {
        let list = req.tools.join(",");
        cmd.args(["--tools", &list]);
        cmd.arg("--allowedTools");
        cmd.args(req.tools);
    }
    if let Some(m) = req.model {
        cmd.args(["--model", m]);
    }
    if let Some(e) = req.effort {
        cmd.args(["--effort", e]);
    }
    let mut child = cmd.spawn().with_context(|| format!("start {}", req.bin))?;
    let mut stdin = child.stdin.take().context("claude stdin")?;
    let prompt = req.prompt.to_string();
    tokio::spawn(async move {
        let _ = stdin.write_all(prompt.as_bytes()).await;
        let _ = stdin.shutdown().await;
    });
    let output = child.wait_with_output().await.context("wait for claude")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() && stdout.trim().is_empty() {
        bail!("claude exited {}: {}", output.status, stderr.trim());
    }
    parse_structured(&stdout).with_context(|| format!("claude output: {}", excerpt(&stdout, 300)))
}

/// `structured_output` when the schema was honoured; otherwise the JSON object inside the
/// `result` text (some runs answer in prose with a fenced block).
pub fn parse_structured(stdout: &str) -> Result<Value> {
    // A rate-limit notice or a stray log line can precede the JSON envelope.
    let envelope: Value = stdout
        .lines()
        .rev()
        .find_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .ok_or_else(|| anyhow!("no JSON envelope in claude's output"))?;
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

/// What a running agent reports back.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    /// The stream named its Claude session — what a later `--resume` takes.
    Session(String),
    /// A one-line description of what it is doing now.
    Activity(String),
    /// The process ended. `text` is the final assistant message.
    Ended {
        is_error: bool,
        /// The user (or a quit) stopped it; not the agent's own failure.
        stopped: bool,
        /// Whether the stream ever announced a session. A `--resume` that dies before
        /// this is one whose session Claude no longer has.
        saw_init: bool,
        text: String,
        cost_usd: Option<f64>,
        exit_code: Option<i32>,
    },
}

/// How to start the agent.
pub struct AgentSpawn<'a> {
    pub bin: &'a str,
    pub cwd: &'a Path,
    pub prompt: String,
    pub model: Option<&'a str>,
    /// `<run_dir>/stream.jsonl` (raw) and `<run_dir>/agent.log` (readable) are appended to.
    pub run_dir: &'a Path,
    /// `claude --resume <id>`: carry an earlier conversation on instead of starting one.
    pub resume: Option<&'a str>,
    /// A settings file for `--settings` (the on-done Stop hook lives there).
    pub settings: Option<&'a Path>,
    /// Extra environment for the agent and everything it runs, hooks included.
    pub env: Vec<(String, String)>,
}

/// A full-permission `claude -p` agent in `spec.cwd`. Its stream is logged under the run
/// dir and each step is relayed on `tx`. Flip `stop` to `true` to terminate it: SIGTERM
/// to its process group, then SIGKILL after [`STOP_GRACE`]. The returned task resolves
/// when the process is gone.
pub fn spawn_agent(
    spec: AgentSpawn<'_>,
    mut stop: watch::Receiver<bool>,
    tx: mpsc::UnboundedSender<AgentEvent>,
) -> Result<tokio::task::JoinHandle<()>> {
    use std::io::Write;
    std::fs::create_dir_all(spec.run_dir)
        .with_context(|| format!("create {}", spec.run_dir.display()))?;
    let mut cmd = command(spec.bin, spec.cwd);
    cmd.args([
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",
        "--permission-mode",
        "bypassPermissions",
        "--dangerously-skip-permissions",
    ]);
    if let Some(sid) = spec.resume {
        cmd.args(["--resume", sid]);
    }
    if let Some(m) = spec.model {
        cmd.args(["--model", m]);
    }
    if let Some(s) = spec.settings {
        cmd.arg("--settings");
        cmd.arg(s);
    }
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    // Its own process group, so a stop takes the tools it is running down with it.
    cmd.process_group(0);
    let mut child = cmd.spawn().with_context(|| format!("start {}", spec.bin))?;
    let pid = child.id();
    let mut stdin = child.stdin.take().context("claude stdin")?;
    let stdout = child.stdout.take().context("claude stdout")?;
    let stderr = child.stderr.take().context("claude stderr")?;
    let open = |name: &str| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(spec.run_dir.join(name))
    };
    let mut raw = open("stream.jsonl")?;
    let mut log = open("agent.log")?;
    if let Some(sid) = spec.resume {
        let _ = writeln!(log, "── resuming session {sid}");
    }

    let prompt = spec.prompt;
    tokio::spawn(async move {
        let _ = stdin.write_all(prompt.as_bytes()).await;
        let _ = stdin.shutdown().await;
    });
    let stderr_lines = tokio::spawn(async move {
        let mut out = String::new();
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            out.push_str(&line);
            out.push('\n');
        }
        out
    });
    Ok(tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        let mut ended: Option<(bool, String, Option<f64>)> = None;
        let mut saw_init = false;
        let mut stopped = false;
        // Once the sender is gone or the stop is done there is nothing left to watch.
        let mut watch_stop = true;
        loop {
            let line = tokio::select! {
                line = lines.next_line() => match line {
                    Ok(Some(l)) => l,
                    _ => break,
                },
                changed = stop.changed(), if watch_stop => {
                    match changed {
                        Ok(()) if *stop.borrow() => {
                            stopped = true;
                            watch_stop = false;
                            let _ = writeln!(log, "── stopped by the user");
                            terminate(&mut child, pid).await;
                        }
                        Ok(()) => {}
                        Err(_) => watch_stop = false,
                    }
                    continue;
                }
            };
            let _ = writeln!(raw, "{line}");
            let Some(item) = parse_line(&line) else {
                continue;
            };
            let (activity, log_line) = match &item {
                StreamItem::Init { session_id } => {
                    saw_init = true;
                    if let Some(sid) = session_id {
                        let _ = tx.send(AgentEvent::Session(sid.clone()));
                    }
                    (
                        Some("starting".to_string()),
                        format!(
                            "── session started{}",
                            session_id
                                .as_deref()
                                .map(|s| format!(" ({s})"))
                                .unwrap_or_default()
                        ),
                    )
                }
                StreamItem::Text(t) => (
                    Some(crate::stream_json::excerpt(t, 100)),
                    format!("assistant: {t}"),
                ),
                StreamItem::ToolUse { name, detail } => {
                    let line = if detail.is_empty() {
                        name.clone()
                    } else {
                        format!("{name}: {detail}")
                    };
                    (Some(line.clone()), format!("→ {line}"))
                }
                StreamItem::ToolResult { excerpt, is_error } => (
                    None,
                    format!("  {} {excerpt}", if *is_error { "✗" } else { "←" }),
                ),
                StreamItem::Result {
                    is_error,
                    text,
                    cost_usd,
                    duration_ms,
                } => {
                    ended = Some((*is_error, text.clone(), *cost_usd));
                    let cost = cost_usd.map(|c| format!(" · ${c:.2}")).unwrap_or_default();
                    let secs = duration_ms
                        .map(|d| format!(" · {}s", d / 1000))
                        .unwrap_or_default();
                    (
                        None,
                        format!(
                            "── {}{cost}{secs}\n{text}",
                            if *is_error {
                                "ended with an error"
                            } else {
                                "finished"
                            }
                        ),
                    )
                }
                StreamItem::Other => (None, String::new()),
            };
            if !log_line.is_empty() {
                let _ = writeln!(log, "{log_line}");
            }
            if let Some(a) = activity {
                let _ = tx.send(AgentEvent::Activity(a));
            }
        }
        let exit_code = child.wait().await.ok().and_then(|s| s.code());
        let stderr = stderr_lines.await.unwrap_or_default();
        if !stderr.trim().is_empty() {
            let _ = writeln!(log, "── stderr\n{}", stderr.trim());
        }
        let (is_error, text, cost_usd) = match ended {
            Some(e) => e,
            None if stopped => (true, "stopped by the user".to_string(), None),
            None => (
                true,
                if stderr.trim().is_empty() {
                    format!("claude exited with {exit_code:?} before reporting a result")
                } else {
                    crate::stream_json::excerpt(stderr.trim(), 300)
                },
                None,
            ),
        };
        let _ = tx.send(AgentEvent::Ended {
            is_error: is_error || exit_code.is_some_and(|c| c != 0),
            stopped,
            saw_init,
            text,
            cost_usd,
            exit_code,
        });
    }))
}

/// SIGTERM the agent's process group, then SIGKILL it if it is still there after the grace.
async fn terminate(child: &mut tokio::process::Child, pid: Option<u32>) {
    let Some(pid) = pid else {
        let _ = child.kill().await;
        return;
    };
    // Negative pid: the whole group (the agent and the tools it is running).
    unsafe {
        libc::kill(-(pid as i32), libc::SIGTERM);
    }
    if tokio::time::timeout(STOP_GRACE, child.wait()).await.is_err() {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
        let _ = child.kill().await;
    }
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

    /// A stand-in for `claude` that prints an init line, then sleeps: stopping it must
    /// end the task with `stopped`, and the session id must have been relayed first.
    #[tokio::test]
    async fn a_stopped_agent_reports_stopped_after_relaying_its_session() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("claude");
        std::fs::write(
            &fake,
            "#!/bin/sh\ncat >/dev/null &\necho '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"sid-9\"}'\nsleep 30\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (stop_tx, stop_rx) = watch::channel(false);
        let run_dir = dir.path().join("run");
        let task = spawn_agent(
            AgentSpawn {
                bin: fake.to_str().unwrap(),
                cwd: dir.path(),
                prompt: "hi".into(),
                model: None,
                run_dir: &run_dir,
                resume: Some("sid-9"),
                settings: None,
                env: vec![("MEET_TEST".into(), "1".into())],
            },
            stop_rx,
            tx,
        )
        .unwrap();
        assert_eq!(
            rx.recv().await,
            Some(AgentEvent::Session("sid-9".into()))
        );
        assert_eq!(rx.recv().await, Some(AgentEvent::Activity("starting".into())));
        stop_tx.send(true).unwrap();
        let ended = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("the agent ends within the grace");
        match ended {
            Some(AgentEvent::Ended {
                stopped, saw_init, ..
            }) => {
                assert!(stopped);
                assert!(saw_init);
            }
            other => panic!("{other:?}"),
        }
        let _ = task.await;
        let log = std::fs::read_to_string(run_dir.join("agent.log")).unwrap();
        assert!(log.contains("resuming session sid-9"), "{log}");
        assert!(log.contains("stopped by the user"), "{log}");
    }
}
