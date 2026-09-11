//! Drawing: a header with the recording clock, the live transcript (or a past session's)
//! and its chunk summaries on the left, and on the right either the facts the lookup found
//! (Related) or the action items with the selected one's prompt, a footer of keys — and
//! the overlays (agent log, typed note, quit / stop / end confirms, the session list, help).

use crate::app::{App, ChunkState, Overlay, RecState, RightPane, WrapUp};
use crate::chunker::clock;
use crate::store::{ActionItem, ItemStatus, MeetingRow, RunPhase};
use crate::theme::Theme;
use crate::when::{human_duration, local_datetime, local_time};
use crate::wrap::wrap;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .split(area);
    draw_header(f, app, rows[0]);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(46), Constraint::Percentage(54)])
        .split(rows[1]);
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
        .split(cols[0]);
    draw_transcript(f, app, left[0]);
    draw_summaries(f, app, left[1]);

    match app.right_pane {
        RightPane::Related => draw_related(f, app, cols[1]),
        RightPane::Items => {
            let visible = app.visible_items().len();
            let items_h =
                (visible as u16 + 2).clamp(4, cols[1].height.saturating_mul(45) / 100);
            let right = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(items_h), Constraint::Min(4)])
                .split(cols[1]);
            draw_items(f, app, right[0]);
            draw_detail(f, app, right[1]);
        }
    }

    draw_footer(f, app, rows[2]);
    if app.overlay.is_some() {
        draw_overlay(f, app, area);
    }
}

fn block(title: Line<'static>, th: Theme) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(th.edge))
        .title(title)
}

fn title(text: &str, th: Theme) -> Line<'static> {
    Line::from(Span::styled(
        format!(" {text} "),
        Style::default().fg(th.muted).add_modifier(Modifier::BOLD),
    ))
}

/// A pane title with a highlighted tag after it (the viewed session).
fn title_tagged(text: &str, tag: &str, th: Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!(" {text} "),
            Style::default().fg(th.muted).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{tag} "),
            Style::default().fg(th.special).add_modifier(Modifier::BOLD),
        ),
    ])
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let th = app.theme;
    let mut left = vec![
        Span::styled(
            " meet ",
            Style::default().fg(th.accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            app.repo_name.clone(),
            Style::default().fg(th.text).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" ({})", app.base_branch),
            Style::default().fg(th.muted),
        ),
    ];
    if let Some(v) = &app.session {
        left.push(Span::styled(
            format!(
                "  ◂ session {} of {} · {}",
                app.session_index() + 1,
                app.sessions.len(),
                local_datetime(v.meeting.started_at)
            ),
            Style::default().fg(th.special).add_modifier(Modifier::BOLD),
        ));
        left.push(Span::styled(
            "  Esc live",
            Style::default().fg(th.dim),
        ));
    }
    let left = Line::from(left);
    let (state, color) = match &app.rec {
        RecState::Starting => (
            app.status_text
                .clone()
                .unwrap_or_else(|| "starting…".into()),
            th.warn,
        ),
        RecState::Recording => (
            format!(
                "{} {}",
                if app.replay { "▶ REPLAY" } else { "● REC" },
                clock(app.elapsed())
            ),
            if app.replay { th.special } else { th.err },
        ),
        RecState::Paused => (format!("⏸ PAUSED {}", clock(app.elapsed())), th.warn),
        RecState::Finalizing => (
            app.status_text
                .clone()
                .unwrap_or_else(|| "finalizing…".into()),
            th.warn,
        ),
        RecState::Ended => (format!("■ ended {}", clock(app.elapsed())), th.muted),
        RecState::Failed(_) => ("✗ recorder failed".into(), th.err),
    };
    // The engine's sources first (so a silent one still shows `system 0`), then anything
    // else that spoke (a typed note, a replayed transcript's own labels).
    let mut names: Vec<String> = app.sources.clone();
    let mut extra: Vec<String> = app
        .counts
        .keys()
        .filter(|k| !names.contains(k))
        .cloned()
        .collect();
    extra.sort();
    names.extend(extra);
    let counts = names
        .iter()
        .map(|s| format!("{s} {}", app.counts.get(s).copied().unwrap_or(0)))
        .collect::<Vec<_>>()
        .join(" · ");
    let mut right = vec![Span::styled(
        state,
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )];
    if !counts.is_empty() {
        right.push(Span::styled(
            format!("  {counts} "),
            Style::default().fg(th.muted),
        ));
    }
    let right = Line::from(right);
    let w = right.width() as u16;
    f.render_widget(Paragraph::new(left), area);
    if area.width > w {
        let r = Rect {
            x: area.x + area.width - w,
            width: w,
            ..area
        };
        f.render_widget(Paragraph::new(right), r);
    }
}

