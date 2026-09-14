//! Starting `claude` the way a typed command line would: through the user's login and
//! interactive shell. This is nebula's approach for every agent session (its daemon's
//! `login_shell_wrap`), copied here so `meet` and nebula can never disagree about what
//! "run claude" means.
//!
//! `$SHELL -l -i -c 'unset …; export …; claude args…'`: `-l` makes it a login shell, so zsh
//! sources /etc/zprofile (path_helper) and ~/.zprofile; `-i` makes it interactive, so
//! ~/.zshrc runs too. Between them the child sees the PATH a Terminal.app tab has, and — the
//! point — the command word goes in bare, so the shell resolves it the way a typed one is
//! resolved: an alias or function from those files wins over the binary on PATH. That is
//! where a work setup reroutes `claude` through a wrapper (another backend, another login,
//! per-directory logic); `Command::new("claude")` skips all of it.
//!
//! Without an `exec` the shell stays in charge of the launch: zsh execs a plain last
//! command itself, so the agent is still the direct child there; bash, and any command that
//! resolves to a function, run it as a job under the shell instead — and that job can sit
//! in a process group of its own (an interactive bash turns job control on even with no
//! terminal; zsh does on a pseudo-terminal). An interactive shell also ignores SIGTERM. So a
//! signal to the shell's group alone can miss the agent entirely: every stop and kill sweeps
//! all the process groups under the shell ([`process_groups_under`]), as nebula's does.
//!
//! The prelude restates what the child is *after* the rc files have run: `TERM` and
//! `COLORTERM` name the pane's own grid, and a `NO_COLOR` / `FORCE_COLOR` a login-only
//! profile exports is dropped (Claude Code takes either as "no colour at all").

use std::io;

/// `$SHELL -l -i -c <cmd>`: a login *and* interactive shell.
pub const LOGIN_SHELL_ARGS: [&str; 3] = ["-l", "-i", "-c"];

/// The shell the user's terminal runs, else `/bin/sh`.
pub fn user_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
}

/// `bin args…` as the user's login shell would run it: the program and argv to spawn.
pub fn claude_launch(bin: &str, args: &[String]) -> (String, Vec<String>) {
    login_shell_wrap(&user_shell(), bin, args)
}

/// Wrap `program args…` in `shell -l -i -c '<prelude>; program args…'`. `program` goes in
/// bare when it is a plain word or path (a quoted word is exempt from alias expansion in
/// every shell); every argument is single-quoted.
pub fn login_shell_wrap(shell: &str, program: &str, args: &[String]) -> (String, Vec<String>) {
    let mut cmdline = String::from(
        "unset NO_COLOR FORCE_COLOR; export TERM=xterm-256color COLORTERM=truecolor; ",
    );
    cmdline.push_str(&command_word(program));
    for arg in args {
        cmdline.push(' ');
        cmdline.push_str(&shell_quote(arg));
    }
    let args = LOGIN_SHELL_ARGS
        .iter()
        .map(|s| s.to_string())
        .chain([cmdline])
        .collect();
    (shell.to_string(), args)
}

/// Put the child in a session of its own. An interactive shell off a terminal must not
/// reach the one `meet` is drawing on: zsh's job-control init opens /dev/tty and makes
/// itself the foreground process group, stopping the TUI with SIGTTIN. With no controlling
/// terminal there is nothing for it to take.
pub fn own_session(cmd: &mut tokio::process::Command) {
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

/// SIGKILLs every process group under a child when dropped while still armed: the login
/// shell, the `claude` it started, and the tools that one is running. tokio's
/// `kill_on_drop` reaches the shell's pid alone, and meet drops its runtime on quit — so
/// without this a `claude` that runs as a job under the shell would keep going with nobody
/// reading it. Disarm it once the child is reaped, so a recycled pid is never signalled.
pub struct GroupKill(Option<u32>);

impl GroupKill {
    pub fn new(pid: Option<u32>) -> Self {
        Self(pid)
    }

    pub fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for GroupKill {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            signal_groups(&process_groups_under(pid), libc::SIGKILL);
        }
    }
}

/// Every process group with a member in `root`'s subtree, `root`'s own first (the child
/// leads its session, so that group is its pid, named even when `ps` fails). Take it while
/// the tree is intact: a shell that dies of a signal leaves its job to init, where no walk
/// from its pid would find it afterwards.
pub fn process_groups_under(root: u32) -> Vec<u32> {
    let table = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,pgid="])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    process_groups_in_table(&table, root)
}

