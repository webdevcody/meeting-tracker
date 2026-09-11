//! The question session: an interactive Claude Code terminal the user asks about the
//! meeting while it goes on ("what did they say about the deploy?", "which files came up?")
//! — and afterwards. It is an ordinary `claude` run in the repository, told where the live
//! transcript and the running summary are (`live`) and to re-read them before every answer.
//!
//! `meet ask` runs one in whatever terminal it is typed in. `a` in the TUI opens one where
//! it can: `nebula spawn` when `meet` itself runs inside a nebula agent session (nebula only
//! takes spawn requests from there), a tmux pane beside `meet` when inside tmux, and
//! otherwise it says what to run. Either way the context reaches Claude as text: a system
//! prompt file when `meet` starts `claude` itself, the starting prompt through nebula.

use crate::live::{self, LiveFiles, SUMMARY_FILE, TRANSCRIPT_FILE};
use crate::store::{MeetingRow, Store};
use anyhow::{bail, Context, Result};
use std::path::PathBuf;

pub const SYSTEM_FILE: &str = "ask-system.md";
pub const SESSION_NAME: &str = "meet · questions";

/// What the session is about.
#[derive(Debug, Clone, PartialEq)]
pub struct AskContext {
    pub repo: PathBuf,
    pub meeting_id: String,
    /// `<data dir>/sessions/<meeting id>`: the transcript and the summary live here.
    pub session_dir: PathBuf,
    /// The recording is still going (the transcript keeps growing).
    pub live: bool,
}

impl AskContext {
    pub fn new(repo: PathBuf, meeting_id: &str, live: bool) -> Self {
        Self {
            repo,
            meeting_id: meeting_id.to_string(),
            session_dir: crate::paths::session_dir(meeting_id),
            live,
        }
    }

    pub fn transcript(&self) -> PathBuf {
        self.session_dir.join(TRANSCRIPT_FILE)
    }

    pub fn summary(&self) -> PathBuf {
        self.session_dir.join(SUMMARY_FILE)
    }

    pub fn system_file(&self) -> PathBuf {
        self.session_dir.join(SYSTEM_FILE)
    }
}

/// How to start it.
#[derive(Debug, Clone)]
pub struct Launch {
    /// This binary — what the tmux pane and the printed command run (`meet ask`).
    pub exe: PathBuf,
    pub claude_bin: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub ctx: AskContext,
}

/// What the TUI hears back from `open`.
#[derive(Debug, Clone, PartialEq)]
pub enum AskEvent {
    Opened(Opened),
    Failed(String),
}

/// Where the session ended up.
#[derive(Debug, Clone, PartialEq)]
pub enum Opened {
    /// A new agent beside this one in nebula's session list.
    Nebula,
    /// A pane beside `meet`.
    Tmux,
    /// Nowhere yet: this is the command to run in another terminal.
    Manual(String),
}

pub fn system_prompt(ctx: &AskContext) -> String {
    let name = crate::git::repo_name(&ctx.repo);
    let when = if ctx.live {
        "A meeting is being recorded right now (the `state:` line in the summary says when it ends)"
    } else {
        "A meeting was recorded"
    };
    format!(
        r#"You are the question desk inside `meet`, a terminal tool a developer talks into while working on the git repository "{name}" at {repo}. {when}; the developer opens this session to ask what was said, what was decided, and how any of it relates to this repository.

The transcript is at {transcript}: one line per sentence as it was heard, `[mm:ss] source: text`, appended while the meeting goes on. Sources: "mic" is this Mac's microphone (the developer and anyone in the room), "system" is audio the Mac played (remote call participants, videos), "typed" is a note typed into meet. Expect recognition errors, filler words and half sentences; read through them.

The running summary is at {summary}: the recording state and length, the meeting's summary once written, meet's one-line summaries per minute of talk, the facts about the repository it looked up along the way, and the action items on its board.

Rules:
- Before answering ANY question, Read {transcript} again from the top — it has grown since you last looked; never answer from memory of an earlier read. Read {summary} when the question is about what meet concluded, the action items, or how long things have run.
- When the question is about the code, use Glob, Grep and Read on the repository at {repo}. Do not edit files, run builds or tests, commit, or take any other action: this session is for answers. If asked to draft something (a message, a list, a summary), write it in your reply, not into a file, unless a file is asked for.
- Answer in plain prose, briefly — under about 150 words unless asked for more. Quote the transcript line (with its [mm:ss]) that supports what you say, and say plainly when the transcript does not answer the question. Do not invent what was not said."#,
        repo = ctx.repo.display(),
        transcript = ctx.transcript().display(),
        summary = ctx.summary().display(),
    )
}

