//! SQLite persistence: meetings (one per `meet` launch) with every transcript segment that
//! was spoken or typed, the transcript chunks with their summaries, and the action items
//! with everything an agent run leaves behind (branch, worktree, Claude session id, PR URL,
//! log). One mutex-guarded connection — the write volume is a few rows a minute.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::Mutex;

const MIGRATIONS: &[&str] = &[
    // 1: initial schema
    "
    CREATE TABLE meetings (
      id            TEXT PRIMARY KEY,
      repo_path     TEXT NOT NULL,
      meeting_dir   TEXT,
      started_at    INTEGER NOT NULL,
      ended_at      INTEGER,
      segment_count INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE chunks (
      id          TEXT PRIMARY KEY,
      meeting_id  TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
      idx         INTEGER NOT NULL,
      start_secs  REAL NOT NULL,
      end_secs    REAL NOT NULL,
      text        TEXT NOT NULL,
      summary     TEXT,
      created_at  INTEGER NOT NULL
    );
    CREATE TABLE action_items (
      id            TEXT PRIMARY KEY,
      meeting_id    TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
      chunk_id      TEXT,
      repo_path     TEXT NOT NULL,
      title         TEXT NOT NULL,
      prompt        TEXT NOT NULL,
      why           TEXT NOT NULL DEFAULT '',
      status        TEXT NOT NULL DEFAULT 'suggested',
      branch        TEXT,
      worktree_path TEXT,
      pr_url        TEXT,
      log_path      TEXT,
      error         TEXT,
      created_at    INTEGER NOT NULL,
      updated_at    INTEGER NOT NULL
    );
    CREATE INDEX action_items_repo ON action_items (repo_path, created_at);
    ",
    // 2: every transcript line is kept so a past session can be read back in the TUI;
    // an item remembers its Claude session id (for `claude --resume`) and which phase
    // of the run it was in, so an agent cut off by a quit or a crash carries on where
    // it was instead of starting over.
    "
    CREATE TABLE segments (
      id          INTEGER PRIMARY KEY AUTOINCREMENT,
      meeting_id  TEXT NOT NULL REFERENCES meetings(id) ON DELETE CASCADE,
      source      TEXT NOT NULL,
      text        TEXT NOT NULL,
      start_secs  REAL NOT NULL,
      end_secs    REAL NOT NULL,
      created_at  INTEGER NOT NULL
    );
    CREATE INDEX segments_meeting ON segments (meeting_id, id);
    ALTER TABLE action_items ADD COLUMN session_id TEXT;
    ALTER TABLE action_items ADD COLUMN phase TEXT NOT NULL DEFAULT 'agent';
    ",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemStatus {
    /// Proposed by the suggester; Enter runs it.
    Suggested,
    /// An agent is working in its worktree.
    Running,
    /// The user stopped its agent (X). Enter resumes the conversation where it was.
    Stopped,
    /// The agent finished and the pull request is open.
    Done,
    /// The run stopped short; `error` says where. Enter tries again from where it got to.
    Failed,
    /// Hidden by the user.
    Dismissed,
}

impl ItemStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Suggested => "suggested",
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Dismissed => "dismissed",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "running" => Self::Running,
            "stopped" => Self::Stopped,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "dismissed" => Self::Dismissed,
            _ => Self::Suggested,
        }
    }

    /// Enter starts (or resumes) the item from here.
    pub fn runnable(self) -> bool {
        matches!(self, Self::Suggested | Self::Stopped | Self::Failed)
    }
}

/// Where a run is, so an interrupted one knows what is left to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunPhase {
    /// The agent is (or was) working; resuming means resuming its conversation.
    Agent,
    /// The agent is done; what remains is meet's own commit → push → pull request tail.
    Publish,
}

