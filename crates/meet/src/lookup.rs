//! The live lookup: every closed transcript chunk goes to a fast, low-effort headless
//! Claude with read-only tools, which answers with a one-to-two-sentence summary of the
//! chunk and, for that chunk, what is already known that bears on it: facts from the
//! repository and from earlier meetings (which file does that, what the flag is called,
//! how it behaves now, what was decided last week), contradictions between what was said
//! and what the code or an earlier meeting shows, and the questions worth asking next.
//! The Related pane shows them per summary; the action items are written once the
//! meeting ends (`suggest`).
//!
//! Requests are handled one at a time, in order, so every call sees the summaries and
//! what was already shown as they stood when its chunk closed.

use crate::claude::{structured, Structured};
use crate::stream_json::Usage;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::sync::{mpsc, watch};

pub const SCHEMA: &str = r#"{"type":"object","properties":{"summary":{"type":"string"},"facts":{"type":"array","items":{"type":"object","properties":{"text":{"type":"string"},"where":{"type":"string"}},"required":["text","where"]}},"contradictions":{"type":"array","items":{"type":"object","properties":{"text":{"type":"string"},"where":{"type":"string"}},"required":["text","where"]}},"questions":{"type":"array","items":{"type":"string"}}},"required":["summary","facts","contradictions","questions"]}"#;

pub const TOOLS: &[&str] = &["Read", "Glob", "Grep"];

pub const MAX_FACTS: usize = 4;
pub const MAX_CONTRADICTIONS: usize = 3;
pub const MAX_QUESTIONS: usize = 3;

/// Something true about the repository (or concluded in an earlier meeting), with the
/// place that shows it. A contradiction is the same shape: what was said against what
/// is so, and where that is visible.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fact {
    pub text: String,
    #[serde(rename = "where", default)]
    pub where_: String,
}

impl Fact {
    /// `text (where)` — how a fact is quoted back to Claude and written to disk.
    pub fn line(&self) -> String {
        if self.where_.is_empty() {
            self.text.clone()
        } else {
            format!("{} ({})", self.text, self.where_)
        }
    }
}

/// The lookup's answer for one chunk.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Lookup {
    pub summary: String,
    /// What the repository and earlier meetings say that bears on the chunk.
    #[serde(default)]
    pub facts: Vec<Fact>,
    /// Where what was said does not match the code or an earlier decision.
    #[serde(default)]
    pub contradictions: Vec<Fact>,
    /// Worth asking in the conversation right now.
    #[serde(default)]
    pub questions: Vec<String>,
}

impl Lookup {
    /// Trimmed, empties dropped, capped.
    pub fn tidy(&mut self) {
        self.summary = self.summary.trim().to_string();
        for list in [&mut self.facts, &mut self.contradictions] {
            list.retain(|f| !f.text.trim().is_empty());
            for f in list.iter_mut() {
                f.text = f.text.trim().to_string();
                f.where_ = f.where_.trim().to_string();
            }
        }
        self.facts.truncate(MAX_FACTS);
        self.contradictions.truncate(MAX_CONTRADICTIONS);
        self.questions.retain(|q| !q.trim().is_empty());
        for q in &mut self.questions {
            *q = q.trim().to_string();
        }
        self.questions.truncate(MAX_QUESTIONS);
    }
}

