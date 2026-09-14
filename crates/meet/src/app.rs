//! The TUI's state and the pure transitions on it. The event loop feeds it recorder,
//! suggester and runner events and key presses; `ui` draws it.
//!
//! Two views share the screen: the live meeting (what is being said now), and a past
//! session picked from the session bar — its transcript and summaries on the left, and on
//! the right either what the live lookup found for the selected summary (Related: facts,
//! contradictions, questions) or the action items it produced, with any agent still
//! running on them updating live. The bar under the header holds every session, newest at
//! the left; Tab cycles the focus through the bar and the right panes; ←/→ (h/l on the
//! bar) step through the sessions; ↑/↓ pick the summary.

use crate::chunker::{Chunk, Chunker, Limits};
use crate::engine_hook::Hooks;
use crate::lookup::{Fact, Lookup};
use crate::recorder::Segment;
use crate::settings::{Overrides, Settings};
use crate::store::{ActionItem, ItemStatus, MeetingRow, RunOutcome, RunPhase};
use crate::stream_json::Usage;
use crate::term::Term;
use crate::layout::{
    self, HitMap, LayoutPrefs, PointerShape, Splitter, SplitterDrag,
};
use crate::selection::{self, TextRow, TextSelection};
use crate::theme::Theme;
use ratatui::layout::Position;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub enum RecState {
    /// No recording in this meeting yet — meet was just launched, or a new meeting was
    /// opened for the next one: `r` starts it. Nothing records on its own.
    Idle,
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

/// A chunk of the transcript and what the lookup made of it.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkView {
    pub id: String,
    pub idx: usize,
    pub start: f64,
    pub end: f64,
    pub words: usize,
    /// `[mm:ss] source: text` per segment — sent along with the next chunk's lookup.
    pub text: String,
    pub summary: Option<String>,
    pub state: ChunkState,
    /// What the repository and earlier meetings say that bears on this chunk.
    pub facts: Vec<Fact>,
    /// Where what was said does not match the code or an earlier decision.
    pub contradictions: Vec<Fact>,
    /// Worth asking next.
    pub questions: Vec<String>,
}

/// What the right column shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RightPane {
    /// What the lookup found for the selected summary — the default while recording.
    Related,
    /// The action items and the selected one's prompt.
    Items,
    /// The meeting's summary (`m`; the default once the recording ended): while it is
    /// being written, what is working on it — the engine's onDone hooks and meet's own
    /// summary call — then the write-up, and the file a hook wrote.
    Summary,
    /// The question session: Claude Code on the embedded terminal (`a`).
    Ask,
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
    /// Throw the recording away and quit — nothing kept, nothing summarized?
    ConfirmDiscard,
    /// The session list; `cursor` indexes `App::sessions` (0 is the live meeting).
    Sessions {
        cursor: usize,
    },
    /// The settings modal (`,`): which Claude model and effort each feature runs with.
    /// `cursor` indexes `settings::ROWS`; `notice` is a one-line result of the last change
    /// (`true`: a warning), shown until the cursor moves.
    Settings {
        cursor: usize,
        notice: Option<(String, bool)>,
    },
    /// `R` in the settings: put every setting back to its default?
    ConfirmReset {
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
    /// The engine's sources whose audio is silence right now (`M` / `N`, or a click on
    /// the header's `mic` / `system`); the engine confirms each flip with a `muted` event.
    pub muted: Vec<String>,
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
    /// The furthest the transcript could scroll on the last frame (the newest line on the
    /// bottom row), so scrolling up while following starts from there.
    pub transcript_max: usize,
    pub chunker: Chunker,
    pub chunks: Vec<ChunkView>,
    /// Which summary the Related pane shows: `None` follows the newest one that has an
    /// answer; `Some(i)` was placed with ↑/↓ (an index into `shown_chunks()`).
    pub chunk_cursor: Option<usize>,
    /// How far down the Related pane is scrolled (in lines).
    pub related_scroll: usize,
    pub right_pane: RightPane,
    pub wrap_up: WrapUp,
    /// The live meeting's summary, once its action items were written.
    pub meeting_summary: Option<String>,
    /// Meeting id → its write-up (Markdown), for the Summary pane: the live meeting's
    /// once written, a past session's once it is viewed.
    pub notes: HashMap<String, String>,
    /// How far down the Summary pane is scrolled (in lines).
    pub summary_scroll: usize,
    /// The engine's onDone hooks once the recording stopped: which is running, what it
    /// printed, how each ended.
    pub hooks: Hooks,
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
    /// What every Claude call this launch made has spent — lookups, the action-item
    /// writer, agents that ended — tokens and dollars as claude reported them.
    pub spent: Usage,
    /// item id → what its running agent has read so far, one API turn at a time; folded
    /// into `spent` (replaced by the run's own total when it reports one) once it ends.
    pub agent_usage: HashMap<String, Usage>,
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
    /// `D`: the recording is being thrown away. Nothing more is looked up or written,
    /// the engine deletes its meeting directory and runs no hook, and the session is
    /// deleted on the way out.
    pub discard: bool,
    /// The on-disk `summary.md` (see `live`) no longer matches what is on screen.
    pub live_stale: bool,
    /// The question sessions' terminals, one per meeting `a` was pressed on (the live one
    /// or a past session), keyed by meeting id. Kept after they exit, so the last screen
    /// stays readable until the next `a`.
    pub terms: HashMap<String, Term>,
    /// Keys go to the shown session's terminal, not to meet.
    pub term_focused: bool,
    /// The session bar has the keys: ←/→ and h/l step through the sessions.
    pub bar_focused: bool,
    /// The first tab the session bar draws (it scrolls to keep the shown session in view).
    pub bar_first: usize,
    /// Which Claude model and effort each feature runs with (the settings file).
    pub settings: Settings,
    /// The `--*-model` / `--*-effort` flags passed this launch, which win over `settings`.
    pub overrides: Overrides,
    /// Where the seams between the panes sit (dragged with the mouse; `layout.json`).
    pub layout: LayoutPrefs,
    /// Where the last frame drew everything, for the mouse.
    pub hit: HitMap,
    /// A seam being dragged right now.
    pub splitter_drag: Option<SplitterDrag>,
    /// The seam under the pointer (a drag counts): its grip is highlighted.
    pub hover_splitter: Option<Splitter>,
    /// The pointer shape the terminal should show; the event loop sends it when it changes.
    pub pointer_shape: PointerShape,
    /// Transcript text dragged over or double-clicked (`selection`): highlighted, and
    /// already copied, until the next click lets it go.
    pub transcript_sel: Option<TextSelection>,
    /// The last press on the transcript, for telling a double-click.
    last_transcript_press: Option<(Instant, (usize, usize))>,
    /// The first transcript row the last frame showed.
    pub transcript_top: usize,
    /// A copy for the terminal to put on its clipboard (OSC 52); the event loop sends it.
    pub pending_clipboard: Option<String>,
}

