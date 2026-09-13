//! Turns the trickle of final transcript segments into chunks worth summarizing. A chunk
//! closes when it is long enough and the speaker has paused, or when it hits the size cap
//! mid-flow; a meeting's tail is flushed on stop. Pure state machine — the event loop
//! feeds it segments and ticks it with the recording clock.

use crate::recorder::Segment;

#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub start: f64,
    pub end: f64,
    pub segments: Vec<Segment>,
}

impl Chunk {
    /// `[mm:ss] source: text` per segment — what the summarizer reads.
    pub fn text(&self) -> String {
        self.segments
            .iter()
            .map(|s| format!("[{}] {}: {}", clock(s.start), s.source, s.text))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn words(&self) -> usize {
        self.segments
            .iter()
            .map(|s| s.text.split_whitespace().count())
            .sum()
    }
}

pub fn clock(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

#[derive(Debug, Clone)]
pub struct Limits {
    /// A pause only closes a chunk that has at least this many words.
    pub min_words: usize,
    /// Close at once past this many words, pause or not.
    pub max_words: usize,
    /// Close at once once the chunk spans this many seconds of recording.
    pub max_secs: f64,
    /// Silence after the last segment that counts as "they stopped talking".
    pub quiet_secs: f64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            min_words: 40,
            max_words: 180,
            max_secs: 90.0,
            quiet_secs: 8.0,
        }
    }
}

#[derive(Debug)]
pub struct Chunker {
    limits: Limits,
    pending: Vec<Segment>,
    /// Recording time when the newest pending segment ended.
    last_end: f64,
}

impl Chunker {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            pending: Vec::new(),
            last_end: 0.0,
        }
    }

    /// Start over for the next recording: whatever is pending goes (a recording that
    /// ended was flushed already) and the clock is back at zero.
    pub fn reset(&mut self) {
        self.pending.clear();
        self.last_end = 0.0;
    }

    pub fn pending_words(&self) -> usize {
        self.pending
            .iter()
            .map(|s| s.text.split_whitespace().count())
            .sum()
    }

    /// Add a final segment; returns a chunk when this segment fills one.
    pub fn push(&mut self, seg: Segment) -> Option<Chunk> {
        self.last_end = self.last_end.max(seg.end);
        self.pending.push(seg);
        let span = self.last_end - self.pending[0].start;
        if self.pending_words() >= self.limits.max_words || span >= self.limits.max_secs {
            return self.flush();
        }
        None
    }

    /// The recording clock moved to `now`; closes a chunk the speaker has walked away from.
    pub fn tick(&mut self, now: f64) -> Option<Chunk> {
        if self.pending.is_empty() {
            return None;
        }
        if now - self.last_end >= self.limits.quiet_secs
            && self.pending_words() >= self.limits.min_words
        {
            return self.flush();
        }
        None
    }

    /// Everything pending, whatever its size (meeting over, or the user asked).
    pub fn flush(&mut self) -> Option<Chunk> {
        if self.pending.is_empty() {
            return None;
        }
        let segments = std::mem::take(&mut self.pending);
        let start = segments.first().map(|s| s.start).unwrap_or(0.0);
        let end = segments.iter().map(|s| s.end).fold(start, f64::max);
        Some(Chunk {
            start,
            end,
            segments,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(start: f64, end: f64, words: usize) -> Segment {
        Segment {
            source: "mic".into(),
            text: vec!["word"; words].join(" "),
            start,
            end,
        }
    }

    fn limits() -> Limits {
        Limits {
            min_words: 5,
            max_words: 20,
            max_secs: 60.0,
            quiet_secs: 8.0,
        }
    }

    #[test]
    fn a_pause_closes_a_chunk_only_once_it_has_enough_words() {
        let mut c = Chunker::new(limits());
        assert!(c.push(seg(0.0, 2.0, 3)).is_none());
        assert!(
            c.tick(20.0).is_none(),
            "3 words is too little to bother the summarizer"
        );
        assert!(c.push(seg(20.0, 22.0, 3)).is_none());
        assert!(c.tick(25.0).is_none(), "not quiet yet");
        let chunk = c.tick(31.0).expect("quiet and big enough");
        assert_eq!(chunk.start, 0.0);
        assert_eq!(chunk.end, 22.0);
        assert_eq!(chunk.words(), 6);
        assert_eq!(c.pending_words(), 0);
    }

    #[test]
    fn size_and_span_caps_close_mid_flow() {
        let mut c = Chunker::new(limits());
        assert!(c.push(seg(0.0, 1.0, 12)).is_none());
        let chunk = c.push(seg(1.0, 2.0, 9)).expect("21 words > max 20");
        assert_eq!(chunk.segments.len(), 2);

        let mut c = Chunker::new(limits());
        assert!(c.push(seg(0.0, 1.0, 1)).is_none());
        assert!(c.push(seg(70.0, 71.0, 1)).is_some(), "71 s span > max 60");
    }

    #[test]
    fn flush_takes_the_tail_and_text_is_timestamped() {
        let mut c = Chunker::new(limits());
        assert!(c.flush().is_none());
        c.push(Segment {
            source: "system".into(),
            text: "let's ship it".into(),
            start: 65.0,
            end: 66.0,
        });
        let chunk = c.flush().unwrap();
        assert_eq!(chunk.text(), "[01:05] system: let's ship it");
        assert_eq!(clock(3661.0), "1:01:01");
    }
}
