//! The summary writer: once the recording has ended, the whole transcript goes to a
//! headless Claude that answers with a short summary of the meeting and its write-up
//! (`notes`: a Markdown page — summary, key points, decisions, open questions — that the
//! Summary pane shows the moment it lands).
//!
//! This is meet's own summary, independent of the engine's onDone hooks (the bundled
//! `summarize-transcript.sh`, or whatever `meet.json` names): those write wherever they
//! like; this one lands in the database and on screen.

use crate::claude::{structured, Structured};
use crate::stream_json::Usage;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

pub const SCHEMA: &str = r#"{"type":"object","properties":{"summary":{"type":"string"},"notes":{"type":"string"}},"required":["summary","notes"]}"#;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Suggestion {
    pub summary: String,
    /// The write-up, Markdown; empty when the model left it out.
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Clone)]
pub struct SuggestRequest {
    /// The meeting the answer is for, carried back on the event: an answer that lands
    /// after the next recording started still goes to its own meeting.
    pub meeting_id: String,
    /// The whole meeting, `[mm:ss] source: text` per line.
    pub transcript: String,
}

#[derive(Debug, Clone)]
pub enum SuggestEvent {
    Done {
        meeting_id: String,
        suggestion: Suggestion,
        /// What the call spent.
        usage: Usage,
    },
    Failed {
        meeting_id: String,
        error: String,
        /// What the call spent before it failed (zero when claude never answered).
        usage: Usage,
    },
}

#[derive(Debug, Clone)]
pub struct SuggesterConfig {
    pub claude_bin: String,
    pub model: Option<String>,
    /// `--effort` (`None`: claude's own).
    pub effort: Option<String>,
    pub repo: PathBuf,
    pub max_budget_usd: f64,
}

pub fn system_prompt(repo: &Path) -> String {
    let name = crate::git::repo_name(repo);
    format!(
        r#"You are the summary writer inside `meet`, a terminal tool a developer talks into while working on the git repository "{name}" at {repo}. A meeting just ended. You receive its whole live, on-device speech transcript (expect recognition errors, filler words and half sentences). You answer with JSON only; the schema is enforced.

Your two outputs:

1. `summary` — two to four plain sentences: what the meeting was about, what the speaker wants, decided, or is worried about. Written for someone skimming a session list; no preamble, no "the speaker".

2. `notes` — the meeting's write-up, shown in meet right after the meeting for the people who were in it to read: Markdown, about 150 to 400 words. These sections, each under a `## ` heading, in this order, leaving out a section that would be empty: `## Summary` (one short paragraph), `## Key points` (bullets, in the order they came up), `## Decisions` (bullets: what was settled, with the reason when one was given), `## Open questions` (bullets: what was left undecided). Bullets start with `- `. Plain sentences, no preamble, no speaker labels unless who said it matters; do not invent anything the transcript does not say.

Transcript lines are `[mm:ss] source: text`. Sources: "mic" is this Mac's microphone (the developer and anyone in the room), "system" is audio the Mac played (remote call participants, videos), "typed" is a note typed into meet."#,
        repo = repo.display(),
    )
}

/// A few lines about the repo so a call with no tool use still knows what it is looking at.
/// The README excerpt is fenced: unfenced, a README that talks about transcripts and
/// summaries (like meet's own) reads as if the sections after it were still the README.
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
            out.push_str(&format!(
                "{doc}, first lines (for orientation only):\n````\n{}\n````\n",
                head.join("\n")
            ));
            break;
        }
    }
    out
}

pub fn user_prompt(req: &SuggestRequest, brief: &str) -> String {
    let lines = req.transcript.lines().count();
    let mut p = format!(
        "## The meeting transcript ({lines} line{})\n{}\n",
        if lines == 1 { "" } else { "s" },
        req.transcript
    );
    p.push_str("\n## About the repository\n");
    p.push_str(brief);
    p
}

