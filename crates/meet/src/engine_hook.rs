//! The engine's onDone hooks as the TUI sees them. Once the recording is saved, the
//! engine runs the commands `meet.json` names under `hooks.onDone` — the bundled
//! `summarize-transcript.sh`, which calls Claude to write a Markdown summary of the
//! transcript, or the user's own — one after another, and reports each one starting and
//! ending (`hook` events) with everything it prints in between. This is that, kept as
//! state for the Summary pane and the header: which hook is running and for how long,
//! its last lines, how it ended, and the file it said it wrote (shown in the pane too).
//!
//! Distinct from `hook`, the Claude Code Stop hook meet gives its own agents.

use std::path::{Path, PathBuf};
use std::time::Instant;

/// How many of a hook's lines are kept.
pub const MAX_OUTPUT: usize = 200;
/// How much of a file a hook wrote is read for the pane.
pub const MAX_FILE_BYTES: usize = 64 * 1024;
const TEXT_EXTENSIONS: &[&str] = &["md", "markdown", "txt"];

#[derive(Debug, Clone, PartialEq)]
pub enum HookState {
    Running,
    /// Exit status 0.
    Done,
    /// A non-zero exit status.
    Failed(i32),
    /// The engine went away before it said how the hook ended.
    Lost,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HookRun {
    /// 1-based, of `count`.
    pub index: usize,
    pub count: usize,
    /// The shell command as configured.
    pub command: String,
    pub state: HookState,
    /// What it printed (stdout and stderr both), the last `MAX_OUTPUT` lines.
    pub output: Vec<String>,
    started: Instant,
    /// How long it took, once it ended, as the engine measured it.
    pub secs: Option<f64>,
    /// A file it said it wrote — a path at the end of one of its last lines that exists
    /// and is Markdown or text — and that file's contents (up to `MAX_FILE_BYTES`).
    pub wrote: Option<(PathBuf, String)>,
}

impl HookRun {
    /// The hook's short name: the basename of the program in its command — past a shell
    /// or `env` when the command starts with one — so `/opt/meet/hooks/summarize-transcript.sh`
    /// and `bash ~/bin/notes.sh --quiet` read as `summarize-transcript.sh` and `notes.sh`.
    pub fn name(&self) -> String {
        fn base(w: &str) -> String {
            Path::new(w)
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or(w)
                .to_string()
        }
        let mut words = self.command.split_whitespace().filter(|w| !w.starts_with('-'));
        let first = words.next().unwrap_or("hook");
        let program = match base(first).as_str() {
            "sh" | "bash" | "zsh" | "env" | "nohup" => words
                .next()
                .filter(|w| !w.starts_with(['\'', '"']))
                .unwrap_or(first),
            _ => first,
        };
        base(program)
    }

    /// How long it has run, or took.
    pub fn elapsed_secs(&self) -> f64 {
        self.secs
            .unwrap_or_else(|| self.started.elapsed().as_secs_f64())
    }

    /// The last few non-empty lines it printed, for a status display.
    pub fn tail(&self, n: usize) -> Vec<&str> {
        let mut lines: Vec<&str> = self
            .output
            .iter()
            .rev()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .take(n)
            .collect();
        lines.reverse();
        lines
    }
}

/// Every hook of this recording, in the order the engine ran them.
#[derive(Debug, Default)]
pub struct Hooks {
    /// How many the engine said it would run; `None` until it says — and for good with an
    /// engine from before it announced them, in which case only `runs` tells.
    pub expected: Option<usize>,
    /// Hooks are configured but `--no-hooks` skipped them.
    pub skipped: bool,
    pub runs: Vec<HookRun>,
    /// The engine exited: nothing announced can still start.
    engine_gone: bool,
}

impl Hooks {
    /// The engine's `hooks` event, right after `finished`.
    pub fn on_announced(&mut self, count: usize, skipped: bool) {
        self.expected = Some(count);
        self.skipped = skipped;
    }

    pub fn on_started(&mut self, index: usize, count: usize, command: String) {
        // The engine runs hooks one after another: whatever was still running is over.
        for r in &mut self.runs {
            if r.state == HookState::Running {
                r.state = HookState::Lost;
            }
        }
        self.runs.push(HookRun {
            index,
            count,
            command,
            state: HookState::Running,
            output: Vec::new(),
            started: Instant::now(),
            secs: None,
            wrote: None,
        });
    }