/// The first turn: prove the files are readable and say where things stand.
pub fn first_prompt(ctx: &AskContext) -> String {
    format!(
        "Read {} and {} now. Then answer in two or three lines: how long the meeting has run, what it has been about so far, and that you are ready for questions. After that, wait for my questions.",
        ctx.transcript().display(),
        ctx.summary().display()
    )
}

/// nebula takes no system prompt, only a first prompt: both in one.
pub fn starting_prompt(ctx: &AskContext) -> String {
    format!("{}\n\n{}", system_prompt(ctx), first_prompt(ctx))
}

/// The `meet ask …` argv (after the binary) that reopens this session anywhere.
pub fn ask_args(l: &Launch) -> Vec<String> {
    let mut args = Vec::new();
    // A top-level flag: it goes before the subcommand.
    if l.claude_bin != "claude" {
        args.push("--claude-bin".into());
        args.push(l.claude_bin.clone());
    }
    args.push("ask".into());
    args.push("--meeting".into());
    args.push(l.ctx.meeting_id.clone());
    if let Some(m) = &l.model {
        args.push("--model".into());
        args.push(m.clone());
    }
    if let Some(e) = &l.effort {
        args.push("--effort".into());
        args.push(e.clone());
    }
    args.push(l.ctx.repo.to_string_lossy().into_owned());
    args
}

/// `meet ask …` as one shell line. A `MEET_DATA_DIR` override travels with it: a tmux pane
/// or another terminal does not inherit this process's environment.
pub fn ask_command(l: &Launch) -> String {
    let data_dir = crate::paths::non_empty(crate::paths::DATA_DIR_ENV)
        .map(|d| format!("{}={} ", crate::paths::DATA_DIR_ENV, crate::hook::shell_quote(&d)))
        .unwrap_or_default();
    let cmd = std::iter::once(l.exe.to_string_lossy().into_owned())
        .chain(ask_args(l))
        .map(|a| crate::hook::shell_quote(&a))
        .collect::<Vec<_>>()
        .join(" ");
    format!("{data_dir}{cmd}")
}

/// The `claude` argv (after the binary) for the session `meet ask` starts itself.
pub fn claude_args(l: &Launch) -> Vec<String> {
    let mut args = vec![
        "--append-system-prompt-file".to_string(),
        l.ctx.system_file().to_string_lossy().into_owned(),
        "--add-dir".into(),
        l.ctx.session_dir.to_string_lossy().into_owned(),
        "--name".into(),
        SESSION_NAME.into(),
    ];
    if let Some(m) = &l.model {
        args.push("--model".into());
        args.push(m.clone());
    }
    if let Some(e) = &l.effort {
        args.push("--effort".into());
        args.push(e.clone());
    }
    args.push(first_prompt(&l.ctx));
    args
}

/// Open the session where this `meet` can: nebula, then tmux, else say what to run.
pub async fn open(l: &Launch) -> Result<Opened> {
    let mut notes = Vec::new();
    if crate::paths::non_empty("NEBULA_AGENT_ID").is_some() {
        match nebula_spawn(l).await {
            Ok(()) => return Ok(Opened::Nebula),
            Err(e) => notes.push(format!("nebula: {e:#}")),
        }
    }
    if crate::paths::non_empty("TMUX").is_some() {
        match tmux_split(l).await {
            Ok(()) => return Ok(Opened::Tmux),
            Err(e) => notes.push(format!("tmux: {e:#}")),
        }
    }
    let cmd = ask_command(l);
    if notes.is_empty() {
        Ok(Opened::Manual(cmd))
    } else {
        Ok(Opened::Manual(format!("{cmd}  ({})", notes.join("; "))))
    }
}

/// `nebula spawn <starting prompt>`: a Claude session beside the one `meet` runs in.
async fn nebula_spawn(l: &Launch) -> Result<()> {
    let out = tokio::process::Command::new("nebula")
        .arg("spawn")
        .arg("--kind")
        .arg("claude")
        .arg(starting_prompt(&l.ctx))
        .current_dir(&l.ctx.repo)
        .output()
        .await
        .context("run nebula spawn")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        bail!("{}", if err.is_empty() { "nebula spawn failed".into() } else { err });
    }
    Ok(())
}

/// A pane to the right of `meet` running `meet ask`.
async fn tmux_split(l: &Launch) -> Result<()> {
    let out = tokio::process::Command::new("tmux")
        .args(["split-window", "-h", "-c"])
        .arg(&l.ctx.repo)
        .arg(ask_command(l))
        .output()
        .await
        .context("run tmux split-window")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        bail!("{}", if err.is_empty() { "tmux split-window failed".into() } else { err });
    }
    Ok(())
}

