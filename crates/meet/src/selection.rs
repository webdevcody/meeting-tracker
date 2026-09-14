//! Selecting text with the mouse, the way nebula has it. meet owns the mouse — the clicks,
//! the wheel and the seams need it — so the terminal's own drag-to-select never reaches a
//! pane (⇧drag still does, borders, indentation and all). The selection is meet's instead:
//! a drag over the transcript highlights what it covers and copies it to the clipboard
//! when the button comes up, a double-click copies the word under the pointer, and the
//! highlight stays until the next click.
//!
//! A selection is held in the pane's rows rather than on the screen, so lines arriving at
//! the bottom, or the wheel, move the highlight along with its text; only a reflow (the
//! pane changed width) or another transcript on screen drops it. What it copies is the
//! text as it was before the wrap: a row broken between two words meets the next with a
//! space, a word split for being too long meets it with nothing, and the indentation under
//! a line's clock and source is never taken along.

use crate::wrap::Join;
use std::time::{Duration, Instant};

/// One row of a pane as drawn, to copy out of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextRow {
    /// The line of the pane's content the row belongs to (a transcript line).
    pub line: usize,
    /// The row's cells, a char a cell.
    pub text: String,
    /// How many cells at the start are layout, not text — a wrapped row's indentation.
    /// They are never copied, nor highlighted.
    pub lead: usize,
    /// How the row meets the next one.
    pub join: Join,
}

/// A mouse selection over a pane's rows: inclusive `(col, row)` endpoints, `row` counting
/// the pane's rows from its first (not the screen's), `col` a cell of its text area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextSelection {
    /// Where the button went down.
    pub anchor: (usize, usize),
    /// Where the pointer is, or was when the button came up.
    pub head: (usize, usize),
    /// The width the rows were wrapped to; at any other width they are other rows.
    pub width: usize,
    /// The button is still down.
    pub dragging: bool,
    /// A selection, not just a press: set once a drag leaves the cell it started on (and
    /// kept if it comes back), or at once for a double-clicked word, which may be one cell.
    pub active: bool,
}

impl TextSelection {
    /// A press on `cell`: armed, with nothing selected until the pointer moves off it.
    pub fn press(cell: (usize, usize), width: usize) -> TextSelection {
        TextSelection {
            anchor: cell,
            head: cell,
            width,
            dragging: true,
            active: false,
        }
    }

    /// The endpoints in reading order.
    pub fn bounds(&self) -> ((usize, usize), (usize, usize)) {
        let (a, h) = (self.anchor, self.head);
        if (a.1, a.0) <= (h.1, h.0) {
            (a, h)
        } else {
            (h, a)
        }
    }

    /// The cells of `row` the selection covers, first and last (`usize::MAX`: to the end of
    /// the row) — whole rows between the two endpoints, the way a terminal selects.
    pub fn span(&self, row: usize) -> Option<(usize, usize)> {
        let ((c0, r0), (c1, r1)) = self.bounds();
        if !self.active || row < r0 || row > r1 {
            return None;
        }
        let first = if row == r0 { c0 } else { 0 };
        let last = if row == r1 { c1 } else { usize::MAX };
        Some((first, last))
    }
}

/// The text `sel` covers in `rows`, put back together the way it was before the wrap.
pub fn selected_text(rows: &[TextRow], sel: &TextSelection) -> String {
    let mut out = String::new();
    let mut join = None;
    for (i, row) in rows.iter().enumerate() {
        let Some((first, last)) = sel.span(i) else {
            continue;
        };
        match join {
            Some(Join::Space) => out.push(' '),
            Some(Join::Newline) => out.push('\n'),
            Some(Join::Glued) | None => {}
        }
        let first = first.max(row.lead);
        if first <= last {
            let piece: String = row
                .text
                .chars()
                .skip(first)
                .take((last - first).saturating_add(1))
                .collect();
            out.push_str(piece.trim_end());
        }
        join = Some(row.join);
    }
    out.trim().to_string()
}

/// The run of non-blank cells around `col` on `row` — a double-clicked word, path or URL —
/// as its first and last cell; `None` on a blank cell or in a row's lead.
pub fn word_at(rows: &[TextRow], col: usize, row: usize) -> Option<(usize, usize)> {
    let row = rows.get(row)?;
    let chars: Vec<char> = row.text.chars().collect();
    let filled = |i: usize| i >= row.lead && chars.get(i).is_some_and(|c| !c.is_whitespace());
    if !filled(col) {
        return None;
    }
    let mut first = col;
    while first > 0 && filled(first - 1) {
        first -= 1;
    }
    let mut last = col;
    while filled(last + 1) {
        last += 1;
    }
    Some((first, last))
}

/// Two presses on the same cell within this make a double-click.
pub const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// Whether a press on `key` is the second of a double-click. The slot is spent either way:
/// after a double-click a third press starts over; a single press arms it for the next.
pub fn is_double_click<T: PartialEq>(slot: &mut Option<(Instant, T)>, key: T) -> bool {
    let now = Instant::now();
    let double = slot
        .take()
        .is_some_and(|(at, k)| k == key && now.duration_since(at) <= DOUBLE_CLICK);
    if !double {
        *slot = Some((now, key));
    }
    double
}