    /// A line the engine printed while a hook runs belongs to that hook. Returns whether
    /// one took it. The engine's own `→ hook[n]:` note is not the hook's output (it may
    /// arrive after the start event, the two travelling on different pipes).
    pub fn on_output(&mut self, line: &str) -> bool {
        if line.starts_with("→ hook[") {
            return false;
        }
        let Some(r) = self.runs.iter_mut().rev().find(|r| r.state == HookState::Running) else {
            return false;
        };
        r.output.push(line.to_string());
        if r.output.len() > MAX_OUTPUT {
            let excess = r.output.len() - MAX_OUTPUT;
            r.output.drain(..excess);
        }
        true
    }

    /// The hook ended; `status` is its exit status. Looks for a file it wrote among its
    /// last lines and reads it. Returns the run, for the flash.
    pub fn on_ended(&mut self, index: usize, status: i32, secs: f64) -> Option<&HookRun> {
        let r = self
            .runs
            .iter_mut()
            .rev()
            .find(|r| r.index == index && r.state == HookState::Running)?;
        r.state = if status == 0 {
            HookState::Done
        } else {
            HookState::Failed(status)
        };
        r.secs = Some(secs);
        r.wrote = written_file(&r.output).and_then(|p| read_written(&p).map(|t| (p, t)));
        Some(r)
    }

    /// The engine exited: a hook it never reported the end of is lost, and none announced
    /// can still start.
    pub fn on_engine_exited(&mut self) {
        self.engine_gone = true;
        for r in &mut self.runs {
            if r.state == HookState::Running {
                r.state = HookState::Lost;
            }
        }
    }

    pub fn running(&self) -> Option<&HookRun> {
        self.runs.iter().rev().find(|r| r.state == HookState::Running)
    }

    pub fn any_running(&self) -> bool {
        self.running().is_some()
    }

    /// Hooks were announced but the first has not started yet (and the engine is still
    /// there to start it).
    pub fn pending(&self) -> bool {
        self.runs.is_empty() && self.expected.unwrap_or(0) > 0 && !self.engine_gone
    }
}

/// A file the hook's output names as written: the last path-like token of one of its
/// last lines (the bundled hook ends with `summarize-transcript: wrote /path/to/file.md`;
/// Claude itself prints the path it wrote), if it exists and is Markdown or plain text.
pub fn written_file(output: &[String]) -> Option<PathBuf> {
    output
        .iter()
        .rev()
        .take(20)
        .find_map(|l| {
            let token = l.split_whitespace().last()?;
            let token = token
                .trim_matches(|c: char| matches!(c, '`' | '\'' | '"' | '(' | ')' | ',' | ';' | '<' | '>'));
            let token = token.strip_suffix('.').unwrap_or(token);
            let path = PathBuf::from(token);
            let is_text = path
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase)
                .is_some_and(|e| TEXT_EXTENSIONS.contains(&e.as_str()));
            (path.is_absolute() && is_text && path.is_file()).then_some(path)
        })
}