fn source_color(source: &str, th: Theme) -> ratatui::style::Color {
    match source {
        "mic" | "me" => th.accent,
        "system" => th.special,
        "typed" => th.warn,
        _ => th.muted,
    }
}

fn draw_transcript(f: &mut Frame, app: &mut App, area: Rect) {
    let th = app.theme;
    let b = match &app.session {
        Some(v) => block(
            title_tagged(
                "Transcript",
                &format!(
                    "· {} · {} lines",
                    local_datetime(v.meeting.started_at),
                    v.transcript.len()
                ),
                th,
            ),
            th,
        )
        .border_style(Style::default().fg(th.special)),
        None => block(title("Transcript", th), th),
    };
    let inner = b.inner(area);
    f.render_widget(b, area);
    if inner.width < 8 || inner.height == 0 {
        return;
    }
    let width = inner.width as usize;
    let mut lines: Vec<Line> = Vec::new();
    let viewing = app.session.is_some();
    let transcript = app.shown_transcript();
    if transcript.is_empty() {
        let hint = if viewing {
            "nothing was said in this session"
        } else {
            match &app.rec {
                RecState::Starting => "waiting for the recorder…",
                RecState::Failed(e) => e.as_str(),
                _ => "listening — start talking about what this project should do next",
            }
        };
        lines.push(Line::from(Span::styled(
            hint.to_string(),
            Style::default().fg(th.dim),
        )));
    }
    for t in transcript {
        let prefix = format!("{} {:<6} ", clock(t.at), t.source);
        let plen = prefix.chars().count();
        let body_w = width.saturating_sub(plen).max(8);
        for (i, w) in wrap(&t.text, body_w).into_iter().enumerate() {
            if i == 0 {
                lines.push(Line::from(vec![
                    Span::styled(clock(t.at), Style::default().fg(th.dim)),
                    Span::raw(" "),
                    Span::styled(
                        format!("{:<6}", t.source),
                        Style::default().fg(source_color(&t.source, th)),
                    ),
                    Span::raw(" "),
                    Span::styled(w, Style::default().fg(th.text)),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::raw(" ".repeat(plen)),
                    Span::styled(w, Style::default().fg(th.text)),
                ]));
            }
        }
    }
    let total = lines.len();
    let h = inner.height as usize;
    let max_scroll = total.saturating_sub(h);
    // A past session opens at its start; the live one follows the newest line.
    let scroll = match app.transcript_scroll {
        Some(s) if s < max_scroll => s,
        Some(_) => {
            app.transcript_scroll = None;
            max_scroll
        }
        None if viewing => {
            app.transcript_scroll = Some(0);
            0
        }
        None => max_scroll,
    };
    let visible: Vec<Line> = lines.into_iter().skip(scroll).take(h).collect();
    f.render_widget(Paragraph::new(visible), inner);
    if app.transcript_scroll.is_some() && !viewing {
        let tag = " ↓ G to follow ";
        let r = Rect {
            x: area.x + area.width.saturating_sub(tag.len() as u16 + 1),
            y: area.y,
            width: tag.len() as u16,
            height: 1,
        };
        f.render_widget(
            Paragraph::new(Span::styled(tag, Style::default().fg(th.warn))),
            r,
        );
    }
}

