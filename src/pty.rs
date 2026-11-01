//! PTY ownership.
//!
//! We drive a real pseudo-terminal (ConPTY on Windows, openpty elsewhere) rather
//! than piping a shell's stdio: a pipe makes PowerShell non-interactive, which
//! kills PSReadLine — and with it inline predictions, syntax highlighting and
//! menu completion. Everything below exists to keep that terminal authentic.

use std::io::{Read, Write};
use std::sync::mpsc::{self, Receiver, TryRecvError};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

/// A shell we know how to start.
pub struct ShellSpec {
    pub program: String,
    pub args: Vec<String>,
}

/// Ask PowerShell to predict from history in inline style, and paint the
/// prediction dim so it reads as a suggestion instead of typed input.
/// Wrapped in try/catch: Windows PowerShell 5.1 ships an older PSReadLine
/// without these switches and must keep booting.
const PS_INIT: &str = concat!(
    "try{",
    "Set-PSReadLineOption -PredictionSource History -ErrorAction Stop;",
    "Set-PSReadLineOption -PredictionViewStyle InlineView -ErrorAction Stop;",
    "Set-PSReadLineOption -Colors @{InlinePrediction=\"$([char]27)[90m\"} -ErrorAction Stop",
    "}catch{}"
);

/// PowerShell accepts a UTF-16LE script as base64, which sidesteps Windows
/// command-line quoting entirely — the plain `-Command` form loses the inner
/// quotes of the colour hashtable and the shell then starts up mute.
fn encoded_command(script: &str) -> String {
    let bytes: Vec<u8> = script.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    base64(&bytes)
}

fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let b0 = c[0] as u32;
        let b1 = *c.get(1).unwrap_or(&0) as u32;
        let b2 = *c.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

fn powershell_args() -> Vec<String> {
    vec![
        "-NoLogo".into(),
        "-NoExit".into(),
        "-EncodedCommand".into(),
        encoded_command(PS_INIT),
    ]
}

impl ShellSpec {
    pub fn detect() -> Self {
        if let Ok(p) = std::env::var("P3_SHELL") {
            if !p.trim().is_empty() {
                return Self { program: p, args: Vec::new() };
            }
        }
        #[cfg(windows)]
        {
            if on_path("pwsh.exe") {
                return Self {
                    program: "pwsh.exe".into(),
                    args: powershell_args(),
                };
            }
            Self {
                program: "powershell.exe".into(),
                args: powershell_args(),
            }
        }
        #[cfg(not(windows))]
        {
            let sh = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
            Self { program: sh, args: vec!["-l".into()] }
        }
    }
}

#[cfg(windows)]
fn on_path(exe: &str) -> bool {
    std::process::Command::new("where")
        .arg(exe)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub struct PtySession {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    rx: Receiver<Vec<u8>>,
    pub pid: Option<u32>,
    pub exited: bool,
    pub exit_code: Option<i32>,
}

impl PtySession {
    pub fn spawn(
        ctx: &eframe::egui::Context,
        spec: ShellSpec,
        rows: u16,
        cols: u16,
        cwd: &str,
    ) -> Result<Self, String> {
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| format!("openpty: {e}"))?;

        let mut cmd = CommandBuilder::new(&spec.program);
        for a in &spec.args {
            cmd.arg(a);
        }
        if std::path::Path::new(cwd).is_dir() {
            cmd.cwd(cwd);
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("启动 {} 失败: {e}", spec.program))?;
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().map_err(|e| format!("reader: {e}"))?;
        let writer = pair.master.take_writer().map_err(|e| format!("writer: {e}"))?;
        let pid = child.process_id();

        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 32 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                        repaint.request_repaint();
                    }
                    Err(_) => break,
                }
            }
            repaint.request_repaint();
        });

        Ok(Self {
            master: pair.master,
            writer,
            child,
            rx,
            pid,
            exited: false,
            exit_code: None,
        })
    }

    /// Drain everything the reader thread has queued. Called once per frame.
    pub fn drain(&mut self, sink: &mut impl FnMut(&[u8])) {
        loop {
            match self.rx.try_recv() {
                Ok(chunk) => sink(&chunk),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.poll_exit();
                    break;
                }
            }
        }
    }

    pub fn write(&mut self, bytes: &[u8]) {
        if self.exited || bytes.is_empty() {
            return;
        }
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        let _ = self.master.resize(PtySize {
            rows: rows.max(2),
            cols: cols.max(2),
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    pub fn poll_exit(&mut self) {
        if self.exited {
            return;
        }
        if let Ok(Some(status)) = self.child.try_wait() {
            self.exited = true;
            self.exit_code = Some(status.exit_code() as i32);
        }
    }

    pub fn kill(&mut self) {
        if !self.exited {
            let _ = self.child.kill();
            self.exited = true;
        }
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        self.kill();
    }
}
