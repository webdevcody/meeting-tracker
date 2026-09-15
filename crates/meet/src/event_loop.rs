//! The loop: keys from the terminal, events from the recorder, the suggester and the
//! question terminals, and a one-second tick, all on one `tokio::select!`; redraws when
//! something changed.
//!
//! Nothing records until `r`, and nothing calls Claude while it records. `x` stops the
//! recording and stays: the whole transcript then goes to the suggester once, for the
//! meeting's summary and
//! write-up; `r` then starts the next recording as a new session (the ended one is closed
//! and slides along the bar), and whatever is still working on the ended meeting — its
//! summary call, the engine's onDone hooks — finishes in the background and lands on it:
//! recorder events are tagged with the recorder they came from, and the suggester's
//! answer with its meeting. A session that is closed again without a word said is dropped
//! (and any such leftovers from a crash on the way in), so the session bar holds only real
//! sessions.

use crate::app::{
    App, Overlay, RecState, RightPane, SessionView, TranscriptLine,
    WrapUp,
};
use crate::ask::{self, AskContext};
use crate::engine_hook::HookState;
use crate::term::{Spawn, Term, TermEvent};
use crate::layout::{HitTarget, LayoutPrefs, PointerShape, Splitter};
use crate::live::{self, LiveFiles};
use crate::recorder::{self, RecorderEvent, RecorderHandle, Segment};
use crate::selection;
use crate::settings::{self, Feature, Field, Settings};
use crate::store::Store;
use crate::suggest::{self, SuggestEvent, SuggestRequest, SuggesterConfig};
use crate::{git, ui};
use anyhow::{Context, Result};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{Stdout, Write};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;

pub struct Opts {
    pub dir: PathBuf,
    pub replay: Option<PathBuf>,
    pub replay_speed: f64,
    pub recorder: Option<String>,
    /// Flags for `meet-rec record`.
    pub record_args: Vec<String>,
    pub claude_bin: String,
    /// The `--*-model` / `--*-effort` flags passed this launch; they win over the settings.
    pub overrides: settings::Overrides,
    pub suggest_budget_usd: f64,
    pub no_suggest: bool,
}

const FRAME: Duration = Duration::from_millis(33);
/// How the footer names the key that takes the keys back from the question session.
pub const UNFOCUS_HINT: &str = "Ctrl+q";
/// Lines one wheel notch scrolls.
const WHEEL_LINES: i32 = 3;
const TICK: Duration = Duration::from_secs(1);
/// How long to wait for the engine to write the transcript after `q`.
const FINALIZE_FOR: Duration = Duration::from_secs(45);
/// How long a discard (`D`) waits for the engine to delete the meeting directory and exit
/// before meet leaves anyway (and deletes the directory itself).
const DISCARD_FOR: Duration = Duration::from_secs(10);

/// What `r` starts.
enum Source {
    Engine { path: PathBuf, args: Vec<String> },
    Replay { segments: Vec<Segment>, speed: f64 },
}

struct Loop {
    app: App,
    store: Store,
    /// The transcript and summary files under `<data dir>/sessions/<meeting id>/`, for
    /// the question session; `None` when the directory could not be created.
    live: Option<LiveFiles>,
    /// The engine, or the replay, of the recording going on; `None` while none is.
    recorder: Option<RecorderHandle>,
    /// How `r` starts one.
    source: Source,
    /// Where every recorder reports, tagged with which one it is, so the events of one
    /// still finishing (its onDone hooks) when the next recording starts are told apart.
    rec_tx: mpsc::UnboundedSender<(u64, RecorderEvent)>,
    /// The recorder started last; events tagged with an earlier number are a predecessor's.
    rec_gen: u64,
    suggest_cfg: SuggesterConfig,
    suggest_tx: mpsc::UnboundedSender<SuggestEvent>,
    /// The claude command every call and the question session start.
    claude_bin: String,
    /// Output and exits of the question terminals, tagged with the meeting each is about.
    term_tx: mpsc::UnboundedSender<(String, TermEvent)>,
    /// The question session's model and effort (`a`), as the settings resolve them.
    ask_model: Option<String>,
    ask_effort: Option<String>,
    /// Set by a quit while recording: leave once the engine has finished, no summary.
    finalize_deadline: Option<tokio::time::Instant>,
    /// Set by `x`: give up waiting for the engine's `finished` after this and wrap up anyway.
    stop_deadline: Option<tokio::time::Instant>,
}

