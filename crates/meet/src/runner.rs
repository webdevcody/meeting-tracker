//! Running an action item: a fresh git worktree on its own branch, a full-permission
//! headless Claude Code agent inside it, then `meet` itself does the deterministic tail —
//! commit anything left over, push, `gh pr create` with the description the agent wrote
//! (or an honest fallback) — and reports the pull request URL.
//!
//! A run that was cut off — `meet` quit or crashed under it, the user stopped it, it failed
//! — does not start over: the worktree is still there, and so is the agent's Claude
//! session, so it is resumed (`claude --resume`) and told to finish; a run that had already
//! reached the publish phase only redoes the tail. See [`plan_for`].

use crate::claude::{spawn_agent, AgentEvent, AgentSpawn};
use crate::stream_json::Usage;
use crate::store::{ActionItem, ItemStatus, RunOutcome, RunPhase};
use crate::{branch_name, config, git, hook, pr_body};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use tokio::sync::{mpsc, watch};

#[derive(Debug, Clone)]
pub struct RunContext {
    pub repo: PathBuf,
    pub base_branch: String,
    pub remote: String,
    pub claude_bin: String,
    pub gh_bin: String,
    pub agent_model: Option<String>,
    /// The agent's `--effort` (`None`: claude's own).
    pub agent_effort: Option<String>,
    /// The chunk summary the item came from, for the agent's context.
    pub meeting_context: String,
    /// The on-done prompt template (`agent.onDone` in meet.json, `--on-done`), delivered
    /// through the Stop hook when the agent is about to finish. `None`: no such step.
    pub on_done: Option<String>,
    /// This binary — what the Stop hook runs (`meet hook stop`).
    pub exe: PathBuf,
}

#[derive(Debug, Clone)]
pub enum RunEvent {
    Started {
        item_id: String,
        branch: String,
        worktree: String,
        log_path: String,
        /// Picked up an earlier run rather than starting one.
        resumed: bool,
    },
    /// The agent's stream named its Claude session.
    Session {
        item_id: String,
        session_id: String,
    },
    Phase {
        item_id: String,
        phase: RunPhase,
    },
    Activity {
        item_id: String,
        text: String,
    },
    /// What the agent spent. While it works, one API turn at a time (`ended: false`);
    /// when its process ends (`ended: true`), the whole run's usage, which replaces the
    /// turns' tally — or `None` when it died before reporting one, in which case the
    /// tally stands.
    Usage {
        item_id: String,
        usage: Option<Usage>,
        ended: bool,
    },
    Finished {
        item_id: String,
        outcome: RunOutcome,
    },
    /// The user (X) stopped the agent; the worktree and its commits stay.
    Stopped {
        item_id: String,
        outcome: RunOutcome,
    },
    Failed {
        item_id: String,
        outcome: RunOutcome,
    },
}

/// What a run does first, from what the item already has.
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// New branch, new worktree, new conversation.
    Fresh,
    /// The worktree is still there: the agent resumes in it — its own session when the
    /// stream ever named one, a new conversation over the same files otherwise.
    Resume {
        branch: String,
        worktree: PathBuf,
        session_id: Option<String>,
    },
    /// The agent had finished; only the commit → push → pull request tail is left.
    Publish { branch: String, worktree: PathBuf },
}

/// Pure: the plan for `item` given whether its worktree still exists on disk.
pub fn plan_for(item: &ActionItem, worktree_exists: bool) -> Plan {
    let (Some(branch), Some(wt)) = (&item.branch, &item.worktree_path) else {
        return Plan::Fresh;
    };
    if !worktree_exists {
        return Plan::Fresh;
    }
    let worktree = PathBuf::from(wt);
    match (item.status, item.phase) {
        (ItemStatus::Running | ItemStatus::Stopped | ItemStatus::Failed, RunPhase::Publish) => {
            Plan::Publish {
                branch: branch.clone(),
                worktree,
            }
        }
        (ItemStatus::Running | ItemStatus::Stopped | ItemStatus::Failed, RunPhase::Agent) => {
            Plan::Resume {
                branch: branch.clone(),
                worktree,
                session_id: item.session_id.clone(),
            }
        }
        _ => Plan::Fresh,
    }
}

