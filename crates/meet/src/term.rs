//! An embedded terminal: a child process on a pseudo-terminal, its output parsed into a
//! vt100 screen the TUI paints into a pane, and key presses encoded back the way a
//! terminal would send them. Made for the question session (`a`): Claude Code runs right
//! inside `meet`, in the right column, with the keys while the pane is focused.
//!
//! Keys use the conventional xterm encoding (no kitty keyboard protocol), which every
//! terminal program accepts; Claude Code's own detection asks for DA1, answered here so it
//! settles at once instead of waiting a timeout.

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use std::io::{Read, Write};
use std::path::Path;
use tokio::sync::mpsc;

/// Lines kept above the screen (not shown yet; the parser keeps them for later).
const SCROLLBACK: usize = 2000;
/// DA1 answer: "I am a VT102", the reply terminal-detection loops wait for.
const DA1_REPLY: &[u8] = b"\x1b[?6c";
/// How long a hung-up session gets before whatever is left of it is SIGKILLed.
const KILL_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Debug, PartialEq)]
pub enum TermEvent {
    Output(Vec<u8>),
    /// The child is gone; its exit code when it had one.
    Exited(Option<u32>),
}

pub struct Spawn<'a> {
    pub program: &'a str,
    pub args: &'a [String],
    pub cwd: &'a Path,
    pub env: &'a [(String, String)],
    pub cols: u16,
    pub rows: u16,
}

pub struct Term {
    parser: vt100::Parser,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    /// The child's pid: the login shell, which leads the pseudo-terminal's session.
    pid: Option<u32>,
    /// The last bytes of the previous output chunk, so a DA1 query split across two reads
    /// is still seen.
    tail: Vec<u8>,
    pub exited: bool,
    pub exit_code: Option<u32>,
}