/// `meet ask`: pick the meeting, make sure its files are there, replace this process with
/// `claude`. `meeting`: an id, else the live meeting in this repo, else the newest one.
pub fn run_cli(
    dir: PathBuf,
    meeting: Option<String>,
    claude_bin: String,
    model: Option<String>,
    effort: Option<String>,
) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let repo = crate::runtime()?.block_on(crate::git::toplevel(&dir))?;
    let store = Store::open(&crate::paths::db_path())?;
    let repo_key = repo.to_string_lossy().into_owned();
    let meetings = store.list_meetings(&repo_key)?;
    let row = pick_meeting(&meetings, meeting.as_deref())?;
    let ctx = AskContext::new(repo.clone(), &row.id, row.ended_at.is_none());
    ensure_files(&store, row, &ctx)?;
    std::fs::write(ctx.system_file(), system_prompt(&ctx))
        .with_context(|| format!("write {}", ctx.system_file().display()))?;
    let l = Launch {
        exe: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("meet")),
        claude_bin,
        model,
        effort,
        ctx,
    };
    eprintln!(
        "meet · questions about the meeting of {} in {} ({})",
        crate::when::local_datetime(row.started_at),
        crate::git::repo_name(&repo),
        if l.ctx.live { "recording now" } else { "ended" }
    );
    eprintln!("transcript: {}", l.ctx.transcript().display());
    let err = std::process::Command::new(&l.claude_bin)
        .args(claude_args(&l))
        .current_dir(&repo)
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .env("MEET_MEETING_ID", &l.ctx.meeting_id)
        .env("MEET_SESSION_DIR", &l.ctx.session_dir)
        .env("MEET_REPO", &repo)
        .exec();
    Err(anyhow::Error::new(err).context(format!("exec {}", l.claude_bin)))
}

/// The meeting `meet ask` is about: by id, else the live one, else the newest.
pub fn pick_meeting<'a>(meetings: &'a [MeetingRow], id: Option<&str>) -> Result<&'a MeetingRow> {
    if meetings.is_empty() {
        bail!("no sessions in this repository yet — run `meet` here and start talking");
    }
    if let Some(id) = id {
        return meetings
            .iter()
            .find(|m| m.id == id)
            .with_context(|| format!("no session {id} in this repository (`meet sessions` lists them)"));
    }
    Ok(meetings
        .iter()
        .find(|m| m.ended_at.is_none())
        .unwrap_or(&meetings[0]))
}