#[derive(Debug, Clone)]
pub struct LookupRequest {
    pub chunk_id: String,
    pub chunk_idx: usize,
    pub chunk_text: String,
    pub start: f64,
    pub end: f64,
    /// The raw text of the chunk before this one, so a topic that straddles the cut is
    /// still readable.
    pub previous_text: Option<String>,
    pub summaries_so_far: Vec<String>,
    /// `text (where)` per fact already shown.
    pub facts_so_far: Vec<String>,
    /// `text (where)` per contradiction already shown.
    pub contradictions_so_far: Vec<String>,
    pub questions_so_far: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum LookupEvent {
    Started {
        chunk_id: String,
    },
    Done {
        chunk_id: String,
        lookup: Lookup,
        /// What the call spent.
        usage: Usage,
    },
    Failed {
        chunk_id: String,
        chunk_idx: usize,
        error: String,
        /// What the call spent before it failed (zero when claude never answered).
        usage: Usage,
    },
}

#[derive(Debug, Clone)]
pub struct LookupConfig {
    pub claude_bin: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub repo: PathBuf,
    pub max_budget_usd: f64,
    /// What earlier meetings in this repository concluded (`knowledge::brief`); the same
    /// for every chunk of the meeting.
    pub knowledge: String,
}

pub fn system_prompt(repo: &Path) -> String {
    let name = crate::git::repo_name(repo);
    format!(
        r#"You are the research assistant inside `meet`, a terminal tool a developer talks into while working on the git repository "{name}" at {repo}. Roughly every minute you receive the newest chunk of a live, on-device speech transcript (expect recognition errors, filler words and half sentences), the chunk before it, the summaries of the chunks before that, what you already reported, and what earlier meetings in this repository concluded. You answer with JSON only; the schema is enforced.

Your job is to put next to this chunk what is already known that bears on it, so the listener has it at hand while the conversation goes on. Two sources: THIS repository — use Glob, Grep and Read, briefly: a handful of tool calls; the answer must come back within seconds, and a shallow answer now beats a thorough one later — and the earlier meetings quoted in the prompt, which need no tools.

Your four outputs:

1. `summary` — one or two plain sentences: what was discussed in this chunk, what the speaker wants, decided, or is worried about. Written for someone skimming a sidebar; no preamble, no "the speaker".

2. `facts` — zero to {max_facts} NEW facts that bear on what was said: which file or function does that, what a command, flag or config key is called and does, how something behaves today, where its tests are, what does not exist yet, what an earlier meeting decided about it. Each:
- `text` — one sentence, at most 140 characters, stating what is true now: "`todo.sh list` prints plain text through awk; there is no --json flag", "Decided in the meeting of 2026-09-04: the config file stays JSON". Quote real names.
- `where` — the file that shows it, as `path` or `path:line` relative to the repository; `meeting <date>` for something an earlier meeting concluded; empty when the fact is that something does not exist.

3. `contradictions` — zero to {max_contradictions} places where what was said does not match what the repository or an earlier meeting shows: a claim about how something works that the code contradicts, a plan that reverses an earlier decision, a name that does not exist. `text` gives both sides in one sentence, at most 160 characters: "Said `add` keeps the whole line; `todo.sh add` takes only $1". `where` as above. Only what you verified; a garbled transcript is not a contradiction.

4. `questions` — zero to {max_questions} questions worth asking in this conversation right now, one sentence each, at most 120 characters: the decision the speaker is circling without making, the case the code will hit that nobody mentioned, the thing that must be settled before the work could be handed to someone. Specific to this chunk; no generic "have you considered tests".

Skip small talk, anything already reported (match by meaning), anything about other repositories, and anything you did not verify in a file or in the earlier meetings. Empty lists are a good answer when nothing relates; do not pad.

Transcript lines are `[mm:ss] source: text`. Sources: "mic" is this Mac's microphone (the developer and anyone in the room), "system" is audio the Mac played (remote call participants, videos), "typed" is a note typed into meet."#,
        repo = repo.display(),
        max_facts = MAX_FACTS,
        max_contradictions = MAX_CONTRADICTIONS,
        max_questions = MAX_QUESTIONS,
    )
}

pub fn user_prompt(req: &LookupRequest, knowledge: &str, brief: &str) -> String {
    let mut p = format!(
        "## New transcript chunk #{} ({}–{})\n{}\n",
        req.chunk_idx + 1,
        crate::chunker::clock(req.start),
        crate::chunker::clock(req.end),
        req.chunk_text
    );
    p.push_str("\n## The chunk before it (already summarized; for continuity only)\n");
    match &req.previous_text {
        Some(t) if !t.trim().is_empty() => {
            p.push_str(t);
            p.push('\n');
        }
        _ => p.push_str("(none — this is the first chunk)\n"),
    }
    p.push_str("\n## Summaries so far\n");
    if req.summaries_so_far.is_empty() {
        p.push_str("(none — this is the first chunk)\n");
    }
    for (i, s) in req.summaries_so_far.iter().enumerate() {
        p.push_str(&format!("[{}] {}\n", i + 1, s));
    }
    p.push_str("\n## Already shown for earlier chunks (do not repeat these)\n");
    let sections: [(&str, &[String]); 3] = [
        ("Facts", &req.facts_so_far),
        ("Contradictions", &req.contradictions_so_far),
        ("Questions", &req.questions_so_far),
    ];
    if sections.iter().all(|(_, l)| l.is_empty()) {
        p.push_str("(none)\n");
    }
    for (label, list) in sections {
        if list.is_empty() {
            continue;
        }
        p.push_str(&format!("{label}:\n"));
        for l in list {
            p.push_str(&format!("- {l}\n"));
        }
    }
    p.push_str("\n## What earlier meetings in this repository concluded\n");
    if knowledge.trim().is_empty() {
        p.push_str("(none recorded)\n");
    } else {
        p.push_str(knowledge.trim_end());
        p.push('\n');
    }
    p.push_str("\n## About the repository\n");
    p.push_str(brief);
    p
}

/// One call: what it found, and what it spent (counted whether or not it succeeded).
pub async fn lookup(cfg: &LookupConfig, req: &LookupRequest) -> (Usage, Result<Lookup>) {
    let system = system_prompt(&cfg.repo);
    let prompt = user_prompt(req, &cfg.knowledge, &crate::suggest::repo_brief(&cfg.repo));
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
        let mut l: Lookup =
            serde_json::from_value(value).context("lookup did not match the schema")?;
        l.tidy();
        Ok(l)
    });
    (answer.usage, parsed)
}