/// `sig` to each of `groups`.
pub fn signal_groups(groups: &[u32], sig: i32) {
    for &g in groups {
        unsafe {
            libc::killpg(g as i32, sig);
        }
    }
}

/// Pure core of [`process_groups_under`]: `table` is `ps -axo pid=,ppid=,pgid=` output.
fn process_groups_in_table(table: &str, root: u32) -> Vec<u32> {
    let mut children: std::collections::HashMap<u32, Vec<u32>> = Default::default();
    let mut group_of: std::collections::HashMap<u32, u32> = Default::default();
    for line in table.lines() {
        let mut cols = line.split_whitespace().map(|c| c.parse::<u32>().ok());
        let (Some(Some(pid)), Some(Some(ppid)), Some(Some(pgid))) =
            (cols.next(), cols.next(), cols.next())
        else {
            continue;
        };
        children.entry(ppid).or_default().push(pid);
        group_of.insert(pid, pgid);
    }
    let mut groups = vec![root];
    let mut stack = vec![root];
    let mut seen = std::collections::HashSet::new();
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        if let Some(&pgid) = group_of.get(&pid) {
            if !groups.contains(&pgid) {
                groups.push(pgid);
            }
        }
        if let Some(kids) = children.get(&pid) {
            stack.extend(kids);
        }
    }
    groups
}

/// `program` as the command word of a shell line: bare when it is a plain name (letters,
/// digits, `-`, `_`, `.`, `/`), single-quoted otherwise.
fn command_word(program: &str) -> String {
    let plain = !program.is_empty()
        && program
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./".contains(&b));
    if plain {
        program.to_string()
    } else {
        shell_quote(program)
    }
}

