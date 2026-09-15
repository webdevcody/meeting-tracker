//! The adjustable layout, done the way nebula does it: the seam between the transcript and
//! the right pane can be dragged with the mouse, its share is kept in `layout.json` in the
//! data directory, and what was drawn where on the last frame is kept for the clicks — a
//! session tab, a pane — to land on the right thing.
//!
//! The pure parts live here: the splits, the hit-testing, the drag arithmetic. `ui`
//! fills the hit map while drawing; the event loop asks it what the mouse is on.

use crate::app::RightPane;
use anyhow::{Context, Result};
use ratatui::layout::{Position, Rect};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const FILE: &str = "layout.json";
/// A column keeps at least this many cells, so neither side can be dragged shut.
pub const MIN_COL_W: u16 = 24;

/// Where the seam sits, as a share of the width it splits — a share survives a terminal
/// resize where a cell count would not.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LayoutPrefs {
    /// The left column's share of the body width.
    pub left: f64,
}

impl Default for LayoutPrefs {
    fn default() -> Self {
        Self {
            left: 0.46,
        }
    }
}

impl LayoutPrefs {
    pub fn path() -> PathBuf {
        crate::paths::data_dir().join(FILE)
    }

    /// Read the file; a missing file is the defaults, a broken one is an error naming it.
    pub fn load_from(path: &Path) -> Result<LayoutPrefs> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(LayoutPrefs::default()),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        let mut prefs: LayoutPrefs = serde_json::from_str(&text)
            .with_context(|| format!("could not parse {}", path.display()))?;
        prefs.sanitize();
        Ok(prefs)
    }

    pub fn load() -> Result<LayoutPrefs> {
        Self::load_from(&Self::path())
    }

    /// Write the file whole, through a temp file so a crash mid-write never leaves a
    /// half file behind.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let mut text = serde_json::to_string_pretty(self)?;
        text.push('\n');
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("move {} to {}", tmp.display(), path.display()))?;
        Ok(())
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::path())
    }

    /// A hand-edited or damaged file never yields a share outside 0..1 (or NaN).
    fn sanitize(&mut self) {
        if !(self.left.is_finite() && (0.0..=1.0).contains(&self.left)) {
            self.left = LayoutPrefs::default().left;
        }
    }
}

// ----- splits -----

/// `total` split at `share`, each side keeping `min` when `total` allows; halved when it
/// cannot hold two minimums.
fn split_at(total: u16, share: f64, min: u16) -> u16 {
    let want = (f64::from(total) * share).round() as u16;
    if total < 2 * min {
        total / 2
    } else {
        want.clamp(min, total - min)
    }
}

/// The body split into the left and right columns.
pub fn split_cols(body: Rect, left_share: f64) -> (Rect, Rect) {
    let left_w = split_at(body.width, left_share, MIN_COL_W);
    (
        Rect {
            width: left_w,
            ..body
        },
        Rect {
            x: body.x + left_w,
            width: body.width - left_w,
            ..body
        },
    )
}

/// The share that puts a seam at `pos` inside a span starting at `start` and `len` long,
/// each side keeping `min` when the span allows: the inverse of the splits, so a dragged
/// seam lands under the pointer, and what is kept is what is shown.
pub fn share_at(start: u16, len: u16, pos: i32, min: u16) -> f64 {
    if len == 0 {
        return 0.5;
    }
    let (lo, hi) = if len >= 2 * min {
        (i32::from(min), i32::from(len - min))
    } else {
        (0, i32::from(len))
    };
    let offset = (pos - i32::from(start)).clamp(lo, hi);
    f64::from(offset) / f64::from(len)
}

// ----- the seams -----

/// A draggable seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Splitter {
    /// Between the left and the right column.
    Columns,
}

/// An in-progress drag of a seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitterDrag {
    pub which: Splitter,
    /// `seam - pointer` at mouse-down, so the seam tracks the pointer without jumping a
    /// cell depending on which of the two border cells was grabbed.
    pub grab_offset: i32,
}

/// The pointer shape the outer terminal is asked to show (xterm OSC 22, CSS cursor
/// names). Terminals that do not know the escape drop it, so asking is always safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PointerShape {
    #[default]
    Default,
    ColResize,
}

impl PointerShape {
    pub fn osc_name(self) -> &'static str {
        match self {
            PointerShape::Default => "default",
            PointerShape::ColResize => "col-resize",
        }
    }
}

// ----- what was drawn where -----

/// Where the last frame put everything, for the mouse. `ui::draw` fills it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HitMap {
    /// The body: everything between the session bar and the footer.
    pub body: Rect,
    pub bar: Rect,
    /// Each session tab's cells on the bar, with its index in `app.sessions`.
    pub bar_tabs: Vec<(Rect, usize)>,
    pub transcript: Rect,
    /// The transcript's text, inside its border: a drag there selects, and copies.
    pub transcript_text: Rect,
    /// The whole right column.
    pub right: Rect,
    /// The right pane's tabs on its top border.
    pub right_tabs: Vec<(Rect, RightPane)>,
    /// The open overlay's box: a click outside it closes the overlay.
    pub overlay: Option<Rect>,
    /// Rows inside the overlay that a click picks (the session list, the settings).
    pub overlay_rows: Vec<(Rect, usize)>,
    /// The header's `mic` / `system` labels while recording, with the index of each in
    /// `app.sources`: a click mutes or unmutes that source.
    pub sources: Vec<(Rect, usize)>,
}

