//! `meet` — talk to your repo. Run it in a git checkout: the recorder listens, the
//! transcript becomes summaries and action items, Enter sends an action item to a
//! headless Claude Code agent in its own worktree, and the agent's work comes back as a
//! pull request. Every session is kept; agents cut off by a quit resume on the next launch.

mod app;
mod branch_name;
mod chunker;
mod claude;
mod config;
mod event_loop;
mod git;
mod hook;
mod paths;
mod pr_body;
mod recorder;
mod runner;
mod store;
mod stream_json;
mod suggest;
mod theme;
mod ui;
mod when;
mod wrap;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "meet",
    version,
    about = "Talk to your repo: live transcript → action items → Claude Code agents → pull requests.",
    long_about = "Run `meet` (or `meet .`) inside a git checkout. It records the microphone and system audio \
through the meet-rec engine, transcribes on-device, and every minute or so hands the newest chunk of \
transcript to a headless Claude that writes a summary and proposes action items — each one a fully \
specified prompt. Select an item and press Enter: meet creates a git worktree on a new branch, runs a \
Claude Code agent in it, commits, pushes, and opens a pull request with `gh`. State lives in a SQLite \
database per user: every session's transcript, summaries and action items are kept (S lists them), \
and an agent that was running when meet quit is resumed on the next launch in the same repo.",
    after_help = "Examples:\n  meet                          record in the current repo\n  meet ~/code/app --no-system   microphone only, in another checkout\n  meet --replay meetings/2026-09-04_10-00/transcript.json --replay-speed 8\n                                replay a saved transcript instead of recording\n  meet --no-resume              leave interrupted agents stopped instead of resuming them\n  meet --on-done 'Comment a summary on the GitHub issue this came from with gh.'\n                                one more instruction for each agent when it is about to finish\n  meet record --duration 5      run the engine directly (flags pass through to meet-rec)\n  meet list                     print this repo's action items and their pull requests\n  meet sessions                 print this repo's sessions"
)]
struct Cli {
    /// Directory inside the git repository to work on (default: the current directory).
    dir: Option<PathBuf>,

    /// Replay a saved transcript.json instead of recording.
    #[arg(long, value_name = "FILE")]
    replay: Option<PathBuf>,

    /// Replay speed multiplier.
    #[arg(long, default_value_t = 4.0, value_name = "X")]
    replay_speed: f64,

    /// Path to the meet-rec engine (default: beside this binary, then PATH).
    #[arg(long, value_name = "PATH", env = "MEET_RECORDER")]
    recorder: Option<String>,

    /// Record system audio only (no microphone).
    #[arg(long)]
    no_mic: bool,

    /// Record the microphone only (no system audio).
    #[arg(long)]
    no_system: bool,

    /// Echo cancellation on the microphone (built-in mic + speakers).
    #[arg(long)]
    aec: bool,

    /// Faster, slightly less accurate transcription.
    #[arg(long)]
    fast: bool,

    /// Skip the recorder's onDone hooks (the summary hook).
    #[arg(long)]
    no_hooks: bool,

    /// Config file (meet.json) for the engine and the agent block alike.
    #[arg(long, value_name = "FILE")]
    config: Option<String>,

    /// Where the recorder stores meetings (config: outputDir).
    #[arg(long, value_name = "DIR")]
    out_dir: Option<String>,

    /// Model for the summarizer / action-item writer (a claude alias or id).
    #[arg(long, default_value = "sonnet", value_name = "MODEL")]
    suggest_model: String,

    /// Model for the implementing agent (default: meet.json agent.model, else claude's own).
    #[arg(long, value_name = "MODEL")]
    agent_model: Option<String>,

    /// Spending cap per summarizer call.
    #[arg(long, default_value_t = 1.0, value_name = "USD")]
    suggest_budget: f64,

    /// Only transcribe; never call the summarizer.
    #[arg(long)]
    no_suggest: bool,

