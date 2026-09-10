//! The suggester: every closed transcript chunk goes to a headless Claude that answers
//! with a one-to-three-sentence summary and zero to three new action items. Each item's
//! `prompt` is written under prompt-doctor rules (nebula's `prompt-daddy` skill): one
//! fully specified request in the speaker's voice, ambiguity closed, "keep X as-is"
//! named, the why carried, gaps filled with visibly marked assumptions — so the agent that
//! later runs it never has to guess and the user can read it at a glance.
//!
//! Requests are handled one at a time, in order, so every call sees the summaries and the
//! board as they stood when its chunk closed.

use crate::claude::{structured, Structured};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

pub const SCHEMA: &str = r#"{"type":"object","properties":{"summary":{"type":"string"},"items":{"type":"array","items":{"type":"object","properties":{"title":{"type":"string"},"prompt":{"type":"string"},"why":{"type":"string"}},"required":["title","prompt","why"]}}},"required":["summary","items"]}"#;

/// Read-only tools, so the prompt can name real files and flags.
pub const TOOLS: &[&str] = &["Read", "Glob", "Grep"];

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SuggestedItem {
    pub title: String,
    pub prompt: String,
    #[serde(default)]
    pub why: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Suggestion {
    pub summary: String,
    #[serde(default)]
    pub items: Vec<SuggestedItem>,
}

#[derive(Debug, Clone)]
pub struct SuggestRequest {
    pub chunk_id: String,
    pub chunk_idx: usize,
    pub chunk_text: String,
    pub start: f64,
    pub end: f64,
    pub summaries_so_far: Vec<String>,
    /// `title (status)` per item already on the board.
    pub board: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum SuggestEvent {
    Started {
        chunk_id: String,
    },
    Done {
        chunk_id: String,
        suggestion: Suggestion,
    },
    Failed {
        chunk_id: String,
        chunk_idx: usize,
        error: String,
    },
}

#[derive(Debug, Clone)]
pub struct SuggesterConfig {
    pub claude_bin: String,
    pub model: Option<String>,
    pub repo: PathBuf,
    pub max_budget_usd: f64,
}

pub fn system_prompt(repo: &Path) -> String {
    let name = crate::git::repo_name(repo);
    format!(
        r#"You are the action-item scout inside `meet`, a terminal tool a developer talks into while working on the git repository "{name}" at {repo}. Roughly every minute you receive the newest chunk of a live, on-device speech transcript (expect recognition errors, filler words and half sentences), the summaries of the chunks before it, and the action items already on the board. You answer with JSON only; the schema is enforced.

Your two outputs:

1. `summary` — one to three plain sentences: what was discussed in this chunk, what the speaker wants, decided, or is worried about. Written for someone skimming a sidebar; no preamble, no "the speaker".

2. `items` — zero to three NEW action items: concrete engineering changes to THIS repository that the speaker asked for, decided on, or clearly implied should happen ("we should…", "let's…", "it'd be nice if…", "that's a bug", "todo"). Skip small talk, open questions with no decision, anything already on the board (match by meaning, not wording), and work outside this repo. An empty list is a good answer when nothing new is actionable; do not pad.

Each item:
- `title` — at most 60 characters, imperative, specific: "Add a --json flag to `meet record`", never "JSON support". It is the label the user reads and presses Enter on.
- `prompt` — the exact task a headless coding agent will receive with no chance to ask questions. Write it the way a careful colleague rewrites a hurried request (prompt-doctor rules):
  * one complete request in the speaker's own voice — first person, imperative, something they could have typed;
  * close every ambiguity: name the real file, command, flag, type or behaviour. Use Read, Glob and Grep on the repository to find the real names when that makes the prompt concrete; keep the lookup brief;
  * say what must stay exactly as it is when the change touches something that works;
  * spell out the when/then behaviour instead of hanging the spec on one word ("done", "fix", "clean up");
  * give one literal example of the expected output or behaviour where it helps;
  * carry the why: "so that …", from what was said;
  * fill a gap you cannot verify with a visibly marked assumption: "(assuming …)". Never invent facts the speaker did not say and the repo does not show;
  * keep the scope the speaker gave — no bonus features, no investigation they did not ask for;
  * under about 120 words, plain prose, no headings or lists.
- `why` — one sentence quoting or closely paraphrasing what was said that motivates the item, so the user can trust where it came from.

Transcript lines are `[mm:ss] source: text`. Sources: "mic" is this Mac's microphone (the developer and anyone in the room), "system" is audio the Mac played (remote call participants, videos), "typed" is a note typed into meet."#,
        repo = repo.display()
    )
}

/// A few lines about the repo so a call with no tool use still knows what it is looking at.
pub fn repo_brief(repo: &Path) -> String {
    let mut out = String::new();
    if let Ok(entries) = std::fs::read_dir(repo) {
        let mut names: Vec<String> = entries
            .flatten()
            .map(|e| {
                let mut n = e.file_name().to_string_lossy().into_owned();
                if e.path().is_dir() {
                    n.push('/');
                }
                n
            })
            .filter(|n| !n.starts_with('.') || n == ".claude/")
            .collect();
        names.sort();
        names.truncate(40);
        out.push_str("Top-level entries: ");
        out.push_str(&names.join(" "));
        out.push('\n');
    }
    for doc in ["CLAUDE.md", "AGENTS.md", "README.md"] {
        if let Ok(text) = std::fs::read_to_string(repo.join(doc)) {
            let head: Vec<&str> = text.lines().take(25).collect();
            out.push_str(&format!("\n{doc} (first lines):\n{}\n", head.join("\n")));
            break;
        }
    }
    out
}

pub fn user_prompt(req: &SuggestRequest, brief: &str) -> String {
    let mut p = String::new();
    p.push_str(brief);
    p.push_str("\nSummaries so far:\n");
    if req.summaries_so_far.is_empty() {
        p.push_str("(none — this is the first chunk)\n");
    }
    for (i, s) in req.summaries_so_far.iter().enumerate() {
        p.push_str(&format!("[{}] {}\n", i + 1, s));
    }
    p.push_str("\nAction items already on the board (do not repeat these):\n");
    if req.board.is_empty() {
        p.push_str("(none)\n");
    }
    for b in &req.board {
        p.push_str(&format!("- {b}\n"));
    }
    p.push_str(&format!(
        "\nNew transcript chunk #{} ({}–{}):\n{}\n",
        req.chunk_idx + 1,
        crate::chunker::clock(req.start),
        crate::chunker::clock(req.end),
        req.chunk_text
    ));
    p
}

pub async fn suggest(cfg: &SuggesterConfig, req: &SuggestRequest) -> Result<Suggestion> {
    let system = system_prompt(&cfg.repo);
    let prompt = user_prompt(req, &repo_brief(&cfg.repo));
    let value = structured(Structured {
        bin: &cfg.claude_bin,
        cwd: &cfg.repo,
        model: cfg.model.as_deref(),
        system_prompt: &system,
        prompt: &prompt,
        schema: SCHEMA,
        tools: TOOLS,
        max_budget_usd: cfg.max_budget_usd,
    })
    .await?;
    let mut s: Suggestion =
        serde_json::from_value(value).context("suggestion did not match the schema")?;
    s.summary = s.summary.trim().to_string();
    s.items
        .retain(|i| !i.title.trim().is_empty() && !i.prompt.trim().is_empty());
    for i in &mut s.items {
        i.title = i.title.trim().to_string();
        i.prompt = i.prompt.trim().to_string();
        i.why = i.why.trim().to_string();
    }
    s.items.truncate(3);
    Ok(s)
}

/// The worker: one request at a time, results back on `tx`.
pub fn spawn_worker(
    cfg: SuggesterConfig,
    mut rx: mpsc::UnboundedReceiver<SuggestRequest>,
    tx: mpsc::UnboundedSender<SuggestEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(req) = rx.recv().await {
            let _ = tx.send(SuggestEvent::Started {
                chunk_id: req.chunk_id.clone(),
            });
            let ev = match suggest(&cfg, &req).await {
                Ok(suggestion) => SuggestEvent::Done {
                    chunk_id: req.chunk_id,
                    suggestion,
                },
                Err(e) => SuggestEvent::Failed {
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
    fn the_schema_is_valid_json_and_the_prompt_carries_the_board() {
        let v: serde_json::Value = serde_json::from_str(SCHEMA).unwrap();
        assert_eq!(v["required"][0], "summary");
        let req = SuggestRequest {
            chunk_id: "c".into(),
            chunk_idx: 1,
            chunk_text: "[00:05] mic: let's add a flag".into(),
            start: 5.0,
            end: 65.0,
            summaries_so_far: vec!["first".into()],
            board: vec!["Add X (running)".into()],
        };
        let p = user_prompt(&req, "brief\n");
        assert!(p.starts_with("brief"));
        assert!(p.contains("[1] first"));
        assert!(p.contains("- Add X (running)"));
        assert!(p.contains("chunk #2 (00:05–01:05)"));
        let sys = system_prompt(Path::new("/w/my-app"));
        assert!(sys.contains("\"my-app\""));
        assert!(sys.contains("(assuming …)"));
    }

    #[test]
    fn suggestion_parses_with_missing_optional_fields() {
        let s: Suggestion =
            serde_json::from_str(r#"{"summary":"x","items":[{"title":"t","prompt":"p"}]}"#)
                .unwrap();
        assert_eq!(s.items[0].why, "");
        let s: Suggestion = serde_json::from_str(r#"{"summary":"x"}"#).unwrap();
        assert!(s.items.is_empty());
    }
}