fn draw_summaries(f: &mut Frame, app: &App, area: Rect) {
    let th = app.theme;
    let pending = app.chunker.pending_words();
    let t = if app.session.is_some() {
        "Summary".to_string()
    } else if pending > 0 {
        format!("Summary · {pending} words pending")
    } else {
        "Summary".to_string()
    };
    let b = block(title(&t, th), th);
    let inner = b.inner(area);
    f.render_widget(b, area);
    if inner.width < 8 || inner.height == 0 {
        return;
    }
    let width = inner.width as usize;
    let mut lines: Vec<Line> = Vec::new();
    let chunks = app.shown_chunks();
    if chunks.is_empty() {
        let hint = if app.session.is_some() {
            "this session has no summaries"
        } else if app.suggest_disabled {
            "suggestions are off (--no-suggest)"
        } else if app.lookup_disabled {
            "lookups are off (--no-lookup)"
        } else {
            "each minute or so of talk becomes a summary here, and related facts on the right"
        };
        lines.push(Line::from(Span::styled(hint, Style::default().fg(th.dim))));
    }
    for c in chunks {
        let head = format!("[{}] {}–{} ", c.idx + 1, clock(c.start), clock(c.end));
        let (text, style) = match (&c.state, &c.summary) {
            (ChunkState::Done, Some(s)) => (s.clone(), Style::default().fg(th.text)),
            (ChunkState::Summarizing, _) => {
                ("looking up…".to_string(), Style::default().fg(th.warn))
            }
            (ChunkState::Queued, _) => (
                format!("queued ({} words)", c.words),
                Style::default().fg(th.dim),
            ),
            (ChunkState::Failed(e), _) => (format!("failed: {e}"), Style::default().fg(th.err)),
            (ChunkState::Done, None) => ("(no summary)".to_string(), Style::default().fg(th.dim)),
        };
        let hlen = head.chars().count();
        for (i, w) in wrap(&text, width.saturating_sub(hlen).max(8))
            .into_iter()
            .enumerate()
        {
            if i == 0 {
                lines.push(Line::from(vec![
                    Span::styled(head.clone(), Style::default().fg(th.dim)),
                    Span::styled(w, style),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::raw(" ".repeat(hlen)),
                    Span::styled(w, style),
                ]));
            }
        }
    }
    let h = inner.height as usize;
    // A past session reads from the top; the live one shows the newest.
    let skip = if app.session.is_some() {
        0
    } else {
        lines.len().saturating_sub(h)
    };
    let visible: Vec<Line> = lines.into_iter().skip(skip).take(h).collect();
    f.render_widget(Paragraph::new(visible), inner);
}

/// The facts the lookup found, newest at the bottom; follows the newest unless scrolled.
fn draw_related(f: &mut Frame, app: &mut App, area: Rect) {
    let th = app.theme;
    let facts = app.shown_facts();
    let looking_up = app.looking_up().filter(|_| app.session.is_none());
    let t = match (facts.len(), looking_up) {
        (0, None) => "Related".to_string(),
        (n, None) => format!("Related · {n}"),
        (n, Some(idx)) => format!("Related · {n} · looking up #{}", idx + 1),
    };
    let b = block(title(&t, th), th);
    let inner = b.inner(area);
    f.render_widget(b, area);
    if inner.width < 8 || inner.height == 0 {
        return;
    }
    let width = inner.width as usize;
    let mut lines: Vec<Line> = Vec::new();
    if facts.is_empty() {
        let hint = if app.session.is_some() {
            "nothing was looked up in this session"
        } else if app.suggest_disabled {
            "suggestions are off (--no-suggest)"
        } else if app.lookup_disabled {
            "lookups are off (--no-lookup)"
        } else {
            "as you talk, what this repository says about it lands here — files, flags, how things work today. Tab shows the action items."
        };
        for w in wrap(hint, width) {
            lines.push(Line::from(Span::styled(w, Style::default().fg(th.dim))));
        }
    }
    let mut last_chunk = None;
    for fact in facts {
        let head = if last_chunk == Some(fact.chunk_idx) {
            " ".repeat(6)
        } else {
            format!("{} ", clock(fact.at))
        };
        last_chunk = Some(fact.chunk_idx);
        let hlen = head.chars().count();
        let body_w = width.saturating_sub(hlen).max(8);
        for (i, w) in wrap(&fact.text, body_w).into_iter().enumerate() {
            lines.push(Line::from(vec![
                Span::styled(
                    if i == 0 {
                        head.clone()
                    } else {
                        " ".repeat(hlen)
                    },
                    Style::default().fg(th.dim),
                ),
                Span::styled(w, Style::default().fg(th.text)),
            ]));
        }
        if !fact.where_.is_empty() {
            for w in wrap(&fact.where_, body_w) {
                lines.push(Line::from(vec![
                    Span::raw(" ".repeat(hlen)),
                    Span::styled(w, Style::default().fg(th.accent)),
                ]));
            }
        }
    }
    let total = lines.len();
    let h = inner.height as usize;
    let max_scroll = total.saturating_sub(h);
    let scroll = match app.related_scroll {
        Some(s) if s < max_scroll => s,
        Some(_) => {
            app.related_scroll = None;
            max_scroll
        }
        None => max_scroll,
    };
    let visible: Vec<Line> = lines.into_iter().skip(scroll).take(h).collect();
    f.render_widget(Paragraph::new(visible), inner);
    if app.related_scroll.is_some() {
        let tag = " ↓ ] to follow ";
        let r = Rect {
            x: area.x + area.width.saturating_sub(tag.len() as u16 + 1),
            y: area.y,
            width: tag.len() as u16,
            height: 1,
        };
        f.render_widget(
            Paragraph::new(Span::styled(tag, Style::default().fg(th.warn))),
            r,
        );
    }
}