impl RunPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Publish => "publish",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "publish" => Self::Publish,
            _ => Self::Agent,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ActionItem {
    pub id: String,
    pub meeting_id: String,
    pub chunk_id: Option<String>,
    pub repo_path: String,
    pub title: String,
    pub prompt: String,
    pub why: String,
    pub status: ItemStatus,
    pub branch: Option<String>,
    pub worktree_path: Option<String>,
    pub pr_url: Option<String>,
    pub log_path: Option<String>,
    pub error: Option<String>,
    /// The Claude Code session the agent ran in, once its stream reported one.
    pub session_id: Option<String>,
    pub phase: RunPhase,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChunkRow {
    pub id: String,
    pub meeting_id: String,
    pub idx: i64,
    pub start_secs: f64,
    pub end_secs: f64,
    pub text: String,
    pub summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MeetingRow {
    pub id: String,
    pub repo_path: String,
    pub meeting_dir: Option<String>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub segment_count: i64,
    /// The first chunk summary, as the meeting's one-line description.
    pub first_summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SegmentRow {
    pub source: String,
    pub text: String,
    pub start_secs: f64,
    pub end_secs: f64,
}

/// What an agent run writes back when it ends.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunOutcome {
    pub branch: Option<String>,
    pub worktree_path: Option<String>,
    pub pr_url: Option<String>,
    pub log_path: Option<String>,
    pub error: Option<String>,
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn new_id() -> String {
    ulid::Ulid::generate().to_string().to_lowercase()
}

const ITEM_COLUMNS: &str = "id, meeting_id, chunk_id, repo_path, title, prompt, why, status, branch, worktree_path,
                    pr_url, log_path, error, created_at, updated_at, session_id, phase";

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            conn.execute_batch(sql)
                .with_context(|| format!("migration {}", i + 1))?;
            conn.pragma_update(None, "user_version", (i + 1) as i64)?;
        }
        Ok(())
    }

    // ----- meetings -----

    pub fn insert_meeting(&self, repo_path: &str) -> Result<String> {
        let id = new_id();
        self.conn.lock().unwrap().execute(
            "INSERT INTO meetings (id, repo_path, started_at) VALUES (?1, ?2, ?3)",
            params![id, repo_path, now()],
        )?;
        Ok(id)
    }

    pub fn set_meeting_dir(&self, id: &str, dir: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE meetings SET meeting_dir = ?2 WHERE id = ?1",
            params![id, dir],
        )?;
        Ok(())
    }

    pub fn end_meeting(&self, id: &str, segment_count: i64) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE meetings SET ended_at = ?2, segment_count = ?3 WHERE id = ?1",
            params![id, now(), segment_count],
        )?;
        Ok(())
    }

    /// A meeting the last `meet` never closed (it crashed, or was killed) ends when its
    /// last line was spoken, so the session list does not show it as still going.
    pub fn close_stale_meetings(&self, repo_path: &str) -> Result<usize> {
        let n = self.conn.lock().unwrap().execute(
            "UPDATE meetings
             SET ended_at = COALESCE((SELECT MAX(created_at) FROM segments WHERE meeting_id = meetings.id), started_at),
                 segment_count = (SELECT COUNT(*) FROM segments WHERE meeting_id = meetings.id)
             WHERE repo_path = ?1 AND ended_at IS NULL",
            params![repo_path],
        )?;
        Ok(n)
    }

    /// Every meeting held in this repo, newest first.
    pub fn list_meetings(&self, repo_path: &str) -> Result<Vec<MeetingRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT m.id, m.repo_path, m.meeting_dir, m.started_at, m.ended_at,
                    CASE WHEN m.ended_at IS NULL
                         THEN (SELECT COUNT(*) FROM segments WHERE meeting_id = m.id)
                         ELSE m.segment_count END,
                    (SELECT summary FROM chunks WHERE meeting_id = m.id AND summary IS NOT NULL
                       ORDER BY idx LIMIT 1)
             FROM meetings m WHERE m.repo_path = ?1
             ORDER BY m.started_at DESC, m.rowid DESC LIMIT 500",
        )?;
        let rows = stmt.query_map(params![repo_path], |r| {
            Ok(MeetingRow {
                id: r.get(0)?,
                repo_path: r.get(1)?,
                meeting_dir: r.get(2)?,
                started_at: r.get(3)?,
                ended_at: r.get(4)?,
                segment_count: r.get(5)?,
                first_summary: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    // ----- segments -----

    pub fn insert_segment(
        &self,
        meeting_id: &str,
        source: &str,
        text: &str,
        start_secs: f64,
        end_secs: f64,
    ) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO segments (meeting_id, source, text, start_secs, end_secs, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![meeting_id, source, text, start_secs, end_secs, now()],
        )?;
        Ok(())
    }

    /// A meeting's transcript in the order it was heard.
    pub fn list_segments(&self, meeting_id: &str) -> Result<Vec<SegmentRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT source, text, start_secs, end_secs FROM segments
             WHERE meeting_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![meeting_id], |r| {
            Ok(SegmentRow {
                source: r.get(0)?,
                text: r.get(1)?,
                start_secs: r.get(2)?,
                end_secs: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    // ----- chunks -----

    pub fn insert_chunk(
        &self,
        meeting_id: &str,
        idx: i64,
        start_secs: f64,
        end_secs: f64,
        text: &str,
    ) -> Result<String> {
        let id = new_id();
        self.conn.lock().unwrap().execute(
            "INSERT INTO chunks (id, meeting_id, idx, start_secs, end_secs, text, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![id, meeting_id, idx, start_secs, end_secs, text, now()],
        )?;
        Ok(id)
    }

    pub fn set_chunk_summary(&self, id: &str, summary: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE chunks SET summary = ?2 WHERE id = ?1",
            params![id, summary],
        )?;
        Ok(())
    }

    pub fn get_chunk(&self, id: &str) -> Result<Option<ChunkRow>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, meeting_id, idx, start_secs, end_secs, text, summary FROM chunks WHERE id = ?1",
            params![id],
            row_to_chunk,
        )
        .optional()
        .map_err(Into::into)
    }

    /// A meeting's chunks in order, summaries included.
    pub fn list_chunks(&self, meeting_id: &str) -> Result<Vec<ChunkRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, meeting_id, idx, start_secs, end_secs, text, summary FROM chunks
             WHERE meeting_id = ?1 ORDER BY idx",
        )?;
        let rows = stmt.query_map(params![meeting_id], row_to_chunk)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    // ----- action items -----

    pub fn insert_item(
        &self,
        meeting_id: &str,
        chunk_id: Option<&str>,
        repo_path: &str,
        title: &str,
        prompt: &str,
        why: &str,
    ) -> Result<ActionItem> {
        let id = new_id();
        let ts = now();
        self.conn.lock().unwrap().execute(
            "INSERT INTO action_items (id, meeting_id, chunk_id, repo_path, title, prompt, why, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'suggested', ?8, ?8)",
            params![id, meeting_id, chunk_id, repo_path, title, prompt, why, ts],
        )?;
        Ok(ActionItem {
            id,
            meeting_id: meeting_id.to_string(),
            chunk_id: chunk_id.map(str::to_string),
            repo_path: repo_path.to_string(),
            title: title.to_string(),
            prompt: prompt.to_string(),
            why: why.to_string(),
            status: ItemStatus::Suggested,
            branch: None,
            worktree_path: None,
            pr_url: None,
            log_path: None,
            error: None,
            session_id: None,
            phase: RunPhase::Agent,
            created_at: ts,
            updated_at: ts,
        })
    }

    pub fn set_item_status(&self, id: &str, status: ItemStatus) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE action_items SET status = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, status.as_str(), now()],
        )?;
        Ok(())
    }

    pub fn set_item_session(&self, id: &str, session_id: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE action_items SET session_id = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, session_id, now()],
        )?;
        Ok(())
    }

    pub fn set_item_phase(&self, id: &str, phase: RunPhase) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE action_items SET phase = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, phase.as_str(), now()],
        )?;
        Ok(())
    }

    /// Status plus whatever the run learned; `None` fields are left as they were.
    pub fn set_item_outcome(&self, id: &str, status: ItemStatus, out: &RunOutcome) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE action_items SET status = ?2, updated_at = ?3,
               branch = COALESCE(?4, branch),
               worktree_path = COALESCE(?5, worktree_path),
               pr_url = COALESCE(?6, pr_url),
               log_path = COALESCE(?7, log_path),
               error = ?8
             WHERE id = ?1",
            params![
                id,
                status.as_str(),
                now(),
                out.branch,
                out.worktree_path,
                out.pr_url,
                out.log_path,
                out.error
            ],
        )?;
        Ok(())
    }

    /// Every item for the repo that is not dismissed, newest first. A row still `running`
    /// is one whose agent the last `meet` took down with it (quit or crash); it stays
    /// `running` so the next launch resumes it — see `interrupted_items`.
    pub fn list_items(&self, repo_path: &str) -> Result<Vec<ActionItem>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {ITEM_COLUMNS}
             FROM action_items WHERE repo_path = ?1 AND status != 'dismissed'
             ORDER BY created_at DESC, id DESC LIMIT 500"
        ))?;
        let rows = stmt.query_map(params![repo_path], row_to_item)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Items whose agent was running when the last `meet` exited, oldest first.
    pub fn interrupted_items(&self, repo_path: &str) -> Result<Vec<ActionItem>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {ITEM_COLUMNS}
             FROM action_items WHERE repo_path = ?1 AND status = 'running'
             ORDER BY updated_at ASC, id ASC"
        ))?;
        let rows = stmt.query_map(params![repo_path], row_to_item)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// `running` rows become `stopped`: what `--no-resume` does instead of resuming them.
    pub fn stop_interrupted_items(&self, repo_path: &str) -> Result<usize> {
        let n = self.conn.lock().unwrap().execute(
            "UPDATE action_items SET status = 'stopped', updated_at = ?2,
               error = 'meet exited while the agent was running'
             WHERE repo_path = ?1 AND status = 'running'",
            params![repo_path, now()],
        )?;
        Ok(n)
    }

    #[cfg(test)]
    pub fn get_item(&self, id: &str) -> Result<Option<ActionItem>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            &format!("SELECT {ITEM_COLUMNS} FROM action_items WHERE id = ?1"),
            params![id],
            row_to_item,
        )
        .optional()
        .map_err(Into::into)
    }
}