/// Single-quote `arg` for a POSIX shell; the only escape needed is `'`.
fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What every launch runs once the login shell's files have run, ahead of the
    /// command itself.
    const PRELUDE: &str =
        "unset NO_COLOR FORCE_COLOR; export TERM=xterm-256color COLORTERM=truecolor;";

    /// A kill must reach a job the login shell forked into a group of its own, not just
    /// the shell's group.
    #[test]
    fn process_groups_cover_a_job_the_shell_forked_into_its_own_group() {
        // shell 20 leads its session (group 20); the agent 21 is a job in group 21 with a
        // worker 22; 30 is another session, 99 unrelated.
        let table =
            " 20    10    20\n 21    20    21\n 22    21    21\n 30    10    30\n 99     1    99\n";
        assert_eq!(process_groups_in_table(table, 20), vec![20, 21]);
        // A failed sweep still names the leader's own group.
        assert_eq!(process_groups_in_table("", 20), vec![20]);
    }

    /// The case that needs the sweep: an interactive bash with no terminal still runs the
    /// command as a job in a process group of its own, and ignores SIGTERM itself. SIGTERM
    /// to the shell's group alone leaves the job (and the stdout it holds) running; to every
    /// group under the shell, it ends. `HOME` is a temp dir so no real profile runs.
    #[test]
    fn a_sweep_stops_a_job_an_interactive_bash_put_in_its_own_group() {
        use std::io::Read;
        use std::os::unix::process::CommandExt;
        let home = tempfile::tempdir().unwrap();
        let fake = home.path().join("claude");
        std::fs::write(&fake, "#!/bin/sh\necho up\nsleep 30\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (program, args) = login_shell_wrap("/bin/bash", fake.to_str().unwrap(), &[]);
        let mut cmd = std::process::Command::new(program);
        cmd.args(&args)
            .env("HOME", home.path())
            .env_remove("BASH_ENV")
            .env_remove("ENV")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        unsafe {
            cmd.pre_exec(|| match libc::setsid() {
                -1 => Err(std::io::Error::last_os_error()),
                _ => Ok(()),
            });
        }
        let mut child = cmd.spawn().unwrap();
        let pid = child.id();
        let mut stdout = child.stdout.take().unwrap();
        let mut up = [0u8; 3];
        stdout.read_exact(&mut up).unwrap();
        assert_eq!(&up, b"up\n");
        let groups = process_groups_under(pid);
        assert!(
            groups.len() > 1,
            "bash ran the job in its own group: {groups:?}"
        );
        signal_groups(&groups, libc::SIGTERM);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut rest = Vec::new();
            let _ = stdout.read_to_end(&mut rest);
            let _ = tx.send(child.wait().ok());
        });
        let ended = rx.recv_timeout(std::time::Duration::from_secs(5));
        if ended.is_err() {
            signal_groups(&process_groups_under(pid), libc::SIGKILL);
        }
        assert!(
            ended.is_ok(),
            "the shell and its job ended within 5s of the sweep"
        );
    }

    #[test]
    fn login_shell_wrap_quotes_args_and_leaves_the_command_word_bare() {
        let (program, args) = login_shell_wrap(
            "/bin/zsh",
            "claude",
            &["--resume".to_string(), "sid-1".to_string()],
        );
        assert_eq!(program, "/bin/zsh");
        assert_eq!(
            args,
            vec![
                "-l",
                "-i",
                "-c",
                &format!("{PRELUDE} claude '--resume' 'sid-1'")
            ]
        );
        // Single quotes in an arg survive the wrapping.
        let (_, args) = login_shell_wrap("/bin/zsh", "echo", &["it's".to_string()]);
        assert_eq!(args[3], format!(r"{PRELUDE} echo 'it'\''s'"));
        // A path is a plain word too (the e2e's fake claude); a command word that is not
        // is quoted like an argument.
        let (_, args) = login_shell_wrap("/bin/zsh", "/opt/bin/claude", &[]);
        assert_eq!(args[3], format!("{PRELUDE} /opt/bin/claude"));
        let (_, args) = login_shell_wrap("/bin/zsh", "my tool", &[]);
        assert_eq!(args[3], format!("{PRELUDE} 'my tool'"));
    }

    /// The command word resolves through the shell, so an alias or function from the rc
    /// files takes precedence over the binary on PATH — the reason a launch goes through
    /// the login shell at all. A function stands in for the alias: every POSIX sh honours
    /// one in a `-c` string.
    #[test]
    fn login_shell_wrap_lets_the_shell_resolve_the_command() {
        let (_, args) = login_shell_wrap(
            "/bin/sh",
            "claude",
            &["--resume".to_string(), "sid-1".to_string()],
        );
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                "claude() {{ printf 'routed %s' \"$*\"; }}; {}",
                args[3]
            ))
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "routed --resume sid-1",
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The shape of the concern itself: zsh, `-l -i -c`, and an alias in `.zshrc` that
    /// reroutes `claude` — honoured, with the directory the launch was given as `$PWD`,
    /// and with stdin still reaching the command (the headless calls send the prompt on
    /// it). Skipped where there is no zsh.
    #[test]
    fn login_shell_wrap_honours_a_zshrc_alias() {
        use std::io::Write;
        use std::os::unix::process::CommandExt;
        if std::process::Command::new("zsh")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .status()
            .is_err()
        {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".zshrc"),
            "alias claude='echo routed in $PWD:'\nclaude_stdin() { cat; }\n",
        )
        .unwrap();
        let (program, args) = login_shell_wrap(
            "zsh",
            "claude",
            &["--resume".to_string(), "sid-1".to_string()],
        );
        let mut cmd = std::process::Command::new(program);
        cmd.args(&args)
            .env("ZDOTDIR", home.path())
            .current_dir(home.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        // Own session, as `own_session` gives the headless calls: an interactive zsh must
        // not make itself the foreground of the terminal running the tests.
        unsafe {
            cmd.pre_exec(|| match libc::setsid() {
                -1 => Err(std::io::Error::last_os_error()),
                _ => Ok(()),
            });
        }
        let mut child = cmd.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"the prompt\n")
            .unwrap();
        let out = child.wait_with_output().unwrap();
        let cwd = std::fs::canonicalize(home.path()).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim_end(),
            format!("routed in {}: --resume sid-1", cwd.display()),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        // Piped stdin passes through the shell to the command.
        let (program, args) = login_shell_wrap("zsh", "claude_stdin", &[]);
        let mut cmd = std::process::Command::new(program);
        cmd.args(&args)
            .env("ZDOTDIR", home.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        unsafe {
            cmd.pre_exec(|| match libc::setsid() {
                -1 => Err(std::io::Error::last_os_error()),
                _ => Ok(()),
            });
        }
        let mut child = cmd.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"the prompt\n")
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "the prompt\n");
    }
}