/// A meeting recorded before `meet` wrote live files (or whose files were deleted) gets
/// them now, from the database; a meeting that has them is left alone — its `meet` is
/// still appending to them.
fn ensure_files(store: &Store, m: &MeetingRow, ctx: &AskContext) -> Result<()> {
    if ctx.transcript().is_file() {
        return Ok(());
    }
    let header = live::transcript_header(
        &crate::git::repo_name(&ctx.repo),
        &crate::when::local_datetime(m.started_at),
    );
    let files = LiveFiles::create(ctx.session_dir.clone(), &header)?;
    let segments = store.list_segments(&m.id)?;
    for s in &segments {
        files.append(&crate::app::TranscriptLine {
            at: s.start_secs,
            source: s.source.clone(),
            text: s.text.clone(),
        })?;
    }
    let chunks = store.list_chunks(&m.id)?;
    let items = store.list_items(&ctx.repo.to_string_lossy())?;
    files.write_summary(&live::past_summary_text(
        m,
        &crate::git::repo_name(&ctx.repo),
        &files.transcript_path(),
        segments.len(),
        &chunks,
        &items,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> AskContext {
        AskContext {
            repo: "/w/my-app".into(),
            meeting_id: "01m".into(),
            session_dir: "/data/sessions/01m".into(),
            live: true,
        }
    }

    fn launch() -> Launch {
        Launch {
            exe: "/usr/local/bin/meet".into(),
            claude_bin: "claude".into(),
            model: Some("sonnet".into()),
            effort: None,
            ctx: ctx(),
        }
    }

    #[test]
    fn the_prompts_name_the_files_and_the_rules() {
        let s = system_prompt(&ctx());
        assert!(s.contains("\"my-app\" at /w/my-app"));
        assert!(s.contains("being recorded right now"));
        assert!(s.contains("/data/sessions/01m/transcript.md"));
        assert!(s.contains("/data/sessions/01m/summary.md"));
        assert!(s.contains("Read /data/sessions/01m/transcript.md again"));
        assert!(s.contains("Do not edit files"));
        let mut past = ctx();
        past.live = false;
        assert!(system_prompt(&past).contains("A meeting was recorded;"));
        let f = first_prompt(&ctx());
        assert!(f.starts_with("Read /data/sessions/01m/transcript.md and /data/sessions/01m/summary.md now."));
        let both = starting_prompt(&ctx());
        assert!(both.starts_with(&s) && both.ends_with(&f));
        assert!(both.len() < 16 * 1024, "nebula caps the starting prompt at 16 KiB");
    }

    #[test]
    fn the_commands_carry_the_meeting_the_model_and_the_context() {
        let l = launch();
        assert_eq!(
            ask_args(&l),
            vec!["ask", "--meeting", "01m", "--model", "sonnet", "/w/my-app"]
        );
        let cmd = ask_command(&l);
        assert!(
            cmd.ends_with("/usr/local/bin/meet ask --meeting 01m --model sonnet /w/my-app"),
            "{cmd}"
        );
        match crate::paths::non_empty(crate::paths::DATA_DIR_ENV) {
            Some(d) => assert!(cmd.starts_with(&format!("MEET_DATA_DIR={d} ")), "{cmd}"),
            None => assert!(cmd.starts_with('/'), "{cmd}"),
        }
        let mut l2 = launch();
        l2.claude_bin = "/tmp/fake claude".into();
        l2.effort = Some("low".into());
        let cmd = ask_command(&l2);
        assert!(cmd.contains("/usr/local/bin/meet --claude-bin '/tmp/fake claude' ask "), "{cmd}");
        assert!(cmd.contains("--effort low"), "{cmd}");

        let args = claude_args(&l);
        assert_eq!(
            &args[..6],
            &[
                "--append-system-prompt-file",
                "/data/sessions/01m/ask-system.md",
                "--add-dir",
                "/data/sessions/01m",
                "--name",
                SESSION_NAME
            ]
        );
        assert_eq!(&args[6..8], &["--model", "sonnet"]);
        assert_eq!(args.last().unwrap(), &first_prompt(&ctx()));
        assert!(!args.contains(&"-p".to_string()), "interactive, not headless");
    }

    #[test]
    fn the_live_meeting_wins_then_the_newest_then_the_named_one() {
        let m = |id: &str, ended: Option<i64>| MeetingRow {
            id: id.into(),
            repo_path: "/r".into(),
            meeting_dir: None,
            started_at: 0,
            ended_at: ended,
            segment_count: 0,
            summary: None,
        };
        assert!(pick_meeting(&[], None).is_err());
        let list = vec![m("new", Some(5)), m("live", None), m("old", Some(1))];
        assert_eq!(pick_meeting(&list, None).unwrap().id, "live");
        assert_eq!(pick_meeting(&list, Some("old")).unwrap().id, "old");
        assert!(pick_meeting(&list, Some("nope")).is_err());
        let list = vec![m("new", Some(5)), m("old", Some(1))];
        assert_eq!(pick_meeting(&list, None).unwrap().id, "new");
    }

    #[test]
    fn a_past_meeting_without_files_gets_them_from_the_database() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let id = store.insert_meeting("/w/my-app").unwrap();
        store.insert_segment(&id, "mic", "hello there", 1.0, 2.0).unwrap();
        let c = store.insert_chunk(&id, 0, 1.0, 2.0, "hello there").unwrap();
        store.set_chunk_summary(&c, "a greeting").unwrap();
        store.set_meeting_summary(&id, "Someone said hello.").unwrap();
        store.end_meeting(&id, 1).unwrap();
        let row = store.list_meetings("/w/my-app").unwrap().remove(0);
        let ctx = AskContext {
            repo: "/w/my-app".into(),
            meeting_id: id.clone(),
            session_dir: tmp.path().join("sessions").join(&id),
            live: false,
        };
        ensure_files(&store, &row, &ctx).unwrap();
        let t = std::fs::read_to_string(ctx.transcript()).unwrap();
        assert!(t.ends_with("[00:01] mic: hello there\n"), "{t}");
        let s = std::fs::read_to_string(ctx.summary()).unwrap();
        assert!(s.contains("state: ended"), "{s}");
        assert!(s.contains("## Meeting summary\nSomeone said hello.\n"), "{s}");
        assert!(s.contains("[1] 00:01–00:02 a greeting\n"), "{s}");
        // A second call leaves the files alone.
        std::fs::write(ctx.transcript(), "kept\n").unwrap();
        ensure_files(&store, &row, &ctx).unwrap();
        assert_eq!(std::fs::read_to_string(ctx.transcript()).unwrap(), "kept\n");
    }
}