fn row_to_chunk(r: &rusqlite::Row<'_>) -> rusqlite::Result<ChunkRow> {
    Ok(ChunkRow {
        id: r.get(0)?,
        meeting_id: r.get(1)?,
        idx: r.get(2)?,
        start_secs: r.get(3)?,
        end_secs: r.get(4)?,
        text: r.get(5)?,
        summary: r.get(6)?,
    })
}

fn row_to_item(r: &rusqlite::Row<'_>) -> rusqlite::Result<ActionItem> {
    Ok(ActionItem {
        id: r.get(0)?,
        meeting_id: r.get(1)?,
        chunk_id: r.get(2)?,
        repo_path: r.get(3)?,
        title: r.get(4)?,
        prompt: r.get(5)?,
        why: r.get(6)?,
        status: ItemStatus::parse(&r.get::<_, String>(7)?),
        branch: r.get(8)?,
        worktree_path: r.get(9)?,
        pr_url: r.get(10)?,
        log_path: r.get(11)?,
        error: r.get(12)?,
        created_at: r.get(13)?,
        updated_at: r.get(14)?,
        session_id: r.get(15)?,
        phase: RunPhase::parse(&r.get::<_, String>(16)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_round_trip_and_running_rows_survive_a_reload() {
        let store = Store::open_in_memory().unwrap();
        let meeting = store.insert_meeting("/repo").unwrap();
        let chunk = store.insert_chunk(&meeting, 0, 0.0, 30.0, "hello").unwrap();
        store.set_chunk_summary(&chunk, "said hello").unwrap();
        assert_eq!(
            store.get_chunk(&chunk).unwrap().unwrap().summary.as_deref(),
            Some("said hello")
        );

        let a = store
            .insert_item(
                &meeting,
                Some(&chunk),
                "/repo",
                "Add X",
                "do x",
                "they said x",
            )
            .unwrap();
        let b = store
            .insert_item(&meeting, None, "/repo", "Add Y", "do y", "")
            .unwrap();
        let _other = store
            .insert_item(&meeting, None, "/elsewhere", "Z", "z", "")
            .unwrap();
        store.set_item_status(&b.id, ItemStatus::Running).unwrap();
        store.set_item_session(&b.id, "sid-1").unwrap();
        store.set_item_status(&a.id, ItemStatus::Dismissed).unwrap();

        let items = store.list_items("/repo").unwrap();
        assert_eq!(items.len(), 1, "dismissed and other-repo rows are hidden");
        assert_eq!(items[0].id, b.id);
        assert_eq!(
            items[0].status,
            ItemStatus::Running,
            "a running row is kept for the next launch to resume"
        );
        assert_eq!(items[0].session_id.as_deref(), Some("sid-1"));
        assert_eq!(items[0].phase, RunPhase::Agent);

        let interrupted = store.interrupted_items("/repo").unwrap();
        assert_eq!(interrupted.len(), 1);
        assert_eq!(interrupted[0].id, b.id);

        store.set_item_phase(&b.id, RunPhase::Publish).unwrap();
        assert_eq!(
            store.get_item(&b.id).unwrap().unwrap().phase,
            RunPhase::Publish
        );

        store
            .set_item_outcome(
                &b.id,
                ItemStatus::Done,
                &RunOutcome {
                    branch: Some("add-y".into()),
                    worktree_path: Some("/repo-worktrees/add-y".into()),
                    pr_url: Some("https://github.com/o/r/pull/7".into()),
                    log_path: None,
                    error: None,
                },
            )
            .unwrap();
        let b2 = store.get_item(&b.id).unwrap().unwrap();
        assert_eq!(b2.status, ItemStatus::Done);
        assert_eq!(b2.pr_url.as_deref(), Some("https://github.com/o/r/pull/7"));
        assert_eq!(b2.error, None, "a successful outcome clears the old error");
        assert!(store.interrupted_items("/repo").unwrap().is_empty());
        store.end_meeting(&meeting, 12).unwrap();
    }

    #[test]
    fn no_resume_turns_running_rows_into_stopped_ones() {
        let store = Store::open_in_memory().unwrap();
        let meeting = store.insert_meeting("/repo").unwrap();
        let a = store
            .insert_item(&meeting, None, "/repo", "A", "a", "")
            .unwrap();
        store.set_item_status(&a.id, ItemStatus::Running).unwrap();
        assert_eq!(store.stop_interrupted_items("/repo").unwrap(), 1);
        let a2 = store.get_item(&a.id).unwrap().unwrap();
        assert_eq!(a2.status, ItemStatus::Stopped);
        assert!(a2.status.runnable(), "Enter resumes a stopped item");
        assert!(a2.error.unwrap().contains("meet exited"));
    }

    #[test]
    fn meetings_keep_their_transcript_and_close_when_stale() {
        let store = Store::open_in_memory().unwrap();
        let m1 = store.insert_meeting("/repo").unwrap();
        store
            .insert_segment(&m1, "mic", "hello there", 0.0, 1.5)
            .unwrap();
        store
            .insert_segment(&m1, "typed", "ship it", 4.0, 4.0)
            .unwrap();
        let c = store.insert_chunk(&m1, 0, 0.0, 4.0, "…").unwrap();
        store.set_chunk_summary(&c, "greetings and a decision").unwrap();
        // The last meet crashed: m1 was never ended.
        assert_eq!(store.close_stale_meetings("/repo").unwrap(), 1);
        let m2 = store.insert_meeting("/repo").unwrap();
        let _elsewhere = store.insert_meeting("/other").unwrap();

        let segs = store.list_segments(&m1).unwrap();
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].text, "hello there");
        assert_eq!(segs[1].source, "typed");
        assert_eq!(store.list_chunks(&m1).unwrap().len(), 1);

        let meetings = store.list_meetings("/repo").unwrap();
        assert_eq!(meetings.len(), 2, "other repos' meetings are not listed");
        assert_eq!(meetings[0].id, m2, "newest first");
        assert_eq!(meetings[1].id, m1);
        assert!(meetings[1].ended_at.is_some(), "stale meeting was closed");
        assert_eq!(meetings[1].segment_count, 2);
        assert_eq!(
            meetings[1].first_summary.as_deref(),
            Some("greetings and a decision")
        );
        assert_eq!(meetings[0].ended_at, None, "the live meeting stays open");
    }

    #[test]
    fn migrations_are_idempotent_on_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meet.db");
        {
            let s = Store::open(&path).unwrap();
            s.insert_meeting("/r").unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert!(s.list_items("/r").unwrap().is_empty());
        assert_eq!(s.list_meetings("/r").unwrap().len(), 1);
    }

    #[test]
    fn a_v1_database_upgrades_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meet.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATIONS[0]).unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
            conn.execute(
                "INSERT INTO meetings (id, repo_path, started_at) VALUES ('m', '/r', 1)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO action_items (id, meeting_id, repo_path, title, prompt, status, created_at, updated_at)
                 VALUES ('i', 'm', '/r', 'T', 'p', 'running', 1, 1)",
                [],
            )
            .unwrap();
        }
        let s = Store::open(&path).unwrap();
        let items = s.list_items("/r").unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].session_id, None);
        assert_eq!(items[0].phase, RunPhase::Agent);
        assert_eq!(items[0].status, ItemStatus::Running);
    }
}