/// The task text the agent receives.
pub fn agent_prompt(
    item: &ActionItem,
    ctx: &RunContext,
    branch: &str,
    worktree: &Path,
    run_dir: &Path,
) -> String {
    let why = if item.why.trim().is_empty() {
        String::new()
    } else {
        format!("\nWHY (from the meeting): {}\n", item.why.trim())
    };
    let context = if ctx.meeting_context.trim().is_empty() {
        String::new()
    } else {
        format!(
            "\nMEETING CONTEXT (what was being discussed):\n{}\n",
            ctx.meeting_context.trim()
        )
    };
    format!(
        r#"You are running headless inside `meet`; no one can answer questions, so decide and note your assumptions. You are in a fresh git worktree of "{repo_name}" at {worktree}, on branch `{branch}` created from `{base}`. The main checkout at {repo} must not be touched.

TASK — {title}

{prompt}
{why}{context}
HOW TO WORK
1. Read CLAUDE.md, AGENTS.md and README.md if present and follow the repository's conventions.
2. Implement the task completely. Where it leaves a choice, take the conventional option and record it under Notes in the PR body.
3. Run the cheapest relevant check the repo has (build, tests, lint) and fix what you broke. Do not sink time into unrelated failures; note them instead.
4. Commit everything on this branch with a clear message whose subject says what a user now gets. Do NOT push and do NOT open a pull request — meet does both once you finish.
5. Write the pull request text with the Write tool, then stop:
   - {title_file} — one line: what the user now gets, not what the diff did.
   - {body_file} — Markdown following the template below exactly; fill every <placeholder> and delete the guidance.
6. Your final message: one sentence saying what you did, or what blocked you.

PR BODY TEMPLATE
{template}"#,
        repo_name = git::repo_name(&ctx.repo),
        worktree = worktree.display(),
        base = ctx.base_branch,
        repo = ctx.repo.display(),
        title = item.title.trim(),
        prompt = item.prompt.trim(),
        title_file = run_dir.join("pr-title.txt").display(),
        body_file = run_dir.join("pr-body.md").display(),
        template = pr_body::template(),
    )
}

/// The text a resumed agent receives: where it was, what is left, then the original task
/// for reference (a resumed session remembers it; a new conversation over the same
/// worktree needs it).
pub fn resume_prompt(
    item: &ActionItem,
    ctx: &RunContext,
    branch: &str,
    worktree: &Path,
    run_dir: &Path,
) -> String {
    let how = match (item.status, item.error.as_deref().map(str::trim)) {
        (ItemStatus::Stopped, _) => "was stopped by the user".to_string(),
        (ItemStatus::Failed, Some(e)) if !e.is_empty() => format!("failed ({e})"),
        (ItemStatus::Failed, _) => "failed".to_string(),
        _ => "was interrupted when `meet` exited".to_string(),
    };
    format!(
        r#"You are being resumed inside `meet`. Your earlier run on the task below {how}. The worktree at {worktree} (branch `{branch}`, created from `{base}`) still holds everything you did: run `git status` and `git log {base}..HEAD --oneline` first to see where you were. Carry on from there and finish the task — do not start over and do not undo committed work. The rules of the original task still apply: commit on this branch, do NOT push and do NOT open a pull request, write {title_file} and {body_file}, and end with one sentence saying what you did or what blocked you.

THE ORIGINAL TASK, FOR REFERENCE
----------------------------------------
{original}"#,
        worktree = worktree.display(),
        base = ctx.base_branch,
        title_file = run_dir.join("pr-title.txt").display(),
        body_file = run_dir.join("pr-body.md").display(),
        original = agent_prompt(item, ctx, branch, worktree, run_dir),
    )
}

/// The on-done template with its placeholders filled for this run.
pub fn on_done_prompt(
    template: &str,
    item: &ActionItem,
    ctx: &RunContext,
    branch: &str,
    worktree: &Path,
    run_dir: &Path,
) -> String {
    let repo = ctx.repo.to_string_lossy().into_owned();
    let repo_name = git::repo_name(&ctx.repo);
    let worktree_s = worktree.to_string_lossy().into_owned();
    let run_dir_s = run_dir.to_string_lossy().into_owned();
    config::expand(
        template,
        &[
            ("title", item.title.trim()),
            ("prompt", item.prompt.trim()),
            ("why", item.why.trim()),
            ("branch", branch),
            ("worktree", &worktree_s),
            ("repo", &repo),
            ("repo_name", &repo_name),
            ("base_branch", &ctx.base_branch),
            ("run_dir", &run_dir_s),
            ("item_id", &item.id),
        ],
    )
}

