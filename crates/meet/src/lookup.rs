//! The live lookup: every closed transcript chunk goes to a fast, low-effort headless
//! Claude with read-only tools, which answers with a one-to-two-sentence summary of the
//! chunk and a few facts from the repository about what is being discussed — which file
//! does that, what the flag is called, how it behaves now. The facts fill the Related pane
//! while the meeting is still going; the action items are written once it ends (`suggest`).
//!
//! Requests are handled one at a time, in order, so every call sees the summaries and the
//! facts as they stood when its chunk closed.

use crate::claude::{structured, Structured};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

pub const SCHEMA: &str = r#"{"type":"object","properties":{"summary":{"type":"string"},"facts":{"type":"array","items":{"type":"object","properties":{"text":{"type":"string"},"where":{"type":"string"}},"required":["text","where"]}}},"required":["summary","facts"]}"#;

pub const TOOLS: &[&str] = &["Read", "Glob", "Grep"];

/// Something true about the repository, with the place that shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fact {
    pub text: String,
    #[serde(rename = "where", default)]
    pub where_: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Lookup {
    pub summary: String,
    #[serde(default)]
    pub facts: Vec<Fact>,
}

#[derive(Debug, Clone)]
pub struct LookupRequest {
    pub chunk_id: String,
    pub chunk_idx: usize,
    pub chunk_text: String,
    pub start: f64,
    pub end: f64,
    pub summaries_so_far: Vec<String>,
    /// `text (where)` per fact already shown.
    pub facts_so_far: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum LookupEvent {
    Started {
        chunk_id: String,
    },
    Done {
        chunk_id: String,
        lookup: Lookup,
    },
    Failed {
        chunk_id: String,
        chunk_idx: usize,
        error: String,
    },
}

#[derive(Debug, Clone)]
pub struct LookupConfig {
    pub claude_bin: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub repo: PathBuf,
    pub max_budget_usd: f64,
}

pub fn system_prompt(repo: &Path) -> String {
    let name = crate::git::repo_name(repo);
    format!(
        r#"You are the research assistant inside `meet`, a terminal tool a developer talks into while working on the git repository "{name}" at {repo}. Roughly every minute you receive the newest chunk of a live, on-device speech transcript (expect recognition errors, filler words and half sentences), the summaries of the chunks before it, and the facts you already reported. You answer with JSON only; the schema is enforced.

Your job is to look up, in THIS repository, what relates to what is being said right now, so the listener has the facts at hand while the conversation goes on: which file or function does that, what a command, flag or config key is called and does, how something behaves today, where its tests are, what does not exist yet. Use Glob, Grep and Read, briefly — a handful of tool calls; the answer must come back within seconds, and a shallow answer now beats a thorough one later.

Your two outputs:

1. `summary` — one or two plain sentences: what was discussed in this chunk, what the speaker wants, decided, or is worried about. Written for someone skimming a sidebar; no preamble, no "the speaker".

2. `facts` — zero to four NEW facts about this repository that bear on what was said. Each:
- `text` — one sentence, at most 140 characters, stating what is true now: "`todo.sh list` prints plain text through awk; there is no --json flag", "`Limits::default()` closes a chunk after 8 s of quiet". Quote real names.
- `where` — the file that shows it, as `path` or `path:line` relative to the repository; empty when the fact is that something does not exist.
Skip small talk, facts already reported (match by meaning), anything about other repositories, and anything you did not verify in a file. An empty list is a good answer when nothing in the repo relates; do not pad.

Transcript lines are `[mm:ss] source: text`. Sources: "mic" is this Mac's microphone (the developer and anyone in the room), "system" is audio the Mac played (remote call participants, videos), "typed" is a note typed into meet."#,
        repo = repo.display()
    )
}

pub fn user_prompt(req: &LookupRequest, brief: &str) -> String {
    let mut p = format!(
        "## New transcript chunk #{} ({}–{})\n{}\n",
        req.chunk_idx + 1,
        crate::chunker::clock(req.start),
        crate::chunker::clock(req.end),
        req.chunk_text
    );
    p.push_str("\n## Summaries so far\n");
    if req.summaries_so_far.is_empty() {
        p.push_str("(none — this is the first chunk)\n");
    }
    for (i, s) in req.summaries_so_far.iter().enumerate() {
        p.push_str(&format!("[{}] {}\n", i + 1, s));
    }
    p.push_str("\n## Facts already shown (do not repeat these)\n");
    if req.facts_so_far.is_empty() {
        p.push_str("(none)\n");
    }
    for f in &req.facts_so_far {
        p.push_str(&format!("- {f}\n"));
    }
    p.push_str("\n## About the repository\n");
    p.push_str(brief);
    p
}

pub async fn lookup(cfg: &LookupConfig, req: &LookupRequest) -> Result<Lookup> {
    let system = system_prompt(&cfg.repo);
    let prompt = user_prompt(req, &crate::suggest::repo_brief(&cfg.repo));
    let value = structured(Structured {
        bin: &cfg.claude_bin,
        cwd: &cfg.repo,
        model: cfg.model.as_deref(),
        effort: cfg.effort.as_deref(),
        system_prompt: &system,
        prompt: &prompt,
        schema: SCHEMA,
        tools: TOOLS,
        max_budget_usd: cfg.max_budget_usd,
    })
    .await?;
    let mut l: Lookup = serde_json::from_value(value).context("lookup did not match the schema")?;
    l.summary = l.summary.trim().to_string();
    l.facts.retain(|f| !f.text.trim().is_empty());
    for f in &mut l.facts {
        f.text = f.text.trim().to_string();
        f.where_ = f.where_.trim().to_string();
    }
    l.facts.truncate(4);
    Ok(l)
}

/// The worker: one request at a time, results back on `tx`.
pub fn spawn_worker(
    cfg: LookupConfig,
    mut rx: mpsc::UnboundedReceiver<LookupRequest>,
    tx: mpsc::UnboundedSender<LookupEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(req) = rx.recv().await {
            let _ = tx.send(LookupEvent::Started {
                chunk_id: req.chunk_id.clone(),
            });
            let ev = match lookup(&cfg, &req).await {
                Ok(lookup) => LookupEvent::Done {
                    chunk_id: req.chunk_id,
                    lookup,
                },
                Err(e) => LookupEvent::Failed {
                    chunk_id: req.chunk_id,
                    chunk_idx: req.chunk_idx,
                    error: format!("{e:#}"),
                },
            };
            if tx.send(ev).is_err() {
                break;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_schema_is_valid_json_and_the_prompt_carries_the_facts_so_far() {
        let v: serde_json::Value = serde_json::from_str(SCHEMA).unwrap();
        assert_eq!(v["required"][1], "facts");
        let req = LookupRequest {
            chunk_id: "c".into(),
            chunk_idx: 1,
            chunk_text: "[00:05] mic: how does list print".into(),
            start: 5.0,
            end: 65.0,
            summaries_so_far: vec!["first".into()],
            facts_so_far: vec!["list uses awk (todo.sh:18)".into()],
        };
        let p = user_prompt(&req, "brief\n");
        assert!(
            p.starts_with("## New transcript chunk #2 (00:05–01:05)\n[00:05] mic: how does list print\n"),
            "{p}"
        );
        assert!(p.contains("[1] first"));
        assert!(p.contains("- list uses awk (todo.sh:18)"));
        assert!(p.ends_with("## About the repository\nbrief\n"), "{p}");
        let sys = system_prompt(Path::new("/w/my-app"));
        assert!(sys.contains("\"my-app\""));
        assert!(sys.contains("zero to four"));
    }

    #[test]
    fn a_lookup_parses_with_missing_optional_fields_and_facts_round_trip_as_json() {
        let l: Lookup = serde_json::from_str(r#"{"summary":"x","facts":[{"text":"t"}]}"#).unwrap();
        assert_eq!(l.facts[0].where_, "");
        let l: Lookup = serde_json::from_str(r#"{"summary":"x"}"#).unwrap();
        assert!(l.facts.is_empty());
        let facts = vec![Fact {
            text: "t".into(),
            where_: "a.rs:1".into(),
        }];
        let json = serde_json::to_string(&facts).unwrap();
        assert_eq!(json, r#"[{"text":"t","where":"a.rs:1"}]"#);
        assert_eq!(serde_json::from_str::<Vec<Fact>>(&json).unwrap(), facts);
    }
}