    /// Do not resume agents that were running when meet last exited; leave them stopped.
    #[arg(long)]
    no_resume: bool,

    /// One more instruction for each agent when it is about to finish, delivered through a
    /// Claude Code Stop hook (config: agent.onDone). `{{title}}`, `{{branch}}`, `{{worktree}}`,
    /// `{{repo}}`, `{{run_dir}}` and friends are filled in.
    #[arg(long, value_name = "TEXT", conflicts_with = "on_done_file")]
    on_done: Option<String>,

    /// Read the on-done instruction from a file.
    #[arg(long, value_name = "FILE")]
    on_done_file: Option<PathBuf>,

    /// A pause this long (seconds) closes a chunk once it has --chunk-min-words.
    #[arg(long, default_value_t = 8.0, value_name = "SECS")]
    quiet_secs: f64,

    /// Fewer words than this never close a chunk on a pause.
    #[arg(long, default_value_t = 40, value_name = "N")]
    chunk_min_words: usize,

    /// This many words closes a chunk at once.
    #[arg(long, default_value_t = 180, value_name = "N")]
    chunk_max_words: usize,

    /// A chunk spanning this many seconds closes at once.
    #[arg(long, default_value_t = 90.0, value_name = "SECS")]
    chunk_max_secs: f64,

    /// The claude binary.
    #[arg(long, default_value = "claude", env = "MEET_CLAUDE_BIN")]
    claude_bin: String,

    /// The gh binary.
    #[arg(long, default_value = "gh", env = "MEET_GH_BIN")]
    gh_bin: String,

    /// Git remote to push branches to.
    #[arg(long, default_value = "origin")]
    remote: String,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the recording engine directly (all flags pass through to `meet-rec record`).
    #[command(trailing_var_arg = true, allow_hyphen_values = true)]
    Record { args: Vec<String> },
    /// Create a meet.json for this project (passes through to `meet-rec init`).
    #[command(trailing_var_arg = true, allow_hyphen_values = true)]
    Init { args: Vec<String> },
    /// Print this repo's action items, newest first, with branches and pull requests.
    List {
        /// Directory inside the git repository (default: the current directory).
        dir: Option<PathBuf>,
    },
    /// Print this repo's sessions, newest first.
    Sessions {
        /// Directory inside the git repository (default: the current directory).
        dir: Option<PathBuf>,
    },
    /// Claude Code hook entry points (installed by meet for its own agents).
    #[command(hide = true)]
    Hook {
        #[command(subcommand)]
        which: HookCommand,
    },
}

#[derive(Subcommand)]
enum HookCommand {
    /// The `Stop` hook: hands the agent its on-done prompt once.
    Stop,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Record { args }) => exec_engine(cli.recorder.as_deref(), "record", &args),
        Some(Command::Init { args }) => exec_engine(cli.recorder.as_deref(), "init", &args),
        Some(Command::List { dir }) => {
            runtime()?.block_on(list(dir.or(cli.dir).unwrap_or_else(|| ".".into())))
        }
        Some(Command::Sessions { dir }) => {
            runtime()?.block_on(sessions(dir.or(cli.dir).unwrap_or_else(|| ".".into())))
        }
        Some(Command::Hook {
            which: HookCommand::Stop,
        }) => hook::run_stop_hook(),
        None => {
            let mut record_args = Vec::new();
            if cli.no_mic {
                record_args.push("--no-mic".into());
            }
            if cli.no_system {
                record_args.push("--no-system".into());
            }
            if cli.aec {
                record_args.push("--aec".into());
            }
            if cli.fast {
                record_args.push("--fast".into());
            }
            if cli.no_hooks {
                record_args.push("--no-hooks".into());
            }
            if let Some(c) = &cli.config {
                record_args.push("--config".into());
                record_args.push(c.clone());
            }
            if let Some(d) = &cli.out_dir {
                record_args.push("--out-dir".into());
                record_args.push(d.clone());
            }
            let on_done = match (&cli.on_done, &cli.on_done_file) {
                (Some(t), _) => Some(t.clone()),
                (None, Some(f)) => Some(
                    std::fs::read_to_string(f)
                        .with_context(|| format!("read --on-done-file {}", f.display()))?,
                ),
                (None, None) => None,
            }
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
            let opts = event_loop::Opts {
                dir: cli.dir.unwrap_or_else(|| ".".into()),
                replay: cli.replay,
                replay_speed: cli.replay_speed,
                recorder: cli.recorder,
                record_args,
                config: cli.config,
                claude_bin: cli.claude_bin,
                gh_bin: cli.gh_bin,
                suggest_model: Some(cli.suggest_model)
                    .filter(|m| !m.trim().is_empty() && m != "default"),
                agent_model: cli.agent_model,
                suggest_budget_usd: cli.suggest_budget,
                no_suggest: cli.no_suggest,
                no_resume: cli.no_resume,
                on_done,
                limits: chunker::Limits {
                    min_words: cli.chunk_min_words,
                    max_words: cli.chunk_max_words,
                    max_secs: cli.chunk_max_secs,
                    quiet_secs: cli.quiet_secs,
                },
                remote: cli.remote,
            };
            runtime()?.block_on(event_loop::run(opts))
        }
    }
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?)
}

