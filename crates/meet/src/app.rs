//! The TUI's state and the pure transitions on it. The event loop feeds it recorder,
//! suggester and runner events and key presses; `ui` draws it.
//!
//! Two views share the screen: the live meeting (what is being said now), and a past
//! session picked from the list — its transcript and summaries on the left, and on the
//! right either the facts the live lookup found (Related) or the action items it produced,
//! with any agent still running on them updating live. Tab flips the right pane.

use crate::chunker::{Chunk, Chunker, Limits};
use crate::lookup::Fact;
use crate::recorder::Segment;
use crate::store::{ActionItem, ItemStatus, MeetingRow, RunOutcome, RunPhase};
use crate::theme::Theme;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub enum RecState {
    /// Waiting for the engine's `started` (model download, permissions).
    Starting,
    Recording,
    Paused,
    /// Stop requested; the engine is finalizing the transcript and running hooks.
    Finalizing,
    Ended,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptLine {
    pub at: f64,
    pub source: String,
    pub text: String,
}

impl TranscriptLine {
    /// `[mm:ss] source: text` — how a line is quoted to Claude and written to disk.
    pub fn line(&self) -> String {
        format!("[{}] {}: {}", crate::chunker::clock(self.at), self.source, self.text)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChunkState {
    Queued,
    Summarizing,
    Done,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChunkView {
    pub id: String,
    pub idx: usize,
    pub start: f64,
    pub end: f64,
    pub words: usize,
    pub summary: Option<String>,
    pub state: ChunkState,
}

/// One fact the live lookup found, tagged with the chunk it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct FactView {
    pub chunk_idx: usize,
    /// Recording time the chunk started.
    pub at: f64,
    pub text: String,
    pub where_: String,
}

impl FactView {
    /// `text (where)` — how the fact is quoted back to Claude.
    pub fn line(&self) -> String {
        if self.where_.is_empty() {
            self.text.clone()
        } else {
            format!("{} ({})", self.text, self.where_)
        }
    }
}

pub fn fact_views(chunk_idx: usize, at: f64, facts: &[Fact]) -> Vec<FactView> {
    facts
        .iter()
        .map(|f| FactView {
            chunk_idx,
            at,
            text: f.text.clone(),
            where_: f.where_.clone(),
        })
        .collect()
}

/// What the right column shows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RightPane {
    /// The facts the live lookup found — the default while recording.
    Related,
    /// The action items and the selected one's prompt — the default once it ended.
    Items,
}

/// Where the end-of-meeting action items are.
#[derive(Debug, Clone, PartialEq)]
pub enum WrapUp {
    /// The recording is still going (or was skipped).
    NotYet,
    Writing,
    Done,
    Failed(String),
}

/// A past session loaded for viewing.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionView {
    pub meeting: MeetingRow,
    pub transcript: Vec<TranscriptLine>,
    pub chunks: Vec<ChunkView>,
    pub facts: Vec<FactView>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Overlay {
    /// The tail of an agent's log, refreshed while open.
    Log {
        item_id: String,
        lines: Vec<String>,
        scroll: usize,
        follow: bool,
    },
    /// A note typed into the transcript.
    Note {
        input: String,
    },
    /// `live`: the recording is still going, so quitting skips the action items.
    ConfirmQuit {
        running: usize,
        live: bool,
    },
    /// Stop the agent working on this item?
    ConfirmStop {
        item_id: String,
    },
    /// Stop the recording (and write the action items)?
    ConfirmEnd,
    /// The session list; `cursor` indexes `App::sessions` (0 is the live meeting).
    Sessions {
        cursor: usize,
    },
    Help,
}

pub struct App {
    pub theme: Theme,
    pub repo: PathBuf,
    pub repo_name: String,
    pub base_branch: String,
    pub replay: bool,
    pub rec: RecState,
    pub status_text: Option<String>,
    /// This launch's meeting.
    pub meeting_id: String,
    pub meeting_dir: Option<String>,
    pub sources: Vec<String>,
    pub counts: HashMap<String, usize>,
    started: Option<Instant>,
    paused_accum: Duration,
    pause_started: Option<Instant>,
    elapsed_at_end: Option<f64>,
    /// Replay: the transcript's own clock, from the source's `Clock` events.
    replay_clock: Option<f64>,
    pub transcript: Vec<TranscriptLine>,
    /// `None` follows the newest line; `Some(n)` is a manual scroll position (in lines).
    pub transcript_scroll: Option<usize>,
    pub chunker: Chunker,
    pub chunks: Vec<ChunkView>,
    /// What the lookup found for the live meeting, in the order it landed.
    pub facts: Vec<FactView>,
    /// `None` follows the newest fact; `Some(n)` is a manual scroll position (in lines).
    pub related_scroll: Option<usize>,
    pub right_pane: RightPane,
    pub wrap_up: WrapUp,
    /// The live meeting's summary, once its action items were written.
    pub meeting_summary: Option<String>,
    /// Every session held in this repo, newest first; index 0 is the live one.
    pub sessions: Vec<MeetingRow>,
    /// A past session on screen instead of the live meeting.
    pub session: Option<SessionView>,
    /// Newest first, every meeting of this repo.
    pub items: Vec<ActionItem>,
    /// Index into `visible_items()`.
    pub selected: usize,
    pub detail_scroll: usize,
    /// item id → what its agent is doing right now.
    pub activity: HashMap<String, String>,
    pub overlay: Option<Overlay>,
    pub flash: Option<(String, Instant, bool)>,
    /// Engine stderr, hook output, lookup and suggester errors — the last few hundred lines.
    pub log: Vec<String>,
    /// `--no-lookup`: no live lookups (the action items are still written at the end).
    pub lookup_disabled: bool,
    /// `--no-suggest`: transcribe only.
    pub suggest_disabled: bool,
    pub dirty: bool,
    pub should_quit: bool,
    /// The on-disk `summary.md` (see `live`) no longer matches what is on screen.
    pub live_stale: bool,
    /// Where the question session (`a`) was opened, or the command that opens one.
    pub ask_where: Option<String>,
}

pub const FLASH_FOR: Duration = Duration::from_secs(6);

impl App {
    pub fn new(
        repo: PathBuf,
        base_branch: String,
        replay: bool,
        limits: Limits,
        meeting_id: String,
        items: Vec<ActionItem>,
    ) -> Self {
        let repo_name = crate::git::repo_name(&repo);
        Self {
            theme: Theme::default(),
            repo,
            repo_name,
            base_branch,
            replay,
            rec: RecState::Starting,
            status_text: None,
            meeting_id,
            meeting_dir: None,
            sources: Vec::new(),
            counts: HashMap::new(),
            started: None,
            paused_accum: Duration::ZERO,
            pause_started: None,
            elapsed_at_end: None,
            replay_clock: None,
            transcript: Vec::new(),
            transcript_scroll: None,
            chunker: Chunker::new(limits),
            chunks: Vec::new(),
            facts: Vec::new(),
            related_scroll: None,
            right_pane: RightPane::Related,
            wrap_up: WrapUp::NotYet,
            meeting_summary: None,
            sessions: Vec::new(),
            session: None,
            items,
            selected: 0,
            detail_scroll: 0,
            activity: HashMap::new(),
            overlay: None,
            flash: None,
            log: Vec::new(),
            lookup_disabled: false,
            suggest_disabled: false,
            dirty: true,
            should_quit: false,
            live_stale: true,
            ask_where: None,
        }
    }

    // ----- clock -----

    pub fn elapsed(&self) -> f64 {
        if let Some(e) = self.elapsed_at_end {
            return e;
        }
        if let Some(c) = self.replay_clock {
            return c;
        }
        let Some(started) = self.started else {
            return 0.0;
        };
        let mut e = started.elapsed().saturating_sub(self.paused_accum);
        if let Some(p) = self.pause_started {
            e = e.saturating_sub(p.elapsed());
        }
        e.as_secs_f64()
    }

    pub fn is_live(&self) -> bool {
        matches!(
            self.rec,
            RecState::Starting | RecState::Recording | RecState::Paused
        )
    }

    // ----- recorder events -----

    pub fn on_started(&mut self, meeting_dir: String, sources: Vec<String>) {
        self.rec = RecState::Recording;
        self.started = Some(Instant::now());
        self.sources = sources;
        if !meeting_dir.is_empty() {
            self.meeting_dir = Some(meeting_dir);
        }
        self.status_text = None;
        self.dirty = true;
        self.live_stale = true;
    }

    pub fn on_status(&mut self, text: String) {
        self.status_text = if text.trim().is_empty() {
            None
        } else {
            Some(text)
        };
        self.dirty = true;
    }

    /// Adds the segment to the transcript and returns a chunk when it closes one.
    pub fn on_segment(&mut self, seg: Segment) -> Option<Chunk> {
        *self.counts.entry(seg.source.clone()).or_default() += 1;
        self.transcript.push(TranscriptLine {
            at: seg.start,
            source: seg.source.clone(),
            text: seg.text.clone(),
        });
        self.dirty = true;
        self.chunker.push(seg)
    }

    pub fn on_clock(&mut self, secs: f64) {
        self.replay_clock = Some(secs);
    }

    pub fn on_paused(&mut self, paused: bool) {
        match (paused, self.pause_started) {
            (true, None) => {
                self.pause_started = Some(Instant::now());
                self.rec = RecState::Paused;
            }
            (false, Some(p)) => {
                self.paused_accum += p.elapsed();
                self.pause_started = None;
                self.rec = RecState::Recording;
            }
            _ => {}
        }
        self.dirty = true;
        self.live_stale = true;
    }

    pub fn on_finished(&mut self) -> Option<Chunk> {
        if let Some(p) = self.pause_started.take() {
            self.paused_accum += p.elapsed();
        }
        self.elapsed_at_end = Some(self.elapsed());
        self.rec = RecState::Ended;
        self.status_text = None;
        self.dirty = true;
        self.live_stale = true;
        self.chunker.flush()
    }

    pub fn on_engine_exited(&mut self, code: Option<i32>) -> Option<Chunk> {
        if self.rec == RecState::Ended {
            return None;
        }
        let tail = self
            .log
            .iter()
            .rev()
            .find(|l| l.starts_with('⚠') || l.to_lowercase().contains("error"))
            .cloned()
            .unwrap_or_default();
        self.elapsed_at_end = Some(self.elapsed());
        self.rec = RecState::Failed(format!(
            "recorder exited{}{}",
            code.map(|c| format!(" with status {c}"))
                .unwrap_or_default(),
            if tail.is_empty() {
                String::new()
            } else {
                format!(": {tail}")
            }
        ));
        self.dirty = true;
        self.live_stale = true;
        self.chunker.flush()
    }

    pub fn push_log(&mut self, line: String) {
        if line.starts_with('⚠') {
            self.flash(line.clone(), true);
        }
        self.log.push(line);
        if self.log.len() > 400 {
            self.log.drain(..self.log.len() - 400);
        }
        self.dirty = true;
    }

    /// The meeting clock moved; closes a chunk the speaker has walked away from.
    pub fn tick(&mut self) -> Option<Chunk> {
        if let Some((_, at, _)) = &self.flash {
            if at.elapsed() > FLASH_FOR {
                self.flash = None;
                self.dirty = true;
            }
        }
        if self.is_live() {
            self.dirty = true;
        }
        if matches!(self.rec, RecState::Recording) {
            return self.chunker.tick(self.elapsed());
        }
        None
    }

    // ----- chunks -----

    pub fn add_chunk(&mut self, id: String, chunk: &Chunk) -> usize {
        let idx = self.chunks.len();
        self.chunks.push(ChunkView {
            id,
            idx,
            start: chunk.start,
            end: chunk.end,
            words: chunk.words(),
            summary: None,
            state: ChunkState::Queued,
        });
        self.dirty = true;
        self.live_stale = true;
        idx
    }

    pub fn chunk_mut(&mut self, id: &str) -> Option<&mut ChunkView> {
        self.chunks.iter_mut().find(|c| c.id == id)
    }

    /// The lookup answered for a chunk: its summary lands under the transcript, its facts
    /// in the Related pane.
    pub fn on_lookup_done(&mut self, chunk_id: &str, summary: String, facts: &[Fact]) {
        let Some(c) = self.chunk_mut(chunk_id) else {
            return;
        };
        c.summary = Some(summary);
        c.state = ChunkState::Done;
        let (idx, at) = (c.idx, c.start);
        self.facts.extend(fact_views(idx, at, facts));
        self.dirty = true;
        self.live_stale = true;
    }

    pub fn summaries(&self) -> Vec<String> {
        self.chunks
            .iter()
            .filter_map(|c| c.summary.clone())
            .collect()
    }

    pub fn fact_lines(&self) -> Vec<String> {
        self.facts.iter().map(FactView::line).collect()
    }

    /// The whole live transcript as the suggester reads it, `[mm:ss] source: text` per line.
    pub fn transcript_text(&self) -> String {
        self.transcript
            .iter()
            .map(TranscriptLine::line)
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn board(&self) -> Vec<String> {
        self.items
            .iter()
            .map(|i| format!("{} ({})", i.title, i.status.as_str()))
            .collect()
    }

    /// The chunk the lookup is on right now.
    pub fn looking_up(&self) -> Option<usize> {
        self.chunks
            .iter()
            .find(|c| c.state == ChunkState::Summarizing)
            .map(|c| c.idx)
    }

    // ----- the right pane -----

    pub fn toggle_pane(&mut self) {
        self.right_pane = match self.right_pane {
            RightPane::Related => RightPane::Items,
            RightPane::Items => RightPane::Related,
        };
        self.related_scroll = None;
        self.detail_scroll = 0;
        self.dirty = true;
    }

    pub fn show_items(&mut self) {
        self.right_pane = RightPane::Items;
        self.detail_scroll = 0;
        self.dirty = true;
    }

    /// The facts on screen: the live meeting's, or the viewed session's.
    pub fn shown_facts(&self) -> &[FactView] {
        match &self.session {
            Some(v) => &v.facts,
            None => &self.facts,
        }
    }

    pub fn queued(&self) -> usize {
        self.chunks
            .iter()
            .filter(|c| c.state == ChunkState::Queued)
            .count()
    }

    // ----- sessions -----

    /// Which session is on screen: 0 is the live meeting, otherwise its index in `sessions`.
    pub fn session_index(&self) -> usize {
        match &self.session {
            Some(v) => self
                .sessions
                .iter()
                .position(|m| m.id == v.meeting.id)
                .unwrap_or(0),
            None => 0,
        }
    }

    /// Show a past session; the selection moves to its first item.
    pub fn enter_session(&mut self, view: SessionView) {
        let keep = self.selected_item().map(|i| i.id.clone());
        self.session = Some(view);
        self.transcript_scroll = None;
        self.related_scroll = None;
        self.detail_scroll = 0;
        self.right_pane = RightPane::Items;
        self.reselect(keep);
        self.dirty = true;
    }

    /// Back to the live meeting.
    pub fn leave_session(&mut self) {
        let keep = self.selected_item().map(|i| i.id.clone());
        self.session = None;
        self.transcript_scroll = None;
        self.related_scroll = None;
        self.detail_scroll = 0;
        self.right_pane = if self.is_live() {
            RightPane::Related
        } else {
            RightPane::Items
        };
        self.reselect(keep);
        self.dirty = true;
    }

    /// The transcript on screen: the live one, or the viewed session's.
    pub fn shown_transcript(&self) -> &[TranscriptLine] {
        match &self.session {
            Some(v) => &v.transcript,
            None => &self.transcript,
        }
    }

    pub fn shown_chunks(&self) -> &[ChunkView] {
        match &self.session {
            Some(v) => &v.chunks,
            None => &self.chunks,
        }
    }

    // ----- items -----

    /// Indices into `items` of what the board shows: every item live, the session's own
    /// items while viewing one.
    pub fn visible_items(&self) -> Vec<usize> {
        match &self.session {
            Some(v) => self
                .items
                .iter()
                .enumerate()
                .filter(|(_, i)| i.meeting_id == v.meeting.id)
                .map(|(n, _)| n)
                .collect(),
            None => (0..self.items.len()).collect(),
        }
    }

    pub fn selected_item(&self) -> Option<&ActionItem> {
        let idx = *self.visible_items().get(self.selected)?;
        self.items.get(idx)
    }

    pub fn item_mut(&mut self, id: &str) -> Option<&mut ActionItem> {
        self.items.iter_mut().find(|i| i.id == id)
    }

    /// Point the selection at `id` when it is visible, else clamp it.
    fn reselect(&mut self, id: Option<String>) {
        let visible = self.visible_items();
        self.selected = id
            .and_then(|id| {
                visible
                    .iter()
                    .position(|&n| self.items[n].id == id)
            })
            .unwrap_or(0)
            .min(visible.len().saturating_sub(1));
    }

    /// New items go on top; the selection stays on the item it was on.
    pub fn insert_items(&mut self, new: Vec<ActionItem>) {
        if new.is_empty() {
            return;
        }
        let keep = self.selected_item().map(|i| i.id.clone());
        for (i, item) in new.into_iter().enumerate() {
            self.items.insert(i, item);
        }
        self.reselect(keep);
        self.dirty = true;
        self.live_stale = true;
    }

    pub fn select_next(&mut self) {
        let n = self.visible_items().len();
        if n > 0 && self.selected + 1 < n {
            self.selected += 1;
            self.detail_scroll = 0;
            self.dirty = true;
        }
    }

    pub fn select_prev(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
            self.detail_scroll = 0;
            self.dirty = true;
        }
    }

    pub fn dismiss_selected(&mut self) -> Option<String> {
        let item = self.selected_item()?;
        if item.status == ItemStatus::Running {
            self.flash("that agent is still running — X stops it".into(), true);
            return None;
        }
        let id = item.id.clone();
        self.items.retain(|i| i.id != id);
        let n = self.visible_items().len();
        if self.selected >= n {
            self.selected = n.saturating_sub(1);
        }
        self.dirty = true;
        self.live_stale = true;
        Some(id)
    }

    pub fn running_count(&self) -> usize {
        self.items
            .iter()
            .filter(|i| i.status == ItemStatus::Running)
            .count()
    }

    /// Items of one status among those on screen.
    pub fn visible_count(&self, status: ItemStatus) -> usize {
        self.visible_items()
            .into_iter()
            .filter(|&n| self.items[n].status == status)
            .count()
    }

    pub fn on_run_started(&mut self, id: &str, branch: String, worktree: String, log_path: String) {
        if let Some(item) = self.item_mut(id) {
            item.status = ItemStatus::Running;
            item.branch = Some(branch);
            item.worktree_path = Some(worktree);
            item.log_path = Some(log_path);
            item.error = None;
        }
        self.dirty = true;
        self.live_stale = true;
    }

    pub fn on_run_session(&mut self, id: &str, session_id: String) {
        if let Some(item) = self.item_mut(id) {
            item.session_id = Some(session_id);
        }
        self.dirty = true;
    }

    pub fn on_run_phase(&mut self, id: &str, phase: RunPhase) {
        if let Some(item) = self.item_mut(id) {
            item.phase = phase;
        }
    }

    pub fn on_run_ended(&mut self, id: &str, status: ItemStatus, out: &RunOutcome) {
        self.activity.remove(id);
        if let Some(item) = self.item_mut(id) {
            item.status = status;
            if out.branch.is_some() {
                item.branch = out.branch.clone();
            }
            if out.worktree_path.is_some() {
                item.worktree_path = out.worktree_path.clone();
            }
            if out.pr_url.is_some() {
                item.pr_url = out.pr_url.clone();
            }
            if out.log_path.is_some() {
                item.log_path = out.log_path.clone();
            }
            item.error = out.error.clone();
        }
        self.dirty = true;
        self.live_stale = true;
    }

    /// One line in the footer for a few seconds; newlines and runs of spaces collapse.
    pub fn flash(&mut self, text: String, warn: bool) {
        let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
        self.flash = Some((one_line, Instant::now(), warn));
        self.dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, status: ItemStatus) -> ActionItem {
        item_in(id, "m", status)
    }

    fn item_in(id: &str, meeting: &str, status: ItemStatus) -> ActionItem {
        ActionItem {
            id: id.into(),
            meeting_id: meeting.into(),
            chunk_id: None,
            repo_path: "/r".into(),
            title: format!("item {id}"),
            prompt: "p".into(),
            why: String::new(),
            status,
            branch: None,
            worktree_path: None,
            pr_url: None,
            log_path: None,
            error: None,
            session_id: None,
            phase: RunPhase::Agent,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn meeting(id: &str) -> MeetingRow {
        MeetingRow {
            id: id.into(),
            repo_path: "/r".into(),
            meeting_dir: None,
            started_at: 0,
            ended_at: Some(60),
            segment_count: 0,
            summary: None,
        }
    }

    fn fresh() -> App {
        App::new(
            "/r".into(),
            "main".into(),
            false,
            Limits::default(),
            "m".into(),
            vec![item("a", ItemStatus::Suggested)],
        )
    }

    #[test]
    fn new_items_go_on_top_and_the_selection_follows_its_item() {
        let mut app = fresh();
        app.select_next();
        assert_eq!(app.selected, 0, "one item: nowhere to go");
        app.insert_items(vec![
            item("b", ItemStatus::Suggested),
            item("c", ItemStatus::Suggested),
        ]);
        assert_eq!(
            app.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            vec!["b", "c", "a"]
        );
        assert_eq!(app.selected_item().unwrap().id, "a");
        app.select_prev();
        app.select_prev();
        assert_eq!(app.selected_item().unwrap().id, "b");
    }

    #[test]
    fn a_running_item_cannot_be_dismissed() {
        let mut app = fresh();
        app.items[0].status = ItemStatus::Running;
        assert_eq!(app.dismiss_selected(), None);
        app.items[0].status = ItemStatus::Failed;
        assert_eq!(app.dismiss_selected().as_deref(), Some("a"));
        assert!(app.items.is_empty());
        assert_eq!(app.dismiss_selected(), None);
    }

    #[test]
    fn lifecycle_events_move_the_state_and_the_clock() {
        let mut app = fresh();
        assert_eq!(app.rec, RecState::Starting);
        app.on_started("/m/1".into(), vec!["mic".into()]);
        assert_eq!(app.rec, RecState::Recording);
        assert_eq!(app.meeting_dir.as_deref(), Some("/m/1"));
        app.on_paused(true);
        assert_eq!(app.rec, RecState::Paused);
        app.on_paused(false);
        assert_eq!(app.rec, RecState::Recording);
        assert!(app
            .on_segment(Segment {
                source: "mic".into(),
                text: "hello there".into(),
                start: 0.0,
                end: 1.0
            })
            .is_none());
        assert_eq!(app.counts["mic"], 1);
        let tail = app.on_finished().expect("the pending words are flushed");
        assert_eq!(tail.words(), 2);
        assert_eq!(app.rec, RecState::Ended);
        assert!(app.elapsed() < 1.0);
        assert!(app.on_engine_exited(Some(0)).is_none(), "already ended");

        let mut app = fresh();
        app.on_started(String::new(), vec![]);
        app.push_log("⚠ microphone access denied".into());
        app.on_engine_exited(Some(1));
        match &app.rec {
            RecState::Failed(msg) => assert!(
                msg.contains("status 1") && msg.contains("microphone"),
                "{msg}"
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn run_events_update_the_item() {
        let mut app = fresh();
        app.on_run_started("a", "b".into(), "/wt".into(), "/log".into());
        assert_eq!(app.items[0].status, ItemStatus::Running);
        assert_eq!(app.running_count(), 1);
        app.on_run_session("a", "sid".into());
        app.on_run_phase("a", RunPhase::Publish);
        assert_eq!(app.items[0].session_id.as_deref(), Some("sid"));
        assert_eq!(app.items[0].phase, RunPhase::Publish);
        app.activity.insert("a".into(), "Bash: ls".into());
        app.on_run_ended(
            "a",
            ItemStatus::Stopped,
            &RunOutcome {
                error: Some("stopped by the user".into()),
                ..Default::default()
            },
        );
        assert_eq!(app.items[0].status, ItemStatus::Stopped);
        assert!(app.items[0].status.runnable(), "Enter resumes it");
        assert!(app.activity.is_empty());
        app.on_run_ended(
            "a",
            ItemStatus::Done,
            &RunOutcome {
                pr_url: Some("https://x/pull/1".into()),
                ..Default::default()
            },
        );
        assert_eq!(app.items[0].status, ItemStatus::Done);
        assert_eq!(
            app.items[0].branch.as_deref(),
            Some("b"),
            "kept from the start event"
        );
        assert_eq!(app.items[0].error, None);
    }

    #[test]
    fn viewing_a_session_narrows_the_board_to_its_items_and_keeps_live_updates() {
        let mut app = App::new(
            "/r".into(),
            "main".into(),
            false,
            Limits::default(),
            "live".into(),
            vec![
                item_in("n1", "live", ItemStatus::Suggested),
                item_in("o2", "old", ItemStatus::Running),
                item_in("o1", "old", ItemStatus::Done),
            ],
        );
        app.sessions = vec![meeting("live"), meeting("old"), meeting("older")];
        assert_eq!(app.visible_items(), vec![0, 1, 2]);
        assert_eq!(app.session_index(), 0);
        app.select_next();
        assert_eq!(app.selected_item().unwrap().id, "o2");

        app.enter_session(SessionView {
            meeting: meeting("old"),
            transcript: vec![TranscriptLine {
                at: 1.0,
                source: "mic".into(),
                text: "earlier".into(),
            }],
            chunks: vec![],
            facts: vec![FactView {
                chunk_idx: 0,
                at: 0.0,
                text: "old fact".into(),
                where_: String::new(),
            }],
        });
        assert_eq!(app.session_index(), 1);
        assert_eq!(app.right_pane, RightPane::Items, "a session opens on its items");
        assert_eq!(app.shown_facts()[0].text, "old fact");
        assert_eq!(app.visible_items(), vec![1, 2]);
        assert_eq!(
            app.selected_item().unwrap().id,
            "o2",
            "the selection followed its item into the narrower list"
        );
        assert_eq!(app.shown_transcript()[0].text, "earlier");
        assert_eq!(app.visible_count(ItemStatus::Running), 1);
        app.select_next();
        assert_eq!(app.selected_item().unwrap().id, "o1");
        app.select_next();
        assert_eq!(app.selected_item().unwrap().id, "o1", "clamped");

        // The agent on o2 finishes while the session is on screen.
        app.on_run_ended(
            "o2",
            ItemStatus::Done,
            &RunOutcome {
                pr_url: Some("https://x/pull/2".into()),
                ..Default::default()
            },
        );
        assert_eq!(app.visible_count(ItemStatus::Done), 2);

        // New live items do not appear in the session view but are kept.
        app.insert_items(vec![item_in("n2", "live", ItemStatus::Suggested)]);
        assert_eq!(app.visible_items().len(), 2);
        assert_eq!(app.selected_item().unwrap().id, "o1");

        app.enter_session(SessionView {
            meeting: meeting("older"),
            transcript: vec![],
            chunks: vec![],
            facts: vec![],
        });
        assert!(app.visible_items().is_empty());
        assert_eq!(app.selected_item(), None);
        assert_eq!(app.dismiss_selected(), None);

        app.leave_session();
        assert_eq!(app.visible_items().len(), 4);
        assert_eq!(app.selected, 0);
        assert!(app.shown_transcript().is_empty(), "the live transcript again");
        assert_eq!(
            app.right_pane,
            RightPane::Related,
            "back to the live meeting, which is still starting: the Related pane"
        );
    }

    #[test]
    fn the_lookup_fills_the_summary_and_the_related_pane_and_the_suggester_gets_it_all() {
        let mut app = fresh();
        app.on_started(String::new(), vec!["mic".into()]);
        app.on_segment(Segment {
            source: "mic".into(),
            text: "does list print json".into(),
            start: 3.0,
            end: 5.0,
        });
        let chunk = app.chunker.flush().unwrap();
        app.add_chunk("c1".into(), &chunk);
        app.on_lookup_done(
            "c1",
            "asked about json".into(),
            &[
                Fact {
                    text: "list prints plain text".into(),
                    where_: "todo.sh:18".into(),
                },
                Fact {
                    text: "no --json flag".into(),
                    where_: String::new(),
                },
            ],
        );
        assert_eq!(app.chunks[0].state, ChunkState::Done);
        assert_eq!(app.summaries(), vec!["asked about json".to_string()]);
        assert_eq!(app.facts.len(), 2);
        assert_eq!(app.facts[0].at, 3.0);
        assert_eq!(
            app.fact_lines(),
            vec![
                "list prints plain text (todo.sh:18)".to_string(),
                "no --json flag".to_string()
            ]
        );
        assert_eq!(app.transcript_text(), "[00:03] mic: does list print json");
        app.on_lookup_done("nope", "x".into(), &[]);
        assert_eq!(app.facts.len(), 2, "an unknown chunk changes nothing");

        assert_eq!(app.right_pane, RightPane::Related);
        app.toggle_pane();
        assert_eq!(app.right_pane, RightPane::Items);
        app.toggle_pane();
        assert_eq!(app.right_pane, RightPane::Related);
        app.show_items();
        assert_eq!(app.right_pane, RightPane::Items);
    }
}