impl Term {
    /// Start `spec.program` on a fresh PTY of `cols`×`rows`; its output arrives on `tx`
    /// (feed it back through [`Term::process`]), then one `Exited`.
    pub fn spawn(spec: Spawn<'_>, tx: mpsc::UnboundedSender<TermEvent>) -> Result<Self> {
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: spec.rows.max(2),
                cols: spec.cols.max(10),
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("open a pseudo-terminal")?;
        let mut cmd = CommandBuilder::new(spec.program);
        cmd.args(spec.args);
        cmd.cwd(spec.cwd);
        // The child paints this pane, not the terminal meet runs in.
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        for name in ["NO_COLOR", "FORCE_COLOR", "CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT"] {
            cmd.env_remove(name);
        }
        for (k, v) in spec.env {
            cmd.env(k, v);
        }
        let mut child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("start {}", spec.program))?;
        drop(pair.slave);
        let killer = child.clone_killer();
        let pid = child.process_id();
        let mut reader = pair.master.try_clone_reader().context("pty reader")?;
        let writer = pair.master.take_writer().context("pty writer")?;
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(TermEvent::Output(buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                }
            }
            let code = child.wait().ok().map(|s| s.exit_code());
            let _ = tx.send(TermEvent::Exited(code));
        });
        Ok(Self {
            parser: vt100::Parser::new(spec.rows.max(2), spec.cols.max(10), SCROLLBACK),
            writer,
            master: pair.master,
            killer,
            pid,
            tail: Vec::new(),
            exited: false,
            exit_code: None,
        })
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// `(cols, rows)`.
    pub fn size(&self) -> (u16, u16) {
        let (rows, cols) = self.parser.screen().size();
        (cols, rows)
    }

    /// Output from the child: onto the screen, and any DA1 query answered.
    pub fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
        let n = count_da1(&self.tail, bytes);
        for _ in 0..n {
            let _ = self.write(DA1_REPLY);
        }
        let keep = bytes.len().min(3);
        self.tail = bytes[bytes.len() - keep..].to_vec();
    }

    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        if self.exited {
            return Ok(());
        }
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }

    /// One key press, encoded for the child.
    pub fn send_key(&mut self, key: &KeyEvent) -> Result<()> {
        let app_cursor = self.parser.screen().application_cursor();
        match encode_key(key, app_cursor) {
            Some(bytes) => self.write(&bytes),
            None => Ok(()),
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        let (cols, rows) = (cols.max(10), rows.max(2));
        if self.size() == (cols, rows) {
            return;
        }
        self.parser.screen_mut().set_size(rows, cols);
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    pub fn on_exited(&mut self, code: Option<u32>) {
        self.exited = true;
        self.exit_code = code;
    }

    /// Hang the session up: SIGHUP to every process group under the child, then SIGKILL
    /// whatever still stands after [`KILL_GRACE`]. The child is the user's login shell (see
    /// [`crate::shell`]); when it runs `claude` as a job, that job has a process group of
    /// its own on the pseudo-terminal, which a signal to the shell alone would leave running
    /// with its pane gone. The hangups go out before this returns, so they land even when
    /// meet exits right after (on quit); the escalation runs on a thread that dies with meet.
    pub fn kill(&mut self) {
        if self.exited {
            return;
        }
        // Taken before the hangup: a shell that dies of it leaves its job to init, where no
        // walk from its pid would find it afterwards.
        let groups = self
            .pid
            .map(crate::shell::process_groups_under)
            .unwrap_or_default();
        let _ = self.killer.kill();
        crate::shell::signal_groups(&groups, libc::SIGHUP);
        if groups.is_empty() {
            return;
        }
        std::thread::spawn(move || {
            let alive = |g: u32| unsafe { libc::killpg(g as i32, 0) } == 0;
            let deadline = std::time::Instant::now() + KILL_GRACE;
            while std::time::Instant::now() < deadline && groups.iter().any(|&g| alive(g)) {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            let left: Vec<u32> = groups.into_iter().filter(|&g| alive(g)).collect();
            crate::shell::signal_groups(&left, libc::SIGKILL);
        });
    }
}

/// How many DA1 queries (`CSI c` / `CSI 0 c`) end inside `chunk`, given the bytes that
/// came just before it.
pub fn count_da1(tail: &[u8], chunk: &[u8]) -> usize {
    let mut joined = Vec::with_capacity(tail.len() + chunk.len());
    joined.extend_from_slice(tail);
    joined.extend_from_slice(chunk);
    let mut n = 0;
    for (i, w) in joined.windows(3).enumerate() {
        let end = i + 3;
        if w == b"\x1b[c" && end > tail.len() {
            n += 1;
        }
    }
    for (i, w) in joined.windows(4).enumerate() {
        let end = i + 4;
        if w == b"\x1b[0c" && end > tail.len() {
            n += 1;
        }
    }
    n
}

/// The bytes a terminal sends for `key` (xterm conventions). `None`: nothing to send
/// (a modifier the encoding has no room for, a key with no sequence).
pub fn encode_key(key: &KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    if key
        .modifiers
        .intersects(KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META)
    {
        return None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let mut out: Vec<u8> = Vec::with_capacity(8);
    if alt {
        out.push(0x1b);
    }
    // xterm modifier parameter: 1 + shift(1) + alt(2) + ctrl(4).
    let modifier = 1 + shift as u8 + (alt as u8) * 2 + (ctrl as u8) * 4;
    let cursor_key = |out: &mut Vec<u8>, letter: u8| {
        if modifier > 1 {
            out.extend_from_slice(format!("\x1b[1;{modifier}{}", letter as char).as_bytes());
        } else if app_cursor {
            out.extend_from_slice(&[0x1b, b'O', letter]);
        } else {
            out.extend_from_slice(&[0x1b, b'[', letter]);
        }
    };
    let tilde_key = |out: &mut Vec<u8>, n: u8| {
        if modifier > 1 {
            out.extend_from_slice(format!("\x1b[{n};{modifier}~").as_bytes());
        } else {
            out.extend_from_slice(format!("\x1b[{n}~").as_bytes());
        }
    };
    match key.code {
        KeyCode::Char(c) if ctrl => {
            let b = match c.to_ascii_lowercase() {
                ch @ 'a'..='z' => (ch as u8) - b'a' + 1,
                ' ' | '@' | '2' => 0,
                '[' | '3' => 0x1b,
                '\\' | '4' => 0x1c,
                ']' | '5' => 0x1d,
                '^' | '6' => 0x1e,
                '_' | '/' | '7' => 0x1f,
                '8' => 0x7f,
                _ => return None,
            };
            out.push(b);
        }
        KeyCode::Char(c) => {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
        KeyCode::Enter => out.push(b'\r'),
        KeyCode::Tab => out.push(b'\t'),
        KeyCode::BackTab => out.extend_from_slice(b"\x1b[Z"),
        KeyCode::Backspace => out.push(if ctrl { 0x08 } else { 0x7f }),
        KeyCode::Esc => out.push(0x1b),
        KeyCode::Up => cursor_key(&mut out, b'A'),
        KeyCode::Down => cursor_key(&mut out, b'B'),
        KeyCode::Right => cursor_key(&mut out, b'C'),
        KeyCode::Left => cursor_key(&mut out, b'D'),
        KeyCode::Home => cursor_key(&mut out, b'H'),
        KeyCode::End => cursor_key(&mut out, b'F'),
        KeyCode::Insert => tilde_key(&mut out, 2),
        KeyCode::Delete => tilde_key(&mut out, 3),
        KeyCode::PageUp => tilde_key(&mut out, 5),
        KeyCode::PageDown => tilde_key(&mut out, 6),
        KeyCode::F(n @ 1..=4) => {
            if modifier > 1 {
                out.extend_from_slice(
                    format!("\x1b[1;{modifier}{}", (b'P' + n - 1) as char).as_bytes(),
                );
            } else {
                out.extend_from_slice(&[0x1b, b'O', b'P' + n - 1]);
            }
        }
        KeyCode::F(n @ 5..=12) => {
            let code = [15, 17, 18, 19, 20, 21, 23, 24][n as usize - 5];
            tilde_key(&mut out, code);
        }
        _ => return None,
    }
    Some(out)
}

fn color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// Paint the screen into `area` of `buf`, top-left aligned; cells past the screen's size
/// are left as they are.
pub fn render(screen: &vt100::Screen, area: Rect, buf: &mut Buffer) {
    let (rows, cols) = screen.size();
    for row in 0..area.height.min(rows) {
        for col in 0..area.width.min(cols) {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let mut style = Style::default()
                .fg(color(cell.fgcolor()))
                .bg(color(cell.bgcolor()));
            if cell.bold() {
                style = style.add_modifier(Modifier::BOLD);
            }
            if cell.italic() {
                style = style.add_modifier(Modifier::ITALIC);
            }
            if cell.underline() {
                style = style.add_modifier(Modifier::UNDERLINED);
            }
            if cell.inverse() {
                style = style.add_modifier(Modifier::REVERSED);
            }
            let symbol = if cell.has_contents() {
                cell.contents().to_string()
            } else {
                " ".to_string()
            };
            if let Some(target) = buf.cell_mut((area.x + col, area.y + row)) {
                target.set_symbol(&symbol);
                target.set_style(style);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pane as the user's work setup reaches it: `claude` is a function in `.zshrc`,
    /// started through the login shell on the pseudo-terminal. The function runs (its
    /// output reaches the screen), and a kill ends the process it started, not just the
    /// shell. Skipped where there is no zsh.
    #[test]
    fn a_zshrc_claude_function_runs_in_the_pane_and_a_kill_ends_it() {
        if std::process::Command::new("zsh")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let pidfile = home.path().join("job.pid");
        std::fs::write(
            home.path().join(".zshrc"),
            "claude() { echo \"routed $*\"; sh -c 'echo $$ > \"$MEET_TEST_PIDFILE\"; exec sleep 30'; }\n",
        )
        .unwrap();
        let (program, args) = crate::shell::login_shell_wrap(
            "zsh",
            "claude",
            &["--name".to_string(), "meet · questions".to_string()],
        );
        let env = vec![
            ("ZDOTDIR".to_string(), home.path().to_string_lossy().into_owned()),
            (
                "MEET_TEST_PIDFILE".to_string(),
                pidfile.to_string_lossy().into_owned(),
            ),
        ];
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut term = Term::spawn(
            Spawn {
                program: &program,
                args: &args,
                cwd: home.path(),
                env: &env,
                cols: 80,
                rows: 24,
            },
            tx,
        )
        .unwrap();
        const ROUTED: &str = "routed --name meet · questions";
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let mut job: Option<i32> = None;
        while std::time::Instant::now() < deadline {
            while let Ok(ev) = rx.try_recv() {
                if let TermEvent::Output(bytes) = ev {
                    term.process(&bytes);
                }
            }
            job = job.or_else(|| {
                std::fs::read_to_string(&pidfile)
                    .ok()
                    .and_then(|s| s.trim().parse().ok())
            });
            if job.is_some() && term.screen().contents().contains(ROUTED) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let job = job.expect("the claude function started its process");
        let screen = term.screen().contents();
        assert!(screen.contains(ROUTED), "screen: {screen}");
        term.kill();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let alive = |pid: i32| unsafe { libc::kill(pid, 0) } == 0;
        while alive(job) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let still = alive(job);
        if still {
            unsafe {
                libc::kill(job, libc::SIGKILL);
            }
        }
        assert!(!still, "the claude function's process outlived the kill");
    }

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    /// Feed output into `term` until `needle` shows on its screen (or it exits / 10 s pass).
    async fn wait_for(
        term: &mut Term,
        rx: &mut mpsc::UnboundedReceiver<TermEvent>,
        needle: &str,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if term.screen().contents().contains(needle) {
                return true;
            }
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(TermEvent::Output(b))) => term.process(&b),
                Ok(Some(TermEvent::Exited(code))) => {
                    term.on_exited(code);
                    return term.screen().contents().contains(needle);
                }
                _ => return false,
            }
        }
    }

    #[test]
    fn keys_encode_the_way_xterm_sends_them() {
        let plain = KeyModifiers::NONE;
        assert_eq!(encode_key(&key(KeyCode::Char('h'), plain), false).unwrap(), b"h");
        assert_eq!(encode_key(&key(KeyCode::Char('é'), plain), false).unwrap(), "é".as_bytes());
        assert_eq!(encode_key(&key(KeyCode::Enter, plain), false).unwrap(), b"\r");
        assert_eq!(encode_key(&key(KeyCode::Backspace, plain), false).unwrap(), &[0x7f]);
        assert_eq!(encode_key(&key(KeyCode::Esc, plain), false).unwrap(), &[0x1b]);
        assert_eq!(
            encode_key(&key(KeyCode::Char('c'), KeyModifiers::CONTROL), false).unwrap(),
            &[0x03]
        );
        assert_eq!(
            encode_key(&key(KeyCode::Char('x'), KeyModifiers::ALT), false).unwrap(),
            b"\x1bx"
        );
        assert_eq!(encode_key(&key(KeyCode::Up, plain), false).unwrap(), b"\x1b[A");
        assert_eq!(encode_key(&key(KeyCode::Up, plain), true).unwrap(), b"\x1bOA");
        assert_eq!(
            encode_key(&key(KeyCode::Up, KeyModifiers::SHIFT), true).unwrap(),
            b"\x1b[1;2A"
        );
        assert_eq!(encode_key(&key(KeyCode::Delete, plain), false).unwrap(), b"\x1b[3~");
        assert_eq!(
            encode_key(&key(KeyCode::PageDown, KeyModifiers::CONTROL), false).unwrap(),
            b"\x1b[6;5~"
        );
        assert_eq!(encode_key(&key(KeyCode::F(1), plain), false).unwrap(), b"\x1bOP");
        assert_eq!(encode_key(&key(KeyCode::F(5), plain), false).unwrap(), b"\x1b[15~");
        assert_eq!(encode_key(&key(KeyCode::BackTab, plain), false).unwrap(), b"\x1b[Z");
        assert_eq!(
            encode_key(&key(KeyCode::Char('c'), KeyModifiers::SUPER), false),
            None,
            "Cmd+c must not type a c"
        );
        assert_eq!(encode_key(&key(KeyCode::Null, plain), false), None);
    }

    #[test]
    fn da1_queries_are_counted_across_chunk_boundaries() {
        assert_eq!(count_da1(b"", b"hello"), 0);
        assert_eq!(count_da1(b"", b"\x1b[c"), 1);
        assert_eq!(count_da1(b"", b"x\x1b[0c\x1b[c"), 2);
        assert_eq!(count_da1(b"\x1b[", b"c"), 1, "split after CSI");
        assert_eq!(count_da1(b"\x1b", b"[c"), 1, "split after ESC");
        assert_eq!(count_da1(b"\x1b[c", b"more"), 0, "already counted last time");
        assert_eq!(count_da1(b"", b"\x1b[?6c"), 0, "our own reply shape is not a query");
    }

    #[test]
    fn the_screen_paints_into_a_buffer() {
        let mut parser = vt100::Parser::new(2, 10, 0);
        parser.process(b"\x1b[1mhi\x1b[0m \x1b[31mred\x1b[0m\r\nline two");
        let mut buf = Buffer::empty(Rect::new(0, 0, 12, 3));
        render(parser.screen(), Rect::new(1, 1, 10, 2), &mut buf);
        assert_eq!(buf.cell((1, 1)).unwrap().symbol(), "h");
        assert!(buf.cell((1, 1)).unwrap().modifier.contains(Modifier::BOLD));
        assert_eq!(buf.cell((4, 1)).unwrap().symbol(), "r");
        assert_eq!(buf.cell((4, 1)).unwrap().fg, Color::Indexed(1));
        assert_eq!(buf.cell((1, 2)).unwrap().symbol(), "l");
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), " ", "outside the area is untouched");
    }

    /// A real PTY: the child's output lands on the screen, keys reach it, and its exit is
    /// reported.
    #[tokio::test]
    async fn a_child_on_a_pty_echoes_what_it_is_sent_and_reports_its_exit() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let dir = tempfile::tempdir().unwrap();
        let args = vec![
            "-c".to_string(),
            "printf 'ready\\n'; read x; printf 'got:%s\\n' \"$x\"; exit 3".to_string(),
        ];
        let mut term = Term::spawn(
            Spawn {
                program: "/bin/sh",
                args: &args,
                cwd: dir.path(),
                env: &[("MEET_TEST".into(), "1".into())],
                cols: 40,
                rows: 5,
            },
            tx,
        )
        .unwrap();
        assert_eq!(term.size(), (40, 5));
        assert!(wait_for(&mut term, &mut rx, "ready").await, "{}", term.screen().contents());
        for c in "yo".chars() {
            term.send_key(&key(KeyCode::Char(c), KeyModifiers::NONE)).unwrap();
        }
        term.send_key(&key(KeyCode::Enter, KeyModifiers::NONE)).unwrap();
        assert!(wait_for(&mut term, &mut rx, "got:yo").await, "{}", term.screen().contents());
        // Drain to the exit.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while !term.exited {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(TermEvent::Output(b))) => term.process(&b),
                Ok(Some(TermEvent::Exited(code))) => term.on_exited(code),
                _ => break,
            }
        }
        assert!(term.exited);
        assert_eq!(term.exit_code, Some(3));
        assert!(term.write(b"x").is_ok(), "writing after the exit is a no-op");
        term.resize(60, 8);
        assert_eq!(term.size(), (60, 8));
        term.kill();
    }
}