/// The one call: the summary and the write-up, and what it spent (counted whether or not
/// it succeeded). No tools: everything it needs is in the prompt.
pub async fn suggest(cfg: &SuggesterConfig, req: &SuggestRequest) -> (Usage, Result<Suggestion>) {
    let system = system_prompt(&cfg.repo);
    let prompt = user_prompt(req, &repo_brief(&cfg.repo));
    let answer = structured(Structured {
        bin: &cfg.claude_bin,
        cwd: &cfg.repo,
        model: cfg.model.as_deref(),
        effort: cfg.effort.as_deref(),
        system_prompt: &system,
        prompt: &prompt,
        schema: SCHEMA,
        tools: &[],
        max_budget_usd: cfg.max_budget_usd,
    })
    .await;
    let parsed = answer.value.and_then(|value| {
        let mut s: Suggestion =
            serde_json::from_value(value).context("suggestion did not match the schema")?;
        s.summary = s.summary.trim().to_string();
        s.notes = s.notes.trim().to_string();
        Ok(s)
    });
    (answer.usage, parsed)
}

/// One call for the meeting that just ended; the result comes back on `tx`.
pub fn spawn(cfg: SuggesterConfig, req: SuggestRequest, tx: mpsc::UnboundedSender<SuggestEvent>) {
    tokio::spawn(async move {
        let (usage, result) = suggest(&cfg, &req).await;
        let meeting_id = req.meeting_id;
        let ev = match result {
            Ok(suggestion) => SuggestEvent::Done {
                meeting_id,
                suggestion,
                usage,
            },
            Err(e) => SuggestEvent::Failed {
                meeting_id,
                error: format!("{e:#}"),
                usage,
            },
        };
        let _ = tx.send(ev);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_schema_is_valid_json_and_the_prompt_carries_the_transcript() {
        let v: serde_json::Value = serde_json::from_str(SCHEMA).unwrap();
        assert_eq!(v["required"][0], "summary");
        assert_eq!(v["required"][1], "notes");
        assert_eq!(v["properties"]["notes"]["type"], "string");
        assert!(v["properties"].get("items").is_none(), "no action items are asked for");
        let req = SuggestRequest {
            meeting_id: "m".into(),
            transcript: "[00:05] mic: let's add a flag".into(),
        };
        let p = user_prompt(&req, "brief\n");
        assert!(
            p.starts_with("## The meeting transcript (1 line)\n[00:05] mic: let's add a flag\n"),
            "{p}"
        );
        assert_eq!(p.matches("## ").count(), 2, "the transcript and the repository, nothing else: {p}");
        assert!(p.ends_with("## About the repository\nbrief\n"), "{p}");
        let sys = system_prompt(Path::new("/w/my-app"));
        assert!(sys.contains("\"my-app\""));
        assert!(sys.contains("2. `notes`"), "the write-up is asked for:\n{sys}");
        for heading in ["## Summary", "## Key points", "## Decisions", "## Open questions"] {
            assert!(sys.contains(heading), "{heading} missing from the prompt");
        }
        assert!(!sys.contains("## Action items"), "{sys}");
        assert!(!sys.contains("looked up"), "no live lookup feeds it any more:\n{sys}");
    }

    #[test]
    fn the_repo_brief_fences_the_readme() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("README.md"), "# app\n\nthe transcript goes here\n").unwrap();
        let brief = repo_brief(dir.path());
        assert!(brief.starts_with("Top-level entries: README.md src/\n"), "{brief}");
        assert!(
            brief.ends_with("README.md, first lines (for orientation only):\n````\n# app\n\nthe transcript goes here\n````\n"),
            "{brief}"
        );
    }

    #[test]
    fn suggestion_parses_with_missing_optional_fields() {
        let s: Suggestion = serde_json::from_str(r#"{"summary":"x"}"#).unwrap();
        assert_eq!(s.notes, "", "a model that skipped the write-up still parses");
        let s: Suggestion =
            serde_json::from_str(r###"{"summary":"x","notes":"## Summary\nShort.\n"}"###)
                .unwrap();
        assert_eq!(s.notes, "## Summary\nShort.\n");
    }
}
