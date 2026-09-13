//! The suggester: once the recording has ended, the whole transcript goes to a headless
//! Claude that answers with a short summary of the meeting, its write-up (`notes`: a
//! Markdown page — summary, key points, decisions, open questions, action items — that
//! the Summary pane shows the moment it lands) and the action items it calls for. Each
//! item's `prompt` is written under prompt-doctor rules (nebula's `prompt-daddy` skill):
//! one fully specified request in the speaker's voice, ambiguity closed, "keep X as-is"
//! named, the why carried, gaps filled with visibly marked assumptions — so the agent that
//! later runs it never has to guess and the user can read it at a glance.
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

pub const SCHEMA: &str = r#"{"type":"object","properties":{"summary":{"type":"string"},"notes":{"type":"string"},"items":{"type":"array","items":{"type":"object","properties":{"title":{"type":"string"},"prompt":{"type":"string"},"why":{"type":"string"}},"required":["title","prompt","why"]}}},"required":["summary","notes","items"]}"#;

/// Read-only tools, so the prompt can name real files and flags.
pub const TOOLS: &[&str] = &["Read", "Glob", "Grep"];

const MAX_ITEMS: usize = 8;

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
    /// The write-up, Markdown; empty when the model left it out.
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub items: Vec<SuggestedItem>,
}

#[derive(Debug, Clone)]
pub struct SuggestRequest {
    /// The meeting the answer is for, carried back on the event: an answer that lands
    /// after the next recording started still goes to its own meeting.
    pub meeting_id: String,
    /// The whole meeting, `[mm:ss] source: text` per line.
    pub transcript: String,
    pub summaries: Vec<String>,
    /// `text (where)` per fact the live lookup found.
    pub facts: Vec<String>,
    /// `text (where)` per contradiction it noticed.
    pub contradictions: Vec<String>,
    /// The questions it suggested asking.
    pub questions: Vec<String>,
    /// `title (status)` per item already on the board.
    pub board: Vec<String>,
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
        r#"You are the action-item writer inside `meet`, a terminal tool a developer talks into while working on the git repository "{name}" at {repo}. A meeting just ended. You receive its whole live, on-device speech transcript (expect recognition errors, filler words and half sentences), the one-line summaries written while it went on, the facts about the repository (and earlier meetings) that were looked up along the way, the contradictions the lookup noticed between what was said and what the code or an earlier meeting shows, the questions it suggested asking, and the action items already on the board from earlier meetings. You answer with JSON only; the schema is enforced.

Your three outputs:

1. `summary` — two to four plain sentences: what the meeting was about, what the speaker wants, decided, or is worried about. Written for someone skimming a session list; no preamble, no "the speaker".

2. `notes` — the meeting's write-up, shown in meet right after the meeting for the people who were in it to read: Markdown, about 150 to 400 words. These sections, each under a `## ` heading, in this order, leaving out a section that would be empty: `## Summary` (one short paragraph), `## Key points` (bullets, in the order they came up), `## Decisions` (bullets: what was settled, with the reason when one was given), `## Open questions` (bullets: what was left undecided, and any contradiction or question above that the transcript never resolved), `## Action items` (one bullet per entry in `items`, its title — plus anything to do outside this repository that was mentioned, marked as such). Bullets start with `- `. Plain sentences, no preamble, no speaker labels unless who said it matters; do not invent anything the transcript does not say.

3. `items` — zero to {max} NEW action items: concrete engineering changes to THIS repository that the speaker asked for, decided on, or clearly implied should happen ("we should…", "let's…", "it'd be nice if…", "that's a bug", "todo"). One item per change; when the same thing was said twice, write it once with everything that was said about it. Skip small talk, open questions with no decision, anything already on the board (match by meaning, not wording), and work outside this repo. An empty list is a good answer when nothing is actionable; do not pad.

Each item:
- `title` — at most 60 characters, imperative, specific: "Add a --json flag to `meet record`", never "JSON support". It is the label the user reads and presses Enter on.
- `prompt` — the exact task a headless coding agent will receive with no chance to ask questions. Write it the way a careful colleague rewrites a hurried request (prompt-doctor rules):
  * one complete request in the speaker's own voice — first person, imperative, something they could have typed;
  * close every ambiguity: name the real file, command, flag, type or behaviour. The looked-up facts name real places; use Read, Glob and Grep on the repository when that makes the prompt more concrete, and keep the lookup brief;
  * say what must stay exactly as it is when the change touches something that works;
  * spell out the when/then behaviour instead of hanging the spec on one word ("done", "fix", "clean up");
  * give one literal example of the expected output or behaviour where it helps;
  * carry the why: "so that …", from what was said;
  * fill a gap you cannot verify with a visibly marked assumption: "(assuming …)". Never invent facts the speaker did not say and the repo does not show. A contradiction or question the transcript never resolved is such a gap: mark the assumption, do not pick a side silently;
  * keep the scope the speaker gave — no bonus features, no investigation they did not ask for;
  * under about 120 words, plain prose, no headings or lists.
- `why` — one sentence quoting or closely paraphrasing what was said that motivates the item, so the user can trust where it came from.

Transcript lines are `[mm:ss] source: text`. Sources: "mic" is this Mac's microphone (the developer and anyone in the room), "system" is audio the Mac played (remote call participants, videos), "typed" is a note typed into meet."#,
        repo = repo.display(),
        max = MAX_ITEMS
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
    p.push_str("\n## Summaries written during the meeting\n");
    if req.summaries.is_empty() {
        p.push_str("(none)\n");
    }
    for (i, s) in req.summaries.iter().enumerate() {
        p.push_str(&format!("[{}] {}\n", i + 1, s));
    }
    p.push_str("\n## Facts looked up during the meeting\n");
    if req.facts.is_empty() {
        p.push_str("(none)\n");
    }
    for f in &req.facts {
        p.push_str(&format!("- {f}\n"));
    }
    p.push_str("\n## Contradictions noticed during the meeting (what was said, against what the code or an earlier meeting shows)\n");
    if req.contradictions.is_empty() {
        p.push_str("(none)\n");
    }
    for c in &req.contradictions {
        p.push_str(&format!("- {c}\n"));
    }
    p.push_str("\n## Questions meet suggested asking (open unless the transcript answers them)\n");
    if req.questions.is_empty() {
        p.push_str("(none)\n");
    }
    for q in &req.questions {
        p.push_str(&format!("- {q}\n"));
    }
    p.push_str("\n## Action items already on the board (do not repeat these)\n");
    if req.board.is_empty() {
        p.push_str("(none)\n");
    }
    for b in &req.board {
        p.push_str(&format!("- {b}\n"));
    }
    p.push_str("\n## About the repository\n");
    p.push_str(brief);
    p
}