/// The agent's environment: what the Stop hook and anything it runs can read.
fn agent_env(item: &ActionItem, ctx: &RunContext, branch: &str, worktree: &Path, run_dir: &Path) -> Vec<(String, String)> {
    vec![
        (hook::RUN_DIR_ENV.into(), run_dir.to_string_lossy().into_owned()),
        ("MEET_ITEM_ID".into(), item.id.clone()),
        ("MEET_ITEM_TITLE".into(), item.title.clone()),
        ("MEET_BRANCH".into(), branch.to_string()),
        ("MEET_WORKTREE".into(), worktree.to_string_lossy().into_owned()),
        ("MEET_REPO".into(), ctx.repo.to_string_lossy().into_owned()),
        ("MEET_BASE_BRANCH".into(), ctx.base_branch.clone()),
    ]
}

/// The error a run ends with when the user stopped its agent.
#[derive(Debug)]
struct StoppedByUser;

impl std::fmt::Display for StoppedByUser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("stopped by the user")
    }
}

impl std::error::Error for StoppedByUser {}

/// Start the run; events arrive on `tx` until `Finished`, `Stopped` or `Failed`. Flip
/// `stop` to `true` to stop the agent.
pub fn spawn_run(
    item: ActionItem,
    ctx: RunContext,
    tx: mpsc::UnboundedSender<RunEvent>,
    stop: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let item_id = item.id.clone();
        let run_dir = crate::paths::run_dir(&item.id);
        let mut outcome = RunOutcome {
            log_path: Some(run_dir.join("agent.log").to_string_lossy().into_owned()),
            ..Default::default()
        };
        match run(&item, &ctx, &run_dir, &mut outcome, &tx, stop).await {
            Ok(url) => {
                outcome.pr_url = Some(url);
                outcome.error = None;
                let _ = tx.send(RunEvent::Finished { item_id, outcome });
            }
            Err(e) if e.downcast_ref::<StoppedByUser>().is_some() => {
                outcome.error = Some(e.to_string());
                let _ = tx.send(RunEvent::Stopped { item_id, outcome });
            }
            Err(e) => {
                outcome.error = Some(format!("{e:#}"));
                let _ = tx.send(RunEvent::Failed { item_id, outcome });
            }
        }
    })
}

