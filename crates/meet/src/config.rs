//! The TUI's share of `meet.json`. The recording engine owns most of that file
//! (`outputDir`, `summary`, `hooks`); the `agent` block is read here, with the engine's
//! lookup order so one file configures both halves:
//!
//! 1. `--config FILE`
//! 2. `$MEET_CONFIG`
//! 3. `meet.json` in the repo or the nearest parent
//! 4. `~/.config/meet/config.json`, then the legacy `~/.config/meeting-tracker/config.json`
//!
//! ```json
//! "agent": {
//!   "model": "opus",
//!   "onDone": "Post a short summary of what you changed as a comment on GitHub issue {{issue}} with `gh issue comment`.",
//!   "onDoneFile": "agent-on-done.md"
//! }
//! ```
//!
//! `onDone` is a string or an array of lines; `onDoneFile` reads it from a file (relative
//! to the config file). Inline text wins over the file. `{{placeholders}}` are filled per
//! run: `title`, `prompt`, `why`, `branch`, `worktree`, `repo`, `repo_name`,
//! `base_branch`, `run_dir`, `item_id`.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub const ENV_VAR: &str = "MEET_CONFIG";
pub const LOCAL_FILE: &str = "meet.json";
const DEFAULT_PATH: &str = "~/.config/meet/config.json";
const LEGACY_PATH: &str = "~/.config/meeting-tracker/config.json";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentConfig {
    /// Model for the implementing agent (`--agent-model` overrides it).
    pub model: Option<String>,
    /// The on-done prompt template, delivered through the Stop hook. `None`: no on-done step.
    pub on_done: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Config {
    /// Where it came from; `None` means built-in defaults.
    pub path: Option<PathBuf>,
    pub agent: AgentConfig,
}

fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(s)
}

/// Candidate files, first match wins: `meet.json` from `start_dir` upward, then the global
/// file and its pre-rename location.
pub fn search_paths(start_dir: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut dir = Some(start_dir);
    while let Some(d) = dir {
        paths.push(d.join(LOCAL_FILE));
        dir = d.parent();
    }
    paths.push(expand_tilde(DEFAULT_PATH));
    paths.push(expand_tilde(LEGACY_PATH));
    paths
}

/// Load the config the way the engine does; a missing file is the defaults, a broken one
/// is an error naming it.
pub fn load(explicit: Option<&Path>, start_dir: &Path) -> Result<Config> {
    let path = if let Some(p) = explicit {
        let p = expand_tilde(&p.to_string_lossy());
        if !p.is_file() {
            bail!("config file not found: {}", p.display());
        }
        Some(p)
    } else if let Some(env) = std::env::var_os(ENV_VAR).filter(|v| !v.is_empty()) {
        let p = expand_tilde(&env.to_string_lossy());
        if !p.is_file() {
            bail!("config file not found (${ENV_VAR}): {}", p.display());
        }
        Some(p)
    } else {
        search_paths(start_dir).into_iter().find(|p| p.is_file())
    };
    let Some(path) = path else {
        return Ok(Config::default());
    };
    let text = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let mut cfg = parse(&text, path.parent().unwrap_or(Path::new(".")))
        .with_context(|| format!("could not parse {}", path.display()))?;
    cfg.path = Some(path);
    Ok(cfg)
}

/// The `agent` block out of the file's JSON. Relative `onDoneFile` paths resolve against
/// `base` (the config file's directory).
pub fn parse(text: &str, base: &Path) -> Result<Config> {
    let root: Value = serde_json::from_str(text)?;
    let mut cfg = Config::default();
    let Some(agent) = root.get("agent") else {
        return Ok(cfg);
    };
    if !agent.is_object() {
        bail!("\"agent\" must be an object");
    }
    cfg.agent.model = agent
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "default")
        .map(str::to_string);
    let inline = text_or_lines(agent.get("onDone"));
    let from_file = match agent.get("onDoneFile").and_then(Value::as_str) {
        Some(f) if inline.is_none() => {
            let p = expand_tilde(f);
            let p = if p.is_absolute() { p } else { base.join(p) };
            Some(
                std::fs::read_to_string(&p)
                    .with_context(|| format!("read agent.onDoneFile {}", p.display()))?,
            )
        }
        _ => None,
    };
    cfg.agent.on_done = inline
        .or(from_file)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Ok(cfg)
}