fn status_span(item: &ActionItem, app: &App) -> Span<'static> {
    let th = app.theme;
    match item.status {
        ItemStatus::Suggested => Span::styled("", Style::default()),
        ItemStatus::Running => {
            let act = app
                .activity
                .get(&item.id)
                .cloned()
                .unwrap_or_else(|| "starting".into());
            Span::styled(
                format!("● {}", crate::stream_json::excerpt(&act, 28)),
                Style::default().fg(th.warn),
            )
        }
        ItemStatus::Stopped => Span::styled("■ stopped", Style::default().fg(th.muted)),
        ItemStatus::Done => {
            let n = item
                .pr_url
                .as_deref()
                .and_then(|u| u.rsplit('/').next())
                .map(|n| format!("PR #{n}"))
                .unwrap_or_else(|| "done".into());
            Span::styled(format!("✓ {n}"), Style::default().fg(th.ok))
        }
        ItemStatus::Failed => Span::styled("✗ failed", Style::default().fg(th.err)),
        ItemStatus::Dismissed => Span::styled("dismissed", Style::default().fg(th.dim)),
    }
}

fn status_dot(status: ItemStatus, th: Theme) -> Span<'static> {
    match status {
        ItemStatus::Suggested => Span::styled("○ ", Style::default().fg(th.new)),
        ItemStatus::Running => Span::styled("● ", Style::default().fg(th.warn)),
        ItemStatus::Stopped => Span::styled("■ ", Style::default().fg(th.muted)),
        ItemStatus::Done => Span::styled("● ", Style::default().fg(th.ok)),
        ItemStatus::Failed => Span::styled("● ", Style::default().fg(th.err)),
        ItemStatus::Dismissed => Span::styled("· ", Style::default().fg(th.dim)),
    }
}