/// Whether meet runs over ssh, where pbcopy would fill the far machine's clipboard rather
/// than the one the user is sitting at.
pub fn over_ssh() -> bool {
    std::env::var_os("SSH_TTY").is_some() || std::env::var_os("SSH_CONNECTION").is_some()
}

/// Copy `text` to this machine's clipboard: pbcopy on macOS; wl-copy on Wayland, else
/// xclip or xsel. `false` when none of them took it.
pub fn to_system_clipboard(text: &str) -> bool {
    // The tests go through the copy paths; they must not fill the developer's clipboard.
    if cfg!(test) {
        return true;
    }
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    let via = |cmd: &str, args: &[&str]| -> bool {
        let Ok(mut child) = Command::new(cmd)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return false;
        };
        // The pipe closes at the end of this statement, so the tool sees the end of it.
        let wrote = child
            .stdin
            .take()
            .is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
        let exited = child.wait().is_ok_and(|status| status.success());
        wrote && exited
    };
    if cfg!(target_os = "macos") {
        return via("pbcopy", &[]);
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        return via("wl-copy", &[]);
    }
    via("xclip", &["-selection", "clipboard"]) || via("xsel", &["--clipboard", "--input"])
}

/// The escape asking the terminal to put `text` on its clipboard (OSC 52): the route over
/// ssh, and where no clipboard tool is installed. Terminals that do not implement it
/// (Terminal.app) drop it. BEL-terminated, the form every implementation takes.
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

/// Base64 (RFC 4648, padded) for OSC 52 — one caller does not earn a dependency.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = u32::from(chunk[0]) << 16
            | u32::from(chunk.get(1).copied().unwrap_or(0)) << 8
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(line: usize, text: &str, lead: usize, join: Join) -> TextRow {
        TextRow {
            line,
            text: text.into(),
            lead,
            join,
        }
    }

    /// Two transcript lines, each wrapped once: one between two words, one inside a URL.
    fn rows() -> Vec<TextRow> {
        vec![
            row(0, "00:01 mic    the quick", 0, Join::Space),
            row(0, "             brown fox", 13, Join::Newline),
            row(1, "00:09 system see https://exam", 0, Join::Glued),
            row(1, "             ple.com/a", 13, Join::Newline),
        ]
    }

    fn selection(anchor: (usize, usize), head: (usize, usize)) -> TextSelection {
        TextSelection {
            anchor,
            head,
            width: 40,
            dragging: false,
            active: true,
        }
    }

    #[test]
    fn a_copy_puts_back_what_the_wrap_took_out_and_leaves_the_indentation() {
        let all = selection((0, 0), (39, 3));
        assert_eq!(
            selected_text(&rows(), &all),
            "00:01 mic    the quick brown fox\n00:09 system see https://example.com/a"
        );
    }

    #[test]
    fn a_selection_dragged_backwards_reads_forwards() {
        // From the n of "brown" back to the q of "quick".
        let sel = selection((17, 1), (17, 0));
        assert_eq!(sel.bounds(), ((17, 0), (17, 1)));
        assert_eq!(selected_text(&rows(), &sel), "quick brown");
        assert_eq!(sel.span(0), Some((17, usize::MAX)));
        assert_eq!(sel.span(1), Some((0, 17)));
        assert_eq!(sel.span(2), None);
    }

    #[test]
    fn ending_in_the_next_rows_indentation_takes_nothing_from_it() {
        let sel = selection((13, 0), (4, 1));
        assert_eq!(selected_text(&rows(), &sel), "the quick");
    }

    #[test]
    fn a_press_that_did_not_move_selects_nothing() {
        let sel = TextSelection::press((5, 0), 40);
        assert_eq!(sel.span(0), None);
        assert_eq!(selected_text(&rows(), &sel), "");
    }

    #[test]
    fn a_word_is_the_run_of_filled_cells_under_the_pointer() {
        let rows = rows();
        assert_eq!(word_at(&rows, 20, 2), Some((17, 28)), "the URL's first row");
        assert_eq!(word_at(&rows, 0, 0), Some((0, 4)), "the clock");
        assert_eq!(word_at(&rows, 16, 2), None, "a space");
        assert_eq!(word_at(&rows, 5, 1), None, "the indentation");
        assert_eq!(word_at(&rows, 99, 0), None, "past the end of the row");
        assert_eq!(word_at(&rows, 0, 9), None, "past the last row");
    }

    #[test]
    fn a_double_click_is_two_presses_on_one_cell() {
        let mut slot = None;
        assert!(!is_double_click(&mut slot, (1, 2)));
        assert!(is_double_click(&mut slot, (1, 2)));
        assert!(!is_double_click(&mut slot, (1, 2)), "a third press starts over");
        assert!(!is_double_click(&mut slot, (3, 2)), "another cell");
    }

    #[test]
    fn base64_matches_the_rfc_vectors() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(plain.as_bytes()), encoded, "{plain:?}");
        }
        assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x07");
    }
}