/// `"text"` or `["line", "line"]` (joined with newlines), like the engine's prompt keys.
fn text_or_lines(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => Some(s.clone()),
        Value::Array(lines) => Some(
            lines
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => None,
    }
}

/// `{{name}}` substitution; unknown placeholders are left as they are.
pub fn expand(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{{{k}}}}}"), v);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_agent_block_is_optional_and_reads_text_lines_or_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = parse(r#"{"outputDir": "meetings"}"#, dir.path()).unwrap();
        assert_eq!(cfg, Config::default());

        let cfg = parse(
            r#"{"agent": {"model": "opus", "onDone": ["Post a summary", "to {{branch}}."]}}"#,
            dir.path(),
        )
        .unwrap();
        assert_eq!(cfg.agent.model.as_deref(), Some("opus"));
        assert_eq!(
            cfg.agent.on_done.as_deref(),
            Some("Post a summary\nto {{branch}}.")
        );

        std::fs::write(dir.path().join("done.md"), "From a file.\n").unwrap();
        let cfg = parse(
            r#"{"agent": {"model": "default", "onDoneFile": "done.md"}}"#,
            dir.path(),
        )
        .unwrap();
        assert_eq!(cfg.agent.model, None, "\"default\" means claude's own");
        assert_eq!(cfg.agent.on_done.as_deref(), Some("From a file."));

        let cfg = parse(
            r#"{"agent": {"onDone": "inline wins", "onDoneFile": "missing.md"}}"#,
            dir.path(),
        )
        .unwrap();
        assert_eq!(cfg.agent.on_done.as_deref(), Some("inline wins"));
        assert!(parse(r#"{"agent": {"onDoneFile": "missing.md"}}"#, dir.path()).is_err());
        assert!(parse(r#"{"agent": []}"#, dir.path()).is_err());
        assert_eq!(
            parse(r#"{"agent": {"onDone": "   "}}"#, dir.path())
                .unwrap()
                .agent
                .on_done,
            None
        );
    }

    #[test]
    fn the_nearest_meet_json_wins_and_explicit_paths_must_exist() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let sub = repo.join("crates/x");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(
            dir.path().join(LOCAL_FILE),
            r#"{"agent": {"onDone": "outer"}}"#,
        )
        .unwrap();
        let paths = search_paths(&sub);
        assert_eq!(paths[0], sub.join(LOCAL_FILE));
        assert_eq!(paths[1], repo.join("crates").join(LOCAL_FILE));
        assert_eq!(paths[2], repo.join(LOCAL_FILE));
        assert!(paths.iter().any(|p| p.ends_with(".config/meet/config.json")));

        let cfg = load(None, &sub).unwrap();
        assert_eq!(cfg.agent.on_done.as_deref(), Some("outer"));
        std::fs::write(repo.join(LOCAL_FILE), r#"{"agent": {"onDone": "inner"}}"#).unwrap();
        let cfg = load(None, &sub).unwrap();
        assert_eq!(cfg.agent.on_done.as_deref(), Some("inner"));
        assert_eq!(cfg.path.as_deref(), Some(repo.join(LOCAL_FILE).as_path()));

        assert!(load(Some(Path::new("/nope/meet.json")), &sub).is_err());
        let explicit = dir.path().join("other.json");
        std::fs::write(&explicit, r#"{"agent": {"model": "haiku"}}"#).unwrap();
        let cfg = load(Some(&explicit), &sub).unwrap();
        assert_eq!(cfg.agent.model.as_deref(), Some("haiku"));
        assert_eq!(cfg.agent.on_done, None);
    }

    #[test]
    fn placeholders_expand_and_unknown_ones_stay() {
        assert_eq!(
            expand(
                "on {{branch}} for {{title}} ({{nope}})",
                &[("branch", "feat-x"), ("title", "Add X")]
            ),
            "on feat-x for Add X ({{nope}})"
        );
    }
}
