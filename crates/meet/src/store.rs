//! SQLite persistence: meetings (one per recording) with every transcript segment that was
//! spoken or typed, and the meeting's summary and write-up. One mutex-guarded connection —
//! the write volume is a few rows a minute.
//!
//! The `chunks` table (the live lookup's per-minute summaries and what it found for them)
//! and the `action_items` table the migrations create belonged to features meet no longer
//! has; they are left in place (and unread) so an existing database keeps its rows.

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
    // 3: the action items are written once the meeting ends, from the whole transcript,
    // so the meeting itself carries the summary they came from; a chunk keeps the facts
    // the live lookup found for it (a JSON array), so a past session's Related pane can
    // be read back.
    "
    ALTER TABLE meetings ADD COLUMN summary TEXT;
    ALTER TABLE chunks ADD COLUMN facts TEXT;
    ",
    // 4: the lookup also reports contradictions (what was said against what the code or
    // an earlier meeting shows) and the questions worth asking, kept per chunk like the
    // facts (JSON arrays), so the Related pane shows them per summary, live or read back.
    "
    ALTER TABLE chunks ADD COLUMN contradictions TEXT;
    ALTER TABLE chunks ADD COLUMN questions TEXT;
    ",
    // 5: besides the short summary, the meeting keeps the write-up the action-item writer
    // produces with it (Markdown: summary, key points, decisions, open questions, action
    // items), so the Summary pane can show it live and read it back for a past session.
    "
    ALTER TABLE meetings ADD COLUMN notes TEXT;
    ",
];

#[derive(Debug, Clone, PartialEq)]
pub struct MeetingRow {
    pub id: String,
    pub repo_path: String,
    pub meeting_dir: Option<String>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub segment_count: i64,
    /// The summary written when the meeting ended — the meeting's description.
    pub summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SegmentRow {
    pub source: String,
    pub text: String,
    pub start_secs: f64,
    pub end_secs: f64,
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

    /// The meeting's recording starts now: its start moves to this moment, so a launch
    /// that sat idle for a while before `r` is dated by when the talking began.
    pub fn restart_meeting(&self, id: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE meetings SET started_at = ?2 WHERE id = ?1",
            params![id, now()],
        )?;
        Ok(())
    }