/// Replace this process with the engine, so `meet record --duration 5` behaves exactly
/// like `meet-rec record --duration 5` (terminal, signals, exit status included).
fn exec_engine(explicit: Option<&str>, subcommand: &str, args: &[String]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let engine = recorder::locate_engine(explicit)?;
    let err = std::process::Command::new(&engine)
        .arg(subcommand)
        .args(args)
        .exec();
    Err(anyhow::Error::new(err).context(format!("exec {}", engine.display())))
}

async fn list(dir: PathBuf) -> Result<()> {
    let repo = git::toplevel(&dir).await?;
    let store = store::Store::open(&paths::db_path())?;
    let items = store.list_items(&repo.to_string_lossy())?;
    if items.is_empty() {
        println!(
            "no action items for {} yet — run `meet` here and start talking",
            repo.display()
        );
        return Ok(());
    }
    for i in &items {
        let status = match i.status {
            store::ItemStatus::Running => "running*",
            s => s.as_str(),
        };
        println!("{:<9} {}", status, i.title);
        if let Some(b) = &i.branch {
            println!("          branch {b}");
        }
        if let Some(s) = &i.session_id {
            println!("          session {s} ({})", i.phase.as_str());
        }
        if let Some(u) = &i.pr_url {
            println!("          {u}");
        }
        if let Some(e) = &i.error {
            println!("          error: {e}");
        }
    }
    if items
        .iter()
        .any(|i| i.status == store::ItemStatus::Running)
    {
        println!("\n* running when meet last exited here; the next `meet` resumes it (--no-resume leaves it stopped)");
    }
    println!(
        "\n{} item(s) · database {}",
        items.len(),
        paths::db_path().display()
    );
    Ok(())
}

async fn sessions(dir: PathBuf) -> Result<()> {
    let repo = git::toplevel(&dir).await?;
    let store = store::Store::open(&paths::db_path())?;
    let repo_key = repo.to_string_lossy().into_owned();
    let _ = store.close_stale_meetings(&repo_key);
    let meetings = store.list_meetings(&repo_key)?;
    if meetings.is_empty() {
        println!(
            "no sessions for {} yet — run `meet` here and start talking",
            repo.display()
        );
        return Ok(());
    }
    let items = store.list_items(&repo_key)?;
    for m in &meetings {
        println!("{}", ui::session_row(m, false, &items, 160));
        if let Some(d) = &m.meeting_dir {
            println!("    {d}");
        }
    }
    println!(
        "\n{} session(s) · database {}",
        meetings.len(),
        paths::db_path().display()
    );
    Ok(())
}