pub async fn run(opts: Opts) -> Result<()> {
    let repo = git::toplevel(&opts.dir).await?;
    let branch = git::current_branch(&repo)
        .await
        .unwrap_or_else(|_| "detached HEAD".into());
    let settings = Settings::load()?;
    let store = Store::open(&crate::paths::db_path())?;
    let repo_key = repo.to_string_lossy().into_owned();
    let _ = store.close_stale_meetings(&repo_key);
    // Sessions a crashed launch left empty go, with their live-file directories.
    for id in store.delete_empty_meetings(&repo_key).unwrap_or_default() {
        let _ = std::fs::remove_dir_all(crate::paths::session_dir(&id));
    }
    let meeting_id = store.insert_meeting(&repo_key)?;
    let sessions = store.list_meetings(&repo_key)?;

    // Nothing records until `r`; but a missing engine or transcript is found out now.
    let source = match &opts.replay {
        Some(path) => Source::Replay {
            segments: recorder::load_transcript(path)?,
            speed: opts.replay_speed,
        },
        None => Source::Engine {
            path: recorder::locate_engine(opts.recorder.as_deref())?,
            args: opts.record_args.clone(),
        },
    };
    let (rec_tx, mut rec_rx) = mpsc::unbounded_channel::<(u64, RecorderEvent)>();

    let (suggest_tx, mut suggest_rx) = mpsc::unbounded_channel::<SuggestEvent>();
    let (term_tx, mut term_rx) = mpsc::unbounded_channel::<(String, TermEvent)>();

    let mut app = App::new(
        repo.clone(),
        branch,
        opts.replay.is_some(),
        meeting_id,
    );
    app.sessions = sessions;
    app.suggest_disabled = opts.no_suggest;
    app.settings = settings;
    app.overrides = opts.overrides.clone();
    match LayoutPrefs::load() {
        Ok(layout) => app.layout = layout,
        Err(e) => app.push_log(format!("⚠ {e:#}; the panes take their default sizes")),
    }
    let settings_path = Settings::path();
    app.push_log(if settings_path.is_file() {
        format!("settings: {}", settings_path.display())
    } else {
        format!(
            "settings: built-in defaults (, in the TUI writes {})",
            settings_path.display()
        )
    });
    let mut lp = Loop {
        app,
        store,
        live: None,
        recorder: None,
        source,
        rec_tx,
        rec_gen: 0,
        suggest_cfg: SuggesterConfig {
            claude_bin: opts.claude_bin.clone(),
            model: None,
            effort: None,
            repo: repo.clone(),
            max_budget_usd: opts.suggest_budget_usd,
        },
        suggest_tx,
        claude_bin: opts.claude_bin.clone(),
        term_tx,
        ask_model: None,
        ask_effort: None,
        finalize_deadline: None,
        stop_deadline: None,
    };
    lp.apply_settings();
    lp.open_live_files();
    let models = lp.models_line();
    lp.app.push_log(format!("models: {models}"));
    lp.app.push_log("nothing is recording — r starts the recording".into());

    let mut terminal = setup_terminal()?;
    let mut input = crossterm::event::EventStream::new();
    let mut next_draw = tokio::time::Instant::now();
    let mut next_tick = tokio::time::Instant::now() + TICK;
    // The pointer shape last asked of the terminal, so hovering a seam asks once.
    let mut pointer_sent = PointerShape::Default;
    let result: Result<()> = loop {
        if lp.app.dirty && tokio::time::Instant::now() >= next_draw {
            if let Err(e) = terminal.draw(|f| ui::draw(f, &mut lp.app)) {
                break Err(e.into());
            }
            lp.app.dirty = false;
            next_draw = tokio::time::Instant::now() + FRAME;
        }
        tokio::select! {
            _ = tokio::time::sleep_until(next_draw), if lp.app.dirty => {}
            _ = tokio::time::sleep_until(next_tick) => {
                next_tick = tokio::time::Instant::now() + TICK;
                lp.app.tick();
                if let Some(deadline) = lp.finalize_deadline {
                    if tokio::time::Instant::now() >= deadline {
                        lp.app.push_log("⚠ the recorder did not finish in time; leaving".into());
                        lp.app.should_quit = true;
                    }
                }
                if lp.stop_deadline.is_some_and(|d| tokio::time::Instant::now() >= d) {
                    lp.stop_deadline = None;
                    lp.app.push_log(
                        "⚠ the recorder did not finish in time; the transcript so far is kept".into(),
                    );
                    lp.app.on_engine_exited(None);
                    lp.wrap_up();
                }
            }
            ev = input.next() => match ev {
                Some(Ok(Event::Key(key))) => lp.on_key(key),
                Some(Ok(Event::Mouse(m))) => lp.on_mouse(m),
                Some(Ok(Event::Resize(_, _))) => lp.app.dirty = true,
                Some(Ok(_)) => {}
                Some(Err(e)) => break Err(e.into()),
                None => break Ok(()),
            },
            ev = rec_rx.recv() => if let Some((gen, ev)) = ev { lp.on_recorder(gen, ev) },
            ev = suggest_rx.recv() => if let Some(ev) = ev { lp.on_suggest(ev) },
            ev = term_rx.recv() => if let Some((id, ev)) = ev { lp.on_term(&id, ev) },
        }
        // The pointer shape (OSC 22): resize arrows over a seam. Terminals that do not
        // know the escape drop it.
        if lp.app.pointer_shape != pointer_sent {
            pointer_sent = lp.app.pointer_shape;
            let backend = terminal.backend_mut();
            let _ = write!(backend, "\x1b]22;{}\x1b\\", pointer_sent.osc_name());
            let _ = backend.flush();
        }
        // A copy the terminal was asked to make (OSC 52): over ssh, or with no clipboard tool.
        if let Some(text) = lp.app.pending_clipboard.take() {
            let backend = terminal.backend_mut();
            let _ = write!(backend, "{}", selection::osc52(&text));
            let _ = backend.flush();
        }
        lp.flush_live();
        if lp.app.should_quit {
            break Ok(());
        }
    };
    restore_terminal();
    for term in lp.app.terms.values_mut() {
        term.kill();
    }
    if lp.app.discard {
        // A discarded recording leaves nothing behind: not the session, not its live files,
        // and not the engine's meeting directory (the engine deletes it itself; this catches
        // one it did not get to).
        let _ = lp.store.delete_meeting(&lp.app.meeting_id);
        let _ = std::fs::remove_dir_all(crate::paths::session_dir(&lp.app.meeting_id));
        if let Some(dir) = lp
            .app
            .meeting_dir
            .as_deref()
            .filter(|d| std::path::Path::new(d).is_dir())
        {
            let _ = std::fs::remove_dir_all(dir);
        }
        eprintln!("recording discarded — nothing was kept");
    } else {
        lp.close_meeting();
        if let Some(dir) = &lp.app.meeting_dir {
            eprintln!("meeting saved to {dir}");
        }
    }
    result
}

impl Loop {
    fn repo_key(&self) -> String {
        self.app.repo.to_string_lossy().into_owned()
    }

    /// Every transcript line is kept in the database as it lands, so the session can be
    /// read back later, and appended to the live transcript file.
    fn push_segment(&mut self, seg: Segment) {
        if let Err(e) = self.store.insert_segment(
            &self.app.meeting_id,
            &seg.source,
            &seg.text,
            seg.start,
            seg.end,
        ) {
            self.app
                .push_log(format!("⚠ could not save a transcript line: {e:#}"));
        }
        if let Some(files) = &self.live {
            let line = TranscriptLine {
                at: seg.start,
                source: seg.source.clone(),
                text: seg.text.clone(),
            };
            if let Err(e) = files.append(&line) {
                self.app
                    .push_log(format!("⚠ could not append to the live transcript: {e:#}"));
            }
        }
        self.app.on_segment(seg);
    }

    /// Rewrite `summary.md` when what it describes changed.
    fn flush_live(&mut self) {
        if !self.app.live_stale {
            return;
        }
        self.app.live_stale = false;
        let Some(files) = &self.live else {
            return;
        };
        let text = live::summary_text(&self.app, &files.transcript_path());
        if let Err(e) = files.write_summary(&text) {
            self.app
                .push_log(format!("⚠ could not write the live summary: {e:#}"));
        }
    }

    /// The recording is over: the whole transcript goes to the suggester once — for the
    /// meeting's summary and its write-up — and the Summary pane takes
    /// the right pane to show that, and the engine's hooks, at work. Skipped when
    /// quitting, or when nothing was said.
    fn wrap_up(&mut self) {
        if self.app.wrap_up != WrapUp::NotYet
            || self.finalize_deadline.is_some()
            || self.app.discard
        {
            return;
        }
        if self.app.suggest_disabled {
            self.app.wrap_up = WrapUp::Done;
            return;
        }
        let transcript = self.app.transcript_text();
        if transcript.trim().is_empty() {
            self.app.wrap_up = WrapUp::Done;
            self.app
                .flash("nothing was said — no summary to write".into(), true);
            return;
        }
        self.app.wrap_up = WrapUp::Writing;
        self.app.live_stale = true;
        if self.app.session.is_none() {
            self.app.show_summary();
        }
        suggest::spawn(
            self.suggest_cfg.clone(),
            SuggestRequest {
                meeting_id: self.app.meeting_id.clone(),
                transcript,
            },
            self.suggest_tx.clone(),
        );
        self.app.dirty = true;
    }

