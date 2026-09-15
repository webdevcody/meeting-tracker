//! Git, shelled out on purpose (nebula does the same): these calls are rare, and git's own
//! stderr is the best error text we could show.

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

/// The branch checked out; an error when HEAD is detached.
pub async fn current_branch(repo: &Path) -> Result<String> {
    let out = git(repo, &["branch", "--show-current"]).await?;
    let b = out.trim();
    if b.is_empty() {
        bail!("HEAD is detached");
    }
    Ok(b.to_string())
}

pub fn repo_name(repo: &Path) -> String {
    repo.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_branch_is_read_and_a_detached_head_has_none() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("my-app");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]).await.unwrap();
        git(&repo, &["config", "user.email", "t@t"]).await.unwrap();
        git(&repo, &["config", "user.name", "t"]).await.unwrap();
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"])
            .await
            .unwrap();
        assert_eq!(current_branch(&repo).await.unwrap(), "main");
        assert_eq!(repo_name(&toplevel(&repo).await.unwrap()), "my-app");
        git(&repo, &["checkout", "-q", "--detach"]).await.unwrap();
        assert!(current_branch(&repo).await.is_err());
        assert!(toplevel(tmp.path()).await.is_err(), "not a repository");
    }
}
