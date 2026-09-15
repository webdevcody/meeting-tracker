//! The question session: an interactive Claude Code session the user asks about the
//! meeting while it goes on ("what did they say about the deploy?", "which files came up?")
//! — and afterwards. It is an ordinary `claude` run in the repository, told where the live
//! transcript and the running summary are (`live`) and to re-read them before every answer.
//!
//! `a` in the TUI runs it inside `meet`, in the right column, on an embedded terminal
//! (`term`); `meet ask` runs the same session in whatever terminal it is typed in.

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

    /// Write the system prompt where `claude --append-system-prompt-file` reads it.
    pub fn write_system_file(&self) -> Result<PathBuf> {
        let path = self.system_file();
        std::fs::write(&path, system_prompt(self))
            .with_context(|| format!("write {}", path.display()))?;
        Ok(path)
    }
}

/// How to start it.
#[derive(Debug, Clone)]
pub struct Launch {
    pub claude_bin: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub ctx: AskContext,
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

The running summary is at {summary}: the recording state and length, and the meeting's summary and write-up once the recording has stopped and they are written.

Rules:
- Before answering ANY question, Read {transcript} again from the top — it has grown since you last looked; never answer from memory of an earlier read. Read {summary} when the question is about what meet concluded or how long things have run.
- When the question is about the code, use Glob, Grep and Read on the repository at {repo}. Do not edit files, run builds or tests, commit, or take any other action: this session is for answers. If asked to draft something (a message, a list, a summary), write it in your reply, not into a file, unless a file is asked for.
- Answer in plain prose, briefly — under about 150 words unless asked for more; you are read in a narrow pane beside the transcript. Quote the transcript line (with its [mm:ss]) that supports what you say, and say plainly when the transcript does not answer the question. Do not invent what was not said."#,
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

/// The `claude` argv (after the binary): the system prompt file, the session directory
/// readable without a prompt, a name, the model, and the first prompt.
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

/// Extra environment for the session, for hooks or skills the user may have.
pub fn claude_env(ctx: &AskContext) -> Vec<(String, String)> {
    vec![
        ("MEET_MEETING_ID".into(), ctx.meeting_id.clone()),
        (
            "MEET_SESSION_DIR".into(),
            ctx.session_dir.to_string_lossy().into_owned(),
        ),
        ("MEET_REPO".into(), ctx.repo.to_string_lossy().into_owned()),
    ]
}

/// `meet ask`: pick the meeting, make sure its files are there, replace this process with
/// `claude` — through the login shell, as `a` and every headless call start it (see
/// [`crate::shell`]). `meeting`: an id, else the live meeting in this repo, else the newest one.
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
    ctx.write_system_file()?;
    let l = Launch {
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
    let (program, args) = crate::shell::claude_launch(&l.claude_bin, &claude_args(&l));
    let mut cmd = std::process::Command::new(&program);
    cmd.args(args)
        .current_dir(&repo)
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT");
    for (k, v) in claude_env(&l.ctx) {
        cmd.env(k, v);
    }
    let err = cmd.exec();
    Err(anyhow::Error::new(err).context(format!("exec {program} for {}", l.claude_bin)))
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

/// The files a question session reads. A meeting that ended gets both written now, from
/// the database — the definitive record, whatever an earlier launch left in them. A live
/// meeting's files are left alone when they exist (its `meet` is still appending to them)
/// and written from the database only when they are missing.
pub fn ensure_files(store: &Store, m: &MeetingRow, ctx: &AskContext) -> Result<()> {
    if ctx.live && ctx.transcript().is_file() {
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
    files.write_summary(&live::past_summary_text(
        m,
        &crate::git::repo_name(&ctx.repo),
        &files.transcript_path(),
        segments.len(),
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
    }

    #[test]
    fn the_claude_argv_carries_the_context_the_name_and_the_model() {
        let l = launch();
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
        let mut l2 = launch();
        l2.effort = Some("low".into());
        let args = claude_args(&l2);
        assert!(args.windows(2).any(|w| w == ["--effort", "low"]));
        let env = claude_env(&ctx());
        assert!(env.contains(&("MEET_MEETING_ID".to_string(), "01m".to_string())));
        assert!(env.contains(&("MEET_SESSION_DIR".to_string(), "/data/sessions/01m".to_string())));
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
        // The meeting ended: every call writes the definitive files from the database.
        std::fs::write(ctx.transcript(), "stale\n").unwrap();
        std::fs::write(ctx.summary(), "stale\n").unwrap();
        ensure_files(&store, &row, &ctx).unwrap();
        assert_eq!(std::fs::read_to_string(ctx.transcript()).unwrap(), t);
        assert_eq!(std::fs::read_to_string(ctx.summary()).unwrap(), s);
        // A live meeting's files are its own meet's: left alone when they exist.
        let live = AskContext {
            live: true,
            ..ctx.clone()
        };
        std::fs::write(ctx.transcript(), "kept\n").unwrap();
        ensure_files(&store, &row, &live).unwrap();
        assert_eq!(std::fs::read_to_string(ctx.transcript()).unwrap(), "kept\n");
        std::fs::remove_file(ctx.transcript()).unwrap();
        ensure_files(&store, &row, &live).unwrap();
        assert_eq!(
            std::fs::read_to_string(ctx.transcript()).unwrap(),
            t,
            "and written when missing"
        );
        let sys = ctx.write_system_file().unwrap();
        assert!(std::fs::read_to_string(sys).unwrap().contains("A meeting was recorded;"));
    }
}
