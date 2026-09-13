//! Drawing: a header with the recording clock, under it the session bar (every session
//! held in this repo as a tab, the live one at the left end), the live transcript (or a
//! past session's) and its chunk summaries on the left, and on the right either what the
//! lookup found for the selected summary (Related: facts, contradictions, questions), the
//! action items with the selected one's prompt, or the meeting's summary (what is writing
//! it once the recording stops — the engine's hooks, meet's own call — then the write-up),
//! a footer of keys — and the overlays (agent log, typed note, quit / stop / end /
//! discard confirms, the session list, help).
//!
//! The seams between the panes sit where the mouse dragged them (`layout`), and every
//! frame records where it put things — the session tabs, the right pane's tabs, the
//! summaries, the action items, the overlay — so clicks land on them.

use crate::app::{App, ChunkState, ChunkView, Overlay, RecState, RightPane, WrapUp};
use crate::engine_hook::{HookRun, HookState};
use crate::layout::{self, HitMap, LayoutPrefs, Splitter, MIN_PANE_H};
use crate::lookup::Fact;
use crate::chunker::clock;
use crate::event_loop::UNFOCUS_HINT;
use crate::store::{ActionItem, ItemStatus, MeetingRow, RunPhase};
use crate::stream_json::{tokens_short, Usage};
use crate::theme::Theme;
use crate::when::{human_duration, local_datetime, local_time, short_datetime};
use crate::wrap::wrap;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

/// The screen: a header row, the session bar, the body, a footer row; the body split into
/// the left column (transcript over summaries) and the right column, at the seam the
/// mouse dragged it to.
fn layout(area: Rect, prefs: &LayoutPrefs) -> (Rect, Rect, Rect, Rect, Rect) {
    let row = |y: u16, height: u16| Rect { y, height, ..area };
    let h = area.height;
    let header = row(area.y, h.min(1));
    let bar = row(area.y + 1, h.saturating_sub(1).min(1));
    let footer_h = h.saturating_sub(2).min(1);
    let footer = row(area.bottom() - footer_h, footer_h);
    let body = row(area.y + 2, h.saturating_sub(3));
    let (left, right) = layout::split_cols(body, prefs.left);
    (header, bar, left, right, footer)
}

/// Where the question session's terminal paints for a screen of `area`: the right
/// column inside its border. What a new session is sized to before it is first drawn.
pub fn ask_pane_inner(area: Rect, prefs: &LayoutPrefs) -> Rect {
    let (_, _, _, right, _) = layout(area, prefs);
    Block::default().borders(Borders::ALL).inner(right)
}

/// The action-item list over the prompt: as tall as the items shown (up to 45% of the
/// column) until the seam was dragged, then where it was dragged to.
fn split_items(right_col: Rect, prefs: &LayoutPrefs, visible: usize) -> (Rect, Rect) {
    let share = match prefs.items {
        Some(share) => share,
        None => {
            let cap = (right_col.height.saturating_mul(45) / 100).max(MIN_PANE_H);
            let want = (visible as u16 + 2).clamp(MIN_PANE_H, cap);
            f64::from(want) / f64::from(right_col.height.max(1))
        }
    };
    layout::split_rows(right_col, share)
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let (header, bar, left_col, right_col, footer) = layout(area, &app.layout);
    let (transcript, summaries) = layout::split_rows(left_col, app.layout.transcript);
    app.hit = HitMap {
        body: Rect {
            width: left_col.width + right_col.width,
            ..left_col
        },
        bar,
        transcript,
        summaries,
        right: right_col,
        ..HitMap::default()
    };
    draw_header(f, app, header);
    draw_bar(f, app, bar);
    draw_transcript(f, app, transcript);
    draw_summaries(f, app, summaries);

    match app.right_pane {
        RightPane::Related => draw_related(f, app, right_col),
        RightPane::Summary => draw_summary(f, app, right_col),
        RightPane::Ask => draw_ask(f, app, right_col),
        RightPane::Items => {
            let visible = app.visible_items().len();
            let (items, detail) = split_items(right_col, &app.layout, visible);
            app.hit.items = Some(items);
            app.hit.detail = Some(detail);
            draw_items(f, app, items);
            draw_detail(f, app, detail);
        }
    }
    draw_right_tabs(f, app, right_col);
    draw_grips(f, app);

    draw_footer(f, app, footer);
    if app.overlay.is_some() {
        draw_overlay(f, app, area);
    }
}

/// The right pane's tabs at the right end of its bottom border — what Tab cycles
/// through, the pane on show highlighted, a click shows another. The bottom border,
/// because it is always bare: the titles on top say what the pane is up to and can run
/// long. Left out when the column is too narrow for them.
fn draw_right_tabs(f: &mut Frame, app: &mut App, area: Rect) {
    let th = app.theme;
    if area.height < 2 || area.width < 12 {
        return;
    }
    let tabs = [
        ("Related", RightPane::Related),
        ("Items", RightPane::Items),
        ("Summary", RightPane::Summary),
        ("Claude", RightPane::Ask),
    ];
    let mut spans: Vec<Span> = Vec::new();
    let mut cells: Vec<(u16, u16, RightPane)> = Vec::new();
    let mut x: u16 = 0;
    for (i, (label, pane)) in tabs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("│", Style::default().fg(th.edge)));
            x += 1;
        }
        let text = format!(" {label} ");
        let width = text.chars().count() as u16;
        let style = if *pane == app.right_pane {
            Style::default()
                .fg(th.accent)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::default().fg(th.muted)
        };
        spans.push(Span::styled(text, style));
        cells.push((x, width, *pane));
        x += width;
    }
    let width = x;
    if width + 4 > area.width {
        return;
    }
    // A cell of border on either side of the tabs, before the corner.
    let start = area.x + area.width - 2 - width;
    let y = area.bottom() - 1;
    // Only onto a bare stretch of border.
    let bare = (start - 1..start + width + 1).all(|cx| {
        f.buffer_mut()
            .cell((cx, y))
            .is_some_and(|c| c.symbol() == "─")
    });
    if !bare {
        return;
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect {
            x: start,
            y,
            width,
            height: 1,
        },
    );
    for (offset, width, pane) in cells {
        app.hit.right_tabs.push((
            Rect {
                x: start + offset,
                y,
                width,
                height: 1,
            },
            pane,
        ));
    }
}

/// The grips on the seams, nebula's: a short thick stroke in the middle of each border
/// the mouse can drag, a shade brighter than the border, accent while hovered or dragged.
/// Only plain border cells are painted, so a title or a corner sitting there stays.
fn draw_grips(f: &mut Frame, app: &App) {
    let th = app.theme;
    let hit = &app.hit;
    let active = |s: Splitter| {
        app.hover_splitter == Some(s) || app.splitter_drag.map(|d| d.which) == Some(s)
    };
    let buf = f.buffer_mut();
    let paint = |buf: &mut Buffer, x: u16, y: u16, plain: &str, grip: &str, on: bool| {
        if let Some(cell) = buf.cell_mut((x, y)) {
            if cell.symbol() == plain {
                cell.set_symbol(grip);
                cell.set_style(Style::default().fg(if on { th.accent } else { th.muted }));
            }
        }
    };
    // The columns' seam: both borders, three rows around the middle.
    if hit.body.height >= 7 && hit.right.x > hit.body.x {
        let mid = hit.body.y + hit.body.height / 2;
        let on = active(Splitter::Columns);
        for y in mid - 1..=mid + 1 {
            paint(buf, hit.right.x - 1, y, "│", "┃", on);
            paint(buf, hit.right.x, y, "│", "┃", on);
        }
    }
    // The transcript/summary seam: the transcript's bottom border.
    if hit.transcript.width >= 9 && hit.summaries.y > hit.transcript.y {
        let mid = hit.transcript.x + hit.transcript.width / 2;
        let on = active(Splitter::LeftRows);
        for x in mid - 1..=mid + 1 {
            paint(buf, x, hit.summaries.y - 1, "─", "━", on);
        }
    }
    // The items/prompt seam: the item list's bottom border.
    if let Some(detail) = hit.detail {
        if hit.right.width >= 9 && detail.y > hit.right.y {
            let mid = hit.right.x + hit.right.width / 2;
            let on = active(Splitter::RightRows);
            for x in mid - 1..=mid + 1 {
                paint(buf, x, detail.y - 1, "─", "━", on);
            }
        }
    }
}

/// The live meeting's tab: its state, colored like the header's clock.
fn live_tab(app: &App) -> (String, Color) {
    let th = app.theme;
    match &app.rec {
        RecState::Idle => ("○ idle".into(), th.muted),
        RecState::Starting => ("○ live".into(), th.warn),
        RecState::Recording if app.replay => ("▶ live".into(), th.special),
        RecState::Recording | RecState::Finalizing => ("● live".into(), th.err),
        RecState::Paused => ("⏸ live".into(), th.warn),
        RecState::Ended => ("■ ended".into(), th.muted),
        RecState::Failed(_) => ("✗ failed".into(), th.err),
    }
}