/// The one call: the summary and items, and what it spent (counted whether or not it
/// succeeded).
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
        tools: TOOLS,
        max_budget_usd: cfg.max_budget_usd,
    })
    .await;
    let parsed = answer.value.and_then(|value| {
        let mut s: Suggestion =
            serde_json::from_value(value).context("suggestion did not match the schema")?;
        s.summary = s.summary.trim().to_string();
        s.notes = s.notes.trim().to_string();
        s.items
            .retain(|i| !i.title.trim().is_empty() && !i.prompt.trim().is_empty());
        for i in &mut s.items {
            i.title = i.title.trim().to_string();
            i.prompt = i.prompt.trim().to_string();
            i.why = i.why.trim().to_string();
        }
        s.items.truncate(MAX_ITEMS);
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
    fn the_schema_is_valid_json_and_the_prompt_carries_the_board_and_the_transcript() {
        let v: serde_json::Value = serde_json::from_str(SCHEMA).unwrap();
        assert_eq!(v["required"][0], "summary");
        assert_eq!(v["required"][1], "notes");
        assert_eq!(v["properties"]["notes"]["type"], "string");
        let req = SuggestRequest {
            meeting_id: "m".into(),
            transcript: "[00:05] mic: let's add a flag".into(),
            summaries: vec!["first".into()],
            facts: vec!["list uses awk (todo.sh:18)".into()],
            contradictions: vec!["said list has --json; it does not (todo.sh:18)".into()],
            questions: vec![],
            board: vec!["Add X (running)".into()],
        };
        let p = user_prompt(&req, "brief\n");
        assert!(
            p.starts_with("## The meeting transcript (1 line)\n[00:05] mic: let's add a flag\n"),
            "{p}"
        );
        assert!(p.contains("[1] first"));
        assert!(p.contains("- list uses awk (todo.sh:18)"));
        assert!(p.contains("## Contradictions noticed during the meeting (what was said, against what the code or an earlier meeting shows)\n- said list has --json; it does not (todo.sh:18)\n"), "{p}");
        assert!(p.contains("## Questions meet suggested asking (open unless the transcript answers them)\n(none)\n"), "{p}");
        assert!(p.contains("- Add X (running)"));
        assert!(p.ends_with("## About the repository\nbrief\n"), "{p}");
        let sys = system_prompt(Path::new("/w/my-app"));
        assert!(sys.contains("\"my-app\""));
        assert!(sys.contains("(assuming …)"));
        assert!(sys.contains("zero to 8"));
        assert!(sys.contains("2. `notes`"), "the write-up is asked for:\n{sys}");
        for heading in ["## Summary", "## Key points", "## Decisions", "## Open questions", "## Action items"] {
            assert!(sys.contains(heading), "{heading} missing from the prompt");
        }
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
        let s: Suggestion =
            serde_json::from_str(r#"{"summary":"x","items":[{"title":"t","prompt":"p"}]}"#)
                .unwrap();
        assert_eq!(s.items[0].why, "");
        assert_eq!(s.notes, "", "a model that skipped the write-up still parses");
        let s: Suggestion = serde_json::from_str(r#"{"summary":"x"}"#).unwrap();
        assert!(s.items.is_empty());
        let s: Suggestion =
            serde_json::from_str(r###"{"summary":"x","notes":"## Summary\nShort.\n","items":[]}"###)
                .unwrap();
        assert_eq!(s.notes, "## Summary\nShort.\n");
    }
}