pub const FLASH_FOR: Duration = Duration::from_secs(6);

/// How a source is called in a sentence: `mic` is the microphone, `system` the system
/// audio; anything else by its own name.
pub fn source_name(source: &str) -> String {
    match source {
        "mic" => "microphone".into(),
        "system" => "system audio".into(),
        other => other.to_string(),
    }
}

/// The key that mutes and unmutes a source, for the sources that have one.
pub fn mute_key(source: &str) -> Option<&'static str> {
    match source {
        "mic" => Some("M"),
        "system" => Some("N"),
        _ => None,
    }
}

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
            rec: RecState::Idle,
            status_text: None,
            meeting_id,
            meeting_dir: None,
            sources: Vec::new(),
            muted: Vec::new(),
            counts: HashMap::new(),
            started: None,
            paused_accum: Duration::ZERO,
            pause_started: None,
            elapsed_at_end: None,
            replay_clock: None,
            transcript: Vec::new(),
            transcript_scroll: None,
            transcript_max: 0,
            chunker: Chunker::new(limits),
            chunks: Vec::new(),
            chunk_cursor: None,
            related_scroll: 0,
            right_pane: RightPane::Related,
            wrap_up: WrapUp::NotYet,
            meeting_summary: None,
            notes: HashMap::new(),
            summary_scroll: 0,
            hooks: Hooks::default(),
            sessions: Vec::new(),
            session: None,
            items,
            selected: 0,
            detail_scroll: 0,
            activity: HashMap::new(),
            spent: Usage::default(),
            agent_usage: HashMap::new(),
            overlay: None,
            flash: None,
            log: Vec::new(),
            lookup_disabled: false,
            suggest_disabled: false,
            dirty: true,
            should_quit: false,
            discard: false,
            live_stale: true,
            terms: HashMap::new(),
            term_focused: false,
            bar_focused: false,
            bar_first: 0,
            settings: Settings::default(),
            overrides: Overrides::default(),
            layout: LayoutPrefs::default(),
            hit: HitMap::default(),
            splitter_drag: None,
            hover_splitter: None,
            pointer_shape: PointerShape::Default,
            transcript_sel: None,
            last_transcript_press: None,
            transcript_top: 0,
            pending_clipboard: None,
        }
    }

    // ----- the mouse -----

    /// A click on one of the right pane's tabs: show that pane and give it the keys — Claude
    /// Code gets them straight away when its session is running.
    pub fn show_pane(&mut self, pane: RightPane) {
        self.bar_focused = false;
        if pane == RightPane::Ask && self.term_running() {
            self.focus_term();
            return;
        }
        if self.right_pane != pane {
            self.right_pane = pane;
            self.related_scroll = 0;
            self.detail_scroll = 0;
            self.summary_scroll = 0;
        }
        self.term_focused = false;
        self.dirty = true;
    }

    /// A click on a summary: the Related pane shows it (following again if it is the
    /// newest, as ↓ does).
    pub fn pick_chunk(&mut self, index: usize) {
        let n = self.shown_chunks().len();
        if index >= n {
            return;
        }
        self.chunk_cursor = if Some(index) == self.followed_chunk() {
            None
        } else {
            Some(index)
        };
        self.related_scroll = 0;
        self.bar_focused = false;
        self.term_focused = false;
        if self.right_pane != RightPane::Related {
            self.right_pane = RightPane::Related;
        }
        self.dirty = true;
    }

    /// A click on an action item's row.
    pub fn select_row(&mut self, row: usize) {
        if row < self.visible_items().len() && row != self.selected {
            self.selected = row;
            self.detail_scroll = 0;
        }
        self.bar_focused = false;
        self.term_focused = false;
        self.dirty = true;
    }

    /// The keys go to the panes: a click landed on one.
    pub fn focus_panes(&mut self) {
        self.bar_focused = false;
        self.term_focused = false;
        self.dirty = true;
    }

    /// Start dragging the seam under the pointer at `(x, y)`.
    pub fn grab_splitter(&mut self, which: Splitter, x: u16, y: u16) {
        let pointer = if which.is_vertical() {
            i32::from(x)
        } else {
            i32::from(y)
        };
        self.splitter_drag = Some(SplitterDrag {
            which,
            grab_offset: self.hit.seam_pos(which) - pointer,
        });
        self.hover_splitter = Some(which);
        self.dirty = true;
    }

    /// The pointer moved to `(x, y)` with a seam in hand: the seam follows it.
    pub fn drag_splitter_to(&mut self, x: u16, y: u16) {
        let Some(drag) = self.splitter_drag else {
            return;
        };
        let hit = &self.hit;
        match drag.which {
            Splitter::Columns => {
                let seam = i32::from(x) + drag.grab_offset;
                self.layout.left =
                    layout::share_at(hit.body.x, hit.body.width, seam, layout::MIN_COL_W);
            }
            Splitter::LeftRows => {
                let seam = i32::from(y) + drag.grab_offset;
                let col = hit.transcript;
                let height = hit.summaries.bottom().saturating_sub(col.y);
                self.layout.transcript =
                    layout::share_at(col.y, height, seam, layout::MIN_PANE_H);
            }
            Splitter::RightRows => {
                let seam = i32::from(y) + drag.grab_offset;
                let col = hit.right;
                self.layout.items =
                    Some(layout::share_at(col.y, col.height, seam, layout::MIN_PANE_H));
            }
        }
        self.dirty = true;
    }

    /// The button came up: the drag is over. `true` when there was one (the layout is
    /// then worth writing).
    pub fn release_splitter(&mut self) -> bool {
        let had = self.splitter_drag.take().is_some();
        if had {
            self.dirty = true;
        }
        had
    }

    /// The transcript row and cell under `(x, y)`, pulled into the text area — a drag past
    /// an edge stays on that edge's row. `None` while the transcript has no text area.
    fn transcript_cell(&self, x: u16, y: u16) -> Option<(usize, usize)> {
        let r = self.hit.transcript_text;
        if r.width == 0 || r.height == 0 {
            return None;
        }
        let col = x.clamp(r.x, r.right() - 1) - r.x;
        let row = y.clamp(r.y, r.bottom() - 1) - r.y;
        Some((usize::from(col), self.transcript_top + usize::from(row)))
    }

    /// The shown transcript wrapped to `width`, row for row as its pane draws it.
    fn transcript_rows(&self, width: usize) -> Vec<TextRow> {
        crate::ui::transcript_rows(self.shown_transcript(), width)
    }

    /// A press on the transcript's text: the second on one cell selects the word under it
    /// and hands it back to copy; any other press arms a drag. (The press itself already let
    /// the last selection go.)
    pub fn press_transcript(&mut self, x: u16, y: u16) -> Option<String> {
        let r = self.hit.transcript_text;
        if !r.contains(Position::new(x, y)) {
            return None;
        }
        let cell = self.transcript_cell(x, y)?;
        let width = usize::from(r.width);
        self.dirty = true;
        if selection::is_double_click(&mut self.last_transcript_press, cell) {
            let rows = self.transcript_rows(width);
            if let Some((first, last)) = selection::word_at(&rows, cell.0, cell.1) {
                let sel = TextSelection {
                    anchor: (first, cell.1),
                    head: (last, cell.1),
                    width,
                    dragging: false,
                    active: true,
                };
                self.transcript_sel = Some(sel);
                return Some(selection::selected_text(&rows, &sel));
            }
        }
        self.transcript_sel = Some(TextSelection::press(cell, width));
        None
    }

    /// The pointer moved with the button down on a selection being made: its far end
    /// follows. Past the top or the bottom of the text the transcript scrolls a row a move,
    /// so a selection can run longer than the pane. `false` when no selection is being made.
    pub fn drag_selection_to(&mut self, x: u16, y: u16) -> bool {
        let Some(mut sel) = self.transcript_sel.filter(|s| s.dragging) else {
            return false;
        };
        let r = self.hit.transcript_text;
        if r.height == 0 {
            return true;
        }
        if y < r.y && self.transcript_top > 0 {
            self.transcript_top -= 1;
            self.transcript_scroll = Some(self.transcript_top);
        } else if y >= r.bottom() && self.transcript_top < self.transcript_max {
            self.transcript_top += 1;
            self.transcript_scroll = Some(self.transcript_top);
        }
        if let Some(cell) = self.transcript_cell(x, y) {
            sel.head = cell;
            // Off its first cell it is a selection, and stays one if it comes back.
            sel.active |= sel.head != sel.anchor;
            self.transcript_sel = Some(sel);
            self.dirty = true;
        }
        true
    }

    /// The button came up on a selection being made. A drag that never left its cell was a
    /// click, and goes; a real one stays highlighted, and its text comes back to copy.
    pub fn release_selection(&mut self) -> Option<String> {
        let mut sel = self.transcript_sel.filter(|s| s.dragging)?;
        self.dirty = true;
        sel.dragging = false;
        let text = if sel.active {
            selection::selected_text(&self.transcript_rows(sel.width), &sel)
        } else {
            String::new()
        };
        if text.is_empty() {
            self.transcript_sel = None;
            return None;
        }
        self.transcript_sel = Some(sel);
        Some(text)
    }

    /// The wheel over the right pane: `delta` lines (negative is up).
    pub fn scroll_right_pane(&mut self, delta: i32) {
        let step = |v: &mut usize| {
            *v = if delta < 0 {
                v.saturating_sub(delta.unsigned_abs() as usize)
            } else {
                *v + delta as usize
            };
        };
        match self.right_pane {
            RightPane::Related => step(&mut self.related_scroll),
            RightPane::Items => step(&mut self.detail_scroll),
            RightPane::Summary => step(&mut self.summary_scroll),
            RightPane::Ask => return,
        }
        self.dirty = true;
    }

    /// The transcript scrolled by `delta` lines (the wheel, PgUp / PgDn): negative is up,
    /// starting from the newest line while following; reaching the newest line again
    /// follows again (the next draw sees the scroll past its end).
    pub fn scroll_transcript(&mut self, delta: i32) {
        let cur = self.transcript_scroll.unwrap_or(self.transcript_max);
        self.transcript_scroll = Some(if delta < 0 {
            cur.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            cur + delta as usize
        });
        self.dirty = true;
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

    /// `r` after a recording ended: the next one is its own meeting. The live state
    /// starts over under the new id — transcript, chunks and what was looked up for them,
    /// the clock, the summary, the hooks, the panes — while the board, the sessions, the
    /// question terminals (each keyed by its meeting), the write-ups and what was spent
    /// stay. The caller puts the new row on `sessions`.
    pub fn begin_meeting(&mut self, meeting_id: String) {
        let keep = self.selected_item().map(|i| i.id.clone());
        self.meeting_id = meeting_id;
        self.rec = RecState::Idle;
        self.status_text = None;
        self.meeting_dir = None;
        self.sources.clear();
        self.muted.clear();
        self.counts.clear();
        self.started = None;
        self.paused_accum = Duration::ZERO;
        self.pause_started = None;
        self.elapsed_at_end = None;
        self.replay_clock = None;
        self.transcript.clear();
        self.transcript_scroll = None;
        self.transcript_sel = None;
        self.chunker.reset();
        self.chunks.clear();
        self.chunk_cursor = None;
        self.related_scroll = 0;
        self.right_pane = RightPane::Related;
        self.wrap_up = WrapUp::NotYet;
        self.meeting_summary = None;
        self.summary_scroll = 0;
        self.hooks = Hooks::default();
        self.session = None;
        self.detail_scroll = 0;
        self.term_focused = false;
        self.bar_focused = false;
        self.bar_first = 0;
        self.reselect(keep);
        self.dirty = true;
        self.live_stale = true;
    }

    // ----- recorder events -----

    pub fn on_started(&mut self, meeting_dir: String, sources: Vec<String>) {
        self.rec = RecState::Recording;
        self.started = Some(Instant::now());
        self.sources = sources;
        self.muted.clear();
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

    /// The recorder confirmed a mute flip. Says so in the footer too, since a click gives
    /// no other feedback than the header's marker.
    pub fn on_muted(&mut self, source: String, muted: bool) {
        let name = source_name(&source);
        let key = mute_key(&source);
        if muted {
            if !self.muted.contains(&source) {
                self.muted.push(source);
            }
            let hint = key.map(|k| format!(" — {k} unmutes")).unwrap_or_default();
            self.flash(format!("⊘ {name} muted: recorded as silence{hint}"), true);
        } else {
            self.muted.retain(|m| *m != source);
            self.flash(format!("{name} live again"), false);
        }
        self.dirty = true;
    }

    pub fn is_muted(&self, source: &str) -> bool {
        self.muted.iter().any(|m| m == source)
    }

    pub fn on_finished(&mut self) -> Option<Chunk> {
        if let Some(p) = self.pause_started.take() {
            self.paused_accum += p.elapsed();
        }
        self.elapsed_at_end = Some(self.elapsed());
        // Nothing is muted once nothing records.
        self.muted.clear();
        self.rec = RecState::Ended;
        self.status_text = None;
        self.dirty = true;
        self.live_stale = true;
        self.chunker.flush()
    }

    /// The engine threw the recording away (`D`): its meeting directory is gone. The
    /// pending chunk is not looked up; the session itself is deleted on the way out.
    pub fn on_discarded(&mut self) {
        if let Some(p) = self.pause_started.take() {
            self.paused_accum += p.elapsed();
        }
        self.elapsed_at_end = Some(self.elapsed());
        // Nothing is muted once nothing records.
        self.muted.clear();
        self.rec = RecState::Ended;
        self.status_text = None;
        self.dirty = true;
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
        // Nothing is muted once nothing records.
        self.muted.clear();
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
            text: chunk.text(),
            summary: None,
            state: ChunkState::Queued,
            facts: Vec::new(),
            contradictions: Vec::new(),
            questions: Vec::new(),
        });
        self.dirty = true;
        self.live_stale = true;
        idx
    }

    pub fn chunk_mut(&mut self, id: &str) -> Option<&mut ChunkView> {
        self.chunks.iter_mut().find(|c| c.id == id)
    }

    /// The lookup answered for a chunk: its summary lands under the transcript, and what
    /// it found shows in the Related pane whenever that summary is the selected one.
    pub fn on_lookup_done(&mut self, chunk_id: &str, lookup: Lookup) {
        let Some(c) = self.chunk_mut(chunk_id) else {
            return;
        };
        c.summary = Some(lookup.summary);
        c.facts = lookup.facts;
        c.contradictions = lookup.contradictions;
        c.questions = lookup.questions;
        c.state = ChunkState::Done;
        // A cursor sitting on the newest summary is following it; and when following,
        // the pane moves on to this chunk from its top.
        if self.session.is_none() && self.chunk_cursor.is_some() && self.chunk_cursor == self.followed_chunk() {
            self.chunk_cursor = None;
        }
        if self.chunk_cursor.is_none() {
            self.related_scroll = 0;
        }
        self.dirty = true;
        self.live_stale = true;
    }

    pub fn summaries(&self) -> Vec<String> {
        self.chunks
            .iter()
            .filter_map(|c| c.summary.clone())
            .collect()
    }

    /// `text (where)` per fact shown so far, oldest first.
    pub fn fact_lines(&self) -> Vec<String> {
        self.chunks
            .iter()
            .flat_map(|c| c.facts.iter().map(Fact::line))
            .collect()
    }

    /// `text (where)` per contradiction shown so far.
    pub fn contradiction_lines(&self) -> Vec<String> {
        self.chunks
            .iter()
            .flat_map(|c| c.contradictions.iter().map(Fact::line))
            .collect()
    }

    pub fn question_lines(&self) -> Vec<String> {
        self.chunks
            .iter()
            .flat_map(|c| c.questions.iter().cloned())
            .collect()
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

    /// Tab: Related → Items → Summary → the question session (once the shown session has
    /// one) → the session bar → Related. The bar keeps whatever the right pane was showing.
    pub fn toggle_pane(&mut self) {
        if self.bar_focused {
            self.bar_focused = false;
            self.right_pane = RightPane::Related;
        } else {
            match self.right_pane {
                RightPane::Related => self.right_pane = RightPane::Items,
                RightPane::Items => self.right_pane = RightPane::Summary,
                RightPane::Summary if self.term().is_some() => self.right_pane = RightPane::Ask,
                RightPane::Summary | RightPane::Ask => self.bar_focused = true,
            }
        }
        self.term_focused = false;
        self.related_scroll = 0;
        self.detail_scroll = 0;
        self.summary_scroll = 0;
        self.dirty = true;
    }

    /// Shift+Tab: the same cycle backwards.
    pub fn toggle_pane_back(&mut self) {
        if self.bar_focused {
            self.bar_focused = false;
            self.right_pane = if self.term().is_some() {
                RightPane::Ask
            } else {
                RightPane::Summary
            };
        } else {
            match self.right_pane {
                RightPane::Related => self.bar_focused = true,
                RightPane::Items => self.right_pane = RightPane::Related,
                RightPane::Summary => self.right_pane = RightPane::Items,
                RightPane::Ask => self.right_pane = RightPane::Summary,
            }
        }
        self.term_focused = false;
        self.related_scroll = 0;
        self.detail_scroll = 0;
        self.summary_scroll = 0;
        self.dirty = true;
    }

    /// The keys leave the session bar for the panes (Enter, Esc, or a key meant for them).
    pub fn leave_bar(&mut self) {
        if self.bar_focused {
            self.bar_focused = false;
            self.dirty = true;
        }
    }

    /// The Summary pane takes the right pane (`m`, and on its own when the recording
    /// stops) — unless the user is talking to Claude there. Takes the keys off the bar.
    pub fn show_summary(&mut self) {
        if self.right_pane == RightPane::Ask {
            return;
        }
        self.right_pane = RightPane::Summary;
        self.summary_scroll = 0;
        self.bar_focused = false;
        self.dirty = true;
    }

    /// The shown meeting's short summary: the live one's once written, a past session's.
    pub fn shown_summary(&self) -> Option<&str> {
        match &self.session {
            Some(v) => v.meeting.summary.as_deref(),
            None => self.meeting_summary.as_deref(),
        }
    }

    /// The shown meeting's write-up, once written (a past session's, once loaded).
    pub fn shown_notes(&self) -> Option<&str> {
        self.notes.get(self.shown_meeting_id()).map(String::as_str)
    }

    /// Something is still working on the meeting that just ended: meet's own summary and
    /// action items, or an onDone hook of the engine.
    pub fn summarizing(&self) -> bool {
        self.wrap_up == WrapUp::Writing || self.hooks.any_running() || self.hooks.pending()
    }

    /// The question session takes the right pane and the keys.
    pub fn focus_term(&mut self) {
        self.right_pane = RightPane::Ask;
        self.term_focused = true;
        self.bar_focused = false;
        self.dirty = true;
    }

    /// The meeting on screen: the viewed session's, else this launch's.
    pub fn shown_meeting_id(&self) -> &str {
        match &self.session {
            Some(v) => &v.meeting.id,
            None => &self.meeting_id,
        }
    }

    /// The shown session's question terminal, once `a` started one on it.
    pub fn term(&self) -> Option<&Term> {
        self.terms.get(self.shown_meeting_id())
    }

    pub fn term_mut(&mut self) -> Option<&mut Term> {
        let id = self.shown_meeting_id().to_string();
        self.terms.get_mut(&id)
    }

    /// The shown session's question terminal is live: started, not exited.
    pub fn term_running(&self) -> bool {
        self.term().is_some_and(|t| !t.exited)
    }

    // ----- the Related pane: which summary it shows -----

    /// The chunk the pane follows while no cursor is placed: the newest with an answer
    /// (or a failure), else the newest at all.
    pub fn followed_chunk(&self) -> Option<usize> {
        let chunks = self.shown_chunks();
        chunks
            .iter()
            .rposition(|c| matches!(c.state, ChunkState::Done | ChunkState::Failed(_)))
            .or_else(|| chunks.len().checked_sub(1))
    }

    /// The chunk the Related pane shows: the cursor (clamped), else the followed one.
    pub fn selected_chunk(&self) -> Option<usize> {
        let n = self.shown_chunks().len();
        if n == 0 {
            return None;
        }
        match self.chunk_cursor {
            Some(i) => Some(i.min(n - 1)),
            None => self.followed_chunk(),
        }
    }

    /// `↓`: the next summary; landing on the followed one lets the pane follow again.
    pub fn chunk_next(&mut self) {
        let Some(cur) = self.selected_chunk() else {
            return;
        };
        let next = (cur + 1).min(self.shown_chunks().len() - 1);
        self.chunk_cursor = if Some(next) == self.followed_chunk() {
            None
        } else {
            Some(next)
        };
        self.related_scroll = 0;
        self.dirty = true;
    }

    /// `↑`: the previous summary.
    pub fn chunk_prev(&mut self) {
        let Some(cur) = self.selected_chunk() else {
            return;
        };
        self.chunk_cursor = Some(cur.saturating_sub(1));
        self.related_scroll = 0;
        self.dirty = true;
    }

    /// `G`: follow the newest summary again.
    pub fn follow_chunks(&mut self) {
        self.chunk_cursor = None;
        self.related_scroll = 0;
        self.dirty = true;
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

    /// Show a past session; the selection moves to its first item, and the Related pane
    /// to its first summary. A question session already open on it stays on the right if
    /// that is what was showing; otherwise its items.
    pub fn enter_session(&mut self, view: SessionView) {
        let keep = self.selected_item().map(|i| i.id.clone());
        self.chunk_cursor = (!view.chunks.is_empty()).then_some(0);
        self.session = Some(view);
        self.term_focused = false;
        self.transcript_scroll = None;
        self.transcript_sel = None;
        self.related_scroll = 0;
        self.detail_scroll = 0;
        if self.right_pane != RightPane::Ask || self.term().is_none() {
            self.right_pane = RightPane::Items;
        }
        self.reselect(keep);
        self.dirty = true;
    }

    /// Back to the live meeting.
    pub fn leave_session(&mut self) {
        let keep = self.selected_item().map(|i| i.id.clone());
        self.session = None;
        self.term_focused = false;
        self.transcript_scroll = None;
        self.transcript_sel = None;
        self.chunk_cursor = None;
        self.related_scroll = 0;
        self.detail_scroll = 0;
        if self.right_pane != RightPane::Ask || self.term().is_none() {
            // Related while a recording goes, or is yet to start; the board once it ended.
            self.right_pane = if self.is_live() || self.rec == RecState::Idle {
                RightPane::Related
            } else {
                RightPane::Items
            };
        }
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

    // ----- token usage -----

    /// Everything spent so far: the calls that ended plus what running agents have read.
    pub fn usage(&self) -> Usage {
        self.agent_usage
            .values()
            .fold(self.spent, |acc, u| acc + *u)
    }

    /// A one-shot call (a lookup, the action-item writer) ended: count what it spent.
    pub fn add_usage(&mut self, usage: Usage) {
        if usage.is_zero() {
            return;
        }
        self.spent += usage;
        self.dirty = true;
    }

    /// An agent's spend: one turn's while it runs; at its end (`ended`), the run's own
    /// total — which replaces the turns' tally — or, when it died before reporting one,
    /// the tally as it stands.
    pub fn on_agent_usage(&mut self, id: &str, usage: Option<Usage>, ended: bool) {
        if ended {
            let so_far = self.agent_usage.remove(id).unwrap_or_default();
            self.spent += usage.unwrap_or(so_far);
        } else if let Some(u) = usage {
            *self.agent_usage.entry(id.to_string()).or_default() += u;
        }
        self.dirty = true;
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
    fn a_new_meeting_starts_the_live_state_over_and_keeps_the_board() {
        let mut app = fresh();
        app.on_started("/m/1".into(), vec!["mic".into()]);
        app.on_segment(Segment {
            source: "mic".into(),
            text: "hello there".into(),
            start: 0.0,
            end: 1.0,
        });
        app.add_chunk(
            "c1".into(),
            &Chunk {
                start: 0.0,
                end: 1.0,
                segments: vec![],
            },
        );
        app.on_finished();
        app.meeting_summary = Some("done".into());
        app.notes.insert("m".into(), "notes".into());
        app.wrap_up = WrapUp::Done;
        app.show_summary();
        assert_eq!(app.rec, RecState::Ended);

        app.begin_meeting("m2".into());
        assert_eq!(app.meeting_id, "m2");
        assert_eq!(app.shown_meeting_id(), "m2");
        assert_eq!(app.rec, RecState::Idle);
        assert!(app.transcript.is_empty() && app.chunks.is_empty() && app.counts.is_empty());
        assert_eq!(app.chunker.pending_words(), 0);
        assert_eq!(app.meeting_dir, None);
        assert_eq!(app.meeting_summary, None);
        assert_eq!(app.wrap_up, WrapUp::NotYet);
        assert_eq!(app.right_pane, RightPane::Related);
        assert_eq!(app.elapsed(), 0.0, "the clock starts over");
        assert_eq!(app.items.len(), 1, "the board stays");
        assert_eq!(app.selected_item().unwrap().id, "a");
        assert_eq!(
            app.notes.get("m").map(String::as_str),
            Some("notes"),
            "the ended meeting's write-up stays, under its own id"
        );
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
    fn a_mute_flip_holds_until_the_engine_says_otherwise_and_a_new_meeting_starts_clear() {
        let mut app = fresh();
        app.on_started("/m/1".into(), vec!["mic".into(), "system".into()]);
        assert!(!app.is_muted("mic"));
        app.on_muted("mic".into(), true);
        assert!(app.is_muted("mic"));
        assert!(!app.is_muted("system"));
        let (text, _, warn) = app.flash.clone().unwrap();
        assert!(text.contains("microphone muted") && text.contains("M unmutes"), "{text}");
        assert!(warn);
        app.on_muted("mic".into(), true);
        assert_eq!(app.muted, vec!["mic".to_string()], "a repeated flip is not doubled");
        app.on_muted("system".into(), true);
        app.on_muted("mic".into(), false);
        assert_eq!(app.muted, vec!["system".to_string()]);
        let (text, _, warn) = app.flash.clone().unwrap();
        assert!(text.contains("microphone live again"), "{text}");
        assert!(!warn);
        app.on_finished();
        assert!(app.muted.is_empty(), "nothing is muted once the recording ended");
        app.on_started("/m/1".into(), vec!["mic".into()]);
        app.on_muted("mic".into(), true);
        app.on_discarded();
        assert!(app.muted.is_empty(), "nor after a discard");
        app.begin_meeting("m2".into());
        assert!(app.muted.is_empty(), "a new engine starts unmuted");
        app.on_started("/m/2".into(), vec!["system".into()]);
        assert!(app.muted.is_empty());
        assert_eq!(source_name("mic"), "microphone");
        assert_eq!(source_name("system"), "system audio");
        assert_eq!(source_name("typed"), "typed");
        assert_eq!(mute_key("mic"), Some("M"));
        assert_eq!(mute_key("system"), Some("N"));
        assert_eq!(mute_key("typed"), None);
    }

    #[test]
    fn lifecycle_events_move_the_state_and_the_clock() {
        let mut app = fresh();
        assert_eq!(app.rec, RecState::Idle, "nothing records until r");
        assert!(!app.is_live());
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
            chunks: vec![ChunkView {
                id: "oc".into(),
                idx: 0,
                start: 0.0,
                end: 30.0,
                words: 1,
                text: "[00:00] mic: earlier".into(),
                summary: Some("old summary".into()),
                state: ChunkState::Done,
                facts: vec![Fact {
                    text: "old fact".into(),
                    where_: String::new(),
                }],
                contradictions: vec![],
                questions: vec![],
            }],
        });
        assert_eq!(app.session_index(), 1);
        assert_eq!(app.right_pane, RightPane::Items, "a session opens on its items");
        assert_eq!(app.chunk_cursor, Some(0), "and its Related pane on its first summary");
        assert_eq!(app.shown_chunks()[0].facts[0].text, "old fact");
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
        });
        assert_eq!(app.chunk_cursor, None, "no summaries to sit on");
        assert_eq!(app.selected_chunk(), None);
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
        assert_eq!(app.chunks[0].text, "[00:03] mic: does list print json");
        app.on_lookup_done(
            "c1",
            Lookup {
                summary: "asked about json".into(),
                facts: vec![
                    Fact {
                        text: "list prints plain text".into(),
                        where_: "todo.sh:18".into(),
                    },
                    Fact {
                        text: "no --json flag".into(),
                        where_: String::new(),
                    },
                ],
                contradictions: vec![Fact {
                    text: "said list has --json; it does not".into(),
                    where_: "todo.sh:18".into(),
                }],
                questions: vec!["one object per line, or an array?".into()],
            },
        );
        assert_eq!(app.chunks[0].state, ChunkState::Done);
        assert_eq!(app.summaries(), vec!["asked about json".to_string()]);
        assert_eq!(app.chunks[0].facts.len(), 2);
        assert_eq!(
            app.fact_lines(),
            vec![
                "list prints plain text (todo.sh:18)".to_string(),
                "no --json flag".to_string()
            ]
        );
        assert_eq!(
            app.contradiction_lines(),
            vec!["said list has --json; it does not (todo.sh:18)".to_string()]
        );
        assert_eq!(
            app.question_lines(),
            vec!["one object per line, or an array?".to_string()]
        );
        assert_eq!(app.transcript_text(), "[00:03] mic: does list print json");
        app.on_lookup_done("nope", Lookup::default());
        assert_eq!(app.fact_lines().len(), 2, "an unknown chunk changes nothing");

        assert_eq!(app.right_pane, RightPane::Related);
        app.toggle_pane();
        assert_eq!(app.right_pane, RightPane::Items);
        app.toggle_pane();
        assert_eq!(app.right_pane, RightPane::Summary);
        app.toggle_pane();
        assert!(app.bar_focused, "no question session: three panes, then the session bar");
        assert_eq!(app.right_pane, RightPane::Summary, "the bar leaves the pane as it was");
        app.toggle_pane();
        assert!(!app.bar_focused);
        assert_eq!(app.right_pane, RightPane::Related, "round again");
        app.bar_focused = true;
        app.show_summary();
        assert_eq!(app.right_pane, RightPane::Summary);
        assert!(!app.bar_focused, "m takes the keys off the bar");
        assert!(!app.term_running());
        assert!(app.term().is_none());
        app.right_pane = RightPane::Ask;
        app.term_focused = true;
        app.show_summary();
        assert_eq!(app.right_pane, RightPane::Ask, "the summary does not take the pane from Claude");
        app.toggle_pane();
        assert!(app.bar_focused, "after Claude Code comes the bar");
        assert!(!app.term_focused, "Tab hands the keys back");
        app.toggle_pane_back();
        assert!(!app.bar_focused);
        assert_eq!(app.right_pane, RightPane::Summary, "backwards from the bar: no terminal, so the summary");
        app.toggle_pane_back();
        assert_eq!(app.right_pane, RightPane::Items);
        app.toggle_pane_back();
        assert_eq!(app.right_pane, RightPane::Related);
        app.toggle_pane_back();
        assert!(app.bar_focused, "and backwards from Related is the bar");
        app.leave_bar();
        assert!(!app.bar_focused);
        assert_eq!(app.right_pane, RightPane::Related, "Enter/Esc leave the bar without moving the pane");
        app.bar_focused = true;
        app.focus_term();
        assert!(!app.bar_focused, "a takes the keys from the bar");
        assert!(app.term_focused);
    }

    #[test]
    fn the_summary_pane_reads_the_shown_meeting_and_knows_while_it_is_being_written() {
        let mut app = fresh();
        let mut old = meeting("old");
        old.summary = Some("An old meeting.".into());
        app.sessions = vec![meeting("m"), old.clone()];
        assert_eq!(app.shown_summary(), None);
        assert_eq!(app.shown_notes(), None);
        assert!(!app.summarizing(), "nothing in flight while recording");

        app.on_started(String::new(), vec!["mic".into()]);
        app.on_finished();
        app.wrap_up = WrapUp::Writing;
        assert!(app.summarizing(), "meet's own summary is being written");
        app.wrap_up = WrapUp::Done;
        assert!(!app.summarizing());
        app.hooks.on_announced(1, false);
        assert!(app.summarizing(), "a hook was announced and is about to run");
        app.hooks.on_started(1, 1, "/x/summarize-transcript.sh".into());
        assert!(app.summarizing(), "a hook is running");
        app.hooks.on_ended(1, 0, 2.0);
        assert!(!app.summarizing());

        app.meeting_summary = Some("Short.".into());
        app.notes.insert("m".into(), "## Summary\nLong.".into());
        assert_eq!(app.shown_summary(), Some("Short."));
        assert_eq!(app.shown_notes(), Some("## Summary\nLong."));
        app.enter_session(SessionView {
            meeting: old,
            transcript: vec![],
            chunks: vec![],
        });
        assert_eq!(app.shown_summary(), Some("An old meeting."));
        assert_eq!(app.shown_notes(), None, "not loaded for this session");
        app.notes.insert("old".into(), "## Summary\nOld and long.".into());
        assert_eq!(app.shown_notes(), Some("## Summary\nOld and long."));
        app.leave_session();
        assert_eq!(app.shown_notes(), Some("## Summary\nLong."));
    }

    #[test]
    fn the_question_terminal_belongs_to_the_shown_session() {
        let mut app = fresh();
        app.sessions = vec![meeting("m"), meeting("old")];
        assert_eq!(app.shown_meeting_id(), "m");
        app.enter_session(SessionView {
            meeting: meeting("old"),
            transcript: vec![],
            chunks: vec![],
        });
        assert_eq!(app.shown_meeting_id(), "old");
        assert!(app.term().is_none() && app.term_mut().is_none());
        assert_eq!(app.right_pane, RightPane::Items);
        // The Ask pane is kept across a switch only when the target has a terminal.
        app.right_pane = RightPane::Ask;
        app.leave_session();
        assert_eq!(app.shown_meeting_id(), "m");
        assert_eq!(app.right_pane, RightPane::Related, "no terminal on the live meeting");
    }

    #[test]
    fn usage_counts_ended_calls_and_running_agents_and_folds_an_agent_in_at_its_end() {
        let mut app = App::new(
            "/r".into(),
            "main".into(),
            false,
            Limits::default(),
            "m".into(),
            Vec::new(),
        );
        assert!(app.usage().is_zero());
        app.add_usage(Usage {
            input: 100,
            output: 10,
            cost_usd: 0.01,
            ..Default::default()
        });
        app.add_usage(Usage::default());
        assert_eq!(app.usage().input, 100);
        // Two turns of a running agent, in-flight; then the run's own total replaces them.
        app.on_agent_usage("a", Some(Usage { cache_read: 500, ..Default::default() }), false);
        app.on_agent_usage("a", Some(Usage { cache_read: 700, ..Default::default() }), false);
        assert_eq!(app.usage().input_total(), 1300);
        assert_eq!(app.spent.input_total(), 100);
        app.on_agent_usage(
            "a",
            Some(Usage {
                cache_read: 1200,
                output: 90,
                cost_usd: 0.20,
                ..Default::default()
            }),
            true,
        );
        assert!(app.agent_usage.is_empty());
        let u = app.usage();
        assert_eq!(u.input_total(), 1300);
        assert_eq!(u.output, 100);
        assert!((u.cost_usd - 0.21).abs() < 1e-9);
        // An agent killed before its result: what it had read so far stands.
        app.on_agent_usage("b", Some(Usage { input: 40, ..Default::default() }), false);
        app.on_agent_usage("b", None, true);
        assert_eq!(app.usage().input_total(), 1340);
        assert!(app.agent_usage.is_empty());
    }

    fn chunk_at(app: &mut App, id: &str, start: f64) {
        app.on_segment(Segment {
            source: "mic".into(),
            text: format!("chunk {id}"),
            start,
            end: start + 1.0,
        });
        let chunk = app.chunker.flush().unwrap();
        app.add_chunk(id.into(), &chunk);
    }

    #[test]
    fn the_related_pane_follows_the_newest_answered_summary_until_the_cursor_is_placed() {
        let mut app = fresh();
        app.on_started(String::new(), vec!["mic".into()]);
        assert_eq!(app.selected_chunk(), None);
        app.chunk_next();
        app.chunk_prev();
        assert_eq!(app.chunk_cursor, None, "nothing to move over");

        chunk_at(&mut app, "c1", 1.0);
        assert_eq!(app.selected_chunk(), Some(0), "no answer yet: the newest chunk");
        app.on_lookup_done("c1", Lookup { summary: "one".into(), ..Default::default() });
        chunk_at(&mut app, "c2", 70.0);
        chunk_at(&mut app, "c3", 140.0);
        assert_eq!(app.followed_chunk(), Some(0), "the newest with an answer");
        assert_eq!(app.selected_chunk(), Some(0));
        app.on_lookup_done("c2", Lookup { summary: "two".into(), ..Default::default() });
        assert_eq!(app.selected_chunk(), Some(1), "moved on with the answer");

        app.related_scroll = 5;
        app.chunk_prev();
        assert_eq!(app.chunk_cursor, Some(0));
        assert_eq!(app.related_scroll, 0, "a new summary reads from its top");
        app.chunk_prev();
        assert_eq!(app.chunk_cursor, Some(0), "clamped at the first");
        app.chunk_next();
        assert_eq!(app.chunk_cursor, None, "back on the followed one: following again");
        app.chunk_next();
        assert_eq!(app.chunk_cursor, Some(2), "past the followed one, onto the pending chunk");
        assert_eq!(app.selected_chunk(), Some(2));
        app.on_lookup_done("c3", Lookup { summary: "three".into(), ..Default::default() });
        assert_eq!(app.chunk_cursor, None, "a cursor on the newest summary is following it");
        assert_eq!(app.selected_chunk(), Some(2));

        app.chunk_prev();
        app.chunk_prev();
        assert_eq!(app.chunk_cursor, Some(0));
        app.related_scroll = 3;
        app.follow_chunks();
        assert_eq!(app.chunk_cursor, None);
        assert_eq!(app.related_scroll, 0);
        assert_eq!(app.selected_chunk(), Some(2));

        app.chunk_cursor = Some(99);
        assert_eq!(app.selected_chunk(), Some(2), "a stale cursor is clamped");
        app.toggle_pane();
        assert_eq!(app.chunk_cursor, Some(99), "Tab leaves the cursor alone");
        assert_eq!(app.related_scroll, 0);
    }

    #[test]
    fn a_session_view_keeps_the_ask_pane_when_that_session_has_a_terminal() {
        // `terms` is keyed by meeting id; a Term needs a PTY, so stand one in with /bin/sh.
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let dir = std::env::temp_dir();
        let args = vec!["-c".to_string(), "sleep 5".to_string()];
        let term = Term::spawn(
            crate::term::Spawn {
                program: "/bin/sh",
                args: &args,
                cwd: &dir,
                env: &[],
                cols: 20,
                rows: 4,
            },
            tx,
        )
        .unwrap();
        let mut app = fresh();
        app.sessions = vec![meeting("m"), meeting("old")];
        app.terms.insert("old".into(), term);
        assert!(app.term().is_none(), "the live meeting has none");
        app.toggle_pane();
        app.toggle_pane();
        app.toggle_pane();
        assert!(app.bar_focused, "Related → Items → Summary → the bar: no Ask pane for the live meeting");
        app.leave_bar();
        app.enter_session(SessionView {
            meeting: meeting("old"),
            transcript: vec![],
            chunks: vec![],
        });
        assert!(app.term_running(), "the past session's terminal is the shown one");
        assert_eq!(app.right_pane, RightPane::Items, "a session opens on its items");
        app.toggle_pane();
        app.toggle_pane();
        assert_eq!(app.right_pane, RightPane::Ask, "Items → Summary → Ask: this session has a terminal");
        app.leave_session();
        assert_eq!(app.right_pane, RightPane::Related, "the live meeting has no terminal: Related");
        app.enter_session(SessionView {
            meeting: meeting("old"),
            transcript: vec![],
            chunks: vec![],
        });
        app.right_pane = RightPane::Ask;
        app.leave_session();
        app.enter_session(SessionView {
            meeting: meeting("old"),
            transcript: vec![],
            chunks: vec![],
        });
        assert_eq!(app.right_pane, RightPane::Items, "coming from Related, a session opens on its items");
        if let Some(t) = app.term_mut() {
            t.kill();
        }
    }
}
