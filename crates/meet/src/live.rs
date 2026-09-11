//! What a meeting leaves on disk as it goes, for anything outside the TUI to read — the
//! question session above all (`ask`). Under `<data dir>/sessions/<meeting-id>/`:
//!
//! - `transcript.md` — one line per segment, `[mm:ss] source: text`, appended the moment
//!   it lands (typed notes included), so a reader always sees the meeting up to now;
//! - `summary.md` — the recording state, the meeting's summary once written, the chunk
//!   summaries, the facts the lookup found and the action items on the board, rewritten
//!   whenever any of that changes.
//!
//! The database stays the source of truth; these are a readable projection of it.

use crate::app::{App, ChunkState, RecState, TranscriptLine, WrapUp};
use crate::chunker::clock;
use crate::store::{ActionItem, ChunkRow, ItemStatus, MeetingRow};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub const TRANSCRIPT_FILE: &str = "transcript.md";
pub const SUMMARY_FILE: &str = "summary.md";

pub struct LiveFiles {
    dir: PathBuf,
}

impl LiveFiles {
    /// Create the directory and start `transcript.md` with `header`.
    pub fn create(dir: PathBuf, header: &str) -> Result<Self> {
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let files = Self { dir };
        std::fs::write(files.transcript_path(), header)
            .with_context(|| format!("write {}", files.transcript_path().display()))?;
        Ok(files)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn transcript_path(&self) -> PathBuf {
        self.dir.join(TRANSCRIPT_FILE)
    }

    pub fn summary_path(&self) -> PathBuf {
        self.dir.join(SUMMARY_FILE)
    }

    /// One more transcript line, appended.
    pub fn append(&self, line: &TranscriptLine) -> Result<()> {
        use std::io::Write;
        let path = self.transcript_path();
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        writeln!(f, "{}", line.line()).with_context(|| format!("append to {}", path.display()))
    }

    /// Replace `summary.md` in one step (a temp file and a rename), so a reader never sees
    /// it half written.
    pub fn write_summary(&self, text: &str) -> Result<()> {
        let path = self.summary_path();
        let tmp = self.dir.join(format!(".{SUMMARY_FILE}.tmp"));
        std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("replace {}", path.display()))
    }
}

/// The first lines of `transcript.md`: what it is and how to read it.
pub fn transcript_header(repo_name: &str, started: &str) -> String {
    format!(
        "# Transcript · meet in {repo_name} · started {started}\n\
         One line per sentence as it was heard, `[mm:ss] source: text`, appended while the meeting goes on. \
         Sources: mic = this Mac's microphone (the developer and anyone in the room), \
         system = audio the Mac played (remote call participants, videos), typed = a note typed into meet.\n\n"
    )
}