fn draw_items(f: &mut Frame, app: &App, area: Rect) {
    let th = app.theme;
    let visible = app.visible_items();
    let running = app.visible_count(ItemStatus::Running);
    let writing = app.session.is_none() && app.wrap_up == WrapUp::Writing;
    let t = match (&app.session, running) {
        (Some(_), 0) => format!("Action items · {} from this session", visible.len()),
        (Some(_), r) => format!(
            "Action items · {} from this session · {r} running",
            visible.len()
        ),
        (None, _) if writing => "Action items · writing…".to_string(),
        (None, 0) => format!("Action items · {}", visible.len()),
        (None, r) => format!("Action items · {r} running"),
    };
    let b = block(title(&t, th), th);
    let inner = b.inner(area);
    f.render_widget(b, area);
    if inner.width < 10 || inner.height == 0 {
        return;
    }
    let h = inner.height as usize;
    let first = app
        .selected
        .saturating_sub(h.saturating_sub(1))
        .min(visible.len().saturating_sub(h));
    let mut lines: Vec<Line> = Vec::new();
    if visible.is_empty() {
        let (hint, style) = if app.session.is_some() {
            (
                "this session produced no action items".to_string(),
                Style::default().fg(th.dim),
            )
        } else {
            match &app.wrap_up {
                _ if app.suggest_disabled => (
                    "suggestions are off (--no-suggest)".to_string(),
                    Style::default().fg(th.dim),
                ),
                WrapUp::NotYet => (
                    "the action items are written when the recording stops — x stops it"
                        .to_string(),
                    Style::default().fg(th.dim),
                ),
                WrapUp::Writing => (
                    "writing the action items from the transcript…".to_string(),
                    Style::default().fg(th.warn),
                ),
                WrapUp::Done => (
                    "no action items came out of this meeting".to_string(),
                    Style::default().fg(th.dim),
                ),
                WrapUp::Failed(e) => (
                    format!("could not write the action items: {e} — s tries again"),
                    Style::default().fg(th.err),
                ),
            }
        };
        for w in wrap(&hint, inner.width as usize) {
            lines.push(Line::from(Span::styled(w, style)));
        }
    }
    for (row, &idx) in visible.iter().enumerate().skip(first).take(h) {
        let item = &app.items[idx];
        let selected = row == app.selected;
        let status = status_span(item, app);
        let status_w = status.width();
        let avail = (inner.width as usize).saturating_sub(2 + status_w + 2);
        let mut name: String = item.title.chars().take(avail).collect();
        if item.title.chars().count() > avail && avail > 1 {
            name.pop();
            name.push('…');
        }
        let pad = avail.saturating_sub(name.chars().count());
        let name_style = if selected {
            Style::default().fg(th.text).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(th.text)
        };
        let mut line = Line::from(vec![
            status_dot(item.status, th),
            Span::styled(name, name_style),
            Span::raw(" ".repeat(pad + 1)),
            status,
            Span::raw(" "),
        ]);
        if selected {
            line = line.style(Style::default().bg(th.sel_bg));
        }
        lines.push(line);
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_detail(f: &mut Frame, app: &mut App, area: Rect) {
    let th = app.theme;
    let Some(item) = app.selected_item().cloned() else {
        let b = block(title("Prompt", th), th);
        f.render_widget(b, area);
        return;
    };
    let head = match item.status {
        ItemStatus::Suggested => "Prompt · Enter runs it",
        ItemStatus::Running => "Prompt · X stops the agent",
        ItemStatus::Stopped | ItemStatus::Failed if item.worktree_path.is_some() => {
            "Prompt · Enter resumes it"
        }
        ItemStatus::Failed => "Prompt · Enter tries again",
        ItemStatus::Done => "Prompt · o opens the pull request",
        ItemStatus::Dismissed | ItemStatus::Stopped => "Prompt",
    };
    let b = block(title(head, th), th);
    let inner = b.inner(area);
    f.render_widget(b, area);
    if inner.width < 8 || inner.height == 0 {
        return;
    }
    let width = inner.width as usize;
    let mut lines: Vec<Line> = Vec::new();
    for w in wrap(&item.title, width) {
        lines.push(Line::from(Span::styled(
            w,
            Style::default().fg(th.accent).add_modifier(Modifier::BOLD),
        )));
    }
    if !item.why.trim().is_empty() {
        for w in wrap(&format!("said: {}", item.why.trim()), width) {
            lines.push(Line::from(Span::styled(
                w,
                Style::default().fg(th.muted).add_modifier(Modifier::ITALIC),
            )));
        }
    }
    lines.push(Line::default());
    for w in wrap(&item.prompt, width) {
        lines.push(Line::from(Span::styled(w, Style::default().fg(th.text))));
    }
    lines.push(Line::default());
    let field = |lines: &mut Vec<Line>, label: &str, value: &str, color| {
        for (i, l) in wrap(value, width.saturating_sub(9)).into_iter().enumerate() {
            lines.push(Line::from(vec![
                Span::styled(
                    if i == 0 {
                        format!("{label:<8} ")
                    } else {
                        " ".repeat(9)
                    },
                    Style::default().fg(th.dim),
                ),
                Span::styled(l, Style::default().fg(color)),
            ]));
        }
    };
    if let Some(b) = &item.branch {
        field(&mut lines, "branch", b, th.muted);
    }
    if let Some(w) = &item.worktree_path {
        field(&mut lines, "worktree", w, th.muted);
    }
    if let Some(s) = &item.session_id {
        let phase = match item.phase {
            RunPhase::Agent => "",
            RunPhase::Publish => " · agent done, publishing",
        };
        field(&mut lines, "session", &format!("{s}{phase}"), th.dim);
    }
    if let Some(u) = &item.pr_url {
        lines.push(Line::from(vec![
            Span::styled(format!("{:<8} ", "PR"), Style::default().fg(th.dim)),
            Span::styled(
                u.clone(),
                Style::default()
                    .fg(th.ok)
                    .add_modifier(Modifier::UNDERLINED),
            ),
            Span::styled("  (o opens it)", Style::default().fg(th.dim)),
        ]));
    }
    if let Some(a) = app.activity.get(&item.id) {
        field(&mut lines, "now", a, th.warn);
    }
    if let Some(e) = &item.error {
        let color = if item.status == ItemStatus::Stopped {
            th.muted
        } else {
            th.err
        };
        field(&mut lines, "ended", e, color);
    }
    if item.log_path.is_some() {
        lines.push(Line::from(Span::styled(
            "l shows the agent log",
            Style::default().fg(th.dim),
        )));
    }
    let h = inner.height as usize;
    let max_scroll = lines.len().saturating_sub(h);
    if app.detail_scroll > max_scroll {
        app.detail_scroll = max_scroll;
    }
    let visible: Vec<Line> = lines.into_iter().skip(app.detail_scroll).take(h).collect();
    f.render_widget(Paragraph::new(visible), inner);
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let th = app.theme;
    let hints = match &app.overlay {
        Some(Overlay::Log { .. }) => "↑/↓ scroll  G follow  Esc close",
        Some(Overlay::Note { .. }) => "type a note into the transcript  Enter add  Esc cancel",
        Some(Overlay::ConfirmQuit { live: true, .. }) => {
            "y quit without action items  x stop the recording and stay  n stay"
        }
        Some(Overlay::ConfirmQuit { .. }) => "y quit  n stay",
        Some(Overlay::ConfirmStop { .. }) => "y stop the agent  n keep it running",
        Some(Overlay::ConfirmEnd) => "y stop the recording  n keep recording",
        Some(Overlay::Sessions { .. }) => "↑/↓ pick a session  Enter view  Esc close",
        Some(Overlay::Help) => "Esc close",
        None => {
            if app.session.is_some() {
                "Esc live  ←/→ session  Tab related/items  Enter run/resume  X stop agent  l log  o open PR  ? help  q quit"
            } else if app.is_live() {
                "x stop recording  a ask  Tab related/items  Enter run  X stop agent  d dismiss  i note  s look up now  space pause  ? help  q quit"
            } else {
                "a ask  Tab related/items  Enter run  X stop agent  d dismiss  l log  o open PR  S sessions  ←/→ session  ? help  q quit"
            }
        }
    };
    let left = match &app.flash {
        Some((text, _, warn)) => Span::styled(
            format!(" {text}"),
            Style::default().fg(if *warn { th.warn } else { th.ok }),
        ),
        None => Span::styled(format!(" {hints}"), Style::default().fg(th.muted)),
    };
    let mut right_parts: Vec<String> = Vec::new();
    if let Some(idx) = app.looking_up() {
        let q = app.queued();
        right_parts.push(if q > 0 {
            format!("⌕ looking up #{} · {q} queued", idx + 1)
        } else {
            format!("⌕ looking up #{}", idx + 1)
        });
    } else if app.queued() > 0 {
        right_parts.push(format!("⌕ {} queued", app.queued()));
    }
    if app.wrap_up == WrapUp::Writing {
        right_parts.push("✎ writing action items".into());
    }
    let running = app.running_count();
    if running > 0 {
        right_parts.push(format!(
            "⚙ {running} agent{} running",
            if running == 1 { "" } else { "s" }
        ));
    }
    let right = Span::styled(
        format!("{} ", right_parts.join("  ")),
        Style::default().fg(th.warn),
    );
    let rw = right.width() as u16;
    f.render_widget(Paragraph::new(Line::from(left)), area);
    if rw > 0 && area.width > rw {
        let r = Rect {
            x: area.x + area.width - rw,
            width: rw,
            ..area
        };
        f.render_widget(Paragraph::new(Line::from(right)), r);
    }
}

fn centered(area: Rect, pct_w: u16, pct_h: u16) -> Rect {
    let w = (area.width * pct_w / 100).max(20).min(area.width);
    let h = (area.height * pct_h / 100).max(5).min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

/// One row of the session list: when, how long, how much was said, what came of it.
pub fn session_row(m: &MeetingRow, live: bool, items: &[ActionItem], width: usize) -> String {
    let when = if live {
        format!("● live · started {}", local_time(m.started_at))
    } else {
        local_datetime(m.started_at)
    };
    let length = match m.ended_at {
        Some(end) => human_duration(end - m.started_at),
        None if live => String::new(),
        None => "unfinished".into(),
    };
    let mine: Vec<&ActionItem> = items.iter().filter(|i| i.meeting_id == m.id).collect();
    let count = |s: ItemStatus| mine.iter().filter(|i| i.status == s).count();
    let mut parts = vec![when];
    if !length.is_empty() {
        parts.push(length);
    }
    parts.push(format!(
        "{} line{}",
        m.segment_count,
        if m.segment_count == 1 { "" } else { "s" }
    ));
    if !mine.is_empty() {
        let mut s = format!("{} item{}", mine.len(), if mine.len() == 1 { "" } else { "s" });
        let done = count(ItemStatus::Done);
        let running = count(ItemStatus::Running);
        let stopped = count(ItemStatus::Stopped);
        if done > 0 {
            s.push_str(&format!(", {done} done"));
        }
        if running > 0 {
            s.push_str(&format!(", {running} running"));
        }
        if stopped > 0 {
            s.push_str(&format!(", {stopped} stopped"));
        }
        parts.push(s);
    }
    let mut row = parts.join(" · ");
    if let Some(sum) = &m.summary {
        let room = width.saturating_sub(row.chars().count() + 4);
        if room > 12 {
            row.push_str("  — ");
            row.push_str(&crate::stream_json::excerpt(sum, room));
        }
    }
    row
}

fn draw_overlay(f: &mut Frame, app: &mut App, area: Rect) {
    let th = app.theme;
    let Some(overlay) = app.overlay.clone() else {
        return;
    };
    match overlay {
        Overlay::Log {
            item_id,
            lines,
            scroll,
            follow,
        } => {
            let name = app
                .items
                .iter()
                .find(|i| i.id == item_id)
                .map(|i| i.title.clone())
                .unwrap_or_default();
            let r = centered(area, 86, 80);
            f.render_widget(Clear, r);
            let b = block(title(&format!("Agent log · {name}"), th), th)
                .border_style(Style::default().fg(th.accent));
            let inner = b.inner(r);
            f.render_widget(b, r);
            let width = inner.width as usize;
            let mut out: Vec<Line> = Vec::new();
            for l in &lines {
                let style = if l.starts_with("→") {
                    Style::default().fg(th.accent)
                } else if l.starts_with("──") {
                    Style::default().fg(th.warn)
                } else if l.starts_with("  ✗") {
                    Style::default().fg(th.err)
                } else if l.starts_with("  ←") {
                    Style::default().fg(th.dim)
                } else {
                    Style::default().fg(th.text)
                };
                for w in wrap(l, width) {
                    out.push(Line::from(Span::styled(w, style)));
                }
            }
            let h = inner.height as usize;
            let max_scroll = out.len().saturating_sub(h);
            let s = if follow {
                max_scroll
            } else {
                scroll.min(max_scroll)
            };
            if let Some(Overlay::Log { scroll, .. }) = &mut app.overlay {
                *scroll = s;
            }
            let visible: Vec<Line> = out.into_iter().skip(s).take(h).collect();
            f.render_widget(Paragraph::new(visible), inner);
        }
        Overlay::Note { input } => {
            let r = centered(area, 70, 20);
            let r = Rect {
                height: 3.min(r.height),
                ..r
            };
            f.render_widget(Clear, r);
            let b = block(title("Note for the transcript", th), th)
                .border_style(Style::default().fg(th.accent));
            let inner = b.inner(r);
            f.render_widget(b, r);
            let shown: String = {
                let w = inner.width.saturating_sub(2) as usize;
                let n = input.chars().count();
                input.chars().skip(n.saturating_sub(w)).collect()
            };
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(shown, Style::default().fg(th.text)),
                    Span::styled("▏", Style::default().fg(th.accent)),
                ])),
                inner,
            );
        }
        Overlay::ConfirmQuit { running, live } => {
            let r = centered(area, 60, 20);
            let r = Rect {
                height: 7.min(r.height),
                ..r
            };
            f.render_widget(Clear, r);
            let b = block(title("Quit?", th), th).border_style(Style::default().fg(th.err));
            let inner = b.inner(r);
            f.render_widget(b, r);
            let mut msg = Vec::new();
            if running > 0 {
                msg.push(format!(
                    "{running} agent{} still running — quitting stops {}; the worktree and commits stay, and meet resumes {} next time it runs here.",
                    if running == 1 { " is" } else { "s are" },
                    if running == 1 { "it" } else { "them" },
                    if running == 1 { "it" } else { "them" },
                ));
            }
            if live {
                msg.push(
                    "The recording is still going. y stops it, saves the transcript and quits — no action items are written. x stops the recording and stays, so the action items land here first."
                        .into(),
                );
            }
            let p = Paragraph::new(msg.join(" "))
                .style(Style::default().fg(th.text))
                .wrap(Wrap { trim: true })
                .alignment(Alignment::Left);
            f.render_widget(p, inner);
        }
        Overlay::ConfirmEnd => {
            let r = centered(area, 60, 20);
            let r = Rect {
                height: 6.min(r.height),
                ..r
            };
            f.render_widget(Clear, r);
            let b = block(title("Stop the recording?", th), th)
                .border_style(Style::default().fg(th.warn));
            let inner = b.inner(r);
            f.render_widget(b, r);
            let p = Paragraph::new(
                "The transcript and audio are saved, the engine's hooks run, and Claude writes the action items from everything that was said. The recording cannot be started again; meet stays open to run the items.",
            )
            .style(Style::default().fg(th.text))
            .wrap(Wrap { trim: true });
            f.render_widget(p, inner);
        }
        Overlay::ConfirmStop { item_id } => {
            let name = app
                .items
                .iter()
                .find(|i| i.id == item_id)
                .map(|i| i.title.clone())
                .unwrap_or_default();
            let r = centered(area, 60, 20);
            let r = Rect {
                height: 6.min(r.height),
                ..r
            };
            f.render_widget(Clear, r);
            let b = block(title("Stop the agent?", th), th)
                .border_style(Style::default().fg(th.warn));
            let inner = b.inner(r);
            f.render_widget(b, r);
            let p = Paragraph::new(format!(
                "Stop the agent working on \"{name}\"? It gets a SIGTERM; its worktree, commits and Claude session stay, and Enter resumes the conversation where it was."
            ))
            .style(Style::default().fg(th.text))
            .wrap(Wrap { trim: true });
            f.render_widget(p, inner);
        }
        Overlay::Sessions { cursor } => {
            let r = centered(area, 80, 70);
            f.render_widget(Clear, r);
            let b = block(
                title(
                    &format!("Sessions · {} in {}", app.sessions.len(), app.repo_name),
                    th,
                ),
                th,
            )
            .border_style(Style::default().fg(th.special));
            let inner = b.inner(r);
            f.render_widget(b, r);
            let width = inner.width as usize;
            let h = inner.height as usize;
            let first = cursor
                .saturating_sub(h.saturating_sub(1))
                .min(app.sessions.len().saturating_sub(h));
            let viewing = app.session_index();
            let mut lines: Vec<Line> = Vec::new();
            for (i, m) in app.sessions.iter().enumerate().skip(first).take(h) {
                let row = session_row(m, i == 0, &app.items, width.saturating_sub(4));
                let mark = if i == viewing { "▸ " } else { "  " };
                let mut text: String = format!("{mark}{row}").chars().take(width).collect();
                let pad = width.saturating_sub(text.chars().count());
                text.push_str(&" ".repeat(pad));
                let style = if i == cursor {
                    Style::default()
                        .fg(th.text)
                        .bg(th.sel_bg)
                        .add_modifier(Modifier::BOLD)
                } else if i == 0 {
                    Style::default().fg(th.ok)
                } else {
                    Style::default().fg(th.text)
                };
                lines.push(Line::from(Span::styled(text, style)));
            }
            f.render_widget(Paragraph::new(lines), inner);
        }
        Overlay::Help => {
            let r = centered(area, 64, 80);
            f.render_widget(Clear, r);
            let b = block(title("Keys", th), th).border_style(Style::default().fg(th.accent));
            let inner = b.inner(r);
            f.render_widget(b, r);
            let keys = [
                (
                    "x",
                    "stop the recording and stay: the action items are written from the transcript",
                ),
                (
                    "a",
                    "ask questions: a Claude Code session over the live transcript (nebula, tmux, or `meet ask` elsewhere)",
                ),
                ("Tab", "flip the right pane: related facts / action items"),
                ("j / k, ↓ / ↑", "select an action item"),
                (
                    "Enter",
                    "run it: a worktree, a Claude Code agent, a pull request — or resume it",
                ),
                ("X", "stop its agent (Enter resumes the conversation later)"),
                ("d", "dismiss it (hidden for good)"),
                ("l", "the agent's log for it"),
                ("o", "open its pull request in the browser"),
                ("S", "the session list: every meeting held in this repo"),
                ("← / →", "step to an older / newer session; Esc back to live"),
                ("i", "type a note into the transcript"),
                (
                    "s",
                    "look up what is pending now; once ended, write the action items again",
                ),
                ("space", "pause / resume the recording"),
                (
                    "PgUp / PgDn, J / K",
                    "scroll the transcript; G follows again",
                ),
                ("[ / ]", "scroll the right pane"),
                (
                    "q, Ctrl+C",
                    "quit; running agents resume when meet starts here again",
                ),
            ];
            let lines: Vec<Line> = keys
                .iter()
                .map(|(k, v)| {
                    Line::from(vec![
                        Span::styled(format!(" {k:<20}"), Style::default().fg(th.accent)),
                        Span::styled((*v).to_string(), Style::default().fg(th.text)),
                    ])
                })
                .collect();
            f.render_widget(Paragraph::new(lines), inner);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_row_says_when_how_long_and_what_came_of_it() {
        let m = MeetingRow {
            id: "m".into(),
            repo_path: "/r".into(),
            meeting_dir: None,
            started_at: 1_756_000_000,
            ended_at: Some(1_756_000_000 + 2520),
            segment_count: 128,
            summary: Some("Decided to ship the JSON flag.".into()),
        };
        let mk = |id: &str, status| ActionItem {
            id: id.into(),
            meeting_id: "m".into(),
            chunk_id: None,
            repo_path: "/r".into(),
            title: "t".into(),
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
        };
        let items = vec![
            mk("a", ItemStatus::Done),
            mk("b", ItemStatus::Running),
            mk("c", ItemStatus::Suggested),
        ];
        let row = session_row(&m, false, &items, 120);
        assert!(row.contains("42m · 128 lines · 3 items, 1 done, 1 running"), "{row}");
        assert!(row.ends_with("— Decided to ship the JSON flag."), "{row}");
        let narrow = session_row(&m, false, &items, 50);
        assert!(!narrow.contains("—"), "no room for the summary: {narrow}");
        let live = session_row(&m, true, &[], 80);
        assert!(live.starts_with("● live · started "), "{live}");
    }
}
