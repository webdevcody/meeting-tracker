//! The loop: keys from the terminal, events from the recorder, the suggester and the agent
//! runs, and a one-second tick, all on one `tokio::select!`; redraws when something changed.
//!
//! On launch, agents the last `meet` took down with it are resumed before anything else
//! (unless `--no-resume`); on quit, running agents get a SIGTERM so Claude flushes their
//! sessions, and their rows stay `running` for the next launch to pick up.

use crate::app::{App, ChunkState, Overlay, RecState, SessionView, TranscriptLine};
use crate::chunker::{Chunk, Limits};
use crate::recorder::{self, RecorderEvent, RecorderHandle, Segment};
use crate::runner::{spawn_run, RunContext, RunEvent};
use crate::store::{ActionItem, ItemStatus, Store};
use crate::suggest::{spawn_worker, SuggestEvent, SuggestRequest, SuggesterConfig};
use crate::{config, git, ui};
use anyhow::{Context, Result};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::collections::HashMap;
use std::io::Stdout;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::{mpsc, watch};

pub struct Opts {
    pub dir: PathBuf,
    pub replay: Option<PathBuf>,
    pub replay_speed: f64,
    pub recorder: Option<String>,
    /// Flags for `meet-rec record`.
    pub record_args: Vec<String>,
    /// `--config`: the meet.json to read (the engine gets the same flag).
    pub config: Option<String>,
    pub claude_bin: String,
    pub gh_bin: String,
    pub suggest_model: Option<String>,
    pub agent_model: Option<String>,
    pub suggest_budget_usd: f64,
    pub no_suggest: bool,
    /// Leave interrupted agents stopped instead of resuming them on launch.
    pub no_resume: bool,
    /// `--on-done` / `--on-done-file`: overrides the config's `agent.onDone`.
    pub on_done: Option<String>,
    pub limits: Limits,
    pub remote: String,
}

const FRAME: Duration = Duration::from_millis(33);
const TICK: Duration = Duration::from_secs(1);
/// How long to wait for the engine to write the transcript after `q`.
const FINALIZE_FOR: Duration = Duration::from_secs(45);
/// How long running agents get to exit on SIGTERM when meet quits.
const AGENT_EXIT_GRACE: Duration = Duration::from_millis(1500);

struct Loop {
    app: App,
    store: Store,
    recorder: RecorderHandle,
    suggest_tx: mpsc::UnboundedSender<SuggestRequest>,
    run_tx: mpsc::UnboundedSender<RunEvent>,
    run_ctx: RunContext,
    /// item id → the stop switch of its running agent.
    stops: HashMap<String, watch::Sender<bool>>,
    finalize_deadline: Option<tokio::time::Instant>,
}