async fn run(
    item: &ActionItem,
    ctx: &RunContext,
    run_dir: &Path,
    outcome: &mut RunOutcome,
    tx: &mpsc::UnboundedSender<RunEvent>,
    stop: watch::Receiver<bool>,
) -> Result<String> {
    std::fs::create_dir_all(run_dir)?;
    let log_path = run_dir.join("agent.log").to_string_lossy().into_owned();
    let worktree_exists = item
        .worktree_path
        .as_deref()
        .is_some_and(|w| Path::new(w).is_dir());
    let (branch, worktree, resume) = match plan_for(item, worktree_exists) {
        Plan::Publish { branch, worktree } => {
            outcome.branch = Some(branch.clone());
            outcome.worktree_path = Some(worktree.to_string_lossy().into_owned());
            let _ = tx.send(RunEvent::Started {
                item_id: item.id.clone(),
                branch: branch.clone(),
                worktree: worktree.to_string_lossy().into_owned(),
                log_path,
                resumed: true,
            });
            let _ = tx.send(RunEvent::Activity {
                item_id: item.id.clone(),
                text: "publishing the existing branch".into(),
            });
            let last_word = std::fs::read_to_string(run_dir.join("agent.log"))
                .ok()
                .and_then(|l| l.lines().last().map(str::to_string))
                .unwrap_or_default();
            return publish(item, ctx, run_dir, &worktree, &branch, &last_word, tx).await;
        }
        Plan::Resume {
            branch,
            worktree,
            session_id,
        } => (branch, worktree, Some(session_id)),
        Plan::Fresh => {
            // 1. A branch of its own, in a worktree of its own.
            let base = branch_name::slugify(&item.title);
            let mut taken = Vec::new();
            for n in std::iter::once(base.clone()).chain((2..20).map(|n| format!("{base}-{n}")))
            {
                if git::branch_exists(&ctx.repo, &n).await
                    || git::worktree_dir(&ctx.repo, &n).exists()
                {
                    taken.push(n);
                } else {
                    break;
                }
            }
            let branch = branch_name::unique(&base, |c| taken.iter().any(|t| t == c));
            let worktree = git::add_worktree(&ctx.repo, &branch, &ctx.base_branch)
                .await
                .context("create the worktree")?;
            (branch, worktree, None)
        }
    };
    outcome.branch = Some(branch.clone());
    outcome.worktree_path = Some(worktree.to_string_lossy().into_owned());
    let _ = tx.send(RunEvent::Started {
        item_id: item.id.clone(),
        branch: branch.clone(),
        worktree: worktree.to_string_lossy().into_owned(),
        log_path,
        resumed: resume.is_some(),
    });
    let _ = tx.send(RunEvent::Phase {
        item_id: item.id.clone(),
        phase: RunPhase::Agent,
    });

    // 2. The agent.
    let prompt = match &resume {
        Some(_) => resume_prompt(item, ctx, &branch, &worktree, run_dir),
        None => agent_prompt(item, ctx, &branch, &worktree, run_dir),
    };
    std::fs::write(run_dir.join("prompt.md"), &prompt)?;
    let settings = match &ctx.on_done {
        Some(template) => {
            let text = on_done_prompt(template, item, ctx, &branch, &worktree, run_dir);
            std::fs::write(run_dir.join(hook::ON_DONE_FILE), text)?;
            Some(hook::write_settings(run_dir, &ctx.exe)?)
        }
        None => None,
    };
    let env = agent_env(item, ctx, &branch, &worktree, run_dir);
    let mut session = resume.clone().flatten();
    let (agent_failed, agent_text) = loop {
        let (agent_tx, mut agent_rx) = mpsc::unbounded_channel::<AgentEvent>();
        let task = spawn_agent(
            AgentSpawn {
                bin: &ctx.claude_bin,
                cwd: &worktree,
                prompt: prompt.clone(),
                model: ctx.agent_model.as_deref(),
                effort: ctx.agent_effort.as_deref(),
                run_dir,
                resume: session.as_deref(),
                settings: settings.as_deref(),
                env: env.clone(),
            },
            stop.clone(),
            agent_tx,
        )
        .context("start claude")?;
        let mut ended = None;
        while let Some(ev) = agent_rx.recv().await {
            match ev {
                AgentEvent::Session(session_id) => {
                    let _ = tx.send(RunEvent::Session {
                        item_id: item.id.clone(),
                        session_id,
                    });
                }
                AgentEvent::Activity(text) => {
                    let _ = tx.send(RunEvent::Activity {
                        item_id: item.id.clone(),
                        text,
                    });
                }
                AgentEvent::Usage(usage) => {
                    let _ = tx.send(RunEvent::Usage {
                        item_id: item.id.clone(),
                        usage: Some(usage),
                        ended: false,
                    });
                }
                AgentEvent::Ended {
                    is_error,
                    stopped,
                    saw_init,
                    text,
                    usage,
                    ..
                } => {
                    let _ = tx.send(RunEvent::Usage {
                        item_id: item.id.clone(),
                        usage,
                        ended: true,
                    });
                    ended = Some((is_error, stopped, saw_init, text));
                    break;
                }
            }
        }
        let _ = task.await;
        let (is_error, stopped, saw_init, text) =
            ended.unwrap_or((true, false, false, "the agent produced no result".into()));
        if stopped {
            bail!(StoppedByUser);
        }
        // A `--resume` that died before announcing a session is one Claude no longer has
        // (sessions are pruned): start a new conversation over the same worktree instead.
        if is_error && !saw_init && session.is_some() {
            let _ = tx.send(RunEvent::Activity {
                item_id: item.id.clone(),
                text: "session gone; starting a new conversation in the worktree".into(),
            });
            note(run_dir, &format!("── could not resume the session ({}); starting a new conversation in the same worktree", crate::stream_json::excerpt(&text, 120)));
            session = None;
            continue;
        }
        break (is_error, text);
    };

    // 3. Whatever it left uncommitted is still its work.
    let _ = tx.send(RunEvent::Phase {
        item_id: item.id.clone(),
        phase: RunPhase::Publish,
    });
    let _ = tx.send(RunEvent::Activity {
        item_id: item.id.clone(),
        text: "checking the branch".into(),
    });
    if git::has_uncommitted_changes(&worktree)
        .await
        .unwrap_or(false)
    {
        git::commit_all(&worktree, &item.title)
            .await
            .context("commit the agent's changes")?;
    }
    let commits = git::commits_ahead(&worktree, &ctx.base_branch).await?;
    if commits.is_empty() {
        // Nothing to publish: the next Enter resumes the agent, not the tail.
        let _ = tx.send(RunEvent::Phase {
            item_id: item.id.clone(),
            phase: RunPhase::Agent,
        });
        if agent_failed {
            bail!(
                "the agent made no commits and ended with an error: {}",
                crate::stream_json::excerpt(&agent_text, 200)
            );
        }
        bail!(
            "the agent made no changes: {}",
            crate::stream_json::excerpt(&agent_text, 200)
        );
    }

    publish(item, ctx, run_dir, &worktree, &branch, &agent_text, tx).await
}