    /// Ask before stopping the recording — the next one is a new session.
    fn confirm_end(&mut self) {
        if !self.app.is_live() {
            let msg = match self.app.rec {
                RecState::Idle => "nothing is recording — r starts the recording",
                RecState::Finalizing => "the recording is already stopping",
                _ => "the recording has already ended — r starts the next one",
            };
            self.app.flash(msg.into(), true);
            return;
        }
        self.app.overlay = Some(Overlay::ConfirmEnd);
        self.app.dirty = true;
    }

    /// Ask before throwing the recording away — there is no getting it back.
    fn confirm_discard(&mut self) {
        if !self.app.is_live() {
            self.app.flash(
                "the recording has already ended and is kept — D discards only a live recording"
                    .into(),
                true,
            );
            return;
        }
        if self.app.session.is_some() {
            self.app.flash(
                "Esc back to the live meeting first — D discards the live recording".into(),
                true,
            );
            return;
        }
        self.app.overlay = Some(Overlay::ConfirmDiscard);
        self.app.dirty = true;
    }

    /// Throw the recording away and quit: the engine stops, deletes its meeting directory
    /// and runs no hook; nothing more is written; the session is deleted on
    /// the way out (see the end of `run`).
    fn discard_now(&mut self) {
        self.app.overlay = None;
        if !self.app.is_live() {
            return;
        }
        self.app.discard = true;
        self.app.rec = RecState::Finalizing;
        self.app.status_text = Some("discarding the recording…".into());
        if let Some(r) = &self.recorder {
            r.discard();
        }
        self.finalize_deadline = Some(tokio::time::Instant::now() + DISCARD_FOR);
        self.app.dirty = true;
    }

    /// Stop the recording and stay: the engine finalizes, then the summary is written.
    fn stop_recording(&mut self) {
        self.app.overlay = None;
        if !self.app.is_live() {
            return;
        }
        self.app.rec = RecState::Finalizing;
        self.app.status_text = Some("stopping the recording…".into());
        if let Some(r) = &self.recorder {
            r.stop();
        }
        self.stop_deadline = Some(tokio::time::Instant::now() + FINALIZE_FOR);
        self.app.dirty = true;
    }

    // ----- starting a recording -----

    /// `r`: start recording. The first time, into the session this launch opened (dated
    /// from now, not from the launch); after a recording ended, into a new session — the
    /// ended one is closed and slides along the bar, and the transcript, the clock and the
    /// summary pane start over. Whatever is still working on the ended
    /// meeting (its summary call, the engine's onDone hooks) finishes in the background
    /// and lands on that meeting.
    fn start_recording(&mut self) {
        if self.app.discard {
            return;
        }
        if self.app.session.is_some() {
            self.view_session(0);
        }
        match self.app.rec {
            RecState::Idle => {
                if let Err(e) = self.store.restart_meeting(&self.app.meeting_id) {
                    self.app
                        .push_log(format!("⚠ could not date the session: {e:#}"));
                }
                self.refresh_sessions();
            }
            RecState::Ended | RecState::Failed(_) => {
                if !self.open_next_meeting() {
                    return;
                }
            }
            RecState::Finalizing => {
                self.app.flash(
                    "the recording is still stopping — r once it has ended".into(),
                    true,
                );
                return;
            }
            RecState::Starting | RecState::Recording | RecState::Paused => {
                self.app
                    .flash("already recording — x stops it".into(), true);
                return;
            }
        }
        self.open_live_files();
        self.spawn_recorder();
    }

    /// The live meeting is over for good — meet quits, or the next recording opens a new
    /// one: its row ends now, or goes (with its live files) when nothing was ever said in
    /// it, so the bar holds only meetings that happened.
    fn close_meeting(&mut self) {
        let id = self.app.meeting_id.clone();
        let segs = self.app.transcript.len() as i64;
        let empty = segs == 0 && self.store.meeting_is_empty(&id).unwrap_or(false);
        if empty {
            let _ = self.store.delete_meeting(&id);
            let _ = std::fs::remove_dir_all(crate::paths::session_dir(&id));
        } else {
            let _ = self.store.end_meeting(&id, segs);
        }
    }

    /// A recording ended here and the next one is starting: close this meeting and open
    /// a new one for it. `false` when the database would not take the new row.
    fn open_next_meeting(&mut self) -> bool {
        self.close_meeting();
        let id = match self.store.insert_meeting(&self.repo_key()) {
            Ok(id) => id,
            Err(e) => {
                self.app
                    .push_log(format!("⚠ could not open a new session: {e:#}"));
                return false;
            }
        };
        self.app.begin_meeting(id);
        self.refresh_sessions();
        self.app.flash(
            "new session — the last one is on the bar to the right".into(),
            false,
        );
        true
    }

    /// `app.sessions` from the database again, after a row was opened, closed or re-dated.
    fn refresh_sessions(&mut self) {
        match self.store.list_meetings(&self.repo_key()) {
            Ok(list) => self.app.sessions = list,
            Err(e) => self
                .app
                .push_log(format!("⚠ could not list the sessions: {e:#}")),
        }
        self.app.bar_first = 0;
        self.app.dirty = true;
    }

    /// The live meeting's transcript and summary files, fresh (a new header, dated by the
    /// session), for the question session: on launch, and again when a recording starts.
    fn open_live_files(&mut self) {
        let started = self
            .app
            .sessions
            .first()
            .map(|m| crate::when::local_datetime(m.started_at))
            .unwrap_or_default();
        self.live = match LiveFiles::create(
            crate::paths::session_dir(&self.app.meeting_id),
            &live::transcript_header(&self.app.repo_name, &started),
        ) {
            Ok(files) => {
                self.app.push_log(format!(
                    "live transcript: {}",
                    files.transcript_path().display()
                ));
                Some(files)
            }
            Err(e) => {
                self.app
                    .push_log(format!("⚠ no live transcript file: {e:#}"));
                None
            }
        };
        self.app.live_stale = true;
    }