pub async fn run(opts: Opts) -> Result<()> {
    let repo = git::toplevel(&opts.dir).await?;
    let base_branch = git::current_branch(&repo).await?;
    let cfg = config::load(opts.config.as_deref().map(Path::new), &repo)?;
    let store = Store::open(&crate::paths::db_path())?;
    let repo_key = repo.to_string_lossy().into_owned();
    let _ = store.close_stale_meetings(&repo_key);
    let interrupted = if opts.no_resume {
        store.stop_interrupted_items(&repo_key)?;
        Vec::new()
    } else {
        store.interrupted_items(&repo_key)?
    };
    let items = store.list_items(&repo_key)?;
    let meeting_id = store.insert_meeting(&repo_key)?;
    let sessions = store.list_meetings(&repo_key)?;

    let (rec_tx, mut rec_rx) = mpsc::unbounded_channel::<RecorderEvent>();
    let recorder = match &opts.replay {
        Some(path) => {
            let segments = recorder::load_transcript(path)?;
            recorder::spawn_replay(segments, opts.replay_speed, rec_tx)
        }
        None => {
            let engine = recorder::locate_engine(opts.recorder.as_deref())?;
            recorder::spawn_engine(&engine, &opts.record_args, &repo, rec_tx)?
        }
    };

    let (suggest_tx, suggest_rx) = mpsc::unbounded_channel::<SuggestRequest>();
    let (suggest_ev_tx, mut suggest_ev_rx) = mpsc::unbounded_channel::<SuggestEvent>();
    let _suggester = spawn_worker(
        SuggesterConfig {
            claude_bin: opts.claude_bin.clone(),
            model: opts.suggest_model.clone(),
            repo: repo.clone(),
            max_budget_usd: opts.suggest_budget_usd,
        },
        suggest_rx,
        suggest_ev_tx,
    );
    let (run_tx, mut run_rx) = mpsc::unbounded_channel::<RunEvent>();

    let mut app = App::new(
        repo.clone(),
        base_branch.clone(),
        opts.replay.is_some(),
        opts.limits.clone(),
        meeting_id,
        items,
    );
    app.sessions = sessions;
    app.suggest_disabled = opts.no_suggest;
    if let Some(p) = &cfg.path {
        app.push_log(format!("agent config: {}", p.display()));
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("meet"));
    let mut lp = Loop {
        app,
        store,
        recorder,
        suggest_tx,
        run_tx,
        run_ctx: RunContext {
            repo: repo.clone(),
            base_branch,
            remote: opts.remote.clone(),
            claude_bin: opts.claude_bin.clone(),
            gh_bin: opts.gh_bin.clone(),
            agent_model: opts.agent_model.clone().or(cfg.agent.model.clone()),
            meeting_context: String::new(),
            on_done: opts.on_done.clone().or(cfg.agent.on_done.clone()),
            exe,
        },
        stops: HashMap::new(),
        finalize_deadline: None,
    };
    if lp.run_ctx.on_done.is_some() {
        lp.app
            .push_log("on-done prompt configured; agents get it through a Stop hook".into());
    }
    lp.resume_interrupted(interrupted);

    let mut terminal = setup_terminal()?;
    let mut input = crossterm::event::EventStream::new();
    let mut next_draw = tokio::time::Instant::now();
    let mut next_tick = tokio::time::Instant::now() + TICK;
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
                if let Some(chunk) = lp.app.tick() {
                    lp.enqueue_chunk(chunk);
                }
                lp.refresh_log_overlay();
                if let Some(deadline) = lp.finalize_deadline {
                    if tokio::time::Instant::now() >= deadline {
                        lp.app.push_log("⚠ the recorder did not finish in time; leaving".into());
                        lp.app.should_quit = true;
                    }
                }
            }
            ev = input.next() => match ev {
                Some(Ok(Event::Key(key))) => lp.on_key(key),
                Some(Ok(Event::Resize(_, _))) => lp.app.dirty = true,
                Some(Ok(_)) => {}
                Some(Err(e)) => break Err(e.into()),
                None => break Ok(()),
            },
            ev = rec_rx.recv() => if let Some(ev) = ev { lp.on_recorder(ev) },
            ev = suggest_ev_rx.recv() => if let Some(ev) = ev { lp.on_suggest(ev) },
            ev = run_rx.recv() => if let Some(ev) = ev { lp.on_run(ev) },
        }
        if lp.app.should_quit {
            break Ok(());
        }
    };
    restore_terminal();
    let segs = lp.app.transcript.len() as i64;
    let _ = lp.store.end_meeting(&lp.app.meeting_id, segs);
    if let Some(dir) = &lp.app.meeting_dir {
        eprintln!("meeting saved to {dir}");
    }
    let done: Vec<_> = lp
        .app
        .items
        .iter()
        .filter(|i| i.status == ItemStatus::Done)
        .collect();
    if !done.is_empty() {
        eprintln!("pull requests opened this session:");
        for i in done {
            if let Some(u) = &i.pr_url {
                eprintln!("  {u}  {}", i.title);
            }
        }
    }
    // Running agents: a SIGTERM so Claude flushes the session, then a moment to exit. Their
    // rows stay `running`, which is what the next launch resumes.
    if !lp.stops.is_empty() {
        let n = lp.stops.len();
        for stop in lp.stops.values() {
            let _ = stop.send(true);
        }
        eprintln!(
            "{n} agent{} still running — stopped; `meet` resumes {} next time it runs here (--no-resume to leave {} stopped)",
            if n == 1 { "" } else { "s" },
            if n == 1 { "it" } else { "them" },
            if n == 1 { "it" } else { "them" },
        );
        tokio::time::sleep(AGENT_EXIT_GRACE).await;
    }
    result
}

impl Loop {
    fn repo_key(&self) -> String {
        self.app.repo.to_string_lossy().into_owned()
    }

    /// Agents the last `meet` left running are started again, oldest first.
    fn resume_interrupted(&mut self, items: Vec<ActionItem>) {
        if items.is_empty() {
            return;
        }
        let n = items.len();
        for item in items {
            self.start_run(item, true);
        }
        self.app.flash(
            format!(
                "resumed {n} agent{} from the last session",
                if n == 1 { "" } else { "s" }
            ),
            false,
        );
    }