/// What the mouse is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitTarget {
    Splitter(Splitter),
    /// The session bar: a tab (its index in `app.sessions`), or the bar's own room.
    Bar(Option<usize>),
    Transcript,
    /// A tab on the right pane's top border.
    RightTab(RightPane),
    /// The rest of the right column: the meeting summary, Claude Code.
    Right,
    /// Inside the open overlay: a row it lists, or its own room.
    Overlay(Option<usize>),
    /// A source's label in the header (its index in `app.sources`): mute / unmute it.
    Source(usize),
    /// Outside the open overlay, or on nothing at all.
    Outside,
}

fn row_at(rows: &[(Rect, usize)], p: Position) -> Option<usize> {
    rows.iter().find(|(r, _)| r.contains(p)).map(|(_, i)| *i)
}

impl HitMap {
    /// Whether `(x, y)` is on the seam's two touching border cells: the left pane's right
    /// border, or the right pane's left border.
    pub fn splitter_at(&self, x: u16, y: u16) -> Option<Splitter> {
        let body = self.body;
        if body.width == 0 || body.height == 0 {
            return None;
        }
        let in_rows = y >= body.y && y < body.bottom();
        // The columns' seam: the left pane's right border and the right pane's left border.
        let seam = self.right.x;
        if in_rows && seam > body.x && x.saturating_add(1) >= seam && x <= seam {
            return Some(Splitter::Columns);
        }
        None
    }

    /// Where the seam `which` sits now: the screen column of the cell where the second
    /// pane starts.
    pub fn seam_pos(&self, which: Splitter) -> i32 {
        match which {
            Splitter::Columns => i32::from(self.right.x),
        }
    }