/// The worker: one request at a time, results back on `tx`. The config is read afresh
/// for every request, so a model or effort changed in the settings applies to the next
/// chunk without restarting the worker.
pub fn spawn_worker(
    cfg: watch::Receiver<LookupConfig>,
    mut rx: mpsc::UnboundedReceiver<LookupRequest>,
    tx: mpsc::UnboundedSender<LookupEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(req) = rx.recv().await {
            let _ = tx.send(LookupEvent::Started {
                chunk_id: req.chunk_id.clone(),
            });
            let current = cfg.borrow().clone();
            let (usage, result) = lookup(&current, &req).await;
            let ev = match result {
                Ok(lookup) => LookupEvent::Done {
                    chunk_id: req.chunk_id,
                    lookup,
                    usage,
                },
                Err(e) => LookupEvent::Failed {
                    chunk_id: req.chunk_id,
                    chunk_idx: req.chunk_idx,
                    error: format!("{e:#}"),
                    usage,
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

    fn req() -> LookupRequest {
        LookupRequest {
            chunk_id: "c".into(),
            chunk_idx: 1,
            chunk_text: "[00:05] mic: how does list print".into(),
            start: 5.0,
            end: 65.0,
            previous_text: Some("[00:01] mic: about the list command".into()),
            summaries_so_far: vec!["first".into()],
            facts_so_far: vec!["list uses awk (todo.sh:18)".into()],
            contradictions_so_far: vec![],
            questions_so_far: vec!["json lines or an array?".into()],
        }
    }

    #[test]
    fn the_schema_is_valid_json_and_the_prompt_carries_everything_shown_so_far() {
        let v: serde_json::Value = serde_json::from_str(SCHEMA).unwrap();
        assert_eq!(v["required"][1], "facts");
        assert_eq!(v["required"][2], "contradictions");
        assert_eq!(v["required"][3], "questions");
        let p = user_prompt(&req(), "### Meeting of 2026-09-04\nDecided X.\n", "brief\n");
        assert!(
            p.starts_with("## New transcript chunk #2 (00:05–01:05)\n[00:05] mic: how does list print\n"),
            "{p}"
        );
        assert!(p.contains("## The chunk before it (already summarized; for continuity only)\n[00:01] mic: about the list command\n"), "{p}");
        assert!(p.contains("[1] first"));
        assert!(p.contains("Facts:\n- list uses awk (todo.sh:18)\n"), "{p}");
        assert!(!p.contains("Contradictions:\n"), "an empty list is left out: {p}");
        assert!(p.contains("Questions:\n- json lines or an array?\n"), "{p}");
        assert!(p.contains("## What earlier meetings in this repository concluded\n### Meeting of 2026-09-04\nDecided X.\n"), "{p}");
        assert!(p.ends_with("## About the repository\nbrief\n"), "{p}");

        let mut first = req();
        first.previous_text = None;
        first.facts_so_far.clear();
        first.questions_so_far.clear();
        let p = user_prompt(&first, "", "brief\n");
        assert!(p.contains("for continuity only)\n(none — this is the first chunk)\n"), "{p}");
        assert!(p.contains("(do not repeat these)\n(none)\n"), "{p}");
        assert!(p.contains("concluded\n(none recorded)\n"), "{p}");

        let sys = system_prompt(Path::new("/w/my-app"));
        assert!(sys.contains("\"my-app\""));
        assert!(sys.contains("zero to 4 NEW facts"));
        assert!(sys.contains("zero to 3 places"));
        assert!(sys.contains("zero to 3 questions"));
    }

    #[test]
    fn a_lookup_parses_with_missing_optional_fields_and_tidy_trims_and_caps() {
        let l: Lookup = serde_json::from_str(r#"{"summary":"x","facts":[{"text":"t"}]}"#).unwrap();
        assert_eq!(l.facts[0].where_, "");
        assert!(l.contradictions.is_empty());
        assert!(l.questions.is_empty());
        let l: Lookup = serde_json::from_str(r#"{"summary":"x"}"#).unwrap();
        assert!(l.facts.is_empty() && l.contradictions.is_empty() && l.questions.is_empty());

        let mut l = Lookup {
            summary: "  s ".into(),
            facts: (0..6)
                .map(|i| Fact {
                    text: format!(" f{i} "),
                    where_: " a.rs:1 ".into(),
                })
                .collect(),
            contradictions: vec![
                Fact {
                    text: "  ".into(),
                    where_: String::new(),
                },
                Fact {
                    text: "said x; code does y".into(),
                    where_: "b.rs".into(),
                },
            ],
            questions: vec![" q1 ".into(), String::new(), "q2".into(), "q3".into(), "q4".into()],
        };
        l.tidy();
        assert_eq!(l.summary, "s");
        assert_eq!(l.facts.len(), MAX_FACTS);
        assert_eq!(l.facts[0].text, "f0");
        assert_eq!(l.facts[0].where_, "a.rs:1");
        assert_eq!(l.facts[0].line(), "f0 (a.rs:1)");
        assert_eq!(l.contradictions.len(), 1, "the blank one is dropped");
        assert_eq!(l.questions, vec!["q1", "q2", "q3"]);

        let facts = vec![Fact {
            text: "t".into(),
            where_: String::new(),
        }];
        let json = serde_json::to_string(&facts).unwrap();
        assert_eq!(json, r#"[{"text":"t","where":""}]"#);
        assert_eq!(serde_json::from_str::<Vec<Fact>>(&json).unwrap(), facts);
        assert_eq!(facts[0].line(), "t");
    }
}
