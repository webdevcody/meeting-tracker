//! The transcript source: the Swift recording engine (`meet-rec record --json`) as a child
//! process whose NDJSON events become [`RecorderEvent`]s, or a saved `transcript.json`
//! replayed on its own clock for demos and tests. Either way the TUI drives it through a
//! [`RecorderHandle`]: pause/resume, mute/unmute a source, stop, and discard.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Segment {
    /// `mic`, `system`, or `typed` for a note entered in the TUI.
    #[serde(alias = "speaker")]
    pub source: String,
    pub text: String,
    pub start: f64,
    pub end: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RecorderEvent {
    Config {
        path: String,
        output_dir: String,
    },
    Started {
        meeting_dir: String,
        sources: Vec<String>,
    },
    Status(String),
    Segment(Segment),
    Paused(bool),
    /// A source's mute was flipped: its audio is silence (in the file and to the
    /// transcriber) while `muted`.
    Muted {
        source: String,
        muted: bool,
    },
    /// Replay only: where the replay clock is, so the TUI's clock and pause detection
    /// follow the transcript's timestamps rather than wall time.
    Clock(f64),
    /// A human-readable line the engine wrote to stderr.
    Log(String),
    Finished {
        meeting_dir: String,
        duration_secs: f64,
        segment_count: u64,
    },
    /// After `finished`: how many onDone hooks the engine is about to run (0: none
    /// configured, or `skipped` by `--no-hooks`). Engines from before this event send
    /// nothing, so its absence means "unknown", not "none".
    Hooks {
        count: usize,
        skipped: bool,
    },
    /// An onDone hook (1-based `index` of `count`) started; its output follows as `Log`
    /// lines until `HookEnded`.
    HookStarted {
        index: usize,
        count: usize,
        command: String,
    },
    HookEnded {
        index: usize,
        status: i32,
        secs: f64,
    },
    /// Instead of `finished`, after a discard: the engine stopped, deleted the meeting
    /// directory (audio, transcript) and ran no hook. `Exited` follows.
    Discarded {
        meeting_dir: String,
    },
    /// The engine process is gone; `code` is its exit status.
    Exited {
        code: Option<i32>,
    },
}

/// One NDJSON line from the engine.
pub fn parse_event(line: &str) -> Option<RecorderEvent> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    Some(match v.get("event")?.as_str()? {
        "config" => RecorderEvent::Config {
            path: s("path"),
            output_dir: s("outputDir"),
        },
        "started" => RecorderEvent::Started {
            meeting_dir: s("meetingDir"),
            sources: v
                .get("sources")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
        },
        "status" => RecorderEvent::Status(s("text")),
        "segment" => RecorderEvent::Segment(serde_json::from_value(v.clone()).ok()?),
        "paused" => {
            RecorderEvent::Paused(v.get("paused").and_then(Value::as_bool).unwrap_or(false))
        }
        "muted" => RecorderEvent::Muted {
            source: s("source"),
            muted: v.get("muted").and_then(Value::as_bool).unwrap_or(false),
        },
        "finished" => RecorderEvent::Finished {
            meeting_dir: s("meetingDir"),
            duration_secs: v.get("durationSecs").and_then(Value::as_f64).unwrap_or(0.0),
            segment_count: v.get("segmentCount").and_then(Value::as_u64).unwrap_or(0),
        },
        "discarded" => RecorderEvent::Discarded {
            meeting_dir: s("meetingDir"),
        },
        "hooks" => RecorderEvent::Hooks {
            count: v.get("count").and_then(Value::as_u64).unwrap_or(0) as usize,
            skipped: v.get("skipped").and_then(Value::as_bool).unwrap_or(false),
        },
        "hook" => {
            let index = v.get("index").and_then(Value::as_u64).unwrap_or(1) as usize;
            match v.get("phase").and_then(Value::as_str)? {
                "start" => RecorderEvent::HookStarted {
                    index,
                    count: v.get("count").and_then(Value::as_u64).unwrap_or(1) as usize,
                    command: s("command"),
                },
                "end" => RecorderEvent::HookEnded {
                    index,
                    status: v.get("status").and_then(Value::as_i64).unwrap_or(0) as i32,
                    secs: v.get("secs").and_then(Value::as_f64).unwrap_or(0.0),
                },
                _ => return None,
            }
        }
        _ => return None,
    })
}