    pub fn hit_at(&self, x: u16, y: u16) -> HitTarget {
        let p = Position::new(x, y);
        if let Some(overlay) = self.overlay {
            return if overlay.contains(p) {
                HitTarget::Overlay(row_at(&self.overlay_rows, p))
            } else {
                HitTarget::Outside
            };
        }
        if let Some(s) = self.splitter_at(x, y) {
            return HitTarget::Splitter(s);
        }
        if let Some(i) = row_at(&self.sources, p) {
            return HitTarget::Source(i);
        }
        if self.bar.height > 0 && self.bar.contains(p) {
            return HitTarget::Bar(row_at(&self.bar_tabs, p));
        }
        if let Some((_, pane)) = self.right_tabs.iter().find(|(r, _)| r.contains(p)) {
            return HitTarget::RightTab(*pane);
        }
        if self.transcript.contains(p) {
            return HitTarget::Transcript;
        }
        if self.right.contains(p) {
            return HitTarget::Right;
        }
        HitTarget::Outside
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_columns_split_at_the_share_and_keep_their_minimums() {
        let body = Rect::new(0, 2, 100, 30);
        let (l, r) = split_cols(body, 0.46);
        assert_eq!((l.x, l.width), (0, 46));
        assert_eq!((r.x, r.width), (46, 54));
        assert_eq!(l.y, 2);
        let (l, r) = split_cols(body, 0.01);
        assert_eq!(l.width, MIN_COL_W, "the left column never closes");
        assert_eq!(r.width, 100 - MIN_COL_W);
        let (l, r) = split_cols(body, 0.99);
        assert_eq!(r.width, MIN_COL_W, "nor the right");
        assert_eq!(l.width, 100 - MIN_COL_W);
        let (l, r) = split_cols(Rect::new(0, 0, 30, 10), 0.9);
        assert_eq!(
            (l.width, r.width),
            (15, 15),
            "too narrow for two minimums: halved"
        );
    }

    #[test]
    fn a_dragged_seam_lands_under_the_pointer() {
        let body = Rect::new(0, 2, 190, 46);
        for x in (MIN_COL_W..190 - MIN_COL_W).step_by(7) {
            let share = share_at(body.x, body.width, i32::from(x), MIN_COL_W);
            let (_, r) = split_cols(body, share);
            assert_eq!(r.x, x, "the seam dragged to column {x} sits there");
        }
        assert_eq!(
            share_at(10, 0, 5, 0),
            0.5,
            "a zero span cannot place a seam"
        );
        assert_eq!(share_at(10, 20, -4, 0), 0.0, "past the start is the start");
        assert_eq!(share_at(10, 20, 99, 0), 1.0, "past the end is the end");
        assert_eq!(
            share_at(0, 100, 5, 24),
            0.24,
            "a drag past the minimum stops there"
        );
        assert_eq!(share_at(0, 100, 99, 24), 0.76);
        assert_eq!(
            share_at(0, 30, 29, 24),
            29.0 / 30.0,
            "too narrow for minimums: free"
        );
    }

    #[test]
    fn the_file_round_trips_and_a_bad_share_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deep").join(FILE);
        assert_eq!(
            LayoutPrefs::load_from(&path).unwrap(),
            LayoutPrefs::default()
        );
        let prefs = LayoutPrefs { left: 0.3 };
        prefs.save_to(&path).unwrap();
        assert_eq!(LayoutPrefs::load_from(&path).unwrap(), prefs);
        assert!(!dir.path().join("deep").join("layout.json.tmp").exists());
        // An older file still carries the summaries' and the action-item list's shares: they
        // are ignored.
        std::fs::write(&path, r#"{"left": 7.0, "transcript": 0.5, "items": 0.4}"#).unwrap();
        let loaded = LayoutPrefs::load_from(&path).unwrap();
        assert_eq!(loaded, LayoutPrefs::default());
        std::fs::write(&path, "{").unwrap();
        assert!(LayoutPrefs::load_from(&path).is_err());
    }

    fn map() -> HitMap {
        // A 100×40 screen: header row 0, bar row 1, body rows 2..38, footer row 39.
        let body = Rect::new(0, 2, 100, 37);
        let (transcript, right) = split_cols(body, 0.46);
        HitMap {
            body,
            bar: Rect::new(0, 1, 100, 1),
            bar_tabs: vec![(Rect::new(10, 1, 8, 1), 0), (Rect::new(19, 1, 13, 1), 1)],
            transcript,
            transcript_text: Rect::default(),
            right,
            right_tabs: vec![(Rect::new(80, 2, 7, 1), RightPane::Summary)],
            overlay: None,
            overlay_rows: Vec::new(),
            sources: vec![(Rect::new(80, 0, 6, 1), 0), (Rect::new(89, 0, 9, 1), 1)],
        }
    }

    #[test]
    fn the_seams_are_the_two_touching_border_cells() {
        let m = map();
        assert_eq!(m.right.x, 46);
        assert_eq!(
            m.splitter_at(45, 10),
            Some(Splitter::Columns),
            "the left pane's border"
        );
        assert_eq!(
            m.splitter_at(46, 10),
            Some(Splitter::Columns),
            "the right pane's border"
        );
        assert_eq!(m.splitter_at(44, 10), None);
        assert_eq!(m.splitter_at(47, 10), None);
        assert_eq!(m.splitter_at(46, 1), None, "not on the bar");
        assert_eq!(m.splitter_at(46, 39), None, "not on the footer");
        assert_eq!(
            m.splitter_at(10, m.transcript.bottom() - 1),
            None,
            "the transcript's bottom border is no seam"
        );
        assert_eq!(m.seam_pos(Splitter::Columns), 46);
    }

    #[test]
    fn clicks_land_on_tabs_rows_and_panes() {
        let m = map();
        assert_eq!(m.hit_at(12, 1), HitTarget::Bar(Some(0)));
        assert_eq!(m.hit_at(25, 1), HitTarget::Bar(Some(1)));
        assert_eq!(m.hit_at(3, 1), HitTarget::Bar(None), "the bar's label");
        assert_eq!(m.hit_at(82, 2), HitTarget::RightTab(RightPane::Summary));
        assert_eq!(m.hit_at(10, 5), HitTarget::Transcript);
        assert_eq!(m.hit_at(10, 30), HitTarget::Transcript, "down the whole left column");
        assert_eq!(m.hit_at(60, 20), HitTarget::Right);
        assert_eq!(m.hit_at(0, 0), HitTarget::Outside, "the header");
        assert_eq!(
            m.hit_at(82, 0),
            HitTarget::Source(0),
            "the header's mic label"
        );
        assert_eq!(
            m.hit_at(97, 0),
            HitTarget::Source(1),
            "the header's system label"
        );
        assert_eq!(m.hit_at(87, 0), HitTarget::Outside, "the gap between them");
        assert_eq!(m.hit_at(50, 39), HitTarget::Outside, "the footer");
    }

    #[test]
    fn an_open_overlay_takes_every_click() {
        let mut m = map();
        m.overlay = Some(Rect::new(20, 10, 60, 20));
        m.overlay_rows = vec![(Rect::new(21, 11, 58, 1), 3)];
        assert_eq!(m.hit_at(30, 11), HitTarget::Overlay(Some(3)));
        assert_eq!(m.hit_at(30, 15), HitTarget::Overlay(None));
        assert_eq!(m.hit_at(5, 5), HitTarget::Outside);
        assert_eq!(m.hit_at(46, 5), HitTarget::Outside, "not even a seam");
        assert_eq!(m.hit_at(82, 0), HitTarget::Outside, "nor a source label");
    }

    #[test]
    fn pointer_shapes_have_their_css_names() {
        assert_eq!(PointerShape::Default.osc_name(), "default");
        assert_eq!(PointerShape::ColResize.osc_name(), "col-resize");
    }
}
