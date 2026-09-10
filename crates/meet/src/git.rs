//! Git and GitHub CLI operations, shelled out on purpose (nebula does the same): these are
//! rare, user-initiated, and git's own stderr is the best error text we could show. The
//! worktree layout is nebula's — `<repo>/../<repo-name>-worktrees/<branch>` — so a `meet`
//! branch and a nebula branch of the same repo sit side by side.

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use tokio::process::Command;

async fn run(program: &str, dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .await
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow!("{program} was not found on your PATH")
            } else {
                anyhow::Error::new(e).context(format!("run {program}"))
            }
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        bail!(
            "{program} {} failed: {}",
            args.first().copied().unwrap_or(""),
            if stderr.is_empty() { stdout } else { stderr }
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub async fn git(repo: &Path, args: &[&str]) -> Result<String> {
    run("git", repo, args).await
}

/// The checkout's top level, or an error naming the directory when it is not a repo.
pub async fn toplevel(dir: &Path) -> Result<PathBuf> {
    let out = git(dir, &["rev-parse", "--show-toplevel"])
        .await
        .with_context(|| format!("{} is not inside a git repository", dir.display()))?;
    Ok(PathBuf::from(out.trim()))
}

pub async fn current_branch(repo: &Path) -> Result<String> {
    let out = git(repo, &["branch", "--show-current"]).await?;
    let b = out.trim();
    if b.is_empty() {
        bail!("HEAD is detached; check out a branch first so the pull request has a base");
    }
    Ok(b.to_string())
}

pub async fn branch_exists(repo: &Path, name: &str) -> bool {
    git(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{name}"),
        ],
    )
    .await
    .is_ok()
}

pub fn repo_name(repo: &Path) -> String {
    repo.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into())
}

/// `<repo>/../<repo-name>-worktrees/<branch>` (slashes in the branch become dashes).
pub fn worktree_dir(repo: &Path, branch: &str) -> PathBuf {
    repo.parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(format!("{}-worktrees", repo_name(repo)))
        .join(branch.replace('/', "-"))
}

/// `git worktree add <dir> -b <branch> <base>`; the branch must not exist yet.
pub async fn add_worktree(repo: &Path, branch: &str, base: &str) -> Result<PathBuf> {
    let path = worktree_dir(repo, branch);
    if path.exists() {
        bail!("worktree path already exists: {}", path.display());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let path_str = path.to_string_lossy().into_owned();
    git(repo, &["worktree", "add", &path_str, "-b", branch, base]).await?;
    Ok(path)
}

/// Commits on the worktree's HEAD that `base` does not have, oldest first.
pub async fn commits_ahead(worktree: &Path, base: &str) -> Result<Vec<String>> {
    let out = git(
        worktree,
        &["log", "--reverse", "--format=%s", &format!("{base}..HEAD")],
    )
    .await?;
    Ok(out
        .lines()
        .map(str::to_string)
        .filter(|l| !l.is_empty())
        .collect())
}

pub async fn has_uncommitted_changes(worktree: &Path) -> Result<bool> {
    let out = git(worktree, &["status", "--porcelain"]).await?;
    Ok(!out.trim().is_empty())
}

pub async fn commit_all(worktree: &Path, message: &str) -> Result<()> {
    git(worktree, &["add", "-A"]).await?;
    git(worktree, &["commit", "-q", "-m", message]).await?;
    Ok(())
}

pub async fn has_remote(repo: &Path, name: &str) -> bool {
    git(repo, &["remote", "get-url", name]).await.is_ok()
}

pub async fn push_branch(worktree: &Path, remote: &str, branch: &str) -> Result<()> {
    git(worktree, &["push", "-u", remote, branch]).await?;
    Ok(())
}

/// `gh pr create` and the URL it prints.
pub async fn gh_pr_create(
    gh: &str,
    worktree: &Path,
    base: &str,
    head: &str,
    title: &str,
    body_file: &Path,
) -> Result<String> {
    let body = body_file.to_string_lossy().into_owned();
    let out = run(
        gh,
        worktree,
        &[
            "pr",
            "create",
            "--base",
            base,
            "--head",
            head,
            "--title",
            title,
            "--body-file",
            &body,
        ],
    )
    .await?;
    pr_url_in(&out)
        .ok_or_else(|| anyhow!("gh pr create printed no pull request URL:\n{}", out.trim()))
}

/// The pull request URL somewhere in `gh`'s output (it is the last line on success, but a
/// notice can follow it).
pub fn pr_url_in(text: &str) -> Option<String> {
    text.split_whitespace()
        .rev()
        .find(|w| w.starts_with("https://") && w.contains("/pull/"))
        .map(|w| w.trim_end_matches(['.', ',', ')']).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn init_repo(dir: &Path) {
        git(dir, &["init", "-q", "-b", "main"]).await.unwrap();
        git(dir, &["config", "user.email", "t@t"]).await.unwrap();
        git(dir, &["config", "user.name", "t"]).await.unwrap();
        git(dir, &["commit", "-q", "--allow-empty", "-m", "init"])
            .await
            .unwrap();
    }

    #[test]
    fn worktree_dir_follows_the_nebula_layout() {
        assert_eq!(
            worktree_dir(Path::new("/w/my-app"), "feat/x"),
            PathBuf::from("/w/my-app-worktrees/feat-x")
        );
    }

    #[test]
    fn finds_the_pr_url() {
        assert_eq!(
            pr_url_in(
                "Creating pull request for x into main in o/r\n\nhttps://github.com/o/r/pull/12\n"
            ),
            Some("https://github.com/o/r/pull/12".into())
        );
        assert_eq!(pr_url_in("nothing here"), None);
    }

    #[tokio::test]
    async fn worktree_commit_and_ahead_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo).await;
        assert_eq!(current_branch(&repo).await.unwrap(), "main");
        assert!(!branch_exists(&repo, "topic").await);

        let wt = add_worktree(&repo, "topic", "main").await.unwrap();
        assert!(branch_exists(&repo, "topic").await);
        assert!(commits_ahead(&wt, "main").await.unwrap().is_empty());
        assert!(!has_uncommitted_changes(&wt).await.unwrap());

        std::fs::write(wt.join("f.txt"), "hi").unwrap();
        assert!(has_uncommitted_changes(&wt).await.unwrap());
        commit_all(&wt, "Add f").await.unwrap();
        assert_eq!(
            commits_ahead(&wt, "main").await.unwrap(),
            vec!["Add f".to_string()]
        );
        assert!(!has_remote(&repo, "origin").await);
        assert!(
            add_worktree(&repo, "topic", "main").await.is_err(),
            "the path exists"
        );
    }
}
