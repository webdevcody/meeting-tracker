//! `meet` — talk to your repo. Run it in a git checkout and press `r`: the recorder
//! listens and transcribes, and when the recording stops the whole transcript becomes the
//! meeting's summary and write-up; `r` again starts the next meeting as a new session.
//! Every session is kept, and a Claude Code session (`a`, `meet ask`) answers questions
//! about any of them.

mod app;
mod ask;
mod claude;
mod engine_hook;
mod event_loop;
mod git;
mod layout;
mod live;
mod paths;
mod recorder;
mod selection;
mod settings;
mod shell;
mod store;
mod stream_json;
mod suggest;
mod term;
mod theme;
mod ui;
mod when;
mod wrap;

use anyhow::Result;
use clap::{Parser, Subcommand};
use settings::{Feature, Field};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "meet",
    version,
    about = "Talk to your repo: a live transcript, and the meeting's summary when it ends.",
    long_about = "Run `meet` (or `meet .`) inside a git checkout and press r: it records the microphone and system audio \
through the meet-rec engine and transcribes on-device; no Claude call is made while it records. Press x to stop \
the recording: the whole transcript then goes to Claude once, which writes the meeting's summary and its \
write-up; r again starts the next meeting as a new session. Press a for a Claude Code session that answers \
questions about the meeting. State lives in a SQLite database per user: every session's transcript, summary \
and write-up are kept (S lists them).",
    after_help = "Examples:\n  meet                          open in the current repo (r starts recording)\n  meet ~/code/app --no-system   microphone only, in another checkout\n  meet --replay meetings/2026-09-04_10-00/transcript.json --replay-speed 8\n                                replay a saved transcript instead of recording\n  meet record --duration 5      run the engine directly (flags pass through to meet-rec)\n  meet sessions                 print this repo's sessions\n  meet ask                      in another terminal: a Claude Code session to ask about the meeting being recorded here"
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

    /// Config file (meet.json) for the engine.
    #[arg(long, value_name = "FILE")]
    config: Option<String>,

    /// Where the recorder stores meetings (config: outputDir).
    #[arg(long, value_name = "DIR")]
    out_dir: Option<String>,

    /// Model for the summary writer that runs when the recording stops, this launch only
    /// (default: the settings, else sonnet).
    #[arg(long, value_name = "MODEL")]
    suggest_model: Option<String>,

    /// Effort for the summary writer, this launch only (default: the settings, else
    /// claude's own).
    #[arg(long, value_name = "LEVEL")]
    suggest_effort: Option<String>,

    /// Spending cap for the summary call.
    #[arg(long, default_value_t = 2.0, value_name = "USD")]
    suggest_budget: f64,

    /// Model for the question session (`a`, `meet ask`), this launch only (default: the
    /// settings, else sonnet).
    #[arg(long, value_name = "MODEL")]
    ask_model: Option<String>,

    /// Effort for the question session, this launch only (default: the settings, else
    /// claude's own).
    #[arg(long, value_name = "LEVEL")]
    ask_effort: Option<String>,

    /// Only transcribe; no summary.
    #[arg(long)]
    no_suggest: bool,

    /// The claude command, resolved by your login shell (an alias or function wins, as at a prompt).
    #[arg(long, default_value = "claude", env = "MEET_CLAUDE_BIN")]
    claude_bin: String,

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
    /// Print this repo's sessions, newest first.
    Sessions {
        /// Directory inside the git repository (default: the current directory).
        dir: Option<PathBuf>,
    },
    /// Open a Claude Code session to ask questions about a meeting: the one being recorded
    /// in this repo right now, else the newest one. Run it in another terminal while `meet`
    /// records (`a` in the TUI does it for you where it can).
    Ask {
        /// Directory inside the git repository (default: the current directory).
        dir: Option<PathBuf>,
        /// A session id from `meet sessions` (default: the live meeting, else the newest).
        #[arg(long, value_name = "ID")]
        meeting: Option<String>,
        /// Model for the session (default: --ask-model, else the settings, else sonnet).
        #[arg(long, value_name = "MODEL")]
        model: Option<String>,
        /// Effort for the session (default: --ask-effort, else the settings, else claude's own).
        #[arg(long, value_name = "LEVEL")]
        effort: Option<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Record { args }) => exec_engine(cli.recorder.as_deref(), "record", &args),
        Some(Command::Init { args }) => exec_engine(cli.recorder.as_deref(), "init", &args),
        Some(Command::Sessions { dir }) => {
            runtime()?.block_on(sessions(dir.or(cli.dir).unwrap_or_else(|| ".".into())))
        }
        Some(Command::Ask {
            dir,
            meeting,
            model,
            effort,
        }) => {
            let settings = settings::Settings::load()?;
            ask::run_cli(
                dir.or(cli.dir).unwrap_or_else(|| ".".into()),
                meeting,
                cli.claude_bin,
                settings::effective(
                    &settings,
                    Feature::Ask,
                    Field::Model,
                    model.or(cli.ask_model).as_deref(),
                ),
                settings::effective(
                    &settings,
                    Feature::Ask,
                    Field::Effort,
                    effort.or(cli.ask_effort).as_deref(),
                ),
            )
        }
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
            let mut overrides = settings::Overrides::default();
            overrides.set(Feature::Suggest, Field::Model, cli.suggest_model);
            overrides.set(Feature::Suggest, Field::Effort, cli.suggest_effort);
            overrides.set(Feature::Ask, Field::Model, cli.ask_model);
            overrides.set(Feature::Ask, Field::Effort, cli.ask_effort);
            let opts = event_loop::Opts {
                dir: cli.dir.unwrap_or_else(|| ".".into()),
                replay: cli.replay,
                replay_speed: cli.replay_speed,
                recorder: cli.recorder,
                record_args,
                claude_bin: cli.claude_bin,
                overrides,
                suggest_budget_usd: cli.suggest_budget,
                no_suggest: cli.no_suggest,
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
    for m in &meetings {
        println!("{}", ui::session_row(m, false, 160));
        println!("    id {}", m.id);
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