/// The file's text, the first `MAX_FILE_BYTES` of it (cut at a line).
pub fn read_written(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if bytes.len() > MAX_FILE_BYTES {
        let cut = text
            .char_indices()
            .map(|(i, _)| i)
            .take_while(|&i| i <= MAX_FILE_BYTES)
            .last()
            .unwrap_or(0);
        text.truncate(cut);
        if let Some(nl) = text.rfind('\n') {
            text.truncate(nl);
        }
        text.push_str("\n… (cut; the file goes on)\n");
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hook_is_named_by_its_program_and_keeps_its_output_until_it_ends() {
        let mut hooks = Hooks::default();
        assert!(!hooks.pending());
        hooks.on_announced(2, false);
        assert!(hooks.pending(), "announced, not started");
        assert!(!hooks.on_output("stray line"), "no hook running takes nothing");

        hooks.on_started(1, 2, "/opt/meet/hooks/summarize-transcript.sh".into());
        assert!(!hooks.pending());
        assert_eq!(hooks.running().unwrap().name(), "summarize-transcript.sh");
        assert!(!hooks.on_output("→ hook[1]: /opt/meet/hooks/summarize-transcript.sh"), "the engine's note is not the hook's output");
        assert!(hooks.on_output("summarize-transcript: summarizing /m/transcript.md → /m/summaries/"));
        assert!(hooks.on_output(""));
        assert!(hooks.on_output("thinking"));
        let r = hooks.running().unwrap();
        assert_eq!(r.output.len(), 3);
        assert_eq!(r.tail(2), vec!["summarize-transcript: summarizing /m/transcript.md → /m/summaries/", "thinking"]);
        assert!(r.elapsed_secs() < 5.0);

        let r = hooks.on_ended(1, 0, 41.5).unwrap();
        assert_eq!(r.state, HookState::Done);
        assert_eq!(r.secs, Some(41.5));
        assert_eq!(r.elapsed_secs(), 41.5, "once ended, the engine's measure");
        assert_eq!(r.wrote, None, "no file named");
        assert!(hooks.running().is_none());
        assert!(!hooks.on_output("late line"), "an ended hook takes no more output");
        assert!(hooks.on_ended(1, 0, 1.0).is_none(), "already ended");

        hooks.on_started(2, 2, "bash ~/bin/notes.sh --quiet".into());
        assert_eq!(hooks.running().unwrap().name(), "notes.sh");
        assert_eq!(hooks.on_ended(2, 1, 0.2).unwrap().state, HookState::Failed(1));
        assert_eq!(hooks.runs.len(), 2);

        // A run the engine never reported the end of.
        hooks.on_started(3, 3, "sleep 999".into());
        hooks.on_engine_exited();
        assert_eq!(hooks.runs[2].state, HookState::Lost);
        assert!(!hooks.any_running());

        // Announced, then the engine died before starting any: nothing is pending.
        let mut crashed = Hooks::default();
        crashed.on_announced(1, false);
        assert!(crashed.pending());
        crashed.on_engine_exited();
        assert!(!crashed.pending());

        // Output is capped.
        hooks.on_started(4, 4, "yes".into());
        for i in 0..(MAX_OUTPUT + 50) {
            hooks.on_output(&format!("line {i}"));
        }
        let r = hooks.running().unwrap();
        assert_eq!(r.output.len(), MAX_OUTPUT);
        assert_eq!(r.output[0], "line 50");
    }

    #[test]
    fn names_fall_back_sensibly() {
        let run = |cmd: &str| HookRun {
            index: 1,
            count: 1,
            command: cmd.into(),
            state: HookState::Running,
            output: vec![],
            started: Instant::now(),
            secs: None,
            wrote: None,
        };
        assert_eq!(run("sh -c 'echo hi'").name(), "sh", "an inline script: the shell itself");
        assert_eq!(run("/usr/bin/env python3 /x/summ.py").name(), "python3", "past env: the interpreter");
        assert_eq!(run("/bin/bash /x/summ.sh").name(), "summ.sh", "past a shell: the script");
        assert_eq!(run("").name(), "hook");
        assert_eq!(run("~/bin/post-to-slack").name(), "post-to-slack");
    }

    #[test]
    fn a_written_markdown_file_is_found_at_the_end_of_a_line_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let md = dir.path().join("2026-09-11_release-plan.md");
        std::fs::write(&md, "# Release plan\n\n## Summary\nShip it.\n").unwrap();
        let missing = dir.path().join("nope.md");
        let audio = dir.path().join("audio.m4a");
        std::fs::write(&audio, b"x").unwrap();
        let lines = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        assert_eq!(
            written_file(&lines(&["thinking", &format!("summarize-transcript: wrote {}", md.display())])),
            Some(md.clone())
        );
        assert_eq!(
            written_file(&lines(&[&format!("`{}`", md.display()), "summarize-transcript: done"])),
            Some(md.clone()),
            "backticks around the path, and the path not on the last line"
        );
        assert_eq!(
            written_file(&lines(&[&format!("I wrote the summary to {}.", md.display())])),
            Some(md.clone()),
            "a full stop after the path"
        );
        assert_eq!(written_file(&lines(&[&format!("wrote {}", missing.display())])), None, "must exist");
        assert_eq!(written_file(&lines(&[&format!("merged {}", audio.display())])), None, "must be text");
        assert_eq!(written_file(&lines(&["wrote relative.md"])), None, "must be absolute");
        assert_eq!(written_file(&[]), None);

        let mut hooks = Hooks::default();
        hooks.on_started(1, 1, "summarize".into());
        hooks.on_output(&format!("summarize-transcript: wrote {}", md.display()));
        let r = hooks.on_ended(1, 0, 3.0).unwrap();
        let (path, text) = r.wrote.clone().expect("the file is picked up at the end");
        assert_eq!(path, md);
        assert_eq!(text, "# Release plan\n\n## Summary\nShip it.\n");

        let big = dir.path().join("big.txt");
        let line = "x".repeat(99) + "\n";
        std::fs::write(&big, line.repeat(MAX_FILE_BYTES / 100 + 20)).unwrap();
        let text = read_written(&big).unwrap();
        assert!(text.len() <= MAX_FILE_BYTES + 40, "{}", text.len());
        assert!(text.ends_with("… (cut; the file goes on)\n"));
        assert_eq!(read_written(&missing), None);
    }
}
