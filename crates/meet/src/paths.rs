//! Where `meet` keeps its own state: one SQLite database and one directory of agent run
//! logs per user, like nebula — never inside the repository being worked on.
//! `MEET_DATA_DIR` overrides everything (tests, a second instance).

use std::path::PathBuf;

pub const DATA_DIR_ENV: &str = "MEET_DATA_DIR";

fn non_empty(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|v| !v.trim().is_empty())
}

/// `~/Library/Application Support/dev.meet.meet` on macOS, `~/.local/share/meet` on Linux.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = non_empty(DATA_DIR_ENV) {
        return PathBuf::from(dir);
    }
    directories::ProjectDirs::from("dev", "meet", "meet")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| {
            let home = non_empty("HOME").unwrap_or_else(|| ".".into());
            PathBuf::from(home).join(".meet")
        })
}

pub fn db_path() -> PathBuf {
    data_dir().join("meet.db")
}

/// One directory per agent run: the human-readable log, the raw stream, the PR text.
pub fn runs_dir() -> PathBuf {
    data_dir().join("runs")
}

pub fn run_dir(item_id: &str) -> PathBuf {
    runs_dir().join(item_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_wins_and_children_hang_off_it() {
        let dir = std::env::temp_dir().join(format!("meet-paths-{}", std::process::id()));
        let previous = std::env::var_os(DATA_DIR_ENV);
        std::env::set_var(DATA_DIR_ENV, &dir);
        assert_eq!(data_dir(), dir);
        assert_eq!(db_path(), dir.join("meet.db"));
        assert_eq!(run_dir("abc"), dir.join("runs").join("abc"));
        match previous {
            Some(v) => std::env::set_var(DATA_DIR_ENV, v),
            None => std::env::remove_var(DATA_DIR_ENV),
        }
    }
}
