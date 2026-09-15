//! Where `meet` keeps its own state: one SQLite database and one directory of live meeting
//! files per user, like nebula — never inside the repository being worked on.
//! `MEET_DATA_DIR` overrides everything (tests, a second instance).

use std::path::PathBuf;

pub const DATA_DIR_ENV: &str = "MEET_DATA_DIR";

pub fn non_empty(var: &str) -> Option<String> {
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

/// One directory per meeting: the live transcript and the running summary, written as
/// the meeting goes for anything outside the TUI (the question session) to read.
pub fn sessions_dir() -> PathBuf {
    data_dir().join("sessions")
}

pub fn session_dir(meeting_id: &str) -> PathBuf {
    sessions_dir().join(meeting_id)
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
        assert_eq!(session_dir("m1"), dir.join("sessions").join("m1"));
        match previous {
            Some(v) => std::env::set_var(DATA_DIR_ENV, v),
            None => std::env::remove_var(DATA_DIR_ENV),
        }
    }
}
