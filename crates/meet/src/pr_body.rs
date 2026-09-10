//! The pull request text. The agent is asked to write it from a template in nebula's house
//! style (an opener, a table of contents with explicit anchors, what changed, how it works,
//! a risk read, the technical overview, notes, the footer); when it does not, `fallback`
//! writes an honest one from what `meet` knows.

use crate::store::ActionItem;

pub const FOOTER: &str = "🤖 Generated with [Claude Code](https://claude.com/claude-code) · started from a live meeting by [meet](https://github.com/webdevcody/meeting-tracker)";

/// The template handed to the agent; `<…>` are placeholders it fills, `<!-- -->` guidance it drops.
pub fn template() -> String {
    format!(
        r#"<One or two sentences: what the meeting asked for and what now happens instead — the two things a reader needs before the diff.>

**Contents:** [1. What was asked](#1-what-was-asked) · [2. What changed](#2-what-changed) · [3. How it works](#3-how-it-works) · [4. Risk](#4-risk) · [5. Technical overview](#5-technical-overview) · [6. Notes](#6-notes)

## 1. What was asked <a id="1-what-was-asked"></a>

<Two or three sentences in the speaker's words: what they wanted and why. Quote the meeting where it helps.>

> <The action item as it was given, quoted verbatim.>

## 2. What changed <a id="2-what-changed"></a>

- **<Two-to-five-word hook>.** <The new behaviour for someone who has not read the diff: the command, flag or screen in backticks.>
- **<Hook>.** <…>
- **Unchanged.** <What the speaker likes and still has, exactly as it was.>

## 3. How it works <a id="3-how-it-works"></a>

<A short paragraph on the mechanism. When the change is a flow worth drawing — a new path through the system, a state that gained a transition — add a ```mermaid fence (flowchart LR / sequenceDiagram / stateDiagram-v2) with ten to twenty nodes; otherwise leave the diagram out.>

## 4. Risk <a id="4-risk"></a>

**Verdict:** <🟢 Low risk · 🟡 Merge with care · 🔴 Do not merge as-is — pick one, then one clause saying why. Written as the reviewer would, not as the seller.>

| | Level | Why |
|---|---|---|
| 🔒 **Security & production** | <Low / Medium / High> | <who can reach the new code and what it reaches, or "no new surface: <why>"> |
| ⚡ **Performance** | <Low / Medium / High> | <the hot path touched, or "off every hot path: <why>"> |
| 🧩 **Fit with the codebase** | <Low / Medium / High> | <the existing pattern it follows, or the departure and why> |

**Rollback:** <one line — `git revert` of the merge, plus anything the revert does not undo (a migration, a pushed branch).>

## 5. Technical overview <a id="5-technical-overview"></a>

- **Mechanism.** <Three or four sentences: the type, flag or function added, who owns the state, what triggers it.>
- **Files.** `<path>` — <one clause>; `<path>` — <one clause>.
- **Not done.** <The obvious alternative and why it was not taken, so the reviewer does not ask.>
- **Gate.** <What was run and passed (`swift build`, `cargo test`, N tests); or what could not run and why. Never imply a gate that did not run.>

## 6. Notes <a id="6-notes"></a>

- <Every assumption made where the meeting left a choice, one bullet each, starting "Assumed …".>
- <Anything the reviewer must do before or after merging.>

{FOOTER}
"#
    )
}

/// The body used when the agent left no `pr-body.md`.
pub fn fallback(item: &ActionItem, base: &str, commits: &[String], agent_summary: &str) -> String {
    let commit_lines = if commits.is_empty() {
        "- (no commits listed)".to_string()
    } else {
        commits
            .iter()
            .map(|c| format!("- {c}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let why = if item.why.trim().is_empty() {
        String::new()
    } else {
        format!("\n> {}\n", item.why.trim())
    };
    format!(
        r#"{title}: opened by `meet` from a live meeting. The agent did not write its own description, so this one is assembled from the task it was given and the commits it made against `{base}`.

**Contents:** [1. What was asked](#1-what-was-asked) · [2. Commits](#2-commits) · [3. Agent's last word](#3-agents-last-word) · [4. Risk](#4-risk)

## 1. What was asked <a id="1-what-was-asked"></a>
{why}
```text
{prompt}
```

## 2. Commits <a id="2-commits"></a>

{commit_lines}

## 3. Agent's last word <a id="3-agents-last-word"></a>

{summary}

## 4. Risk <a id="4-risk"></a>

**Verdict:** 🟡 Merge with care — the description was not written by the agent that made the change, so read the diff before trusting it.

| | Level | Why |
|---|---|---|
| 🔒 **Security & production** | Medium | unreviewed headless change |
| ⚡ **Performance** | Medium | not assessed |
| 🧩 **Fit with the codebase** | Medium | not assessed |

**Rollback:** `git revert` the merge commit; the branch stays pushed.

{footer}
"#,
        title = item.title.trim(),
        prompt = item.prompt.trim(),
        summary = if agent_summary.trim().is_empty() {
            "(none)"
        } else {
            agent_summary.trim()
        },
        footer = FOOTER,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::ItemStatus;

    #[test]
    fn template_toc_links_have_anchors_and_the_footer_is_last() {
        let t = template();
        for id in [
            "1-what-was-asked",
            "2-what-changed",
            "3-how-it-works",
            "4-risk",
            "5-technical-overview",
            "6-notes",
        ] {
            assert!(t.contains(&format!("](#{id})")), "link {id}");
            assert!(t.contains(&format!("<a id=\"{id}\"></a>")), "anchor {id}");
        }
        assert!(t.trim_end().ends_with(FOOTER));
    }

    #[test]
    fn fallback_is_honest_about_its_origin() {
        let item = ActionItem {
            id: "i".into(),
            meeting_id: "m".into(),
            chunk_id: None,
            repo_path: "/r".into(),
            title: "Add X".into(),
            prompt: "do x".into(),
            why: "they said x".into(),
            status: ItemStatus::Done,
            branch: None,
            worktree_path: None,
            pr_url: None,
            log_path: None,
            error: None,
            session_id: None,
            phase: crate::store::RunPhase::Agent,
            created_at: 0,
            updated_at: 0,
        };
        let body = fallback(&item, "main", &["Add x".into()], "did x");
        assert!(body.contains("did not write its own description"));
        assert!(body.contains("> they said x"));
        assert!(body.contains("- Add x"));
        assert!(body.trim_end().ends_with(FOOTER));
    }
}