    /// Start the engine (or the replay) for the live meeting; its events arrive tagged as
    /// the newest recorder's. A start that fails leaves the meeting idle, for `r` again.
    fn spawn_recorder(&mut self) {
        self.rec_gen += 1;
        let gen = self.rec_gen;
        let (tx, mut rx) = mpsc::unbounded_channel::<RecorderEvent>();
        let out = self.rec_tx.clone();
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                if out.send((gen, ev)).is_err() {
                    break;
                }
            }
        });
        let handle = match &self.source {
            Source::Replay { segments, speed } => {
                Ok(recorder::spawn_replay(segments.clone(), *speed, tx))
            }
            Source::Engine { path, args } => {
                recorder::spawn_engine(path, args, &self.app.repo, tx)
            }
        };
        self.stop_deadline = None;
        self.finalize_deadline = None;
        match handle {
            Ok(h) => {
                self.recorder = Some(h);
                self.app.rec = RecState::Starting;
                self.app.status_text = None;
                self.app.push_log(match &self.source {
                    Source::Replay { .. } => "replay started".to_string(),
                    Source::Engine { path, .. } => {
                        format!("recorder started: {}", path.display())
                    }
                });
            }
            Err(e) => {
                self.recorder = None;
                self.app
                    .push_log(format!("⚠ could not start the recorder: {e:#}"));
            }
        }
        self.app.dirty = true;
        self.app.live_stale = true;
    }

    /// An event from the recorder of a meeting that has since been left for the next one
    /// — it was still running its onDone hooks when `r` was pressed. What it prints stays
    /// in the log; the live state is the new recorder's and is left alone.
    fn on_previous_recorder(&mut self, ev: RecorderEvent) {
        match ev {
            RecorderEvent::Log(line) => self.app.push_log(line),
            RecorderEvent::HookEnded {
                index,
                status,
                secs,
            } => self.app.push_log(format!(
                "the previous recording's hook {index} {} after {secs:.0} s",
                if status == 0 {
                    "finished".to_string()
                } else {
                    format!("exited {status}")
                }
            )),
            RecorderEvent::Exited { code } => self.app.push_log(format!(
                "the previous recorder exited{}",
                code.map(|c| format!(" with status {c}")).unwrap_or_default()
            )),
            _ => {}
        }
    }

    fn on_recorder(&mut self, gen: u64, ev: RecorderEvent) {
        if gen != self.rec_gen {
            self.on_previous_recorder(ev);
            return;
        }
        match ev {
            RecorderEvent::Config { path, .. } => {
                if !path.is_empty() {
                    self.app.push_log(format!("config: {path}"));
                }
            }
            RecorderEvent::Started {
                meeting_dir,
                sources,
            } => {
                if !meeting_dir.is_empty() {
                    let _ = self
                        .store
                        .set_meeting_dir(&self.app.meeting_id, &meeting_dir);
                    if let Some(m) = self.app.sessions.first_mut() {
                        m.meeting_dir = Some(meeting_dir.clone());
                    }
                }
                self.app.on_started(meeting_dir, sources);
            }
            RecorderEvent::Status(t) => self.app.on_status(t),
            RecorderEvent::Segment(seg) => self.push_segment(seg),
            RecorderEvent::Paused(p) => self.app.on_paused(p),
            RecorderEvent::Muted { source, muted } => self.app.on_muted(source, muted),
            RecorderEvent::Clock(secs) => self.app.on_clock(secs),
            RecorderEvent::Log(line) => {
                // While an onDone hook runs, what the engine prints is the hook's.
                self.app.hooks.on_output(&line);
                self.app.push_log(line);
            }
            RecorderEvent::Hooks { count, skipped } => {
                self.app.hooks.on_announced(count, skipped);
                if skipped {
                    self.app.push_log("onDone hooks skipped (--no-hooks)".into());
                } else if count > 0 {
                    self.app.push_log(format!(
                        "the engine runs {count} onDone hook{} now",
                        if count == 1 { "" } else { "s" }
                    ));
                }
                self.app.dirty = true;
            }
            RecorderEvent::HookStarted {
                index,
                count,
                command,
            } => {
                self.app.hooks.on_started(index, count, command);
                let name = self.app.hooks.running().map(|r| r.name()).unwrap_or_default();
                self.app.flash(
                    format!("hook {index}/{count} {name} is running — m shows its output"),
                    false,
                );
            }
            RecorderEvent::HookEnded {
                index,
                status,
                secs,
            } => {
                let Some(run) = self.app.hooks.on_ended(index, status, secs) else {
                    return;
                };
                let name = run.name();
                let (msg, warn) = match (&run.state, &run.wrote) {
                    (HookState::Failed(code), _) => (
                        format!("⚠ hook {name} exited {code} after {secs:.0} s — m shows its output"),
                        true,
                    ),
                    (_, Some((path, _))) => (
                        format!("hook {name} wrote {} in {secs:.0} s — m shows it", path.display()),
                        false,
                    ),
                    (_, None) => (format!("hook {name} done in {secs:.0} s"), false),
                };
                self.app.push_log(msg.clone());
                self.app.flash(msg, warn);
            }
            RecorderEvent::Finished { meeting_dir, .. } => {
                self.stop_deadline = None;
                if !meeting_dir.is_empty() {
                    self.app.meeting_dir = Some(meeting_dir);
                }
                self.app.on_finished();
                let msg = match &self.app.meeting_dir {
                    Some(d) => format!("meeting saved to {d}"),
                    None => "replay finished".into(),
                };
                self.app.flash(msg, false);
                self.wrap_up();
            }
            RecorderEvent::Discarded { meeting_dir } => {
                self.stop_deadline = None;
                if !meeting_dir.is_empty() {
                    self.app.meeting_dir = Some(meeting_dir);
                }
                self.app.on_discarded();
                self.app
                    .push_log("recording discarded — nothing kept, no hook run".into());
            }
            RecorderEvent::Exited { code } => {
                self.stop_deadline = None;
                self.app.hooks.on_engine_exited();
                self.app.on_engine_exited(code);
                if self.finalize_deadline.take().is_some() {
                    self.app.should_quit = true;
                } else {
                    // Without a `finished` first, this is the engine dying: still write
                    // the summary from what was heard.
                    self.wrap_up();
                }
            }
        }
    }

    fn on_suggest(&mut self, ev: SuggestEvent) {
        match ev {
            SuggestEvent::Done {
                meeting_id,
                suggestion,
                usage,
            } => {
                self.app.add_usage(usage);
                // The answer is the live meeting's — unless `r` opened the next one
                // while it was being written: then it lands on its own meeting, which is
                // a past session by now.
                let current = meeting_id == self.app.meeting_id;
                let _ = self
                    .store
                    .set_meeting_summary(&meeting_id, &suggestion.summary);
                if !suggestion.notes.is_empty() {
                    let _ = self
                        .store
                        .set_meeting_notes(&meeting_id, &suggestion.notes);
                    self.app
                        .notes
                        .insert(meeting_id.clone(), suggestion.notes.clone());
                }
                if let Some(m) = self.app.sessions.iter_mut().find(|m| m.id == meeting_id) {
                    m.summary = Some(suggestion.summary.clone());
                }
                if let Some(v) = &mut self.app.session {
                    if v.meeting.id == meeting_id {
                        v.meeting.summary = Some(suggestion.summary.clone());
                    }
                }
                if current {
                    self.app.meeting_summary = Some(suggestion.summary.clone());
                    self.app.wrap_up = WrapUp::Done;
                    self.app.live_stale = true;
                }
                let msg = if !current {
                    "the previous meeting's summary is written — it is on the bar"
                } else if self.app.session.is_none() && self.app.right_pane == RightPane::Summary {
                    "summary written"
                } else {
                    "summary written — m shows it"
                };
                self.app.flash(msg.into(), false);
            }
            SuggestEvent::Failed {
                meeting_id,
                error,
                usage,
            } => {
                self.app.add_usage(usage);
                if meeting_id == self.app.meeting_id {
                    self.app.wrap_up = WrapUp::Failed(crate::stream_json::excerpt(&error, 160));
                    self.app.live_stale = true;
                }
                self.app.push_log(format!(
                    "⚠ could not write the summary: {error}"
                ));
            }
        }
    }

    // ----- the question session -----

    /// `a`: Claude Code in the right pane, with the keys, about the session on screen —
    /// the live meeting, or the past session the bar is on. Starts the session on an
    /// embedded terminal the first time (and again after it exited); otherwise just
    /// brings it back into view and hands it the keys. Each session keeps its own.
    fn ask(&mut self) {
        if self.app.term_running() {
            self.app.focus_term();
            self.app.flash(
                format!("Claude Code has the keys — {UNFOCUS_HINT} gives them back to meet"),
                false,
            );
            return;
        }
        let ctx = match &self.app.session {
            Some(v) => {
                // A past session: its files come from the database (the definitive record).
                let ctx = AskContext::new(self.app.repo.clone(), &v.meeting.id, false);
                if let Err(e) = ask::ensure_files(&self.store, &v.meeting, &ctx) {
                    self.app.flash(
                        format!("could not write the session's files: {e:#}"),
                        true,
                    );
                    return;
                }
                ctx
            }
            None => {
                let Some(files) = &self.live else {
                    self.app.flash(
                        "no live transcript file to ask about (see the log)".into(),
                        true,
                    );
                    return;
                };
                let ctx = AskContext {
                    repo: self.app.repo.clone(),
                    meeting_id: self.app.meeting_id.clone(),
                    session_dir: files.dir().to_path_buf(),
                    live: self.app.is_live(),
                };
                self.flush_live();
                ctx
            }
        };
        if let Err(e) = ctx.write_system_file() {
            self.app.flash(format!("could not start the question session: {e:#}"), true);
            return;
        }
        let meeting_id = ctx.meeting_id.clone();
        let launch = ask::Launch {
            claude_bin: self.claude_bin.clone(),
            model: self.ask_model.clone(),
            effort: self.ask_effort.clone(),
            ctx,
        };
        let args = ask::claude_args(&launch);
        let env = ask::claude_env(&launch.ctx);
        // Through the user's login shell, as nebula starts every session (see `shell`).
        let (program, shell_args) = crate::shell::claude_launch(&launch.claude_bin, &args);
        let (w, h) = crossterm::terminal::size().unwrap_or((120, 40));
        let inner = ui::ask_pane_inner(ratatui::layout::Rect::new(0, 0, w, h), &self.app.layout);
        // The terminal reports on its own channel; a task tags what it says with the
        // meeting it is about and forwards it to the loop.
        let (tx, mut rx) = mpsc::unbounded_channel::<TermEvent>();
        match Term::spawn(
            Spawn {
                program: &program,
                args: &shell_args,
                cwd: &self.app.repo,
                env: &env,
                cols: inner.width,
                rows: inner.height,
            },
            tx,
        ) {
            Ok(term) => {
                let out = self.term_tx.clone();
                let id = meeting_id.clone();
                tokio::spawn(async move {
                    while let Some(ev) = rx.recv().await {
                        if out.send((id.clone(), ev)).is_err() {
                            break;
                        }
                    }
                });
                if let Some(mut old) = self.app.terms.insert(meeting_id.clone(), term) {
                    old.kill();
                }
                self.app.focus_term();
                self.app.push_log(format!(
                    "question session started on {}: {} {} (via {program} -l -i)",
                    if self.app.session.is_some() {
                        format!("the session of {}", self.session_label())
                    } else {
                        "the live meeting".to_string()
                    },
                    launch.claude_bin,
                    args[..args.len() - 1].join(" ")
                ));
                self.app.flash(
                    format!("Claude Code has the keys — {UNFOCUS_HINT} gives them back to meet"),
                    false,
                );
            }
            Err(e) => {
                self.app
                    .push_log(format!("⚠ could not start the question session: {e:#}"));
                self.app.flash(
                    format!(
                        "could not start the question session: {}",
                        crate::stream_json::excerpt(&format!("{e:#}"), 120)
                    ),
                    true,
                );
            }
        }
    }

    /// What the question terminal about `meeting_id` said, or that it exited.
    fn on_term(&mut self, meeting_id: &str, ev: TermEvent) {
        let shown = self.app.shown_meeting_id() == meeting_id;
        let Some(term) = self.app.terms.get_mut(meeting_id) else {
            return;
        };
        match ev {
            TermEvent::Output(bytes) => term.process(&bytes),
            TermEvent::Exited(code) => {
                term.on_exited(code);
                if shown {
                    self.app.term_focused = false;
                }
                self.app.push_log(format!(
                    "question session exited{}",
                    code.map(|c| format!(" with status {c}")).unwrap_or_default()
                ));
                self.app
                    .flash("Claude Code exited — a starts a new session".into(), false);
            }
        }
        self.app.dirty = true;
    }

    /// The shown session's date, for messages.
    fn session_label(&self) -> String {
        match &self.app.session {
            Some(v) => crate::when::local_datetime(v.meeting.started_at),
            None => "the live meeting".into(),
        }
    }

    /// The terminal has the keys: everything goes to Claude except the way out.
    fn key_to_term(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Ctrl+q, and Ctrl+] (which arrives as Ctrl+5 from most terminals).
        let unfocus = ctrl
            && matches!(
                key.code,
                KeyCode::Char('q') | KeyCode::Char(']') | KeyCode::Char('5')
            );
        if unfocus {
            self.app.term_focused = false;
            self.app
                .flash("keys back to meet — a returns them to Claude".into(), false);
            self.app.dirty = true;
            return;
        }
        let sent = self
            .app
            .term_mut()
            .map(|term| term.send_key(&key))
            .unwrap_or(Ok(()));
        if let Err(e) = sent {
            self.app
                .flash(format!("could not reach Claude Code: {e}"), true);
        }
    }

    // ----- settings -----

    /// Resolve every feature's model and effort from the settings and this launch's flags,
    /// and hand them on: the suggester takes its at each start, the question session at `a`.
    /// Shared by startup and every change in the modal, so nothing can reach one and miss
    /// the other.
    fn apply_settings(&mut self) {
        let s = &self.app.settings;
        let o = &self.app.overrides;
        let eff = |f: Feature, field: Field| settings::effective(s, f, field, o.get(f, field));
        self.suggest_cfg.model = eff(Feature::Suggest, Field::Model);
        self.suggest_cfg.effort = eff(Feature::Suggest, Field::Effort);
        self.ask_model = eff(Feature::Ask, Field::Model);
        self.ask_effort = eff(Feature::Ask, Field::Effort);
    }

    /// One line for the log: what each feature runs with right now.
    fn models_line(&self) -> String {
        let show = |m: &Option<String>, e: &Option<String>| {
            let m = m.as_deref().unwrap_or("claude's own");
            match e {
                Some(e) => format!("{m} ({e})"),
                None => m.to_string(),
            }
        };
        format!(
            "summary {} · ask {}",
            show(&self.suggest_cfg.model, &self.suggest_cfg.effort),
            show(&self.ask_model, &self.ask_effort),
        )
    }

    /// `,`: the settings modal, on its first row.
    fn open_settings(&mut self) {
        self.app.overlay = Some(Overlay::Settings {
            cursor: 0,
            notice: None,
        });
        self.app.dirty = true;
    }

    fn settings_move(&mut self, to: usize) {
        self.app.overlay = Some(Overlay::Settings {
            cursor: to.min(settings::ROWS.len() - 1),
            notice: None,
        });
    }

    /// Step the row's value, write the file, apply it: a change is live the moment it is
    /// made. A file that cannot be written leaves the value where it was.
    fn settings_cycle(&mut self, cursor: usize, delta: isize) {
        let cursor = cursor.min(settings::ROWS.len() - 1);
        let (feature, field) = settings::ROWS[cursor];
        let mut next = self.app.settings.clone();
        next.cycle(feature, field, delta);
        let notice = match next.save() {
            Ok(()) => {
                self.app.settings = next;
                self.apply_settings();
                let value = self.app.settings.get(feature, field).to_string();
                match self.app.overrides.get(feature, field) {
                    Some(o) => (
                        format!(
                            "saved {value} · {} {o} wins until meet restarts",
                            settings::flag_name(feature, field)
                        ),
                        true,
                    ),
                    None => (
                        format!(
                            "saved · {} {} {value} · applies to {}",
                            feature.title().to_lowercase(),
                            field.label(),
                            feature.applies_to()
                        ),
                        false,
                    ),
                }
            }
            Err(e) => (format!("could not save the settings: {e:#}"), true),
        };
        self.app.overlay = Some(Overlay::Settings {
            cursor,
            notice: Some(notice),
        });
    }

    /// `R`, confirmed: every setting back to its default, written and applied.
    fn reset_settings(&mut self, cursor: usize) {
        let mut next = self.app.settings.clone();
        next.reset();
        let notice = match next.save() {
            Ok(()) => {
                self.app.settings = next;
                self.apply_settings();
                ("every setting is back to its default".to_string(), false)
            }
            Err(e) => (format!("could not save the settings: {e:#}"), true),
        };
        self.app.overlay = Some(Overlay::Settings {
            cursor,
            notice: Some(notice),
        });
    }

    // ----- sessions -----

    fn open_sessions(&mut self) {
        if let Ok(list) = self.store.list_meetings(&self.repo_key()) {
            self.app.sessions = list;
        }
        self.app.overlay = Some(Overlay::Sessions {
            cursor: self.app.session_index(),
        });
        self.app.dirty = true;
    }

    /// Show the session at `index` in `app.sessions`; 0 is the live meeting.
    fn view_session(&mut self, index: usize) {
        if index == 0 || self.app.sessions.get(index).is_none() {
            if self.app.session.is_some() {
                self.app.leave_session();
                self.app.flash("back to the live meeting".into(), false);
            }
            return;
        }
        let meeting = self.app.sessions[index].clone();
        let transcript: Vec<TranscriptLine> = match self.store.list_segments(&meeting.id) {
            Ok(rows) => rows
                .into_iter()
                .map(|s| TranscriptLine {
                    at: s.start_secs,
                    source: s.source,
                    text: s.text,
                })
                .collect(),
            Err(e) => {
                self.app
                    .flash(format!("could not load the session: {e:#}"), true);
                return;
            }
        };
        // Its write-up, for the Summary pane, the first time it is opened.
        if !self.app.notes.contains_key(&meeting.id) {
            if let Ok(Some(notes)) = self.store.meeting_notes(&meeting.id) {
                self.app.notes.insert(meeting.id.clone(), notes);
            }
        }
        let label = crate::when::local_datetime(meeting.started_at);
        self.app.enter_session(SessionView {
            meeting,
            transcript,
        });
        self.app.flash(
            format!("session {label} — ←/→ step, a ask Claude about it, Esc back to live"),
            false,
        );
    }

    /// One tab along the session bar: `→` (`l`) is older, `←` (`h`) newer — the bar has
    /// the newest at its left end, the live meeting.
    fn step_session(&mut self, delta: isize) {
        let cur = self.app.session_index() as isize;
        let next = cur + delta;
        if next < 0 {
            self.app
                .flash("that is the newest: the live meeting".into(), true);
            return;
        }
        if next as usize >= self.app.sessions.len() {
            self.app.flash("that is the oldest session".into(), true);
            return;
        }
        self.view_session(next as usize);
    }

    fn add_note(&mut self, text: String) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        let at = self.app.elapsed();
        self.push_segment(Segment {
            source: "typed".into(),
            text,
            start: at,
            end: at,
        });
    }

    /// Ask before quitting when something would be cut short: the recording, or the
    /// summary still being written (meet's call, or an engine hook — quitting takes the
    /// engine down with it).
    fn request_quit(&mut self) {
        let live = self.app.is_live();
        if live || self.app.summarizing() {
            self.app.overlay = Some(Overlay::ConfirmQuit { live });
            self.app.dirty = true;
        } else {
            self.app.should_quit = true;
        }
    }

    fn quit_now(&mut self) {
        self.app.overlay = None;
        if self.app.is_live() {
            self.app.rec = RecState::Finalizing;
            self.app.status_text = Some("stopping the recording…".into());
            self.app.dirty = true;
            if let Some(r) = &self.recorder {
                r.stop();
            }
            self.finalize_deadline = Some(tokio::time::Instant::now() + FINALIZE_FOR);
        } else {
            self.app.should_quit = true;
        }
    }

    /// `M` / `N`, or a click on the header's `mic` / `system`: mute that source, or
    /// unmute it. The recorder answers with a `Muted` event, which is what flips the
    /// header and the footer — so what is shown is always what the engine does.
    fn toggle_mute(&mut self, source: &str) {
        let Some(r) = self.recorder.as_ref().filter(|_| self.app.is_live()) else {
            self.app
                .flash("nothing is recording — r starts the recording".into(), true);
            return;
        };
        if !self.app.sources.iter().any(|s| s == source) {
            let flag = match source {
                "mic" => " (--no-mic)",
                "system" => " (--no-system)",
                _ => "",
            };
            self.app.flash(
                format!("no {} in this recording{flag}", crate::app::source_name(source)),
                true,
            );
            return;
        }
        r.toggle_mute(source);
    }

    // ----- the mouse -----

    /// The mouse, the way nebula has it: a click focuses what it lands on — a pane, a tab
    /// on the right pane, a session on the bar, a row of an overlay, a source in the header — or closes an overlay when it lands outside it;
    /// the wheel scrolls what it is over; a press on the border between two panes grabs
    /// the seam, dragging moves it, and the shares go to layout.json when the button
    /// comes up. A drag over the transcript selects its text and copies it when the button
    /// comes up; a double-click copies a word (`selection`).
    fn on_mouse(&mut self, m: MouseEvent) {
        let (x, y) = (m.column, m.row);
        let target = self.app.hit.hit_at(x, y);
        // Hover: the grip under the pointer lights up and the pointer turns into resize
        // arrows (in terminals that report plain motion); a drag keeps both honest past
        // the seam.
        let hover = match (self.app.splitter_drag, target) {
            (Some(drag), _) => Some(drag.which),
            // A selection dragged across a seam does not grab it.
            (None, HitTarget::Splitter(s))
                if !self.app.transcript_sel.is_some_and(|sel| sel.dragging) =>
            {
                Some(s)
            }
            _ => None,
        };
        if hover != self.app.hover_splitter {
            self.app.hover_splitter = hover;
            self.app.dirty = true;
        }
        self.app.pointer_shape = match hover {
            Some(Splitter::Columns) => PointerShape::ColResize,
            None => PointerShape::Default,
        };
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // Any press lets the last selection go; one on the transcript arms the next.
                if self.app.transcript_sel.take().is_some() {
                    self.app.dirty = true;
                }
                self.click(x, y, target)
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if !self.app.drag_selection_to(x, y) {
                    self.app.drag_splitter_to(x);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if self.app.release_splitter() {
                    self.save_layout();
                } else if let Some(text) = self.app.release_selection() {
                    self.copy(text);
                }
            }
            MouseEventKind::ScrollUp => self.wheel(target, -WHEEL_LINES),
            MouseEventKind::ScrollDown => self.wheel(target, WHEEL_LINES),
            _ => {}
        }
    }

    fn click(&mut self, x: u16, y: u16, target: HitTarget) {
        match target {
            HitTarget::Splitter(s) => self.app.grab_splitter(s, x),
            HitTarget::Source(i) => {
                if let Some(source) = self.app.sources.get(i).cloned() {
                    self.toggle_mute(&source);
                    self.app.dirty = true;
                }
            }
            HitTarget::Bar(tab) => {
                self.app.term_focused = false;
                self.app.bar_focused = true;
                self.app.dirty = true;
                if let Some(i) = tab {
                    if i != self.app.session_index() {
                        self.view_session(i);
                    }
                }
            }
            HitTarget::Transcript => {
                self.app.focus_panes();
                if let Some(word) = self.app.press_transcript(x, y) {
                    self.copy(word);
                }
            }
            HitTarget::RightTab(pane) => self.app.show_pane(pane),
            HitTarget::Right => {
                // Claude Code's pane: a click hands it the keys, as a does.
                if self.app.right_pane == RightPane::Ask && self.app.term_running() {
                    self.app.focus_term();
                } else {
                    self.app.focus_panes();
                }
            }
            HitTarget::Overlay(Some(row)) => self.click_overlay_row(row),
            HitTarget::Overlay(None) => {}
            HitTarget::Outside => {
                if self.app.overlay.is_some() {
                    self.close_overlay_by_click();
                }
            }
        }
    }

    /// Put `text` on the clipboard of the machine the user sits at, and say so: this one's,
    /// or — over ssh, or with no clipboard tool here — the terminal's, by OSC 52, which some
    /// terminals drop, so the flash names that route rather than promise.
    fn copy(&mut self, text: String) {
        let n = text.chars().count();
        let copied = format!(
            "copied {n} character{} to the clipboard",
            if n == 1 { "" } else { "s" }
        );
        if !selection::over_ssh() && selection::to_system_clipboard(&text) {
            self.app.flash(copied, false);
        } else {
            self.app.pending_clipboard = Some(text);
            self.app
                .flash(format!("{copied} through the terminal (OSC 52)"), false);
        }
    }

    /// A click on a row of the open overlay: a session opens, a setting is picked (and a
    /// second click on the picked one cycles it, as Enter does).
    fn click_overlay_row(&mut self, row: usize) {
        match self.app.overlay.clone() {
            Some(Overlay::Sessions { .. }) => {
                self.app.overlay = None;
                self.view_session(row);
            }
            Some(Overlay::Settings { cursor, .. }) if cursor == row => {
                self.settings_cycle(row, 1)
            }
            Some(Overlay::Settings { .. }) => self.settings_move(row),
            _ => {}
        }
        self.app.dirty = true;
    }

    /// A click outside the open overlay closes it, as Esc does: a confirm box answers no,
    /// the reset confirm goes back to the settings it came from.
    fn close_overlay_by_click(&mut self) {
        self.app.overlay = match self.app.overlay.take() {
            Some(Overlay::ConfirmReset { cursor }) => Some(Overlay::Settings {
                cursor,
                notice: None,
            }),
            _ => None,
        };
        self.app.dirty = true;
    }

    /// The wheel: `delta` lines, negative up — the transcript and the right pane scroll;
    /// the session list and the settings step their selection.
    fn wheel(&mut self, target: HitTarget, delta: i32) {
        let up = delta < 0;
        match target {
            HitTarget::Overlay(_) | HitTarget::Outside if self.app.overlay.is_some() => {
                match &mut self.app.overlay {
                    Some(Overlay::Sessions { cursor }) => {
                        let max = self.app.sessions.len().saturating_sub(1);
                        *cursor = if up { cursor.saturating_sub(1) } else { (*cursor + 1).min(max) };
                    }
                    Some(Overlay::Settings { cursor, .. }) => {
                        let max = settings::ROWS.len() - 1;
                        *cursor = if up { cursor.saturating_sub(1) } else { (*cursor + 1).min(max) };
                    }
                    _ => {}
                }
            }
            HitTarget::Transcript => self.app.scroll_transcript(delta),
            HitTarget::Right => self.app.scroll_right_pane(delta),
            _ => {}
        }
        self.app.dirty = true;
    }

    /// The seam as dragged, to layout.json, for the next launch.
    fn save_layout(&mut self) {
        if let Err(e) = self.app.layout.save() {
            self.app.push_log(format!(
                "⚠ could not write {}: {e:#}",
                LayoutPrefs::path().display()
            ));
            self.app
                .flash("the layout could not be saved (see the log)".into(), true);
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if self.app.term_focused && self.app.overlay.is_none() {
            self.key_to_term(key);
            return;
        }
        let ctrl_c =
            key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
        if let Some(overlay) = self.app.overlay.clone() {
            match overlay {
                Overlay::Note { mut input } => match key.code {
                    KeyCode::Esc => self.app.overlay = None,
                    KeyCode::Enter => {
                        self.app.overlay = None;
                        self.add_note(input);
                    }
                    KeyCode::Backspace => {
                        input.pop();
                        self.app.overlay = Some(Overlay::Note { input });
                    }
                    KeyCode::Char(c) if !ctrl_c => {
                        input.push(c);
                        self.app.overlay = Some(Overlay::Note { input });
                    }
                    _ if ctrl_c => self.app.overlay = None,
                    _ => {}
                },
                Overlay::ConfirmQuit { live, .. } => match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => self.quit_now(),
                    KeyCode::Char('c') if ctrl_c => self.quit_now(),
                    KeyCode::Char('x') | KeyCode::Char('X') if live => self.stop_recording(),
                    KeyCode::Char('d') | KeyCode::Char('D') if live => self.confirm_discard(),
                    _ => self.app.overlay = None,
                },
                Overlay::ConfirmEnd => {
                    self.app.overlay = None;
                    if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter) {
                        self.stop_recording();
                    }
                }
                Overlay::ConfirmDiscard => {
                    self.app.overlay = None;
                    if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter) {
                        self.discard_now();
                    }
                }
                Overlay::Sessions { cursor } => match key.code {
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('S') => {
                        self.app.overlay = None
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.app.overlay = Some(Overlay::Sessions {
                            cursor: cursor.saturating_sub(1),
                        })
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        let max = self.app.sessions.len().saturating_sub(1);
                        self.app.overlay = Some(Overlay::Sessions {
                            cursor: (cursor + 1).min(max),
                        })
                    }
                    KeyCode::Enter => {
                        self.app.overlay = None;
                        self.view_session(cursor);
                    }
                    _ if ctrl_c => self.request_quit(),
                    _ => {}
                },
                Overlay::Settings { cursor, .. } => match key.code {
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char(',') => {
                        self.app.overlay = None
                    }
                    KeyCode::Down | KeyCode::Char('j') => self.settings_move(cursor + 1),
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.settings_move(cursor.saturating_sub(1))
                    }
                    KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter | KeyCode::Char(' ') => {
                        self.settings_cycle(cursor, 1)
                    }
                    KeyCode::Left | KeyCode::Char('h') => self.settings_cycle(cursor, -1),
                    KeyCode::Char(c @ '1'..='2') => {
                        let n = c as usize - '1' as usize;
                        self.settings_move(settings::first_row_of(Feature::ALL[n]))
                    }
                    KeyCode::Char('R') => {
                        self.app.overlay = Some(Overlay::ConfirmReset { cursor })
                    }
                    _ if ctrl_c => self.request_quit(),
                    _ => {}
                },
                Overlay::ConfirmReset { cursor } => {
                    if matches!(
                        key.code,
                        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter
                    ) {
                        self.reset_settings(cursor);
                    } else {
                        self.app.overlay = Some(Overlay::Settings {
                            cursor,
                            notice: None,
                        });
                    }
                }
                Overlay::Help => self.app.overlay = None,
            }
            self.app.dirty = true;
            return;
        }
        // The session bar has the keys: h/l (and ←/→) walk the sessions, Enter and Esc
        // hand the keys back to the panes; a key meant for a pane goes there and takes the
        // focus with it.
        if self.app.bar_focused {
            match key.code {
                KeyCode::Char('h') => {
                    self.step_session(-1);
                    return;
                }
                KeyCode::Char('l') => {
                    self.step_session(1);
                    return;
                }
                KeyCode::Enter | KeyCode::Esc => {
                    self.app.leave_bar();
                    return;
                }
                KeyCode::Char('j') | KeyCode::Down | KeyCode::Char('k') | KeyCode::Up => {
                    self.app.leave_bar();
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Esc if self.app.session.is_some() => self.view_session(0),
            KeyCode::Char('q') | KeyCode::Esc => self.request_quit(),
            KeyCode::Char('c') if ctrl_c => self.request_quit(),
            KeyCode::Char('j') | KeyCode::Down => match self.app.right_pane {
                RightPane::Summary => self.app.summary_scroll += 1,
                RightPane::Ask => {}
            },
            KeyCode::Char('k') | KeyCode::Up => match self.app.right_pane {
                RightPane::Summary => {
                    self.app.summary_scroll = self.app.summary_scroll.saturating_sub(1)
                }
                RightPane::Ask => {}
            },
            KeyCode::Char('m') => self.app.show_summary(),
            KeyCode::Char('r') => self.start_recording(),
            KeyCode::Char('x') => self.confirm_end(),
            KeyCode::Char('D') => self.confirm_discard(),
            KeyCode::Tab => self.app.toggle_pane(),
            KeyCode::BackTab => self.app.toggle_pane_back(),
            KeyCode::Char('a') => self.ask(),
            KeyCode::Char('S') => self.open_sessions(),
            KeyCode::Left => self.step_session(-1),
            KeyCode::Right => self.step_session(1),
            KeyCode::Char('i') | KeyCode::Char('n') => {
                self.app.overlay = Some(Overlay::Note {
                    input: String::new(),
                });
            }
            KeyCode::Char('s') => {
                if self.app.is_live() {
                    self.app.flash(
                        "the summary is written when the recording stops — x stops it".into(),
                        true,
                    );
                } else if self.app.session.is_some() {
                    self.app
                        .flash("Esc back to the live meeting first".into(), true);
                } else if self.app.rec == RecState::Idle {
                    self.app
                        .flash("nothing recorded yet — r starts the recording".into(), true);
                } else if self.app.wrap_up == WrapUp::Writing {
                    self.app.flash(
                        "the summary is being written — m shows the progress".into(),
                        true,
                    );
                } else if self.app.suggest_disabled {
                    self.app
                        .flash("suggestions are off (--no-suggest)".into(), true);
                } else {
                    self.app.wrap_up = WrapUp::NotYet;
                    self.app
                        .flash("writing the summary again…".into(), false);
                    self.wrap_up();
                }
            }
            KeyCode::Char(' ') => {
                if let Some(r) = self.recorder.as_ref().filter(|_| self.app.is_live()) {
                    r.toggle_pause();
                }
            }
            KeyCode::Char('M') => self.toggle_mute("mic"),
            KeyCode::Char('N') => self.toggle_mute("system"),
            KeyCode::PageUp | KeyCode::Char('K') => self.app.scroll_transcript(-10),
            KeyCode::PageDown | KeyCode::Char('J') => self.app.scroll_transcript(10),
            KeyCode::Char('G') => {
                self.app.transcript_scroll = None;
            }
            KeyCode::Char('[') => match self.app.right_pane {
                RightPane::Summary => {
                    self.app.summary_scroll = self.app.summary_scroll.saturating_sub(3)
                }
                RightPane::Ask => {}
            },
            KeyCode::Char(']') => match self.app.right_pane {
                RightPane::Summary => self.app.summary_scroll += 3,
                RightPane::Ask => {}
            },
            KeyCode::Char(',') => self.open_settings(),
            KeyCode::Char('?') => self.app.overlay = Some(Overlay::Help),
            _ => {}
        }
        self.app.dirty = true;
    }
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    crossterm::terminal::enable_raw_mode().context("enable raw mode")?;
    let mut stdout = std::io::stdout();
    // The mouse too: a click on the header's `mic` / `system` mutes that source.
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture
    )
    .context("enter the alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    // A panic must not leave the terminal raw.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));
    Ok(terminal)
}

fn restore_terminal() {
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = crossterm::execute!(
        std::io::stdout(),
        // The pointer back to an arrow, should meet leave mid-hover.
        crossterm::style::Print("\x1b]22;default\x1b\\"),
        crossterm::event::DisableMouseCapture,
        crossterm::terminal::LeaveAlternateScreen
    );
}