/// The session bar: one tab per session held in this repo, the live meeting at the left
/// end and older ones on to the right, each named by when it started. The shown session
/// is highlighted — harder while the bar has the keys (Tab), when ←/→ and h/l walk it.
/// More tabs than fit: the row scrolls to keep the shown one in view and says how many
/// are hidden past each end.
fn draw_bar(f: &mut Frame, app: &mut App, area: Rect) {
    let th = app.theme;
    if area.width < 12 || area.height == 0 {
        return;
    }
    let label = Span::styled(
        " sessions ",
        Style::default()
            .fg(if app.bar_focused { th.accent } else { th.muted })
            .add_modifier(Modifier::BOLD),
    );
    let room = (area.width as usize).saturating_sub(label.width());
    let mut spans = vec![label];
    if app.sessions.is_empty() {
        spans.push(Span::styled("(none yet)", Style::default().fg(th.dim)));
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }
    let tabs: Vec<(String, Color)> = app
        .sessions
        .iter()
        .enumerate()
        .map(|(i, m)| {
            if i == 0 {
                live_tab(app)
            } else {
                (short_datetime(m.started_at), th.muted)
            }
        })
        .collect();
    let n = tabs.len();
    let sel = app.session_index();
    // ` label ` per tab, a separator between neighbours, and room for the hidden counts.
    let width = |i: usize| tabs[i].0.chars().count() + 2;
    let hidden_left = |first: usize| {
        if first > 0 {
            format!("‹{first} ").chars().count()
        } else {
            0
        }
    };
    let hidden_right = |last: usize| {
        if last < n {
            format!(" {}›", n - last).chars().count()
        } else {
            0
        }
    };
    let mut first = app.bar_first.min(sel);
    let mut last;
    loop {
        let mut used = hidden_left(first);
        last = first;
        while last < n {
            let need = width(last) + usize::from(last > first);
            if used + need + hidden_right(last + 1) > room {
                break;
            }
            used += need;
            last += 1;
        }
        if sel < last || first >= sel {
            break;
        }
        first += 1;
    }
    app.bar_first = first;
    if first > 0 {
        spans.push(Span::styled(
            format!("‹{first} "),
            Style::default().fg(th.dim),
        ));
    }
    // Where each tab lands, for the clicks.
    let mut cursor: u16 = spans.iter().map(|s| s.width() as u16).sum();
    for (i, (text, color)) in tabs.iter().enumerate().take(last).skip(first) {
        if i > first {
            spans.push(Span::styled("│", Style::default().fg(th.edge)));
            cursor += 1;
        }
        // The shown tab is bold; with the keys on the bar it is also bracketed and
        // highlighted, so the focus reads even without colour.
        let (text, style) = if i == sel && app.bar_focused {
            (
                format!("[{text}]"),
                Style::default()
                    .fg(th.text)
                    .bg(th.sel_bg)
                    .add_modifier(Modifier::BOLD),
            )
        } else if i == sel {
            (
                format!(" {text} "),
                Style::default()
                    .fg(th.accent)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            )
        } else {
            (format!(" {text} "), Style::default().fg(*color))
        };
        let width = text.chars().count() as u16;
        app.hit.bar_tabs.push((
            Rect {
                x: area.x + cursor,
                y: area.y,
                width,
                height: 1,
            },
            i,
        ));
        cursor += width;
        spans.push(Span::styled(text, style));
    }
    if last < n {
        spans.push(Span::styled(
            format!(" {}›", n - last),
            Style::default().fg(th.dim),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
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

/// The header: the repo at the left; at the right the recording state and clock, then
/// one count per source (`mic 41 · system 8`). While recording, each of the engine's
/// sources is a click target that mutes or unmutes it, and a muted one reads `⊘ mic 41`.
fn draw_header(f: &mut Frame, app: &mut App, area: Rect) {
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
        RecState::Idle => ("○ idle · r to record".into(), th.muted),
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
        // Ended, but still being summarized (meet's call, the engine's hooks): say so
        // where the clock was, until everything has landed.
        RecState::Ended if app.summarizing() => {
            (format!("✎ SUMMARIZING {}", clock(app.elapsed())), th.warn)
        }
        RecState::Ended => (format!("■ ended {}", clock(app.elapsed())), th.muted),
        RecState::Failed(_) if app.summarizing() => ("✎ SUMMARIZING · recorder failed".into(), th.warn),
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
    let mut right = vec![Span::styled(
        state,
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )];
    // Each source's label and where it starts within the right-hand line, for the
    // clicks; only the engine's own sources are targets, and only while it records.
    let mut labels: Vec<(u16, u16, usize)> = Vec::new();
    let mut offset = right[0].width() as u16;
    for (i, name) in names.iter().enumerate() {
        let sep = Span::styled(if i == 0 { "  " } else { " · " }, Style::default().fg(th.muted));
        offset += sep.width() as u16;
        right.push(sep);
        let count = app.counts.get(name).copied().unwrap_or(0);
        let span = if app.is_muted(name) {
            Span::styled(
                format!("⊘ {name} {count}"),
                Style::default().fg(th.warn).add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(format!("{name} {count}"), Style::default().fg(th.muted))
        };
        let w = span.width() as u16;
        if app.is_live() && i < app.sources.len() {
            labels.push((offset, w, i));
        }
        offset += w;
        right.push(span);
    }
    if !names.is_empty() {
        right.push(Span::raw(" "));
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
        app.hit.sources = labels
            .into_iter()
            .map(|(dx, w, i)| (Rect::new(r.x + dx, r.y, w, 1), i))
            .collect();
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
                RecState::Idle => {
                    "press r to start recording — the transcript lands here as people speak"
                }
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
    app.transcript_max = max_scroll;
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

fn draw_summaries(f: &mut Frame, app: &mut App, area: Rect) {
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
            "each minute or so of talk becomes a summary here, and what is known about it lands on the right"
        };
        lines.push(Line::from(Span::styled(hint, Style::default().fg(th.dim))));
    }
    let selected = app.selected_chunk();
    // The picked summary's first and last line, to keep it in view.
    let mut picked_rows: Option<(usize, usize)> = None;
    // Every summary's lines, for the clicks.
    let mut ranges: Vec<(usize, usize, usize)> = Vec::new();
    for (i, c) in chunks.iter().enumerate() {
        let picked = selected == Some(i);
        let head = format!(
            "{}[{}] {}–{} ",
            if picked { "▸" } else { " " },
            c.idx + 1,
            clock(c.start),
            clock(c.end)
        );
        let head_style = if picked {
            Style::default().fg(th.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(th.dim)
        };
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
        let style = if picked {
            style.add_modifier(Modifier::BOLD)
        } else {
            style
        };
        let first = lines.len();
        let hlen = head.chars().count();
        for (i, w) in wrap(&text, width.saturating_sub(hlen).max(8))
            .into_iter()
            .enumerate()
        {
            if i == 0 {
                lines.push(Line::from(vec![
                    Span::styled(head.clone(), head_style),
                    Span::styled(w, style),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::raw(" ".repeat(hlen)),
                    Span::styled(w, style),
                ]));
            }
        }
        ranges.push((first, lines.len(), i));
        if picked {
            picked_rows = Some((first, lines.len()));
        }
    }
    let h = inner.height as usize;
    let max_skip = lines.len().saturating_sub(h);
    // A past session reads from the top and the live one shows the newest — unless a
    // summary was picked with ↑/↓, which then stays in view.
    let mut skip = if app.session.is_some() { 0 } else { max_skip };
    if let (Some((first, end)), Some(_)) = (picked_rows, app.chunk_cursor) {
        if first < skip {
            skip = first;
        } else if end > skip + h {
            skip = end.saturating_sub(h).min(first);
        }
    }
    app.hit.summary_rows = ranges
        .into_iter()
        .filter_map(|(first, end, i)| {
            let top = first.max(skip);
            let bottom = end.min(skip + h);
            (top < bottom).then(|| {
                (
                    Rect {
                        x: inner.x,
                        y: inner.y + (top - skip) as u16,
                        width: inner.width,
                        height: (bottom - top) as u16,
                    },
                    i,
                )
            })
        })
        .collect();
    let visible: Vec<Line> = lines.into_iter().skip(skip).take(h).collect();
    f.render_widget(Paragraph::new(visible), inner);
}

/// `text` wrapped to `width`, `prefix` on its first line and blanks under it after that.
fn push_wrapped(lines: &mut Vec<Line<'static>>, text: &str, width: usize, prefix: &str, style: Style) {
    let plen = prefix.chars().count();
    for (i, w) in wrap(text, width.saturating_sub(plen).max(8))
        .into_iter()
        .enumerate()
    {
        let head = if i == 0 {
            prefix.to_string()
        } else {
            " ".repeat(plen)
        };
        lines.push(Line::from(vec![Span::styled(head, style), Span::styled(w, style)]));
    }
}

/// A section of the Related pane: a heading, then one entry per fact with its `where`
/// under it. Nothing when the list is empty.
#[allow(clippy::too_many_arguments)]
fn push_facts(
    lines: &mut Vec<Line<'static>>,
    heading: &str,
    facts: &[Fact],
    prefix: &str,
    text: Style,
    where_: Style,
    heading_style: Style,
    width: usize,
) {
    if facts.is_empty() {
        return;
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(heading.to_string(), heading_style)));
    let plen = prefix.chars().count();
    for f in facts {
        push_wrapped(lines, &f.text, width, prefix, text);
        if !f.where_.is_empty() {
            push_wrapped(lines, &f.where_, width, &" ".repeat(plen), where_);
        }
    }
}

/// What the lookup found for the selected summary: the facts that bear on it, the
/// contradictions it noticed, the questions worth asking. ↑/↓ pick the summary; the pane
/// follows the newest answered one until a key places the cursor.
fn draw_related(f: &mut Frame, app: &mut App, area: Rect) {
    let th = app.theme;
    let viewing = app.session.is_some();
    let selected: Option<ChunkView> = app
        .selected_chunk()
        .and_then(|i| app.shown_chunks().get(i).cloned());
    let looking_up = app.looking_up().filter(|_| !viewing);
    let mut t = match &selected {
        None => "Related".to_string(),
        Some(c) => format!("Related · [{}] {}–{}", c.idx + 1, clock(c.start), clock(c.end)),
    };
    if let Some(idx) = looking_up {
        if selected.as_ref().map(|c| c.idx) != Some(idx) {
            t.push_str(&format!(" · looking up #{}", idx + 1));
        }
    }
    let b = block(title(&t, th), th);
    let inner = b.inner(area);
    f.render_widget(b, area);
    if inner.width < 8 || inner.height == 0 {
        return;
    }
    let width = inner.width as usize;
    let dim = Style::default().fg(th.dim);
    let heading = Style::default().fg(th.muted).add_modifier(Modifier::BOLD);
    let mut lines: Vec<Line> = Vec::new();
    let Some(c) = selected else {
        let hint = if viewing {
            "nothing was looked up in this session"
        } else if app.suggest_disabled {
            "suggestions are off (--no-suggest)"
        } else if app.lookup_disabled {
            "lookups are off (--no-lookup)"
        } else {
            "as you talk, each minute of it is summarized on the left, and what is already known about it lands here: what the repository and earlier meetings say, where what was said contradicts them, and the questions worth asking. ↑/↓ pick a summary; Tab shows the action items."
        };
        for w in wrap(hint, width) {
            lines.push(Line::from(Span::styled(w, dim)));
        }
        f.render_widget(Paragraph::new(lines), inner);
        return;
    };
    // The summary, or where its lookup stands.
    match (&c.state, &c.summary) {
        (ChunkState::Done, Some(s)) => push_wrapped(
            &mut lines,
            s,
            width,
            "",
            Style::default().fg(th.text).add_modifier(Modifier::BOLD),
        ),
        (ChunkState::Summarizing, _) => lines.push(Line::from(Span::styled(
            "looking up…",
            Style::default().fg(th.warn),
        ))),
        (ChunkState::Queued, _) => lines.push(Line::from(Span::styled(
            format!("queued ({} words)", c.words),
            dim,
        ))),
        (ChunkState::Failed(e), _) => push_wrapped(
            &mut lines,
            &format!("failed: {e}"),
            width,
            "",
            Style::default().fg(th.err),
        ),
        (ChunkState::Done, None) => lines.push(Line::from(Span::styled("(no summary)", dim))),
    }
    if c.state == ChunkState::Done {
        if c.facts.is_empty() && c.contradictions.is_empty() && c.questions.is_empty() {
            lines.push(Line::default());
            push_wrapped(
                &mut lines,
                "nothing known relates to this: no file, flag or earlier decision to point at",
                width,
                "",
                dim,
            );
        }
        push_facts(
            &mut lines,
            "Context",
            &c.facts,
            "• ",
            Style::default().fg(th.text),
            Style::default().fg(th.accent),
            heading,
            width,
        );
        push_facts(
            &mut lines,
            "Contradictions",
            &c.contradictions,
            "⚠ ",
            Style::default().fg(th.warn),
            Style::default().fg(th.accent),
            heading,
            width,
        );
        if !c.questions.is_empty() {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled("Questions", heading)));
            for q in &c.questions {
                push_wrapped(&mut lines, q, width, "? ", Style::default().fg(th.special));
            }
        }
    }
    let total = lines.len();
    let h = inner.height as usize;
    let max_scroll = total.saturating_sub(h);
    if app.related_scroll > max_scroll {
        app.related_scroll = max_scroll;
    }
    let visible: Vec<Line> = lines.into_iter().skip(app.related_scroll).take(h).collect();
    f.render_widget(Paragraph::new(visible), inner);
    if app.chunk_cursor.is_some() && !viewing {
        let tag = " ↓ G to follow ";
        let r = Rect {
            x: area.x + area.width.saturating_sub(tag.chars().count() as u16 + 1),
            y: area.y,
            width: tag.chars().count() as u16,
            height: 1,
        };
        f.render_widget(
            Paragraph::new(Span::styled(tag, Style::default().fg(th.warn))),
            r,
        );
    }
}

/// Markdown, lightly: `#` headings bold, `- ` bullets with a hanging indent, `**` dropped,
/// paragraphs wrapped, runs of blank lines collapsed to one.
fn push_markdown(lines: &mut Vec<Line<'static>>, text: &str, width: usize, th: Theme) {
    let heading = Style::default().fg(th.accent).add_modifier(Modifier::BOLD);
    let body = Style::default().fg(th.text);
    let clean = |s: &str| s.replace("**", "");
    let mut blank = true;
    for raw in text.lines() {
        let line = raw.trim_end();
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            if !blank {
                lines.push(Line::default());
                blank = true;
            }
            continue;
        }
        blank = false;
        let indent = " ".repeat((line.len() - trimmed.len()).min(8));
        if let Some(h) = trimmed.strip_prefix('#') {
            let h = h.trim_start_matches('#').trim();
            push_wrapped(lines, &clean(h), width, "", heading);
        } else if let Some(b) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| trimmed.strip_prefix("• "))
        {
            push_wrapped(lines, &clean(b.trim()), width, &format!("{indent}• "), body);
        } else if let Some(dot) = trimmed.find(". ").filter(|&i| {
            i > 0 && i <= 3 && trimmed[..i].bytes().all(|c| c.is_ascii_digit())
        }) {
            let (n, rest) = trimmed.split_at(dot + 2);
            push_wrapped(lines, &clean(rest.trim()), width, &format!("{indent}{n}"), body);
        } else {
            push_wrapped(lines, &clean(trimmed), width, &indent, body);
        }
    }
}

/// One hook's line in the Summary pane — `⚙ hook 1/2 summarize-transcript.sh: running… 12 s`
/// — then, while it runs or when it failed, the last lines it printed, and the file it wrote.
fn push_hook(lines: &mut Vec<Line<'static>>, r: &HookRun, width: usize, th: Theme) {
    let name = r.name();
    let who = format!("hook {}/{} {name}", r.index, r.count);
    let secs = r.elapsed_secs();
    let (text, style) = match &r.state {
        HookState::Running => (
            format!("⚙ {who}: running… {secs:.0} s"),
            Style::default().fg(th.warn).add_modifier(Modifier::BOLD),
        ),
        HookState::Done => (format!("✓ {who}: done in {secs:.0} s"), Style::default().fg(th.ok)),
        HookState::Failed(code) => (
            format!("✗ {who}: exited {code} after {secs:.0} s"),
            Style::default().fg(th.err),
        ),
        HookState::Lost => (
            format!("? {who}: the engine went away before it ended"),
            Style::default().fg(th.muted),
        ),
    };
    push_wrapped(lines, &text, width, "", style);
    if matches!(r.state, HookState::Running | HookState::Failed(_)) {
        for l in r.tail(3) {
            push_wrapped(lines, l, width, "    ", Style::default().fg(th.dim));
        }
    }
    if let Some((path, _)) = &r.wrote {
        push_wrapped(
            lines,
            &format!("wrote {}", path.display()),
            width,
            "    ",
            Style::default().fg(th.accent),
        );
    }
}

/// The meeting's summary. Once the recording stops, first what is still working on it:
/// the engine's onDone hooks one after another (the bundled `summarize-transcript.sh`
/// runs Claude on the transcript) with their last lines, and meet's own call for the
/// summary and the action items; then the write-up as it lands, and under it the file a
/// hook said it wrote. A past session shows its write-up (or its short summary).
fn draw_summary(f: &mut Frame, app: &mut App, area: Rect) {
    let th = app.theme;
    let viewing = app.session.is_some();
    let hooks_running = app.hooks.any_running();
    let (t, edge) = match (viewing, &app.wrap_up) {
        (true, _) => ("Meeting summary".to_string(), th.edge),
        (false, WrapUp::Writing) if hooks_running => {
            ("Meeting summary · hook running · writing…".to_string(), th.warn)
        }
        (false, WrapUp::Writing) => ("Meeting summary · writing…".to_string(), th.warn),
        (false, _) if hooks_running => ("Meeting summary · hook running…".to_string(), th.warn),
        (false, WrapUp::Failed(_)) => ("Meeting summary · failed".to_string(), th.err),
        (false, _) => ("Meeting summary".to_string(), th.edge),
    };
    let b = block(title(&t, th), th).border_style(Style::default().fg(edge));
    let inner = b.inner(area);
    f.render_widget(b, area);
    if inner.width < 8 || inner.height == 0 {
        return;
    }
    let width = inner.width as usize;
    let dim = Style::default().fg(th.dim);
    let mut lines: Vec<Line> = Vec::new();
    // What is (or was) working on it — the live meeting once the recording stopped.
    if !viewing && !app.is_live() {
        let (text, style) = match &app.wrap_up {
            _ if app.suggest_disabled => (
                "meet's summary and action items: off (--no-suggest)".to_string(),
                dim,
            ),
            WrapUp::NotYet => ("meet's summary and action items: not written".to_string(), dim),
            WrapUp::Writing => (
                "✎ meet's summary and action items: Claude is reading the whole transcript…"
                    .to_string(),
                Style::default().fg(th.warn).add_modifier(Modifier::BOLD),
            ),
            WrapUp::Done => (
                "✓ meet's summary and action items: written".to_string(),
                Style::default().fg(th.ok),
            ),
            WrapUp::Failed(e) => (
                format!("✗ meet's summary and action items: {e} — s tries again"),
                Style::default().fg(th.err),
            ),
        };
        push_wrapped(&mut lines, &text, width, "", style);
        match app.hooks.expected {
            Some(0) if app.hooks.skipped => {
                push_wrapped(&mut lines, "engine hooks: skipped (--no-hooks)", width, "", dim)
            }
            Some(0) => push_wrapped(
                &mut lines,
                "engine hooks: none configured (hooks.onDone in meet.json)",
                width,
                "",
                dim,
            ),
            Some(n) if app.hooks.runs.is_empty() => push_wrapped(
                &mut lines,
                &format!("⚙ engine hooks: {n} about to run"),
                width,
                "",
                Style::default().fg(th.warn),
            ),
            _ => {}
        }
        for r in &app.hooks.runs {
            push_hook(&mut lines, r, width, th);
        }
        if !lines.is_empty() {
            lines.push(Line::default());
        }
    }
    // The write-up, else the short summary, else where things stand.
    match (app.shown_notes(), app.shown_summary()) {
        (Some(notes), _) => push_markdown(&mut lines, notes, width, th),
        (None, Some(summary)) => {
            push_wrapped(&mut lines, summary, width, "", Style::default().fg(th.text));
            if viewing {
                lines.push(Line::default());
                push_wrapped(
                    &mut lines,
                    "(this session has only its short summary; the write-up came later)",
                    width,
                    "",
                    dim,
                );
            }
        }
        (None, None) => {
            let hint = if viewing {
                "no summary was written for this session"
            } else if app.suggest_disabled {
                "suggestions are off (--no-suggest): no summary is written"
            } else if app.is_live() {
                "the meeting's summary is written when the recording stops (x): the engine runs its onDone hooks — the bundled one asks Claude for a Markdown summary of the transcript — and meet's own call reads the whole transcript once for the summary, this write-up and the action items. Their progress shows here, then the write-up."
            } else {
                match &app.wrap_up {
                    WrapUp::Writing => "the write-up lands here the moment Claude answers…",
                    WrapUp::Failed(_) => "no write-up — s tries again",
                    _ => "nothing was said, so there is nothing to summarize",
                }
            };
            for w in wrap(hint, width) {
                lines.push(Line::from(Span::styled(w, dim)));
            }
        }
    }
    // A file a hook wrote (the bundled hook's Markdown summary), read once at its end.
    for r in &app.hooks.runs {
        if let Some((path, text)) = &r.wrote {
            lines.push(Line::default());
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            push_wrapped(
                &mut lines,
                &format!("── {name} · written by {} ──", r.name()),
                width,
                "",
                Style::default().fg(th.special).add_modifier(Modifier::BOLD),
            );
            push_markdown(&mut lines, text, width, th);
        }
    }
    let total = lines.len();
    let h = inner.height as usize;
    let max_scroll = total.saturating_sub(h);
    if app.summary_scroll > max_scroll {
        app.summary_scroll = max_scroll;
    }
    let visible: Vec<Line> = lines.into_iter().skip(app.summary_scroll).take(h).collect();
    f.render_widget(Paragraph::new(visible), inner);
    if max_scroll > 0 {
        let tag = format!(" ↑/↓ scroll · {}/{} ", app.summary_scroll + h.min(total), total);
        let w = tag.chars().count() as u16;
        if area.width > w + 2 {
            let r = Rect {
                x: area.x + area.width - w - 1,
                y: area.y + area.height - 1,
                width: w,
                height: 1,
            };
            f.render_widget(Paragraph::new(Span::styled(tag, dim)), r);
        }
    }
}

/// The question session about the shown session: Claude Code's screen, and the real
/// cursor while it has the keys.
fn draw_ask(f: &mut Frame, app: &mut App, area: Rect) {
    let th = app.theme;
    // "questions" for the live meeting; a past session is named by its date.
    let about = match &app.session {
        Some(v) => local_datetime(v.meeting.started_at),
        None => "questions".to_string(),
    };
    let viewing = app.session.is_some();
    let focused = app.term_focused;
    let (t, edge) = match app.term() {
        None => (format!("Claude Code · {about}"), th.edge),
        Some(term) if term.exited => (
            format!(
                "Claude Code · {about} · exited{} · a starts a new session",
                term.exit_code
                    .filter(|c| *c != 0)
                    .map(|c| format!(" ({c})"))
                    .unwrap_or_default()
            ),
            th.muted,
        ),
        Some(_) if focused => (
            format!("Claude Code · {about} · has the keys · {UNFOCUS_HINT} back to meet"),
            th.accent,
        ),
        Some(_) => (format!("Claude Code · {about} · a gives it the keys"), th.edge),
    };
    let b = block(title(&t, th), th).border_style(Style::default().fg(edge));
    let inner = b.inner(area);
    f.render_widget(b, area);
    if inner.width < 10 || inner.height < 2 {
        return;
    }
    let Some(term) = app.term_mut() else {
        let hint = if viewing {
            "a starts a Claude Code session here that answers questions about this past session — what was said, what was decided, how it relates to the code — from its transcript and summaries."
        } else {
            "a starts a Claude Code session here that answers questions about the meeting so far — it re-reads the transcript before every answer."
        };
        let lines: Vec<Line> = wrap(hint, inner.width as usize)
            .into_iter()
            .map(|w| Line::from(Span::styled(w, Style::default().fg(th.dim))))
            .collect();
        f.render_widget(Paragraph::new(lines), inner);
        return;
    };
    term.resize(inner.width, inner.height);
    let screen = term.screen();
    crate::term::render(screen, inner, f.buffer_mut());
    if focused && !term.exited && !screen.hide_cursor() {
        let (row, col) = screen.cursor_position();
        if row < inner.height && col < inner.width {
            f.set_cursor_position((inner.x + col, inner.y + row));
        }
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

fn draw_items(f: &mut Frame, app: &mut App, area: Rect) {
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
        (None, _) if writing => "Action items · writing… (m shows the progress)".to_string(),
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
                WrapUp::NotYet if app.rec == RecState::Idle => (
                    "the action items are written when a recording stops — r starts one"
                        .to_string(),
                    Style::default().fg(th.dim),
                ),
                WrapUp::NotYet => (
                    "the action items are written when the recording stops — x stops it"
                        .to_string(),
                    Style::default().fg(th.dim),
                ),
                WrapUp::Writing => (
                    "writing the summary and the action items from the transcript… (m shows the progress)".to_string(),
                    Style::default().fg(th.warn),
                ),
                WrapUp::Done => (
                    "no action items came out of this meeting (m shows its summary)".to_string(),
                    Style::default().fg(th.dim),
                ),
                WrapUp::Failed(e) => (
                    format!("could not write the summary and action items: {e} — s tries again"),
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
    // Where each row landed, for the clicks (the hint rows above an empty list are none).
    let hint_rows = lines.len().saturating_sub(visible.len().min(h).saturating_sub(first));
    app.hit.item_rows = visible
        .iter()
        .enumerate()
        .skip(first)
        .take(h)
        .map(|(row, _)| {
            (
                Rect {
                    x: inner.x,
                    y: inner.y + (hint_rows + row - first) as u16,
                    width: inner.width,
                    height: 1,
                },
                row,
            )
        })
        .filter(|(r, _)| r.y < inner.bottom())
        .collect();
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
    let hints_buf: String;
    let hints = match &app.overlay {
        Some(Overlay::Log { .. }) => "↑/↓ scroll  G follow  Esc close",
        Some(Overlay::Note { .. }) => "type a note into the transcript  Enter add  Esc cancel",
        // A confirm box lists its own keys; the footer stays quiet so the eye stays on it.
        Some(
            Overlay::ConfirmQuit { .. }
            | Overlay::ConfirmStop { .. }
            | Overlay::ConfirmEnd
            | Overlay::ConfirmDiscard,
        ) => "",
        Some(Overlay::Sessions { .. }) => "↑/↓ pick a session  Enter view  Esc close",
        // The settings modal lists its keys inside, like a confirm box.
        Some(Overlay::Settings { .. } | Overlay::ConfirmReset { .. }) => "",
        Some(Overlay::Help) => "Esc close",
        None if app.term_focused => {
            "Claude Code has the keys — type your question, Enter sends it  Ctrl+q keys back to meet"
        }
        None if app.bar_focused => {
            "←/→ or h/l step through the sessions  a ask Claude about it  Enter/Esc back to the panes  Tab pane  ? help  q quit"
        }
        None => {
            let nav = match app.right_pane {
                RightPane::Related => "↑/↓ summary  ",
                RightPane::Items => "↑/↓ item  ",
                RightPane::Summary => "↑/↓ scroll  ",
                RightPane::Ask => "",
            };
            hints_buf = if app.session.is_some() {
                format!("Esc live  ←/→ session  a ask Claude  Tab pane  m summary  {nav}Enter run/resume  X stop agent  l log  o open PR  , settings  ? help  q quit")
            } else if app.is_live() {
                format!("x stop recording  D discard  a ask Claude  Tab pane  {nav}Enter run  X stop agent  d dismiss  i note  s look up now  space pause  M/N mute mic/system  , settings  ? help  q quit")
            } else {
                // Finalizing: the recording is still stopping, so no r yet.
                let record = if app.rec == RecState::Finalizing { "" } else { "r record  " };
                format!("{record}a ask Claude  Tab pane  m summary  {nav}Enter run  X stop agent  d dismiss  l log  o open PR  S sessions  ←/→ session  , settings  ? help  q quit")
            };
            hints_buf.as_str()
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
    if let Some(r) = app.hooks.running() {
        right_parts.push(format!("⚙ hook {} {:.0}s", r.name(), r.elapsed_secs()));
    }
    if app.wrap_up == WrapUp::Writing {
        right_parts.push("✎ writing the summary".into());
    }
    let running = app.running_count();
    if running > 0 {
        right_parts.push(format!(
            "⚙ {running} agent{} running",
            if running == 1 { "" } else { "s" }
        ));
    }
    let mut right = vec![Span::styled(
        right_parts.join("  "),
        Style::default().fg(th.warn),
    )];
    // Bottom right: what every Claude call so far has spent, live as the lookups, the
    // action-item writer and the agents go.
    let usage = app.usage();
    if !usage.is_zero() {
        if !right_parts.is_empty() {
            right.push(Span::raw("  "));
        }
        right.push(Span::styled(usage_text(&usage), Style::default().fg(th.muted)));
    }
    right.push(Span::raw(" "));
    let right = Line::from(right);
    let rw = right.width() as u16;
    f.render_widget(Paragraph::new(Line::from(left)), area);
    if rw > 1 && area.width > rw {
        let r = Rect {
            x: area.x + area.width - rw,
            width: rw,
            ..area
        };
        f.render_widget(Paragraph::new(right), r);
    }
}

/// `in 45k · out 3.4k · $0.31`: tokens read (fresh and cached), tokens written, and the
/// dollars claude reported. The cost is left out until a call has reported one — a
/// running agent's turns carry tokens but no price.
pub fn usage_text(u: &Usage) -> String {
    let mut s = format!(
        "in {} · out {}",
        tokens_short(u.input_total()),
        tokens_short(u.output)
    );
    if u.cost_usd > 0.0 {
        s.push_str(&format!(" · ${:.2}", u.cost_usd));
    }
    s
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

/// A confirm box: what happens, a blank row, then every key that answers it — all inside
/// the box, so the reader never has to look down at the footer. The box is exactly as tall
/// as its text; a long key description wraps under itself.
fn draw_confirm(
    f: &mut Frame,
    area: Rect,
    heading: &str,
    edge: Color,
    body: &str,
    keys: &[(&str, &str)],
    th: Theme,
) -> Rect {
    let r = centered(area, 60, 20);
    let width = r.width.saturating_sub(4) as usize;
    let key_w = keys
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(0);
    let mut lines: Vec<Line> = Vec::new();
    if !body.is_empty() {
        for l in wrap(body, width) {
            lines.push(Line::from(Span::styled(
                format!(" {l}"),
                Style::default().fg(th.text),
            )));
        }
        lines.push(Line::default());
    }
    for (k, v) in keys {
        let key = Span::styled(
            format!(" {k:<key_w$}  "),
            Style::default().fg(th.accent).add_modifier(Modifier::BOLD),
        );
        let pad = Span::raw(" ".repeat(key_w + 3));
        for (i, l) in wrap(v, width.saturating_sub(key_w + 2))
            .into_iter()
            .enumerate()
        {
            lines.push(Line::from(vec![
                if i == 0 { key.clone() } else { pad.clone() },
                Span::styled(l, Style::default().fg(th.text)),
            ]));
        }
    }
    let height = (lines.len() as u16 + 2).min(area.height);
    let r = Rect {
        y: area.y + (area.height - height) / 2,
        height,
        ..r
    };
    f.render_widget(Clear, r);
    let b = block(title(heading, th), th).border_style(Style::default().fg(edge));
    let inner = b.inner(r);
    f.render_widget(b, r);
    f.render_widget(Paragraph::new(lines), inner);
    r
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
            app.hit.overlay = Some(r);
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
            app.hit.overlay = Some(r);
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
                msg.push("The recording is still going.".to_string());
            }
            if !live && app.summarizing() {
                let mut busy = Vec::new();
                if app.wrap_up == WrapUp::Writing {
                    busy.push("meet's summary and action items are being written".to_string());
                }
                if let Some(r) = app.hooks.running() {
                    busy.push(format!("the engine's hook {} is still running", r.name()));
                } else if app.hooks.pending() {
                    busy.push("the engine's hooks are about to run".to_string());
                }
                msg.push(format!(
                    "The meeting is still being summarized: {} — quitting cuts that short (the engine goes down with meet).",
                    busy.join(", and ")
                ));
            }
            let mut keys: Vec<(&str, &str)> = Vec::new();
            if live {
                keys.push((
                    "y / Enter",
                    "quit: the recording stops and the transcript is saved, but no summary or action items are written",
                ));
                keys.push(("x", "stop the recording and stay, so the summary and the action items land here first"));
                keys.push((
                    "d",
                    "discard the recording and quit: the transcript and audio are deleted, no hook runs and nothing is summarized (asks once more)",
                ));
            } else {
                keys.push(("y / Enter", "quit"));
            }
            keys.push(("n / Esc", "stay"));
            app.hit.overlay = Some(draw_confirm(f, area, "Quit?", th.err, &msg.join(" "), &keys, th));
        }
        Overlay::ConfirmEnd => app.hit.overlay = Some(draw_confirm(
            f,
            area,
            "Stop the recording?",
            th.warn,
            "The transcript and audio are saved, the engine's onDone hooks run (the bundled one has Claude write a summary file), and meet's own Claude call writes the meeting's summary and its action items from everything that was said — the Summary pane shows all of that as it happens, then the write-up. meet stays open to run the items, and r starts the next recording as a new session.",
            &[("y / Enter", "stop the recording"), ("n / Esc", "keep recording")],
            th,
        )),
        Overlay::ConfirmDiscard => app.hit.overlay = Some(draw_confirm(
            f,
            area,
            "Discard the recording?",
            th.err,
            "Nothing is kept: the recording stops, its transcript and audio are deleted, the engine's onDone hooks do not run, no summary or action items are written, and the session leaves the session bar. meet quits. There is no getting it back.",
            &[
                ("y / Enter", "discard the recording and quit"),
                ("n / Esc", "keep it"),
            ],
            th,
        )),
        Overlay::ConfirmStop { item_id } => {
            let name = app
                .items
                .iter()
                .find(|i| i.id == item_id)
                .map(|i| i.title.clone())
                .unwrap_or_default();
            app.hit.overlay = Some(draw_confirm(
                f,
                area,
                "Stop the agent?",
                th.warn,
                &format!(
                    "Stop the agent working on \"{name}\"? It gets a SIGTERM; its worktree, commits and Claude session stay, and Enter on the item later resumes the conversation where it was."
                ),
                &[("y / Enter", "stop the agent"), ("n / Esc", "keep it running")],
                th,
            ));
        }
        Overlay::Sessions { cursor } => {
            let r = centered(area, 80, 70);
            app.hit.overlay = Some(r);
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
            app.hit.overlay_rows = (first..(first + h).min(app.sessions.len()))
                .map(|i| {
                    (
                        Rect {
                            x: inner.x,
                            y: inner.y + (i - first) as u16,
                            width: inner.width,
                            height: 1,
                        },
                        i,
                    )
                })
                .collect();
            f.render_widget(Paragraph::new(lines), inner);
        }
        Overlay::Settings { cursor, notice } => {
            let (r, rows) = draw_settings(f, app, area, cursor, notice.as_ref());
            app.hit.overlay = Some(r);
            app.hit.overlay_rows = rows;
        }
        Overlay::ConfirmReset { .. } => app.hit.overlay = Some(draw_confirm(
            f,
            area,
            "Reset the settings?",
            th.warn,
            "Every model and effort goes back to its default: the lookup sonnet at low effort, the action items and the question session sonnet, the agent claude's own (or agent.model in meet.json). The settings file is rewritten.",
            &[("y / Enter", "reset every setting"), ("n / Esc", "keep them")],
            th,
        )),
        Overlay::Help => {
            let r = centered(area, 64, 80);
            app.hit.overlay = Some(r);
            f.render_widget(Clear, r);
            let b = block(title("Keys", th), th).border_style(Style::default().fg(th.accent));
            let inner = b.inner(r);
            f.render_widget(b, r);
            let keys = [
                (
                    "r",
                    "start recording; after a recording ended, start the next one — a new session on the bar",
                ),
                (
                    "x",
                    "stop the recording and stay: the engine's hooks run, and the summary and the action items are written from the transcript",
                ),
                (
                    "D",
                    "discard the recording and quit (asks first): it stops, the transcript and audio are deleted, no hook runs and nothing is summarized",
                ),
                (
                    "m",
                    "the meeting summary: the hooks and meet's summary call at work once the recording stops, then the write-up (and the file a hook wrote)",
                ),
                (
                    "a",
                    "ask Claude Code about the session on screen — the live meeting or a past one: it runs in the right pane and takes the keys; Ctrl+q hands them back",
                ),
                (
                    "Tab",
                    "cycle the focus: related context / action items / meeting summary / Claude Code / the session bar (Shift+Tab backwards)",
                ),
                (
                    "j / k, ↓ / ↑",
                    "pick a summary (Related: its facts, contradictions, questions) or an action item (Items)",
                ),
                (
                    "Enter",
                    "run it: a worktree, a Claude Code agent, a pull request — or resume it",
                ),
                ("X", "stop its agent (Enter resumes the conversation later)"),
                ("d", "dismiss it (hidden for good)"),
                ("l", "the agent's log for it"),
                ("o", "open its pull request in the browser"),
                (
                    "← / →",
                    "walk the session bar: → an older session, ← a newer one (the live meeting is at the left end); Esc back to live",
                ),
                ("h / l", "the same, while the bar has the keys (Tab); Enter or Esc hand them back"),
                ("S", "the session list: every meeting held in this repo, with details"),
                (
                    ",",
                    "settings: which Claude model and effort the lookup, the action items, the agent and the question session run with",
                ),
                ("i", "type a note into the transcript"),
                (
                    "s",
                    "look up what is pending now; once ended, write the summary and the action items again",
                ),
                ("space", "pause / resume the recording"),
                (
                    "M / N",
                    "mute / unmute the microphone / the system audio: recorded and transcribed as silence while muted (⊘ in the header); a click on mic / system up there does the same",
                ),
                (
                    "PgUp / PgDn, J / K",
                    "scroll the transcript; G follows the newest line and summary again",
                ),
                ("[ / ]", "scroll the right pane"),
                (
                    "mouse",
                    "click a pane, a tab or a session to focus it, a summary or an action item to pick it; the wheel scrolls; drag the border between two panes to resize them (⇧drag selects text through your terminal)",
                ),
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

/// The settings modal, laid out like nebula's: a header per feature, its rows as a label
/// column with the value in brackets, the selected row highlighted; under a blank line,
/// what the selected row's feature is, the row's hint (or the result of the last change),
/// the keys, and the file being written. A flag passed this launch is named beside the
/// row it overrides.
fn draw_settings(
    f: &mut Frame,
    app: &App,
    area: Rect,
    cursor: usize,
    notice: Option<&(String, bool)>,
) -> (Rect, Vec<(Rect, usize)>) {
    use crate::settings::{self, Row};
    const WIDTH: u16 = 84;
    /// The blank line, the feature line, the hint line, the keys line, the file line.
    const CHROME: u16 = 5;
    let th = app.theme;
    let rows = settings::rows();
    let cursor = cursor.min(settings::ROWS.len() - 1);
    let width = WIDTH.min(area.width);
    let want = rows.len() as u16 + CHROME + 2;
    let height = want
        .min(area.height.saturating_sub(2))
        .max(CHROME + 3)
        .min(area.height);
    let r = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    f.render_widget(Clear, r);
    let b = block(title("Settings", th), th).border_style(Style::default().fg(th.accent));
    let inner = b.inner(r);
    f.render_widget(b, r);
    // Where each setting's row lands, for the clicks.
    let mut hits: Vec<(Rect, usize)> = Vec::new();
    if inner.width < 20 || inner.height == 0 {
        return (r, hits);
    }
    let body_h = inner.height.saturating_sub(CHROME).max(1) as usize;
    let sel_row = rows
        .iter()
        .position(|r| *r == Row::Setting(cursor))
        .unwrap_or(0);
    let first = (sel_row + 1).saturating_sub(body_h);
    let dim = |text: String| Line::from(Span::styled(text, Style::default().fg(th.dim)));
    let mut lines: Vec<Line> = Vec::new();
    for (j, row) in rows.iter().skip(first).take(body_h).enumerate() {
        if let Row::Setting(i) = row {
            hits.push((
                Rect {
                    x: inner.x,
                    y: inner.y + j as u16,
                    width: inner.width,
                    height: 1,
                },
                *i,
            ));
        }
        match row {
            Row::Blank => lines.push(Line::default()),
            Row::Header(feature) => lines.push(Line::from(Span::styled(
                format!(" {}", feature.title()),
                Style::default().fg(th.muted).add_modifier(Modifier::BOLD),
            ))),
            Row::Setting(i) => {
                let (feature, field) = settings::ROWS[*i];
                let value = app.settings.get(feature, field);
                let selected = *i == cursor;
                let mut label_style = Style::default().fg(th.text);
                let mut value_style = Style::default().fg(th.accent);
                if selected {
                    label_style = label_style.bg(th.sel_bg).add_modifier(Modifier::BOLD);
                    value_style = value_style.bg(th.sel_bg).add_modifier(Modifier::BOLD);
                }
                let mut spans = vec![
                    Span::styled(format!("   {:<12}", field.label()), label_style),
                    Span::styled(format!("[{value}]"), value_style),
                ];
                if let Some(o) = app.overrides.get(feature, field) {
                    spans.push(Span::styled(
                        format!("  {} {o} this launch", settings::flag_name(feature, field)),
                        Style::default().fg(th.warn),
                    ));
                }
                lines.push(Line::from(spans));
            }
        }
    }
    while lines.len() < body_h {
        lines.push(Line::default());
    }
    let (feature, field) = settings::ROWS[cursor];
    lines.push(Line::default());
    lines.push(dim(format!(" {}", feature.blurb())));
    lines.push(match notice {
        Some((text, warn)) => Line::from(Span::styled(
            format!(" {text}"),
            Style::default().fg(if *warn { th.warn } else { th.ok }),
        )),
        None => dim(format!(" {}", field.hint())),
    });
    lines.push(dim(
        " ↑/↓ move  ←/→ cycle  Enter next  1-4 jump  R reset all  Esc close".to_string(),
    ));
    lines.push(dim(format!(" {}", settings::Settings::path_display())));
    f.render_widget(Paragraph::new(lines), inner);
    (r, hits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunker::Limits;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// The screen as text, one row per line.
    fn render(app: &mut App, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, app)).unwrap();
        let buf = t.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..h {
            for x in 0..w {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn app_with(overlay: Overlay) -> App {
        let mut app = App::new(
            "/r".into(),
            "main".into(),
            false,
            Limits::default(),
            "m".into(),
            Vec::new(),
        );
        app.overlay = Some(overlay);
        app
    }

    /// A live meeting with two summaries and two action items, for the mouse.
    fn busy_app() -> App {
        use crate::chunker::Chunk;
        use crate::recorder::Segment;
        let item = |id: &str, title: &str| ActionItem {
            id: id.into(),
            meeting_id: "m".into(),
            chunk_id: None,
            repo_path: "/r".into(),
            title: title.into(),
            prompt: "do it".into(),
            why: String::new(),
            status: ItemStatus::Suggested,
            branch: None,
            worktree_path: None,
            pr_url: None,
            log_path: None,
            error: None,
            session_id: None,
            phase: RunPhase::Agent,
            created_at: 1,
            updated_at: 1,
        };
        let mut app = App::new(
            "/r".into(),
            "main".into(),
            false,
            Limits::default(),
            "m".into(),
            vec![item("i1", "Add --json to list"), item("i2", "Fix add")],
        );
        app.on_started(String::new(), vec!["mic".into()]);
        for (i, text) in ["first stretch of talk", "second stretch of talk"]
            .iter()
            .enumerate()
        {
            let start = i as f64 * 60.0;
            let chunk = Chunk {
                start,
                end: start + 60.0,
                segments: vec![Segment {
                    source: "mic".into(),
                    text: (*text).into(),
                    start,
                    end: start + 5.0,
                }],
            };
            app.add_chunk(format!("c{i}"), &chunk);
        }
        app
    }

    /// The character drawn at `(x, y)` of a rendered screen.
    fn cell(screen: &str, x: u16, y: u16) -> char {
        screen
            .lines()
            .nth(y as usize)
            .and_then(|l| l.chars().nth(x as usize))
            .unwrap_or_else(|| panic!("no cell at {x},{y}:\n{screen}"))
    }

    #[test]
    fn the_right_pane_has_tabs_on_its_border_and_the_seams_have_grips() {
        use crate::layout::HitTarget;
        let mut app = busy_app();
        let screen = render(&mut app, 120, 40);
        // The tabs on the right column's bottom border, right-aligned.
        let edge = screen
            .lines()
            .nth(app.hit.right.bottom() as usize - 1)
            .unwrap()
            .to_string();
        assert!(edge.ends_with(" Related │ Items │ Summary │ Claude ─╯"), "{edge:?}");
        assert_eq!(app.hit.right_tabs.len(), 4, "{:?}", app.hit.right_tabs);
        let (tab, pane) = app.hit.right_tabs[1];
        assert_eq!(pane, RightPane::Items);
        assert_eq!(tab.y, app.hit.right.bottom() - 1);
        let label: String = edge
            .chars()
            .skip(tab.x as usize)
            .take(tab.width as usize)
            .collect();
        assert_eq!(label, " Items ", "the tab sits on exactly the cells it was drawn on");
        assert_eq!(
            app.hit.hit_at(tab.x + 1, tab.y),
            HitTarget::RightTab(RightPane::Items)
        );
        // The grips: three rows of the columns' seam mid-body, three cells of the
        // transcript's bottom border mid-width; the rest of the border is left alone.
        let right = app.hit.right;
        let mid = app.hit.body.y + app.hit.body.height / 2;
        assert_eq!(cell(&screen, right.x, mid), '┃');
        assert_eq!(cell(&screen, right.x - 1, mid - 1), '┃');
        assert_eq!(cell(&screen, right.x, mid + 3), '│');
        let t = app.hit.transcript;
        assert_eq!(cell(&screen, t.x + t.width / 2, app.hit.summaries.y - 1), '━');
        assert_eq!(cell(&screen, t.x + 2, app.hit.summaries.y - 1), '─');
        assert_eq!(
            app.hit.hit_at(right.x, mid),
            HitTarget::Splitter(Splitter::Columns)
        );
        // A click on the Items tab shows the board, each item a row.
        app.show_pane(RightPane::Items);
        let screen = render(&mut app, 120, 40);
        assert!(screen.contains("Action items"), "{screen}");
        assert_eq!(app.hit.item_rows.len(), 2, "{:?}", app.hit.item_rows);
        let (row, i) = app.hit.item_rows[1];
        assert_eq!(i, 1);
        assert_eq!(app.hit.hit_at(row.x + 2, row.y), HitTarget::Items(Some(1)));
        app.select_row(1);
        assert_eq!(app.selected, 1);
        let d = app.hit.detail.expect("the prompt pane");
        assert_eq!(cell(&screen, right.x + right.width / 2, d.y - 1), '━');
        assert_eq!(
            app.hit.hit_at(right.x + 5, d.y),
            HitTarget::Splitter(Splitter::RightRows)
        );
        // Claude Code's tab shows its pane, which says how to start a session.
        app.show_pane(RightPane::Ask);
        assert_eq!(app.right_pane, RightPane::Ask);
        assert!(!app.term_focused, "no session running: nothing to hand the keys to");
        let screen = render(&mut app, 120, 40);
        assert!(screen.contains("a starts a Claude Code session here"), "{screen}");
    }

    #[test]
    fn dragging_a_seam_puts_it_under_the_pointer_and_the_share_survives_a_resize() {
        use crate::layout::MIN_COL_W;
        let mut app = busy_app();
        render(&mut app, 120, 40);
        let seam = app.hit.right.x;
        assert_eq!(seam, 55);
        // Grab the left pane's border cell (one left of the seam), drag right by 15.
        app.grab_splitter(Splitter::Columns, seam - 1, 10);
        assert_eq!(app.splitter_drag.unwrap().grab_offset, 1);
        app.drag_splitter_to(seam - 1 + 15, 10);
        render(&mut app, 120, 40);
        assert_eq!(app.hit.right.x, seam + 15, "the seam followed the pointer");
        assert!(app.release_splitter());
        assert!(!app.release_splitter(), "nothing to release twice");
        // Past the minimum, the right column keeps its width.
        app.grab_splitter(Splitter::Columns, seam + 15, 10);
        app.drag_splitter_to(119, 10);
        render(&mut app, 120, 40);
        assert_eq!(app.hit.right.width, MIN_COL_W);
        app.release_splitter();
        // A share, not a cell: a wider screen keeps the proportion.
        let share = app.layout.left;
        render(&mut app, 200, 40);
        assert_eq!(app.hit.right.x, (200.0 * share).round() as u16);
        // The transcript/summary seam.
        render(&mut app, 120, 40);
        let seam_y = app.hit.summaries.y;
        app.grab_splitter(Splitter::LeftRows, 10, seam_y);
        app.drag_splitter_to(10, seam_y - 6);
        render(&mut app, 120, 40);
        assert_eq!(app.hit.summaries.y, seam_y - 6);
        app.release_splitter();
        // The items/prompt seam: sized to the items until dragged, then where it was put.
        app.show_pane(RightPane::Items);
        render(&mut app, 120, 40);
        assert_eq!(app.hit.items.unwrap().height, 4, "two items and the border");
        assert_eq!(app.layout.items, None);
        let seam_y = app.hit.detail.unwrap().y;
        app.grab_splitter(Splitter::RightRows, 70, seam_y - 1);
        app.drag_splitter_to(70, seam_y + 9);
        render(&mut app, 120, 40);
        assert_eq!(
            app.hit.detail.unwrap().y,
            seam_y + 10,
            "grabbed a cell above the seam, so it lands a cell below the pointer"
        );
        assert!(app.layout.items.is_some());
        app.release_splitter();
    }

    #[test]
    fn clicks_land_on_summaries_sessions_and_overlays() {
        use crate::layout::HitTarget;
        let mut app = busy_app();
        app.sessions = vec![
            meeting_at("m", 1_700_000_000),
            meeting_at("old", 1_699_900_000),
        ];
        let screen = render(&mut app, 120, 40);
        assert_eq!(app.hit.bar_tabs.len(), 2, "{:?}", app.hit.bar_tabs);
        let bar = screen.lines().nth(1).unwrap().to_string();
        let (tab, i) = app.hit.bar_tabs[1];
        assert_eq!(i, 1);
        let label: String = bar
            .chars()
            .skip(tab.x as usize)
            .take(tab.width as usize)
            .collect();
        assert_eq!(label.trim(), short_datetime(1_699_900_000), "{bar:?}");
        assert_eq!(app.hit.hit_at(tab.x, 1), HitTarget::Bar(Some(1)));
        assert_eq!(app.hit.hit_at(1, 1), HitTarget::Bar(None));
        // Summaries: each has its rows; a click picks it for the Related pane.
        assert_eq!(app.hit.summary_rows.len(), 2, "{:?}", app.hit.summary_rows);
        let (row, i) = app.hit.summary_rows[0];
        assert_eq!(i, 0);
        assert_eq!(app.hit.hit_at(row.x + 3, row.y), HitTarget::Summaries(Some(0)));
        app.show_pane(RightPane::Summary);
        app.pick_chunk(0);
        assert_eq!(app.right_pane, RightPane::Related);
        assert_eq!(app.selected_chunk(), Some(0));
        app.pick_chunk(1);
        assert_eq!(app.chunk_cursor, None, "the newest is followed, not pinned");
        assert_eq!(app.hit.hit_at(5, 5), HitTarget::Transcript);
        assert_eq!(app.hit.hit_at(80, 20), HitTarget::Right);
        // An overlay takes the clicks: inside is its own, outside closes it.
        app.overlay = Some(Overlay::Sessions { cursor: 0 });
        render(&mut app, 120, 40);
        let r = app.hit.overlay.expect("the session list's box");
        assert_eq!(app.hit.overlay_rows.len(), 2);
        assert_eq!(app.hit.hit_at(r.x + 2, r.y + 1), HitTarget::Overlay(Some(0)));
        assert_eq!(app.hit.hit_at(r.x + 2, r.y + 2), HitTarget::Overlay(Some(1)));
        assert_eq!(app.hit.hit_at(0, 0), HitTarget::Outside);
        app.overlay = Some(Overlay::Help);
        render(&mut app, 120, 40);
        let r = app.hit.overlay.expect("the help box");
        assert_eq!(app.hit.hit_at(r.x + 5, r.y + 5), HitTarget::Overlay(None));
        assert_eq!(app.hit.hit_at(r.x - 1, r.y), HitTarget::Outside);
        app.overlay = Some(Overlay::ConfirmEnd);
        render(&mut app, 120, 40);
        assert!(app.hit.overlay.is_some(), "a confirm box is a box too");
        app.overlay = None;
        render(&mut app, 120, 40);
        assert_eq!(app.hit.overlay, None, "closed: the panes take the clicks again");
    }

    #[test]
    fn the_settings_modal_lists_every_feature_with_its_values_and_the_file() {
        use crate::settings::{Feature, Field, FILE};
        let mut app = app_with(Overlay::Settings {
            cursor: 4,
            notice: None,
        });
        app.overrides
            .set(Feature::Lookup, Field::Model, Some("haiku".into()));
        let screen = render(&mut app, 120, 40);
        for h in ["Settings", "Lookup", "Action items", "Agent", "Ask"] {
            assert!(screen.contains(h), "{h} missing:\n{screen}");
        }
        assert!(screen.contains("[sonnet]"), "{screen}");
        assert!(screen.contains("[low]"), "{screen}");
        assert!(screen.contains("[default]"), "{screen}");
        assert!(
            screen.contains("--lookup-model haiku this launch"),
            "the flag passed this launch is named beside its row:\n{screen}"
        );
        assert!(
            screen.contains("the implementing agent"),
            "the selected row's feature is explained:\n{screen}"
        );
        assert!(screen.contains("passes no --model"), "{screen}");
        assert!(screen.contains("R reset all"), "{screen}");
        assert!(screen.contains(FILE), "the file being written is named:\n{screen}");
        let rows: Vec<&str> = screen.lines().collect();
        let footer = rows.last().unwrap();
        assert!(
            !footer.contains("Esc"),
            "the footer stays quiet under the modal: {footer:?}"
        );

        let mut app = app_with(Overlay::Settings {
            cursor: 0,
            notice: Some(("saved · lookup model opus".into(), false)),
        });
        let screen = render(&mut app, 120, 40);
        assert!(screen.contains("saved · lookup model opus"), "{screen}");
        assert!(
            !screen.contains("passes no --model"),
            "the notice takes the hint line:\n{screen}"
        );
        let mut app = app_with(Overlay::ConfirmReset { cursor: 0 });
        let screen = render(&mut app, 120, 40);
        assert!(screen.contains("Reset the settings?"), "{screen}");
        assert!(screen.contains("y / Enter"), "{screen}");
    }

    fn meeting_at(id: &str, started_at: i64) -> MeetingRow {
        MeetingRow {
            id: id.into(),
            repo_path: "/r".into(),
            meeting_dir: None,
            started_at,
            ended_at: Some(started_at + 60),
            segment_count: 3,
            summary: None,
        }
    }

    #[test]
    fn the_session_bar_lists_every_session_newest_first_and_scrolls_to_the_shown_one() {
        let mut app = App::new(
            "/r".into(),
            "main".into(),
            false,
            Limits::default(),
            "live".into(),
            Vec::new(),
        );
        let screen = render(&mut app, 100, 12);
        let bar = screen.lines().nth(1).unwrap().to_string();
        assert!(bar.contains("sessions"), "{bar:?}");
        assert!(bar.contains("(none yet)"), "{bar:?}");

        // Six sessions a day apart, the live one first.
        let day = 86_400;
        let t0 = 1_756_000_000;
        app.sessions = vec![meeting_at("live", t0)];
        for i in 1..6 {
            app.sessions
                .push(meeting_at(&format!("s{i}"), t0 - i * day));
        }
        app.rec = RecState::Recording;
        let screen = render(&mut app, 120, 12);
        let bar = screen.lines().nth(1).unwrap().to_string();
        assert!(bar.starts_with(" sessions "), "{bar:?}");
        let live_at = bar.find("● live").expect(&bar);
        let dates: Vec<usize> = app.sessions[1..]
            .iter()
            .map(|m| bar.find(&short_datetime(m.started_at)).expect(&bar))
            .collect();
        assert!(live_at < dates[0], "the live meeting is leftmost: {bar:?}");
        assert!(
            dates.windows(2).all(|w| w[0] < w[1]),
            "older sessions run to the right: {bar:?}"
        );
        assert!(!bar.contains('›'), "everything fits: {bar:?}");

        // Too narrow for all six: the shown (live) tab is in view, the rest counted.
        let screen = render(&mut app, 50, 12);
        let bar = screen.lines().nth(1).unwrap().to_string();
        assert!(bar.contains("● live"), "{bar:?}");
        assert!(bar.contains('›'), "hidden tabs on the right are counted: {bar:?}");
        assert!(!bar.contains('‹'), "{bar:?}");

        // Viewing the oldest: the bar scrolls so its tab shows, hidden ones on the left.
        app.enter_session(crate::app::SessionView {
            meeting: app.sessions[5].clone(),
            transcript: vec![],
            chunks: vec![],
        });
        app.bar_focused = true;
        let screen = render(&mut app, 50, 12);
        let bar = screen.lines().nth(1).unwrap().to_string();
        assert!(
            bar.contains(&format!("[{}]", short_datetime(app.sessions[5].started_at))),
            "the shown session's tab is in view, bracketed while the bar has the keys: {bar:?}"
        );
        assert!(bar.contains('‹'), "hidden tabs on the left are counted: {bar:?}");
        assert!(!bar.contains('›'), "{bar:?}");
        assert!(app.bar_first > 0);
        let footer = screen.lines().last().unwrap().to_string();
        assert!(
            footer.contains("h/l step through the sessions"),
            "the footer names the bar's keys while it has them: {footer:?}"
        );
        let header = screen.lines().next().unwrap().to_string();
        assert!(header.contains("session 6 of 6"), "{header:?}");

        // Back on the live meeting, the bar scrolls back to its start.
        app.leave_session();
        let screen = render(&mut app, 50, 12);
        let bar = screen.lines().nth(1).unwrap().to_string();
        assert!(bar.contains("[● live]") && !bar.contains('‹'), "{bar:?}");
        assert_eq!(app.bar_first, 0);
        app.leave_bar();
        let screen = render(&mut app, 50, 12);
        let bar = screen.lines().nth(1).unwrap().to_string();
        assert!(
            bar.contains(" ● live ") && !bar.contains('['),
            "no brackets once the keys left the bar: {bar:?}"
        );
        app.on_finished();
        let screen = render(&mut app, 50, 12);
        assert!(screen.lines().nth(1).unwrap().contains("■ ended"), "{screen}");
    }

    #[test]
    fn the_summary_pane_shows_the_hooks_and_the_call_at_work_then_the_write_up() {
        let mut app = App::new(
            "/r".into(),
            "main".into(),
            false,
            Limits::default(),
            "m".into(),
            Vec::new(),
        );
        app.on_started(String::new(), vec!["mic".into()]);
        app.show_summary();
        let screen = render(&mut app, 120, 30);
        assert!(screen.contains("Meeting summary"), "{screen}");
        assert!(
            screen.contains("written when the recording stops"),
            "while recording, the pane says what will happen:\n{screen}"
        );

        // The recording stopped: meet's call and the engine's hooks are at work.
        app.on_finished();
        app.wrap_up = WrapUp::Writing;
        app.hooks.on_announced(1, false);
        app.hooks
            .on_started(1, 1, "/opt/meet/hooks/summarize-transcript.sh".into());
        app.hooks.on_output("summarize-transcript: summarizing /m/transcript.md");
        let screen = render(&mut app, 120, 30);
        let header = screen.lines().next().unwrap().to_string();
        assert!(header.contains("✎ SUMMARIZING"), "the header says so: {header:?}");
        assert!(
            screen.contains("Meeting summary · hook running · writing…"),
            "{screen}"
        );
        assert!(screen.contains("✎ meet's summary and action items: Claude is reading"), "{screen}");
        assert!(
            screen.contains("⚙ hook 1/1 summarize-transcript.sh: running…"),
            "{screen}"
        );
        assert!(
            screen.contains("summarize-transcript: summarizing /m/transcript.md"),
            "the hook's last lines show under it:\n{screen}"
        );
        let footer = screen.lines().last().unwrap().to_string();
        assert!(footer.contains("⚙ hook summarize-transcript.sh"), "{footer:?}");
        assert!(footer.contains("✎ writing the summary"), "{footer:?}");

        // The hook ends having written a file; the write-up lands.
        let dir = tempfile::tempdir().unwrap();
        let md = dir.path().join("2026-09-11_release-plan.md");
        std::fs::write(&md, "# Release Plan\n\n## Summary\nThe hook's own summary.\n").unwrap();
        app.hooks
            .on_output(&format!("summarize-transcript: wrote {}", md.display()));
        app.hooks.on_ended(1, 0, 41.0);
        app.wrap_up = WrapUp::Done;
        app.meeting_summary = Some("Short.".into());
        app.notes.insert(
            "m".into(),
            "## Summary\nA **long** one.\n\n## Decisions\n- ship it\n- and *then* rest\n\n## Open questions\n1. when?".into(),
        );
        let screen = render(&mut app, 120, 40);
        let header = screen.lines().next().unwrap().to_string();
        assert!(header.contains("■ ended"), "{header:?}");
        assert!(screen.contains("✓ hook 1/1 summarize-transcript.sh: done in 41 s"), "{screen}");
        assert!(
            screen.contains("    wrote") && screen.contains("/2026-09-11_release-plan.md"),
            "the file it wrote is named under it (wrapped when long):\n{screen}"
        );
        assert!(screen.contains("✓ meet's summary and action items: written"), "{screen}");
        assert!(screen.contains("A long one."), "** is dropped:\n{screen}");
        assert!(screen.contains("• ship it"), "bullets:\n{screen}");
        assert!(screen.contains("1. when?"), "numbered lists keep their number:\n{screen}");
        assert!(
            screen.contains("── 2026-09-11_release-plan.md · written by")
                && screen.contains("summarize-transcript.sh ──"),
            "the hook's file has its own heading (wrapped when long):\n{screen}"
        );
        assert!(screen.contains("The hook's own summary."), "{screen}");
        let footer = screen.lines().last().unwrap().to_string();
        assert!(!footer.contains("⚙ hook"), "{footer:?}");
        assert!(footer.contains("m summary"), "{footer:?}");

        // A failed hook shows its status and its last lines; a lost one says so.
        app.hooks.on_started(2, 2, "bash ~/bin/notes.sh".into());
        app.hooks.on_output("notes.sh: no such file");
        app.hooks.on_ended(2, 1, 0.0);
        app.hooks.on_started(3, 3, "sleep 9".into());
        app.hooks.on_engine_exited();
        let screen = render(&mut app, 120, 40);
        assert!(screen.contains("✗ hook 2/2 notes.sh: exited 1 after 0 s"), "{screen}");
        assert!(screen.contains("notes.sh: no such file"), "{screen}");
        assert!(screen.contains("? hook 3/3 sleep: the engine went away"), "{screen}");

        // A past session with only its short summary.
        let old = MeetingRow {
            summary: Some("Talked about greetings.".into()),
            ..meeting_at("old", 1_756_000_000)
        };
        app.sessions = vec![meeting_at("m", 1_756_100_000), old.clone()];
        app.enter_session(crate::app::SessionView {
            meeting: old,
            transcript: vec![],
            chunks: vec![],
        });
        app.show_summary();
        let screen = render(&mut app, 120, 30);
        assert!(screen.contains("Talked about greetings."), "{screen}");
        assert!(screen.contains("only its short summary"), "{screen}");
        assert!(!screen.contains("hook 1/1"), "the hooks belong to the live meeting:\n{screen}");
    }

    #[test]
    fn the_quit_confirm_warns_while_the_meeting_is_still_being_summarized() {
        let mut app = app_with(Overlay::ConfirmQuit {
            running: 0,
            live: false,
        });
        app.on_started(String::new(), vec![]);
        app.on_finished();
        app.wrap_up = WrapUp::Writing;
        app.hooks.on_started(1, 1, "/x/summarize-transcript.sh".into());
        let screen = render(&mut app, 110, 30);
        assert!(screen.contains("still being summarized"), "{screen}");
        assert!(screen.contains("summarize-transcript.sh is still running"), "{screen}");
        assert!(screen.contains("quitting cuts that"), "{screen}");
    }

    #[test]
    fn an_idle_launch_says_how_to_start_recording() {
        let mut app = App::new(
            "/r".into(),
            "main".into(),
            false,
            Limits::default(),
            "m".into(),
            Vec::new(),
        );
        app.sessions = vec![meeting_at("m", 1_756_100_000)];
        let screen = render(&mut app, 140, 30);
        assert!(screen.contains("○ idle · r to record"), "{screen}");
        assert!(screen.contains("press r to start recording"), "{screen}");
        assert!(screen.contains(" sessions  ○ idle"), "{screen}");
        let footer = screen.lines().last().unwrap().to_string();
        assert!(footer.starts_with(" r record  "), "{footer:?}");

        app.on_started(String::new(), vec![]);
        let screen = render(&mut app, 140, 30);
        assert!(!screen.contains("r to record"), "{screen}");
        assert!(screen.contains("● REC"), "{screen}");
        let footer = screen.lines().last().unwrap().to_string();
        assert!(footer.starts_with(" x stop recording"), "{footer:?}");

        // Stopping: no r until the recording has ended; then r again.
        app.rec = RecState::Finalizing;
        let screen = render(&mut app, 140, 30);
        let footer = screen.lines().last().unwrap().to_string();
        assert!(!footer.contains("r record"), "{footer:?}");
        app.on_finished();
        let screen = render(&mut app, 140, 30);
        let footer = screen.lines().last().unwrap().to_string();
        assert!(footer.starts_with(" r record  "), "{footer:?}");
    }

    #[test]
    fn muted_sources_are_marked_in_the_header_and_their_labels_take_clicks() {
        let mut app = App::new(
            "/r".into(),
            "main".into(),
            false,
            Limits::default(),
            "m".into(),
            Vec::new(),
        );
        app.on_started(String::new(), vec!["mic".into(), "system".into()]);
        let screen = render(&mut app, 200, 30);
        let header = screen.lines().next().unwrap().to_string();
        assert!(header.contains("● REC 00:00  mic 0 · system 0 "), "{header:?}");
        let footer = screen.lines().last().unwrap().to_string();
        assert!(footer.contains("M/N mute mic/system"), "{footer:?}");
        // Each label is a click target, on exactly the cells it was drawn on.
        assert_eq!(app.hit.sources.len(), 2, "{:?}", app.hit.sources);
        for (r, i) in &app.hit.sources {
            assert_eq!(r.y, 0);
            let label: String = header.chars().skip(r.x as usize).take(r.width as usize).collect();
            assert_eq!(label, format!("{} 0", app.sources[*i]), "{header:?}");
        }
        let (mic, _) = app.hit.sources[0];
        assert_eq!(app.hit.hit_at(mic.x, 0), crate::layout::HitTarget::Source(0));
        assert_eq!(app.hit.hit_at(mic.x + mic.width, 0), crate::layout::HitTarget::Outside);

        app.on_muted("mic".into(), true);
        let screen = render(&mut app, 200, 30);
        let header = screen.lines().next().unwrap().to_string();
        assert!(header.contains("⊘ mic 0 · system 0"), "{header:?}");
        assert!(screen.lines().last().unwrap().contains("microphone muted"), "{screen}");
        let (r, i) = app.hit.sources[0];
        assert_eq!(i, 0);
        assert_eq!(r.width, "⊘ mic 0".chars().count() as u16, "the marker is part of the target");

        app.on_muted("mic".into(), false);
        let screen = render(&mut app, 200, 30);
        assert!(!screen.lines().next().unwrap().contains('⊘'), "{screen}");
        assert!(screen.lines().last().unwrap().contains("microphone live again"), "{screen}");

        // A typed note is counted but is not a target; once the recording ended, none is.
        app.on_segment(crate::recorder::Segment {
            source: "typed".into(),
            text: "note".into(),
            start: 1.0,
            end: 1.0,
        });
        let screen = render(&mut app, 200, 30);
        assert!(screen.lines().next().unwrap().contains("mic 0 · system 0 · typed 1"), "{screen}");
        assert_eq!(app.hit.sources.len(), 2);
        app.on_muted("system".into(), true);
        app.on_finished();
        let screen = render(&mut app, 200, 30);
        assert!(app.hit.sources.is_empty());
        assert!(!screen.lines().next().unwrap().contains('⊘'), "the marker goes with the recording: {screen}");
    }

    #[test]
    fn the_footer_shows_what_was_spent_at_its_right_end_once_anything_was() {
        let mut app = App::new(
            "/r".into(),
            "main".into(),
            false,
            Limits::default(),
            "m".into(),
            Vec::new(),
        );
        let screen = render(&mut app, 140, 20);
        let footer = screen.lines().last().unwrap().to_string();
        assert!(footer.starts_with(" r record"), "{footer:?}");
        assert!(!footer.contains(" in "), "{footer:?}");

        app.spent = Usage {
            input: 1_000,
            cache_read: 40_000,
            cache_write: 4_200,
            output: 3_400,
            cost_usd: 0.314,
        };
        let screen = render(&mut app, 140, 20);
        let footer = screen.lines().last().unwrap().to_string();
        assert!(
            footer.trim_end().ends_with("in 45k · out 3.4k · $0.31"),
            "{footer:?}"
        );
        assert!(footer.starts_with(" r record"), "{footer:?}");

        // A running agent's turns count as they come, with no price yet.
        app.spent = Usage::default();
        app.agent_usage.insert(
            "a".into(),
            Usage {
                cache_read: 26_503,
                output: 1,
                ..Default::default()
            },
        );
        let screen = render(&mut app, 140, 20);
        let footer = screen.lines().last().unwrap().to_string();
        assert!(footer.trim_end().ends_with("in 26k · out 1"), "{footer:?}");
        assert_eq!(usage_text(&Usage { input: 999, ..Default::default() }), "in 999 · out 0");
    }

    #[test]
    fn a_confirm_box_lists_its_keys_inside_and_the_footer_stays_quiet() {
        let cases: Vec<(Overlay, &str, &[&str])> = vec![
            (
                Overlay::ConfirmQuit {
                    running: 0,
                    live: false,
                },
                "Quit?",
                &["y / Enter", "quit", "n / Esc", "stay"],
            ),
            (
                Overlay::ConfirmQuit {
                    running: 2,
                    live: true,
                },
                "Quit?",
                &[
                    "y / Enter",
                    "x ",
                    "stop the recording and stay",
                    "d ",
                    "discard the recording and quit",
                    "n / Esc",
                    "resumes",
                ],
            ),
            (
                Overlay::ConfirmDiscard,
                "Discard the recording?",
                &[
                    "y / Enter",
                    "discard the recording and quit",
                    "no summary or action items",
                    "n / Esc",
                    "keep it",
                ],
            ),
            (
                Overlay::ConfirmEnd,
                "Stop the recording?",
                &[
                    "y / Enter",
                    "stop the recording",
                    "n / Esc",
                    "keep recording",
                ],
            ),
        ];
        for (overlay, heading, expect) in cases {
            let mut app = app_with(overlay);
            let screen = render(&mut app, 100, 30);
            let rows: Vec<&str> = screen.lines().collect();
            let footer = rows.last().unwrap();
            assert!(screen.contains(heading), "{heading} box missing:\n{screen}");
            for e in expect {
                assert!(
                    rows[..rows.len() - 1].iter().any(|r| r.contains(e)),
                    "{e:?} missing from the {heading} box:\n{screen}"
                );
            }
            assert!(
                !footer.contains("y ") && !footer.contains("n stay"),
                "the footer still carries the confirm keys: {footer:?}"
            );
        }
    }

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