    /// Every transcript line is kept in the database as it lands, so the session can be
    /// read back later — and closes a chunk when it fills one.
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
        if let Some(chunk) = self.app.on_segment(seg) {
            self.enqueue_chunk(chunk);
        }
    }

    fn enqueue_chunk(&mut self, chunk: Chunk) {
        if self.app.suggest_disabled {
            return;
        }
        let idx = self.app.chunks.len() as i64;
        let text = chunk.text();
        let id = match self
            .store
            .insert_chunk(&self.app.meeting_id, idx, chunk.start, chunk.end, &text)
        {
            Ok(id) => id,
            Err(e) => {
                self.app
                    .push_log(format!("⚠ could not save the chunk: {e:#}"));
                return;
            }
        };
        let idx = self.app.add_chunk(id.clone(), &chunk);
        let req = SuggestRequest {
            chunk_id: id,
            chunk_idx: idx,
            chunk_text: text,
            start: chunk.start,
            end: chunk.end,
            summaries_so_far: self.app.summaries(),
            board: self.app.board(),
        };
        let _ = self.suggest_tx.send(req);
    }

    fn on_recorder(&mut self, ev: RecorderEvent) {
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
            RecorderEvent::Clock(secs) => self.app.on_clock(secs),
            RecorderEvent::Log(line) => self.app.push_log(line),
            RecorderEvent::Finished { meeting_dir, .. } => {
                if !meeting_dir.is_empty() {
                    self.app.meeting_dir = Some(meeting_dir);
                }
                if let Some(chunk) = self.app.on_finished() {
                    self.enqueue_chunk(chunk);
                }
                let msg = match &self.app.meeting_dir {
                    Some(d) => format!("meeting saved to {d}"),
                    None => "replay finished".into(),
                };
                self.app.flash(msg, false);
            }
            RecorderEvent::Exited { code } => {
                if let Some(chunk) = self.app.on_engine_exited(code) {
                    self.enqueue_chunk(chunk);
                }
                if self.finalize_deadline.take().is_some() {
                    self.app.should_quit = true;
                }
            }
        }
    }

    fn on_suggest(&mut self, ev: SuggestEvent) {
        match ev {
            SuggestEvent::Started { chunk_id } => {
                if let Some(c) = self.app.chunk_mut(&chunk_id) {
                    c.state = ChunkState::Summarizing;
                }
                self.app.dirty = true;
            }
            SuggestEvent::Done {
                chunk_id,
                suggestion,
            } => {
                let _ = self.store.set_chunk_summary(&chunk_id, &suggestion.summary);
                if let Some(c) = self.app.chunk_mut(&chunk_id) {
                    c.summary = Some(suggestion.summary.clone());
                    c.state = ChunkState::Done;
                }
                if let Some(m) = self.app.sessions.first_mut() {
                    if m.first_summary.is_none() {
                        m.first_summary = Some(suggestion.summary.clone());
                    }
                }
                let repo_key = self.repo_key();
                let mut new = Vec::new();
                for s in &suggestion.items {
                    match self.store.insert_item(
                        &self.app.meeting_id,
                        Some(&chunk_id),
                        &repo_key,
                        &s.title,
                        &s.prompt,
                        &s.why,
                    ) {
                        Ok(item) => new.push(item),
                        Err(e) => self
                            .app
                            .push_log(format!("⚠ could not save an action item: {e:#}")),
                    }
                }
                let n = new.len();
                self.app.insert_items(new);
                if n > 0 {
                    let where_ = if self.app.session.is_some() {
                        " (on the live board)"
                    } else {
                        " — Enter runs the selected one"
                    };
                    self.app.flash(
                        format!(
                            "{n} new action item{}{where_}",
                            if n == 1 { "" } else { "s" }
                        ),
                        false,
                    );
                }
            }
            SuggestEvent::Failed {
                chunk_id,
                chunk_idx,
                error,
            } => {
                if let Some(c) = self.app.chunk_mut(&chunk_id) {
                    c.state = ChunkState::Failed(crate::stream_json::excerpt(&error, 120));
                }
                self.app.push_log(format!(
                    "⚠ summarizer failed on chunk {}: {error}",
                    chunk_idx + 1
                ));
            }
        }
    }

    fn on_run(&mut self, ev: RunEvent) {
        match ev {
            RunEvent::Started {
                item_id,
                branch,
                worktree,
                log_path,
                resumed,
            } => {
                if resumed {
                    let title = self
                        .app
                        .item_mut(&item_id)
                        .map(|i| i.title.clone())
                        .unwrap_or_default();
                    self.app
                        .push_log(format!("resuming the agent for \"{title}\" on {branch}"));
                }
                let _ = self.store.set_item_outcome(
                    &item_id,
                    ItemStatus::Running,
                    &crate::store::RunOutcome {
                        branch: Some(branch.clone()),
                        worktree_path: Some(worktree.clone()),
                        log_path: Some(log_path.clone()),
                        ..Default::default()
                    },
                );
                self.app
                    .on_run_started(&item_id, branch, worktree, log_path);
            }
            RunEvent::Session {
                item_id,
                session_id,
            } => {
                let _ = self.store.set_item_session(&item_id, &session_id);
                self.app.on_run_session(&item_id, session_id);
            }
            RunEvent::Phase { item_id, phase } => {
                let _ = self.store.set_item_phase(&item_id, phase);
                self.app.on_run_phase(&item_id, phase);
            }
            RunEvent::Activity { item_id, text } => {
                self.app.activity.insert(item_id, text);
                self.app.dirty = true;
            }
            RunEvent::Finished { item_id, outcome } => {
                self.stops.remove(&item_id);
                let _ = self
                    .store
                    .set_item_outcome(&item_id, ItemStatus::Done, &outcome);
                self.app.on_run_ended(&item_id, ItemStatus::Done, &outcome);
                if let Some(u) = &outcome.pr_url {
                    self.app.flash(format!("pull request opened: {u}"), false);
                }
            }
            RunEvent::Stopped { item_id, outcome } => {
                self.stops.remove(&item_id);
                let _ = self
                    .store
                    .set_item_outcome(&item_id, ItemStatus::Stopped, &outcome);
                self.app
                    .on_run_ended(&item_id, ItemStatus::Stopped, &outcome);
                self.app
                    .flash("agent stopped — Enter resumes it where it was".into(), false);
            }
            RunEvent::Failed { item_id, outcome } => {
                self.stops.remove(&item_id);
                let _ = self
                    .store
                    .set_item_outcome(&item_id, ItemStatus::Failed, &outcome);
                self.app
                    .on_run_ended(&item_id, ItemStatus::Failed, &outcome);
                let err = outcome.error.clone().unwrap_or_else(|| "failed".into());
                self.app.flash(
                    format!("agent failed: {}", crate::stream_json::excerpt(&err, 120)),
                    true,
                );
                self.app.push_log(format!("⚠ run failed: {err}"));
            }
        }
    }

    /// The chunk summary an item came from, for the agent's context.
    fn meeting_context_for(&self, item: &ActionItem) -> String {
        item.chunk_id
            .as_deref()
            .and_then(|id| self.app.chunks.iter().find(|c| c.id == id))
            .and_then(|c| c.summary.clone())
            .or_else(|| {
                item.chunk_id
                    .as_deref()
                    .and_then(|id| self.store.get_chunk(id).ok().flatten())
                    .and_then(|c| c.summary)
            })
            .unwrap_or_default()
    }

    /// Start (or resume) the run for `item`. `quiet` skips the flash — the launch-time
    /// resume announces itself once for all of them.
    fn start_run(&mut self, item: ActionItem, quiet: bool) {
        let mut ctx = self.run_ctx.clone();
        ctx.meeting_context = self.meeting_context_for(&item);
        let _ = self.store.set_item_status(&item.id, ItemStatus::Running);
        if let Some(i) = self.app.item_mut(&item.id) {
            i.status = ItemStatus::Running;
            i.error = None;
            i.pr_url = None;
        }
        let resuming = item.worktree_path.is_some();
        self.app.activity.insert(
            item.id.clone(),
            if resuming {
                "resuming".into()
            } else {
                "creating the worktree".into()
            },
        );
        if !quiet {
            self.app.flash(
                format!(
                    "{}: {}",
                    if resuming { "resuming" } else { "running" },
                    item.title
                ),
                false,
            );
        }
        let (stop_tx, stop_rx) = watch::channel(false);
        self.stops.insert(item.id.clone(), stop_tx);
        spawn_run(item, ctx, self.run_tx.clone(), stop_rx);
        self.app.dirty = true;
    }

    fn run_selected(&mut self) {
        let Some(item) = self.app.selected_item().cloned() else {
            return;
        };
        if !item.status.runnable() {
            let msg = match item.status {
                ItemStatus::Running => "already running — X stops it",
                ItemStatus::Done => "already done — o opens the pull request",
                _ => "not runnable",
            };
            self.app.flash(msg.into(), true);
            return;
        }
        self.start_run(item, false);
    }

    /// Ask before stopping the selected item's agent.
    fn confirm_stop_selected(&mut self) {
        let Some(item) = self.app.selected_item() else {
            return;
        };
        if item.status != ItemStatus::Running {
            self.app.flash("no agent is running on this item".into(), true);
            return;
        }
        self.app.overlay = Some(Overlay::ConfirmStop {
            item_id: item.id.clone(),
        });
        self.app.dirty = true;
    }

    fn stop_item(&mut self, item_id: &str) {
        match self.stops.get(item_id) {
            Some(stop) => {
                let _ = stop.send(true);
                self.app
                    .activity
                    .insert(item_id.to_string(), "stopping…".into());
                self.app.flash("stopping the agent…".into(), false);
            }
            None => self
                .app
                .flash("that agent is already finishing up".into(), true),
        }
        self.app.dirty = true;
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
        let chunks = self
            .store
            .list_chunks(&meeting.id)
            .unwrap_or_default()
            .into_iter()
            .map(|c| crate::app::ChunkView {
                id: c.id,
                idx: c.idx.max(0) as usize,
                start: c.start_secs,
                end: c.end_secs,
                words: c.text.split_whitespace().count(),
                state: if c.summary.is_some() {
                    ChunkState::Done
                } else {
                    ChunkState::Failed("no summary".into())
                },
                summary: c.summary,
            })
            .collect();
        let label = crate::when::local_datetime(meeting.started_at);
        self.app.enter_session(SessionView {
            meeting,
            transcript,
            chunks,
        });
        self.app.flash(
            format!("session {label} — ←/→ step, Esc back to live"),
            false,
        );
    }

    /// `←` older, `→` newer (past the newest is the live meeting).
    fn step_session(&mut self, delta: isize) {
        let cur = self.app.session_index() as isize;
        let next = cur + delta;
        if next < 0 {
            if self.app.session.is_some() {
                self.view_session(0);
            }
            return;
        }
        if next as usize >= self.app.sessions.len() {
            self.app.flash("that is the oldest session".into(), true);
            return;
        }
        self.view_session(next as usize);
    }

    fn open_selected(&mut self) {
        let Some(item) = self.app.selected_item() else {
            return;
        };
        let Some(url) = item.pr_url.clone() else {
            self.app
                .flash("no pull request for this item yet".into(), true);
            return;
        };
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        match std::process::Command::new(opener).arg(&url).spawn() {
            Ok(_) => self.app.flash(format!("opened {url}"), false),
            Err(e) => self
                .app
                .flash(format!("could not open the browser: {e}"), true),
        }
    }

    fn open_log(&mut self) {
        let Some(item) = self.app.selected_item() else {
            return;
        };
        let Some(path) = item.log_path.clone() else {
            self.app
                .flash("no agent log for this item yet".into(), true);
            return;
        };
        let id = item.id.clone();
        let lines = read_tail(&path, 2000);
        self.app.overlay = Some(Overlay::Log {
            item_id: id,
            lines,
            scroll: 0,
            follow: true,
        });
        self.app.dirty = true;
    }

    fn refresh_log_overlay(&mut self) {
        let Some(Overlay::Log { item_id, .. }) = &self.app.overlay else {
            return;
        };
        let Some(path) = self
            .app
            .items
            .iter()
            .find(|i| &i.id == item_id)
            .and_then(|i| i.log_path.clone())
        else {
            return;
        };
        let fresh = read_tail(&path, 2000);
        if let Some(Overlay::Log { lines, .. }) = &mut self.app.overlay {
            if *lines != fresh {
                *lines = fresh;
                self.app.dirty = true;
            }
        }
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

    fn request_quit(&mut self) {
        let running = self.app.running_count();
        if running > 0 || self.app.is_live() {
            self.app.overlay = Some(Overlay::ConfirmQuit { running });
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
            self.recorder.stop();
            self.finalize_deadline = Some(tokio::time::Instant::now() + FINALIZE_FOR);
        } else {
            self.app.should_quit = true;
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
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
                Overlay::ConfirmQuit { .. } => match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => self.quit_now(),
                    KeyCode::Char('c') if ctrl_c => self.quit_now(),
                    _ => self.app.overlay = None,
                },
                Overlay::ConfirmStop { item_id } => {
                    self.app.overlay = None;
                    if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter) {
                        self.stop_item(&item_id);
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
                Overlay::Log {
                    item_id,
                    lines,
                    scroll,
                    ..
                } => match key.code {
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('l') => {
                        self.app.overlay = None
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.app.overlay = Some(Overlay::Log {
                            item_id,
                            lines,
                            scroll: scroll.saturating_sub(1),
                            follow: false,
                        })
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.app.overlay = Some(Overlay::Log {
                            item_id,
                            lines,
                            scroll: scroll + 1,
                            follow: false,
                        })
                    }
                    KeyCode::PageUp => {
                        self.app.overlay = Some(Overlay::Log {
                            item_id,
                            lines,
                            scroll: scroll.saturating_sub(20),
                            follow: false,
                        })
                    }
                    KeyCode::PageDown => {
                        self.app.overlay = Some(Overlay::Log {
                            item_id,
                            lines,
                            scroll: scroll + 20,
                            follow: false,
                        })
                    }
                    KeyCode::Char('G') => {
                        self.app.overlay = Some(Overlay::Log {
                            item_id,
                            lines,
                            scroll,
                            follow: true,
                        })
                    }
                    _ if ctrl_c => self.request_quit(),
                    _ => {}
                },
                Overlay::Help => self.app.overlay = None,
            }
            self.app.dirty = true;
            return;
        }
        match key.code {
            KeyCode::Esc if self.app.session.is_some() => self.view_session(0),
            KeyCode::Char('q') | KeyCode::Esc => self.request_quit(),
            KeyCode::Char('c') if ctrl_c => self.request_quit(),
            KeyCode::Char('j') | KeyCode::Down => self.app.select_next(),
            KeyCode::Char('k') | KeyCode::Up => self.app.select_prev(),
            KeyCode::Enter => self.run_selected(),
            KeyCode::Char('X') => self.confirm_stop_selected(),
            KeyCode::Char('d') | KeyCode::Char('x') => {
                if let Some(id) = self.app.dismiss_selected() {
                    let _ = self.store.set_item_status(&id, ItemStatus::Dismissed);
                }
            }
            KeyCode::Char('l') => self.open_log(),
            KeyCode::Char('o') => self.open_selected(),
            KeyCode::Char('S') => self.open_sessions(),
            KeyCode::Left => self.step_session(1),
            KeyCode::Right => self.step_session(-1),
            KeyCode::Char('i') | KeyCode::Char('n') => {
                self.app.overlay = Some(Overlay::Note {
                    input: String::new(),
                });
            }
            KeyCode::Char('s') => {
                if self.app.chunker.pending_words() == 0 {
                    self.app.flash("nothing pending to summarize".into(), true);
                } else if let Some(chunk) = self.app.chunker.flush() {
                    self.enqueue_chunk(chunk);
                }
            }
            KeyCode::Char(' ') => {
                if self.app.is_live() {
                    self.recorder.toggle_pause();
                }
            }
            KeyCode::PageUp | KeyCode::Char('K') => {
                let cur = self.app.transcript_scroll.unwrap_or(usize::MAX / 2);
                self.app.transcript_scroll = Some(cur.saturating_sub(10));
            }
            KeyCode::PageDown | KeyCode::Char('J') => {
                if let Some(s) = self.app.transcript_scroll {
                    self.app.transcript_scroll = Some(s + 10);
                }
            }
            KeyCode::Char('G') => self.app.transcript_scroll = None,
            KeyCode::Char('[') => self.app.detail_scroll = self.app.detail_scroll.saturating_sub(3),
            KeyCode::Char(']') => self.app.detail_scroll += 3,
            KeyCode::Char('?') => self.app.overlay = Some(Overlay::Help),
            _ => {}
        }
        self.app.dirty = true;
    }
}

fn read_tail(path: &str, max_lines: usize) -> Vec<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let lines: Vec<String> = text.lines().map(str::to_string).collect();
            let skip = lines.len().saturating_sub(max_lines);
            lines.into_iter().skip(skip).collect()
        }
        Err(e) => vec![format!("(no log yet: {e})")],
    }
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    crossterm::terminal::enable_raw_mode().context("enable raw mode")?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)
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
    let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen);
}