    pub fn set_meeting_dir(&self, id: &str, dir: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE meetings SET meeting_dir = ?2 WHERE id = ?1",
            params![id, dir],
        )?;
        Ok(())
    }

    pub fn set_meeting_summary(&self, id: &str, summary: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE meetings SET summary = ?2 WHERE id = ?1",
            params![id, summary],
        )?;
        Ok(())
    }

    /// The meeting's write-up (Markdown), written with its summary.
    pub fn set_meeting_notes(&self, id: &str, notes: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE meetings SET notes = ?2 WHERE id = ?1",
            params![id, notes],
        )?;
        Ok(())
    }

    /// The write-up, when one was written (an empty one reads as none).
    pub fn meeting_notes(&self, id: &str) -> Result<Option<String>> {
        let notes: Option<String> = self
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT notes FROM meetings WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(notes.filter(|n| !n.trim().is_empty()))
    }

    pub fn end_meeting(&self, id: &str, segment_count: i64) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE meetings SET ended_at = ?2, segment_count = ?3 WHERE id = ?1",
            params![id, now(), segment_count],
        )?;
        Ok(())
    }

    /// Drop a meeting and everything under it.
    pub fn delete_meeting(&self, id: &str) -> Result<()> {
        self.conn
            .lock()
            .unwrap()
            .execute("DELETE FROM meetings WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Whether nothing was ever said in this meeting — a launch that was closed again
    /// without a recording.
    pub fn meeting_is_empty(&self, id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM segments WHERE meeting_id = ?1",
            params![id],
            |r| r.get(0),
        )?;
        Ok(n == 0)
    }

    /// Delete this repo's meetings that hold nothing at all — launches that were closed
    /// again without a word being said (or crashed before one was) — so opening and
    /// closing `meet` does not pile up empty sessions. Returns the ids dropped, so their
    /// live-file directories can go too.
    pub fn delete_empty_meetings(&self, repo_path: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let ids: Vec<String> = {
            let mut stmt = conn.prepare(
                "SELECT id FROM meetings m WHERE repo_path = ?1
                   AND NOT EXISTS (SELECT 1 FROM segments WHERE meeting_id = m.id)",
            )?;
            let rows = stmt.query_map(params![repo_path], |r| r.get(0))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        for id in &ids {
            conn.execute("DELETE FROM meetings WHERE id = ?1", params![id])?;
        }
        Ok(ids)
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
                    m.summary
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
                summary: r.get(6)?,
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // The last meet crashed: m1 was never ended.
        assert_eq!(store.close_stale_meetings("/repo").unwrap(), 1);
        let m2 = store.insert_meeting("/repo").unwrap();
        let _elsewhere = store.insert_meeting("/other").unwrap();

        let segs = store.list_segments(&m1).unwrap();
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].text, "hello there");
        assert_eq!(segs[1].source, "typed");

        let meetings = store.list_meetings("/repo").unwrap();
        assert_eq!(meetings.len(), 2, "other repos' meetings are not listed");
        assert_eq!(meetings[0].id, m2, "newest first");
        assert_eq!(meetings[1].id, m1);
        assert!(meetings[1].ended_at.is_some(), "stale meeting was closed");
        assert_eq!(meetings[1].segment_count, 2);
        assert_eq!(meetings[1].summary, None, "no summary until one is written");
        store.set_meeting_summary(&m1, "the whole thing").unwrap();
        let meetings = store.list_meetings("/repo").unwrap();
        assert_eq!(meetings[1].summary.as_deref(), Some("the whole thing"));
        assert_eq!(meetings[0].ended_at, None, "the live meeting stays open");
        assert_eq!(store.meeting_notes(&m1).unwrap(), None, "no write-up yet");
        store.set_meeting_notes(&m1, " \n").unwrap();
        assert_eq!(store.meeting_notes(&m1).unwrap(), None, "a blank write-up is none");
        store
            .set_meeting_notes(&m1, "## Summary\nGreetings.\n\n## Decisions\n- ship it\n")
            .unwrap();
        assert_eq!(
            store.meeting_notes(&m1).unwrap().as_deref(),
            Some("## Summary\nGreetings.\n\n## Decisions\n- ship it\n")
        );
        assert_eq!(store.meeting_notes("nope").unwrap(), None);
    }

    #[test]
    fn empty_meetings_are_dropped_and_ones_with_anything_in_them_are_kept() {
        let store = Store::open_in_memory().unwrap();
        let spoken = store.insert_meeting("/repo").unwrap();
        store
            .insert_segment(&spoken, "mic", "hello", 0.0, 1.0)
            .unwrap();
        let empty_open = store.insert_meeting("/repo").unwrap();
        let empty_ended = store.insert_meeting("/repo").unwrap();
        store.end_meeting(&empty_ended, 0).unwrap();
        let elsewhere = store.insert_meeting("/other").unwrap();
        assert!(store.meeting_is_empty(&empty_open).unwrap());
        assert!(!store.meeting_is_empty(&spoken).unwrap());

        let mut dropped = store.delete_empty_meetings("/repo").unwrap();
        dropped.sort();
        let mut expect = vec![empty_open.clone(), empty_ended.clone()];
        expect.sort();
        assert_eq!(dropped, expect, "open or ended, an empty meeting goes");
        let kept: Vec<String> = store
            .list_meetings("/repo")
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(kept, vec![spoken.clone()]);
        assert_eq!(
            store.list_meetings("/other").unwrap()[0].id,
            elsewhere,
            "another repo's empty meeting is not this repo's business"
        );
        assert!(store.delete_empty_meetings("/repo").unwrap().is_empty());

        store.delete_meeting(&spoken).unwrap();
        assert!(store.list_segments(&spoken).unwrap().is_empty(), "cascaded");
        assert!(store.list_meetings("/repo").unwrap().is_empty());
        assert!(
            store.meeting_is_empty("nope").unwrap(),
            "an unknown id holds nothing"
        );
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
        }
        let s = Store::open(&path).unwrap();
        let meetings = s.list_meetings("/r").unwrap();
        assert_eq!(meetings[0].summary, None);
        s.set_meeting_summary("m", "s").unwrap();
        assert_eq!(s.meeting_notes("m").unwrap(), None);
        s.set_meeting_notes("m", "## Summary\ns\n").unwrap();
        assert_eq!(s.meeting_notes("m").unwrap().as_deref(), Some("## Summary\ns\n"));
    }
}
