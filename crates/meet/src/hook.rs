//! The on-done step: one more instruction handed to the agent when it is about to finish
//! — "post a summary to the GitHub issue", "append what you did to CHANGES.md" — through a
//! Claude Code `Stop` hook. The runner writes the expanded prompt to `<run_dir>/on-done.md`
//! and a settings file naming this binary as the hook (`meet hook stop`, passed with
//! `claude --settings`, so nothing is written into the worktree); Claude runs the hook
//! when it wants to stop, the hook answers `{"decision":"block","reason":<prompt>}` exactly
//! once per run (a marker file remembers), and Claude carries on with the prompt as its
//! next instruction. The command is env-guarded on `MEET_RUN_DIR`, so a `claude` started
//! by hand with the same settings file does nothing.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};

pub const RUN_DIR_ENV: &str = "MEET_RUN_DIR";
pub const ON_DONE_FILE: &str = "on-done.md";
pub const DELIVERED_FILE: &str = "on-done.delivered";
pub const SETTINGS_FILE: &str = "settings.json";
/// Seconds Claude gives the hook; it only reads two small files.
const HOOK_TIMEOUT_SECS: u32 = 15;

/// The settings JSON `claude --settings` loads: one `Stop` hook running this binary.
pub fn settings_json(exe: &Path) -> Value {
    json!({
        "hooks": {
            "Stop": [{
                "hooks": [{
                    "type": "command",
                    "command": stop_hook_command(exe),
                    "timeout": HOOK_TIMEOUT_SECS,
                }]
            }]
        }
    })
}

/// The shell one-liner in the settings file. Inert without `MEET_RUN_DIR`.
pub fn stop_hook_command(exe: &Path) -> String {
    format!(
        "if [ -z \"${RUN_DIR_ENV}\" ]; then exit 0; fi; exec {} hook stop",
        shell_quote(&exe.to_string_lossy())
    )
}

pub(crate) fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-+".contains(&b))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Write the settings file for a run; returns its path.
pub fn write_settings(run_dir: &Path, exe: &Path) -> Result<PathBuf> {
    let path = run_dir.join(SETTINGS_FILE);
    std::fs::write(&path, serde_json::to_string_pretty(&settings_json(exe))?)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

/// What the hook answers for this Stop, given the run dir and the hook's stdin payload:
/// the on-done prompt the first time the agent tries to stop, nothing after that (or when
/// no prompt was configured, or when Claude says a Stop hook already blocked this turn).
/// Delivering writes the marker, so the decision is made once per run, resumes included.
pub fn decide(run_dir: &Path, payload: &Value) -> Result<Option<String>> {
    if payload.get("stop_hook_active").and_then(Value::as_bool) == Some(true) {
        return Ok(None);
    }
    let marker = run_dir.join(DELIVERED_FILE);
    if marker.exists() {
        return Ok(None);
    }
    let prompt = match std::fs::read_to_string(run_dir.join(ON_DONE_FILE)) {
        Ok(p) if !p.trim().is_empty() => p.trim().to_string(),
        _ => return Ok(None),
    };
    std::fs::write(&marker, crate::store::now().to_string())
        .with_context(|| format!("write {}", marker.display()))?;
    Ok(Some(prompt))
}

/// The hook's stdout when it blocks the stop.
pub fn block_response(reason: &str) -> String {
    json!({ "decision": "block", "reason": reason }).to_string()
}

/// `meet hook stop`: read the payload, decide, answer. Never fails loudly — a broken hook
/// must not fault the agent's turn — but notes what happened in the run's log.
pub fn run_stop_hook() -> Result<()> {
    let Some(run_dir) = std::env::var_os(RUN_DIR_ENV).filter(|v| !v.is_empty()) else {
        return Ok(());
    };
    let run_dir = PathBuf::from(run_dir);
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let payload: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    match decide(&run_dir, &payload) {
        Ok(Some(prompt)) => {
            log_line(&run_dir, "── on-done prompt delivered; the agent continues with it");
            println!("{}", block_response(&prompt));
        }
        Ok(None) => {}
        Err(e) => log_line(&run_dir, &format!("⚠ on-done hook: {e:#}")),
    }
    Ok(())
}

fn log_line(run_dir: &Path, line: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(run_dir.join("agent.log"))
    {
        let _ = writeln!(f, "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_settings_name_this_binary_behind_an_env_guard() {
        let v = settings_json(Path::new("/opt/bin/meet"));
        let cmd = v["hooks"]["Stop"][0]["hooks"][0]["command"].as_str().unwrap();
        assert_eq!(
            cmd,
            "if [ -z \"$MEET_RUN_DIR\" ]; then exit 0; fi; exec /opt/bin/meet hook stop"
        );
        assert_eq!(v["hooks"]["Stop"][0]["hooks"][0]["type"], "command");
        let quoted = stop_hook_command(Path::new("/Users/a b/it's/meet"));
        assert!(quoted.ends_with("exec '/Users/a b/it'\\''s/meet' hook stop"), "{quoted}");
    }

    #[test]
    fn the_prompt_is_delivered_once_and_never_while_a_stop_hook_is_active() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path();
        let payload = json!({ "hook_event_name": "Stop", "stop_hook_active": false });
        assert_eq!(decide(run, &payload).unwrap(), None, "nothing configured");

        std::fs::write(run.join(ON_DONE_FILE), "Post a summary to issue #12.\n").unwrap();
        assert_eq!(
            decide(run, &json!({ "stop_hook_active": true })).unwrap(),
            None,
            "another hook already blocked this turn"
        );
        assert!(!run.join(DELIVERED_FILE).exists());
        assert_eq!(
            decide(run, &payload).unwrap().as_deref(),
            Some("Post a summary to issue #12.")
        );
        assert!(run.join(DELIVERED_FILE).exists());
        assert_eq!(decide(run, &payload).unwrap(), None, "second Stop: let it stop");
        assert_eq!(
            decide(run, &Value::Null).unwrap(),
            None,
            "a resumed run does not deliver again"
        );
        let resp: Value = serde_json::from_str(&block_response("do x")).unwrap();
        assert_eq!(resp["decision"], "block");
        assert_eq!(resp["reason"], "do x");
    }
}