fn note(run_dir: &Path, line: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(run_dir.join("agent.log"))
    {
        let _ = writeln!(f, "{line}");
    }
}

/// The deterministic tail: push the branch, open the pull request with the text the agent
/// wrote (or an honest fallback), return its URL.
async fn publish(
    item: &ActionItem,
    ctx: &RunContext,
    run_dir: &Path,
    worktree: &Path,
    branch: &str,
    agent_text: &str,
    tx: &mpsc::UnboundedSender<RunEvent>,
) -> Result<String> {
    let commits = git::commits_ahead(worktree, &ctx.base_branch).await?;
    if !git::has_remote(&ctx.repo, &ctx.remote).await {
        bail!(
            "committed {} change(s) on `{}` in {}, but the repository has no `{}` remote to push to",
            commits.len(),
            branch,
            worktree.display(),
            ctx.remote
        );
    }
    let _ = tx.send(RunEvent::Activity {
        item_id: item.id.clone(),
        text: format!("pushing {branch}"),
    });
    git::push_branch(worktree, &ctx.remote, branch)
        .await
        .context("push the branch")?;

    let title = std::fs::read_to_string(run_dir.join("pr-title.txt"))
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| item.title.clone());
    let body_file = run_dir.join("pr-body.md");
    let body_ok = std::fs::read_to_string(&body_file).is_ok_and(|b| b.trim().len() > 40);
    if !body_ok {
        std::fs::write(
            &body_file,
            pr_body::fallback(item, &ctx.base_branch, &commits, agent_text),
        )?;
    }
    let _ = tx.send(RunEvent::Activity {
        item_id: item.id.clone(),
        text: "opening the pull request".into(),
    });
    let url = git::gh_pr_create(
        &ctx.gh_bin,
        worktree,
        &ctx.base_branch,
        branch,
        &title,
        &body_file,
    )
    .await
    .context("open the pull request")?;
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item() -> ActionItem {
        ActionItem {
            id: "01abc".into(),
            meeting_id: "m".into(),
            chunk_id: None,
            repo_path: "/w/app".into(),
            title: "Add a --json flag".into(),
            prompt: "I want a --json flag on record…".into(),
            why: "so the TUI can parse it".into(),
            status: ItemStatus::Suggested,
            branch: None,
            worktree_path: None,
            pr_url: None,
            log_path: None,
            error: None,
            session_id: None,
            phase: RunPhase::Agent,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn ctx() -> RunContext {
        RunContext {
            repo: "/w/app".into(),
            base_branch: "main".into(),
            remote: "origin".into(),
            claude_bin: "claude".into(),
            gh_bin: "gh".into(),
            agent_model: None,
            agent_effort: None,
            meeting_context: "we talked about json".into(),
            on_done: None,
            exe: "/usr/local/bin/meet".into(),
        }
    }

    #[test]
    fn the_agent_prompt_names_the_worktree_the_files_and_the_rules() {
        let p = agent_prompt(
            &item(),
            &ctx(),
            "add-a-json-flag",
            Path::new("/w/app-worktrees/add-a-json-flag"),
            Path::new("/data/runs/01abc"),
        );
        assert!(p.contains("on branch `add-a-json-flag` created from `main`"));
        assert!(p.contains("/data/runs/01abc/pr-title.txt"));
        assert!(p.contains("/data/runs/01abc/pr-body.md"));
        assert!(p.contains("Do NOT push"));
        assert!(p.contains("WHY (from the meeting): so the TUI can parse it"));
        assert!(p.contains("we talked about json"));
        assert!(p.contains(pr_body::FOOTER));
    }

    #[test]
    fn the_resume_prompt_says_how_it_ended_and_carries_the_task() {
        let mut it = item();
        it.status = ItemStatus::Stopped;
        let p = resume_prompt(
            &it,
            &ctx(),
            "add-a-json-flag",
            Path::new("/w/app-worktrees/add-a-json-flag"),
            Path::new("/data/runs/01abc"),
        );
        assert!(p.starts_with("You are being resumed"));
        assert!(p.contains("was stopped by the user"));
        assert!(p.contains("git log main..HEAD"));
        assert!(p.contains("do not start over"));
        assert!(p.contains("TASK — Add a --json flag"), "the original task is included");

        it.status = ItemStatus::Failed;
        it.error = Some("push the branch: no remote".into());
        let p = resume_prompt(&it, &ctx(), "b", Path::new("/wt"), Path::new("/r"));
        assert!(p.contains("failed (push the branch: no remote)"));

        it.status = ItemStatus::Running;
        let p = resume_prompt(&it, &ctx(), "b", Path::new("/wt"), Path::new("/r"));
        assert!(p.contains("interrupted when `meet` exited"));
    }

    #[test]
    fn the_plan_follows_status_phase_and_whether_the_worktree_survived() {
        let mut it = item();
        assert_eq!(plan_for(&it, false), Plan::Fresh, "never ran");
        it.branch = Some("b".into());
        it.worktree_path = Some("/wt".into());
        it.status = ItemStatus::Running;
        assert_eq!(plan_for(&it, false), Plan::Fresh, "worktree deleted by hand");
        assert_eq!(
            plan_for(&it, true),
            Plan::Resume {
                branch: "b".into(),
                worktree: "/wt".into(),
                session_id: None
            },
            "interrupted before the stream named a session: new conversation, same worktree"
        );
        it.session_id = Some("sid".into());
        assert!(matches!(
            plan_for(&it, true),
            Plan::Resume { session_id: Some(ref s), .. } if s == "sid"
        ));
        it.status = ItemStatus::Stopped;
        assert!(matches!(plan_for(&it, true), Plan::Resume { .. }));
        it.status = ItemStatus::Failed;
        assert!(matches!(plan_for(&it, true), Plan::Resume { .. }));
        it.phase = RunPhase::Publish;
        assert_eq!(
            plan_for(&it, true),
            Plan::Publish {
                branch: "b".into(),
                worktree: "/wt".into()
            },
            "the agent was done; only the tail is left"
        );
        it.status = ItemStatus::Running;
        assert!(matches!(plan_for(&it, true), Plan::Publish { .. }));
        it.status = ItemStatus::Done;
        assert_eq!(plan_for(&it, true), Plan::Fresh, "done is not resumable");
    }

    #[test]
    fn the_on_done_prompt_fills_the_placeholders_and_the_env_names_the_run() {
        let it = item();
        let c = ctx();
        let p = on_done_prompt(
            "Comment on the issue about {{title}} from branch {{branch}} in {{repo_name}} ({{run_dir}}).",
            &it,
            &c,
            "add-a-json-flag",
            Path::new("/wt"),
            Path::new("/data/runs/01abc"),
        );
        assert_eq!(
            p,
            "Comment on the issue about Add a --json flag from branch add-a-json-flag in app (/data/runs/01abc)."
        );
        let env = agent_env(&it, &c, "add-a-json-flag", Path::new("/wt"), Path::new("/r"));
        assert!(env.contains(&("MEET_RUN_DIR".to_string(), "/r".to_string())));
        assert!(env.contains(&("MEET_BRANCH".to_string(), "add-a-json-flag".to_string())));
        assert!(env.contains(&("MEET_REPO".to_string(), "/w/app".to_string())));
    }
}