/// `summary.md` from the state on screen.
pub fn summary_text(app: &App, transcript_path: &Path) -> String {
    let mut out = format!(
        "# meet · {} ({}) · this meeting\n",
        app.repo_name, app.base_branch
    );
    let state = match &app.rec {
        RecState::Starting => "starting (waiting for the recorder)".to_string(),
        RecState::Recording => format!("recording · {} so far", clock(app.elapsed())),
        RecState::Paused => format!("paused at {}", clock(app.elapsed())),
        RecState::Finalizing => format!("stopping · {} recorded", clock(app.elapsed())),
        RecState::Ended => format!("ended · {} recorded", clock(app.elapsed())),
        RecState::Failed(e) => format!("recorder failed after {} ({e})", clock(app.elapsed())),
    };
    out.push_str(&format!("state: {state}\n"));
    out.push_str(&format!(
        "transcript: {} ({} line{}; appended as people speak)\n",
        transcript_path.display(),
        app.transcript.len(),
        if app.transcript.len() == 1 { "" } else { "s" }
    ));
    if let Some(d) = &app.meeting_dir {
        out.push_str(&format!(
            "recording files: {d} (audio.m4a and the recorder's own transcript, written at the end and checkpointed about once a minute before that)\n"
        ));
    }
    out.push_str(&format!("repository: {}\n", app.repo.display()));

    out.push_str("\n## Meeting summary\n");
    match (&app.meeting_summary, &app.wrap_up) {
        (Some(s), _) => out.push_str(&format!("{s}\n")),
        (None, WrapUp::Writing) => out.push_str("(being written now that the recording stopped)\n"),
        (None, WrapUp::Failed(e)) => out.push_str(&format!("(could not be written: {e})\n")),
        (None, _) if app.suggest_disabled => out.push_str("(off: --no-suggest)\n"),
        (None, _) => out.push_str("(written when the recording stops)\n"),
    }

    out.push_str("\n## Summaries so far (about a minute of talk each)\n");
    if app.chunks.is_empty() {
        out.push_str(if app.lookup_disabled {
            "(off: --no-lookup)\n"
        } else {
            "(none yet)\n"
        });
    }
    for c in &app.chunks {
        let text = match (&c.state, &c.summary) {
            (_, Some(s)) => s.clone(),
            (ChunkState::Summarizing, None) => "(looking up…)".into(),
            (ChunkState::Queued, None) => "(queued)".into(),
            (ChunkState::Failed(e), None) => format!("(failed: {e})"),
            (ChunkState::Done, None) => "(no summary)".into(),
        };
        out.push_str(&format!(
            "[{}] {}–{} {text}\n",
            c.idx + 1,
            clock(c.start),
            clock(c.end)
        ));
    }

    out.push_str("\n## Facts looked up in the repository while talking\n");
    if app.facts.is_empty() {
        out.push_str("(none yet)\n");
    }
    for f in &app.facts {
        out.push_str(&format!("- {} {}\n", clock(f.at), f.line()));
    }

    out.push_str("\n## Action items on the board\n");
    if app.items.is_empty() {
        out.push_str("(none)\n");
    }
    for i in &app.items {
        let mut line = format!("- {} ({})", i.title, i.status.as_str());
        if i.status == ItemStatus::Done {
            if let Some(u) = &i.pr_url {
                line.push_str(&format!(" {u}"));
            }
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// `summary.md` for a meeting read back from the database (`meet ask` on a meeting that
/// was recorded before these files existed): the same sections, from its rows.
pub fn past_summary_text(
    m: &MeetingRow,
    repo_name: &str,
    transcript_path: &Path,
    lines: usize,
    chunks: &[ChunkRow],
    items: &[ActionItem],
) -> String {
    let mut out = format!(
        "# meet · {repo_name} · meeting of {}\n",
        crate::when::local_datetime(m.started_at)
    );
    match m.ended_at {
        Some(end) => out.push_str(&format!(
            "state: ended · {} recorded\n",
            clock((end - m.started_at) as f64)
        )),
        None => out.push_str("state: unknown (written from the database; not updated live)\n"),
    }
    out.push_str(&format!(
        "transcript: {} ({lines} line{})\n",
        transcript_path.display(),
        if lines == 1 { "" } else { "s" }
    ));
    if let Some(d) = &m.meeting_dir {
        out.push_str(&format!("recording files: {d} (audio.m4a and the recorder's own transcript)\n"));
    }
    out.push_str(&format!("repository: {}\n", m.repo_path));
    out.push_str("\n## Meeting summary\n");
    out.push_str(m.summary.as_deref().unwrap_or("(none was written)"));
    out.push('\n');
    out.push_str("\n## Summaries so far (about a minute of talk each)\n");
    if chunks.is_empty() {
        out.push_str("(none)\n");
    }
    for c in chunks {
        out.push_str(&format!(
            "[{}] {}–{} {}\n",
            c.idx + 1,
            clock(c.start_secs),
            clock(c.end_secs),
            c.summary.as_deref().unwrap_or("(no summary)")
        ));
    }
    out.push_str("\n## Facts looked up in the repository while talking\n");
    let facts: Vec<String> = chunks
        .iter()
        .flat_map(|c| {
            c.facts.iter().map(move |f| {
                let where_ = if f.where_.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", f.where_)
                };
                format!("- {} {}{where_}\n", clock(c.start_secs), f.text)
            })
        })
        .collect();
    if facts.is_empty() {
        out.push_str("(none)\n");
    }
    for f in facts {
        out.push_str(&f);
    }
    out.push_str("\n## Action items from this meeting\n");
    let mine: Vec<&ActionItem> = items.iter().filter(|i| i.meeting_id == m.id).collect();
    if mine.is_empty() {
        out.push_str("(none)\n");
    }
    for i in mine {
        out.push_str(&format!("- {} ({})", i.title, i.status.as_str()));
        if let Some(u) = i.pr_url.as_deref().filter(|_| i.status == ItemStatus::Done) {
            out.push_str(&format!(" {u}"));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunker::Limits;
    use crate::lookup::Fact;
    use crate::recorder::Segment;

    fn app() -> App {
        App::new(
            "/w/my-app".into(),
            "main".into(),
            false,
            Limits::default(),
            "m".into(),
            vec![],
        )
    }

    #[test]
    fn the_transcript_is_appended_and_the_summary_replaced_whole() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sessions").join("m");
        let files = LiveFiles::create(dir.clone(), &transcript_header("my-app", "2026-09-11 10:00")).unwrap();
        assert!(dir.is_dir());
        let t = std::fs::read_to_string(files.transcript_path()).unwrap();
        assert!(t.starts_with("# Transcript · meet in my-app · started 2026-09-11 10:00\n"), "{t}");
        assert!(t.ends_with("\n\n"), "a blank line before the first spoken line");

        files
            .append(&TranscriptLine {
                at: 3.0,
                source: "mic".into(),
                text: "does list print json".into(),
            })
            .unwrap();
        files
            .append(&TranscriptLine {
                at: 65.0,
                source: "typed".into(),
                text: "ship it".into(),
            })
            .unwrap();
        let t = std::fs::read_to_string(files.transcript_path()).unwrap();
        assert!(t.ends_with("[00:03] mic: does list print json\n[01:05] typed: ship it\n"), "{t}");

        files.write_summary("one\n").unwrap();
        files.write_summary("two\n").unwrap();
        assert_eq!(std::fs::read_to_string(files.summary_path()).unwrap(), "two\n");
        assert!(!dir.join(".summary.md.tmp").exists(), "the temp file was renamed away");
    }

    #[test]
    fn the_summary_says_where_things_stand() {
        let mut app = app();
        let path = Path::new("/data/sessions/m/transcript.md");
        let s = summary_text(&app, path);
        assert!(s.starts_with("# meet · my-app (main) · this meeting\nstate: starting"), "{s}");
        assert!(s.contains("transcript: /data/sessions/m/transcript.md (0 lines"), "{s}");
        assert!(s.contains("## Meeting summary\n(written when the recording stops)"), "{s}");
        assert!(s.contains("## Summaries so far (about a minute of talk each)\n(none yet)"), "{s}");
        assert!(s.contains("## Facts looked up in the repository while talking\n(none yet)"), "{s}");
        assert!(s.contains("## Action items on the board\n(none)"), "{s}");

        app.on_started("/m/2026".into(), vec!["mic".into()]);
        app.on_segment(Segment {
            source: "mic".into(),
            text: "hello".into(),
            start: 1.0,
            end: 2.0,
        });
        let chunk = app.chunker.flush().unwrap();
        app.add_chunk("c1".into(), &chunk);
        app.on_lookup_done(
            "c1",
            "said hello".into(),
            &[Fact {
                text: "README greets".into(),
                where_: "README.md:1".into(),
            }],
        );
        app.items.push(crate::store::ActionItem {
            id: "a".into(),
            meeting_id: "m".into(),
            chunk_id: None,
            repo_path: "/w/my-app".into(),
            title: "Add X".into(),
            prompt: "p".into(),
            why: String::new(),
            status: ItemStatus::Done,
            branch: None,
            worktree_path: None,
            pr_url: Some("https://x/pull/3".into()),
            log_path: None,
            error: None,
            session_id: None,
            phase: crate::store::RunPhase::Agent,
            created_at: 0,
            updated_at: 0,
        });
        let s = summary_text(&app, path);
        assert!(s.contains("state: recording · 00:0"), "{s}");
        assert!(s.contains("(1 line; appended"), "{s}");
        assert!(s.contains("recording files: /m/2026 (audio.m4a"), "{s}");
        assert!(s.contains("[1] 00:01–00:02 said hello\n"), "{s}");
        assert!(s.contains("- 00:01 README greets (README.md:1)\n"), "{s}");
        assert!(s.contains("- Add X (done) https://x/pull/3\n"), "{s}");

        app.on_finished();
        app.wrap_up = WrapUp::Writing;
        let s = summary_text(&app, path);
        assert!(s.contains("state: ended · 00:0"), "{s}");
        assert!(s.contains("(being written now that the recording stopped)"), "{s}");
        app.meeting_summary = Some("A short meeting.".into());
        assert!(summary_text(&app, path).contains("## Meeting summary\nA short meeting.\n"));
    }
}