enum Ctrl {
    TogglePause,
    /// Flip the mute of one source (`mic` or `system`).
    ToggleMute(String),
    Stop,
    /// Stop and keep nothing: the engine deletes the meeting directory and runs no hook.
    Discard,
}

/// Pause/resume, mute, stop and discard, for either kind of source.
#[derive(Clone)]
pub struct RecorderHandle {
    ctrl: mpsc::UnboundedSender<Ctrl>,
}

impl RecorderHandle {
    pub fn toggle_pause(&self) {
        let _ = self.ctrl.send(Ctrl::TogglePause);
    }
    /// Mute `source` (`mic` or `system`) if it is live, unmute it if it is muted. The
    /// recorder answers with a `Muted` event.
    pub fn toggle_mute(&self, source: &str) {
        let _ = self.ctrl.send(Ctrl::ToggleMute(source.to_string()));
    }
    pub fn stop(&self) {
        let _ = self.ctrl.send(Ctrl::Stop);
    }
    pub fn discard(&self) {
        let _ = self.ctrl.send(Ctrl::Discard);
    }
}

/// Where the engine binary is: `--recorder`, `$MEET_RECORDER`, `meet-rec` beside this
/// binary, the SwiftPM build of a source checkout this binary was built in, or PATH.
pub fn locate_engine(explicit: Option<&str>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
        bail!("recorder not found at {}", p.display());
    }
    if let Ok(p) = std::env::var("MEET_RECORDER") {
        if !p.trim().is_empty() {
            let p = PathBuf::from(p);
            if p.is_file() {
                return Ok(p);
            }
            bail!(
                "$MEET_RECORDER points at {}, which does not exist",
                p.display()
            );
        }
    }
    if let Ok(exe) = std::env::current_exe().and_then(std::fs::canonicalize) {
        let dir = exe.parent().map(Path::to_path_buf).unwrap_or_default();
        let sibling = dir.join("meet-rec");
        if sibling.is_file() {
            return Ok(sibling);
        }
        // <checkout>/target/<profile>/meet → <checkout>/.build/release/meet-rec
        if let Some(checkout) = dir.parent().and_then(Path::parent) {
            let built = checkout.join(".build/release/meet-rec");
            if built.is_file() {
                return Ok(built);
            }
        }
    }
    if let Some(found) = which("meet-rec") {
        return Ok(found);
    }
    bail!("meet-rec (the recording engine) was not found — run `make install` in the meet checkout, or pass --recorder <path>")
}

pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// Start the engine in `cwd` (so it finds that project's `meet.json`) and stream its events.
/// `args` are the record flags (`--no-mic`, `--out-dir …`); `--json` is added here.
pub fn spawn_engine(
    engine: &Path,
    args: &[String],
    cwd: &Path,
    tx: mpsc::UnboundedSender<RecorderEvent>,
) -> Result<RecorderHandle> {
    let mut child = tokio::process::Command::new(engine)
        .arg("record")
        .arg("--json")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("start {}", engine.display()))?;
    let mut stdin = child.stdin.take().context("engine stdin")?;
    let stdout = child.stdout.take().context("engine stdout")?;
    let stderr = child.stderr.take().context("engine stderr")?;

    let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel::<Ctrl>();
    // Keys the engine understands on stdin: space pauses, m/n mute the mic/system audio,
    // q stops, D discards.
    tokio::spawn(async move {
        while let Some(c) = ctrl_rx.recv().await {
            let byte: &[u8] = match c {
                Ctrl::TogglePause => b" ",
                Ctrl::ToggleMute(s) if s == "mic" => b"m",
                Ctrl::ToggleMute(s) if s == "system" => b"n",
                Ctrl::ToggleMute(_) => continue,
                Ctrl::Stop => b"q",
                Ctrl::Discard => b"D",
            };
            if stdin.write_all(byte).await.is_err() || stdin.flush().await.is_err() {
                break;
            }
        }
    });
    let tx_out = tx.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(ev) = parse_event(&line) {
                if tx_out.send(ev).is_err() {
                    break;
                }
            }
        }
    });
    let tx_err = tx.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = strip_ansi(&line).trim().to_string();
            if !line.is_empty() && tx_err.send(RecorderEvent::Log(line)).is_err() {
                break;
            }
        }
    });
    tokio::spawn(async move {
        let code = child.wait().await.ok().and_then(|s| s.code());
        let _ = tx.send(RecorderEvent::Exited { code });
    });
    Ok(RecorderHandle { ctrl: ctrl_tx })
}

