//! Word wrapping for the panes that must know their own line count (auto-scroll to the
//! newest transcript line, scroll bounds for a long prompt). ratatui's `Paragraph` wraps on
//! its own but does not report how many rows it used without an unstable feature.

/// How a wrapped row meets the row after it — what the wrap took out between them, for a
/// copy to put back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Join {
    /// A space: the row broke between two words.
    Space,
    /// Nothing: a word too long for the width goes on in the next row.
    Glued,
    /// A line break: the row ends a line of the text (the last row ends the text).
    Newline,
}

/// Greedy word wrap to `width` columns; words longer than the width are split. A blank
/// input line stays a blank output line. Widths are counted in chars, which is exact for
/// the ASCII this tool mostly shows and close enough for the rest.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    wrap_rows(text, width).into_iter().map(|(row, _)| row).collect()
}

/// `wrap`, each row with how it meets the next.
pub fn wrap_rows(text: &str, width: usize) -> Vec<(String, Join)> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.trim_end();
        if raw.is_empty() {
            rows.push((String::new(), Join::Newline));
            continue;
        }
        let mut line = String::new();
        let mut line_len = 0;
        for word in raw.split_whitespace() {
            let wlen = word.chars().count();
            if line_len > 0 && line_len + 1 + wlen > width {
                rows.push((std::mem::take(&mut line), Join::Space));
                line_len = 0;
            }
            if wlen > width {
                // Split a monster token (a URL, a path) across rows.
                let mut chunk = String::new();
                let mut chunk_len = 0;
                for ch in word.chars() {
                    if line_len + chunk_len == width {
                        if line_len > 0 {
                            line.push(' ');
                        }
                        line.push_str(&chunk);
                        rows.push((std::mem::take(&mut line), Join::Glued));
                        line_len = 0;
                        chunk.clear();
                        chunk_len = 0;
                    }
                    chunk.push(ch);
                    chunk_len += 1;
                }
                if line_len > 0 {
                    line.push(' ');
                    line_len += 1;
                }
                line.push_str(&chunk);
                line_len += chunk_len;
                continue;
            }
            if line_len > 0 {
                line.push(' ');
                line_len += 1;
            }
            line.push_str(word);
            line_len += wlen;
        }
        rows.push((line, Join::Newline));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_on_words_and_keeps_blank_lines() {
        assert_eq!(
            wrap("the quick brown fox", 9),
            vec!["the quick", "brown fox"]
        );
        assert_eq!(wrap("a\n\nb", 10), vec!["a", "", "b"]);
        assert_eq!(wrap("", 10), vec![""]);
    }

    #[test]
    fn splits_tokens_wider_than_the_pane() {
        let lines = wrap("see https://example.com/a/very/long/path/that/goes/on", 12);
        assert!(lines.iter().all(|l| l.chars().count() <= 12), "{lines:?}");
        assert_eq!(
            lines.concat().replace(' ', ""),
            "seehttps://example.com/a/very/long/path/that/goes/on"
        );
    }

    #[test]
    fn rows_say_how_they_meet_the_next() {
        let row = |s: &str, join| (s.to_string(), join);
        assert_eq!(
            wrap_rows("the quick brown fox\nabcdefghij\n\nend", 9),
            vec![
                row("the quick", Join::Space),
                row("brown fox", Join::Newline),
                row("abcdefghi", Join::Glued),
                row("j", Join::Newline),
                row("", Join::Newline),
                row("end", Join::Newline),
            ]
        );
    }
}
