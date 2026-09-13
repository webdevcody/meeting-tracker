//! What earlier meetings in this repository concluded — the "already known" side of the
//! live lookup besides the code itself: each past meeting's summary, the facts and
//! contradictions its lookups turned up, and the action items on the board. Built once
//! when `meet` starts and sent with every lookup, so it is kept to a few dozen lines.

use crate::lookup::Fact;
use crate::store::{ActionItem, ChunkRow, ItemStatus, MeetingRow};
use crate::when::{human_duration, local_datetime};

/// How many past meetings the brief covers, newest first.
pub const MAX_MEETINGS: usize = 6;
const MAX_FACTS_PER_MEETING: usize = 6;
const MAX_CONTRADICTIONS_PER_MEETING: usize = 3;
const MAX_ITEMS: usize = 20;
const SUMMARY_CHARS: usize = 320;
const FACT_CHARS: usize = 160;

/// `meetings` newest first, each with its chunks; meetings with nothing concluded are
/// skipped. Empty when there is nothing to tell.
pub fn brief(meetings: &[(MeetingRow, Vec<ChunkRow>)], items: &[ActionItem]) -> String {
    let mut out = String::new();
    let mut shown = 0;
    for (m, chunks) in meetings {
        if shown == MAX_MEETINGS {
            break;
        }
        let summary = m
            .summary
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let facts: Vec<&Fact> = chunks.iter().flat_map(|c| c.facts.iter()).collect();
        let contradictions: Vec<&Fact> = chunks
            .iter()
            .flat_map(|c| c.contradictions.iter())
            .collect();
        if summary.is_none() && facts.is_empty() && contradictions.is_empty() {
            continue;
        }
        shown += 1;
        let length = match m.ended_at {
            Some(end) if end > m.started_at => format!(" ({})", human_duration(end - m.started_at)),
            _ => String::new(),
        };
        out.push_str(&format!(
            "### Meeting of {}{length}\n",
            local_datetime(m.started_at)
        ));
        if let Some(s) = summary {
            out.push_str(&crate::stream_json::excerpt(s, SUMMARY_CHARS));
            out.push('\n');
        }
        for f in facts.iter().take(MAX_FACTS_PER_MEETING) {
            out.push_str(&format!(
                "- {}\n",
                crate::stream_json::excerpt(&f.line(), FACT_CHARS)
            ));
        }
        for f in contradictions.iter().take(MAX_CONTRADICTIONS_PER_MEETING) {
            out.push_str(&format!(
                "- ⚠ {}\n",
                crate::stream_json::excerpt(&f.line(), FACT_CHARS)
            ));
        }
    }
    let board: Vec<&ActionItem> = items
        .iter()
        .filter(|i| i.status != ItemStatus::Dismissed)
        .take(MAX_ITEMS)
        .collect();
    if !board.is_empty() {
        out.push_str("### Action items on the board (from every meeting here)\n");
        for i in board {
            out.push_str(&format!("- {} ({})\n", i.title, i.status.as_str()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meeting(id: &str, started_at: i64, summary: Option<&str>) -> MeetingRow {
        MeetingRow {
            id: id.into(),
            repo_path: "/r".into(),
            meeting_dir: None,
            started_at,
            ended_at: Some(started_at + 600),
            segment_count: 3,
            summary: summary.map(Into::into),
        }
    }

    fn chunk(meeting_id: &str, facts: &[(&str, &str)], contradictions: &[(&str, &str)]) -> ChunkRow {
        let f = |(t, w): &(&str, &str)| Fact {
            text: (*t).into(),
            where_: (*w).into(),
        };
        ChunkRow {
            id: format!("{meeting_id}-c"),
            meeting_id: meeting_id.into(),
            idx: 0,
            start_secs: 0.0,
            end_secs: 60.0,
            text: "t".into(),
            summary: Some("s".into()),
            facts: facts.iter().map(f).collect(),
            contradictions: contradictions.iter().map(f).collect(),
            questions: vec!["q".into()],
        }
    }

    fn item(title: &str, status: ItemStatus) -> ActionItem {
        ActionItem {
            id: title.into(),
            meeting_id: "m".into(),
            chunk_id: None,
            repo_path: "/r".into(),
            title: title.into(),
            prompt: "p".into(),
            why: String::new(),
            status,
            branch: None,
            worktree_path: None,
            pr_url: None,
            log_path: None,
            error: None,
            session_id: None,
            phase: crate::store::RunPhase::Agent,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn the_brief_lists_what_past_meetings_concluded_and_skips_empty_ones() {
        assert_eq!(brief(&[], &[]), "");
        let meetings = vec![
            (meeting("empty", 1_700_000_000, None), vec![]),
            (
                meeting("m1", 1_700_000_000, Some("  Decided the config stays JSON.  ")),
                vec![chunk(
                    "m1",
                    &[("config.rs parses JSON", "config.rs:10")],
                    &[("said YAML; the parser is JSON-only", "config.rs:10")],
                )],
            ),
            (
                meeting("m2", 1_690_000_000, None),
                vec![chunk("m2", &[("list uses awk", "")], &[])],
            ),
        ];
        let items = vec![
            item("Add --json", ItemStatus::Suggested),
            item("Old thing", ItemStatus::Dismissed),
            item("Fix add", ItemStatus::Done),
        ];
        let b = brief(&meetings, &items);
        assert!(b.starts_with("### Meeting of "), "{b}");
        assert!(b.contains("(10m)"), "{b}");
        assert!(b.contains("Decided the config stays JSON.\n- config.rs parses JSON (config.rs:10)\n- ⚠ said YAML; the parser is JSON-only (config.rs:10)\n"), "{b}");
        assert!(b.contains("- list uses awk\n"), "a fact with no where: {b}");
        assert_eq!(b.matches("### Meeting of").count(), 2, "the empty meeting is skipped: {b}");
        assert!(b.ends_with("### Action items on the board (from every meeting here)\n- Add --json (suggested)\n- Fix add (done)\n"), "{b}");
        assert!(!b.contains("Old thing"), "dismissed items are left out");
    }

    #[test]
    fn the_brief_is_capped() {
        let meetings: Vec<(MeetingRow, Vec<ChunkRow>)> = (0..10)
            .map(|i| {
                let id = format!("m{i}");
                let facts: Vec<(&str, &str)> = vec![("f", "w"); 10];
                (meeting(&id, 1_700_000_000 - i * 1000, Some("s")), vec![chunk(&id, &facts, &facts)])
            })
            .collect();
        let items: Vec<ActionItem> = (0..30)
            .map(|i| item(&format!("item {i}"), ItemStatus::Suggested))
            .collect();
        let b = brief(&meetings, &items);
        assert_eq!(b.matches("### Meeting of").count(), MAX_MEETINGS);
        let first = b.split("### Meeting of").nth(1).unwrap();
        assert_eq!(first.matches("\n- f (w)").count(), MAX_FACTS_PER_MEETING);
        assert_eq!(first.matches("\n- ⚠ f (w)").count(), MAX_CONTRADICTIONS_PER_MEETING);
        assert_eq!(b.matches("\n- item ").count(), MAX_ITEMS);
    }
}