/// The engine's stderr lines are written for a terminal: its warnings start with a
/// carriage return and a clear-line sequence (`\r\x1b[2K`), and a hook may print colour.
/// Drop the CSI escapes (`ESC [ … final`) and carriage returns so the line reads as text
/// — and so a `⚠` line is seen to start with `⚠`.
pub fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => {
                if chars.peek() == Some(&'[') {
                    chars.next();
                    // Parameter and intermediate bytes, then one final byte 0x40–0x7e.
                    for c in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
            }
            '\r' => {}
            _ => out.push(c),
        }
    }
    out
}

#[derive(Deserialize)]
struct TranscriptFile {
    #[serde(default)]
    segments: Vec<Segment>,
}

/// Read a saved `transcript.json` (both the current `source` and the older `speaker` key).
pub fn load_transcript(path: &Path) -> Result<Vec<Segment>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let file: TranscriptFile =
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    Ok(file.segments)
}

/// Replay saved segments as if they were being spoken now: each one lands when the replay
/// clock reaches its `end`, `speed` times faster than real time. Pause holds the clock; a
/// muted source's segments are dropped while it is muted, as the engine would hear nothing.
pub fn spawn_replay(
    segments: Vec<Segment>,
    speed: f64,
    tx: mpsc::UnboundedSender<RecorderEvent>,
) -> RecorderHandle {
    let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel::<Ctrl>();
    let speed = if speed > 0.0 { speed } else { 1.0 };
    tokio::spawn(async move {
        // The transcript's own sources, in order of first appearance, as the engine names
        // what it records — so the header lists them and M/N have something to mute.
        let mut sources: Vec<String> = Vec::new();
        for s in &segments {
            if !sources.contains(&s.source) {
                sources.push(s.source.clone());
            }
        }
        let _ = tx.send(RecorderEvent::Started {
            meeting_dir: String::new(),
            sources,
        });
        let mut segments = segments.into_iter().peekable();
        let mut clock = 0.0_f64;
        let mut paused = false;
        let mut muted: Vec<String> = Vec::new();
        let tick = std::time::Duration::from_millis(100);
        let mut count = 0_u64;
        let mut discard = false;
        loop {
            tokio::select! {
                ctrl = ctrl_rx.recv() => match ctrl {
                    Some(Ctrl::TogglePause) => {
                        paused = !paused;
                        let _ = tx.send(RecorderEvent::Paused(paused));
                    }
                    Some(Ctrl::ToggleMute(source)) => {
                        let now = match muted.iter().position(|m| *m == source) {
                            Some(i) => {
                                muted.remove(i);
                                false
                            }
                            None => {
                                muted.push(source.clone());
                                true
                            }
                        };
                        let _ = tx.send(RecorderEvent::Muted { source, muted: now });
                    }
                    Some(Ctrl::Discard) => {
                        discard = true;
                        break;
                    }
                    Some(Ctrl::Stop) | None => break,
                },
                _ = tokio::time::sleep(tick) => {
                    if paused { continue; }
                    clock += tick.as_secs_f64() * speed;
                    if tx.send(RecorderEvent::Clock(clock)).is_err() { return; }
                    while segments.peek().is_some_and(|s| s.end <= clock) {
                        let seg = segments.next().unwrap();
                        if muted.contains(&seg.source) { continue; }
                        count += 1;
                        if tx.send(RecorderEvent::Segment(seg)).is_err() { return; }
                    }
                    if segments.peek().is_none() {
                        // Let the tail settle before declaring the meeting over.
                        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                        break;
                    }
                }
            }
        }
        if discard {
            let _ = tx.send(RecorderEvent::Discarded {
                meeting_dir: String::new(),
            });
        } else {
            let _ = tx.send(RecorderEvent::Finished {
                meeting_dir: String::new(),
                duration_secs: clock,
                segment_count: count,
            });
        }
        let _ = tx.send(RecorderEvent::Exited { code: Some(0) });
    });
    RecorderHandle { ctrl: ctrl_tx }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_engine_events() {
        let seg = r#"{"end":1.9,"event":"segment","source":"mic","start":0,"text":"Exercise."}"#;
        assert_eq!(
            parse_event(seg),
            Some(RecorderEvent::Segment(Segment {
                source: "mic".into(),
                text: "Exercise.".into(),
                start: 0.0,
                end: 1.9
            }))
        );
        let started = r#"{"event":"started","locale":"en_US","meetingDir":"/m/1","sources":["mic","system"],"startedAt":"x"}"#;
        assert_eq!(
            parse_event(started),
            Some(RecorderEvent::Started {
                meeting_dir: "/m/1".into(),
                sources: vec!["mic".into(), "system".into()]
            })
        );
        let gone = r#"{"event":"discarded","meetingDir":"/m/1","durationSecs":4.0,"segmentCount":2}"#;
        assert_eq!(
            parse_event(gone),
            Some(RecorderEvent::Discarded {
                meeting_dir: "/m/1".into()
            })
        );
        let fin = r#"{"event":"finished","meetingDir":"/m/1","durationSecs":4.0,"segmentCount":2}"#;
        assert_eq!(
            parse_event(fin),
            Some(RecorderEvent::Finished {
                meeting_dir: "/m/1".into(),
                duration_secs: 4.0,
                segment_count: 2
            })
        );
        assert_eq!(
            parse_event(r#"{"event":"paused","paused":true}"#),
            Some(RecorderEvent::Paused(true))
        );
        assert_eq!(
            parse_event(r#"{"elapsed":3.2,"event":"muted","muted":true,"source":"mic"}"#),
            Some(RecorderEvent::Muted {
                source: "mic".into(),
                muted: true
            })
        );
        assert_eq!(
            parse_event(r#"{"event":"status","text":"hi"}"#),
            Some(RecorderEvent::Status("hi".into()))
        );
        assert_eq!(
            parse_event(r#"{"count":1,"event":"hooks","skipped":false}"#),
            Some(RecorderEvent::Hooks {
                count: 1,
                skipped: false
            })
        );
        assert_eq!(
            parse_event(
                r#"{"command":"/opt/meet/hooks/summarize-transcript.sh","count":1,"event":"hook","index":1,"phase":"start"}"#
            ),
            Some(RecorderEvent::HookStarted {
                index: 1,
                count: 1,
                command: "/opt/meet/hooks/summarize-transcript.sh".into()
            })
        );
        assert_eq!(
            parse_event(
                r#"{"command":"x","count":1,"event":"hook","index":1,"phase":"end","secs":41.5,"status":1}"#
            ),
            Some(RecorderEvent::HookEnded {
                index: 1,
                status: 1,
                secs: 41.5
            })
        );
        assert_eq!(parse_event(r#"{"event":"hook","phase":"middle"}"#), None);
        assert_eq!(parse_event("garbage"), None);
    }

    #[test]
    fn engine_stderr_loses_its_terminal_escapes() {
        assert_eq!(strip_ansi("\r\u{1b}[2K⚠ hook[2] exited 3"), "⚠ hook[2] exited 3");
        assert_eq!(strip_ansi("\u{1b}[1;31mred\u{1b}[0m plain"), "red plain");
        assert_eq!(strip_ansi("no escapes"), "no escapes");
        assert_eq!(strip_ansi("cut \u{1b}["), "cut ", "an unfinished sequence is dropped");
        assert_eq!(strip_ansi("bare \u{1b}x"), "bare x", "a lone ESC goes, the rest stays");
    }

    #[test]
    fn old_transcripts_with_speaker_still_load() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("transcript.json");
        std::fs::write(
            &p,
            r#"{"startedAt":"x","durationSecs":2,"segments":[{"end":2.7,"speaker":"me","start":0,"text":"hi"}]}"#,
        )
        .unwrap();
        let segs = load_transcript(&p).unwrap();
        assert_eq!(segs[0].source, "me");
        assert_eq!(segs[0].text, "hi");
    }

    /// Mute holds a source's lines back, as the engine would hear nothing from it, and
    /// every flip is answered with a `Muted` event.
    #[tokio::test]
    async fn a_muted_source_is_left_out_of_the_replay() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let seg = |source: &str, text: &str, end: f64| Segment {
            source: source.into(),
            text: text.into(),
            start: end - 0.5,
            end,
        };
        let segs = vec![
            seg("mic", "one", 0.5),
            seg("system", "two", 1.0),
            seg("mic", "three", 1.5),
        ];
        let h = spawn_replay(segs, 50.0, tx);
        // All three land before the first tick: mic muted, system muted and back.
        h.toggle_mute("mic");
        h.toggle_mute("system");
        h.toggle_mute("system");
        let mut got = Vec::new();
        while let Some(ev) = rx.recv().await {
            let done = matches!(ev, RecorderEvent::Exited { .. });
            got.push(ev);
            if done {
                break;
            }
        }
        assert_eq!(
            got[0],
            RecorderEvent::Started {
                meeting_dir: String::new(),
                sources: vec!["mic".into(), "system".into()]
            }
        );
        let flips: Vec<_> = got
            .iter()
            .filter_map(|e| match e {
                RecorderEvent::Muted { source, muted } => Some((source.as_str(), *muted)),
                _ => None,
            })
            .collect();
        assert_eq!(flips, vec![("mic", true), ("system", true), ("system", false)]);
        let texts: Vec<_> = got
            .iter()
            .filter_map(|e| match e {
                RecorderEvent::Segment(s) => Some(s.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["two"], "the muted mic's lines never land");
        assert!(got.iter().any(|e| matches!(
            e,
            RecorderEvent::Finished {
                segment_count: 1,
                ..
            }
        )));
    }

    #[tokio::test]
    async fn replay_delivers_segments_then_finishes() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let segs = vec![
            Segment {
                source: "mic".into(),
                text: "one".into(),
                start: 0.0,
                end: 0.5,
            },
            Segment {
                source: "mic".into(),
                text: "two".into(),
                start: 0.5,
                end: 1.0,
            },
        ];
        let _h = spawn_replay(segs, 50.0, tx);
        let mut got = Vec::new();
        while let Some(ev) = rx.recv().await {
            let done = matches!(ev, RecorderEvent::Exited { .. });
            got.push(ev);
            if done {
                break;
            }
        }
        assert_eq!(
            got[0],
            RecorderEvent::Started {
                meeting_dir: String::new(),
                sources: vec!["mic".into()]
            },
            "a replay names the transcript's own sources"
        );
        let texts: Vec<_> = got
            .iter()
            .filter_map(|e| match e {
                RecorderEvent::Segment(s) => Some(s.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["one", "two"]);
        assert!(got.iter().any(|e| matches!(
            e,
            RecorderEvent::Finished {
                segment_count: 2,
                ..
            }
        )));
    }
}
