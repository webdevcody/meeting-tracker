//! Word wrapping for the panes that must know their own line count (auto-scroll to the
//! newest transcript line, scroll bounds for a long prompt). ratatui's `Paragraph` wraps on
//! its own but does not report how many rows it used without an unstable feature.

/// Greedy word wrap to `width` columns; words longer than the width are split. A blank
/// input line stays a blank output line. Widths are counted in chars, which is exact for
/// the ASCII this tool mostly shows and close enough for the rest.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.trim_end();
        if raw.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut line = String::new();
        let mut line_len = 0;
        for word in raw.split_whitespace() {
            let wlen = word.chars().count();
            if line_len > 0 && line_len + 1 + wlen > width {
                lines.push(std::mem::take(&mut line));
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
                        lines.push(std::mem::take(&mut line));
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
        lines.push(line);
    }
    lines
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
}
