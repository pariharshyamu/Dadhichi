//! # dadhichi-term
//!
//! Integrated terminal sessions built on a real pseudo-terminal
//! ([`portable_pty`]). A [`PtySession`] spawns a shell or command under a PTY,
//! streams its output, and accepts input — the model behind the IDE's terminal
//! panel. Because a PTY needs no display, this works and tests headlessly.

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::{Read, Write};
use thiserror::Error;

/// Errors from a terminal session.
#[derive(Debug, Error)]
pub enum TermError {
    /// The PTY layer failed.
    #[error("pty error: {0}")]
    Pty(String),
    /// An I/O error reading from or writing to the PTY.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// A live pseudo-terminal running a child process.
pub struct PtySession {
    master: Box<dyn portable_pty::MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
}

impl std::fmt::Debug for PtySession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PtySession").finish_non_exhaustive()
    }
}

impl PtySession {
    /// Spawn `program` with `args` under a new PTY of `rows`×`cols`.
    pub fn spawn(program: &str, args: &[&str], rows: u16, cols: u16) -> Result<Self, TermError> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| TermError::Pty(e.to_string()))?;

        let mut cmd = CommandBuilder::new(program);
        cmd.args(args);
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| TermError::Pty(e.to_string()))?;
        // Drop the slave so that when the child exits the master reader sees EOF.
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| TermError::Pty(e.to_string()))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| TermError::Pty(e.to_string()))?;

        Ok(Self {
            master: pair.master,
            child,
            reader,
            writer,
        })
    }

    /// Write bytes to the terminal's input.
    pub fn write(&mut self, data: &[u8]) -> Result<(), TermError> {
        self.writer.write_all(data)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Read up to `buf.len()` bytes of output. Returns `0` at EOF.
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize, TermError> {
        Ok(self.reader.read(buf)?)
    }

    /// Resize the terminal (e.g. when the panel is resized).
    pub fn resize(&self, rows: u16, cols: u16) -> Result<(), TermError> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| TermError::Pty(e.to_string()))
    }

    /// Wait for the child to exit and return whether it succeeded.
    pub fn wait(&mut self) -> Result<bool, TermError> {
        let status = self
            .child
            .wait()
            .map_err(|e| TermError::Pty(e.to_string()))?;
        Ok(status.success())
    }
}

/// Run `program` with `args` to completion under a PTY and return its output as
/// a lossy UTF-8 string. Convenience for one-shot commands and tests.
pub fn capture(program: &str, args: &[&str]) -> Result<String, TermError> {
    let mut session = PtySession::spawn(program, args, 24, 80)?;
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match session.read(&mut buf)? {
            0 => break,
            n => out.extend_from_slice(&buf[..n]),
        }
    }
    let _ = session.wait();
    Ok(String::from_utf8_lossy(&out).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_command_output() {
        let out = capture("echo", &["hello-dadhichi"]).unwrap();
        assert!(out.contains("hello-dadhichi"), "got: {out:?}");
    }

    #[test]
    fn interactive_echo_via_shell() {
        // `cat` echoes back what we write, then EOF when input closes.
        let mut session = PtySession::spawn("cat", &[], 24, 80).unwrap();
        session.write(b"ping\n").unwrap();
        // Close input so cat exits.
        session.write(&[4]).unwrap(); // Ctrl-D (EOT)

        let mut buf = [0u8; 256];
        let mut seen = String::new();
        while let Ok(n) = session.read(&mut buf) {
            if n == 0 {
                break;
            }
            seen.push_str(&String::from_utf8_lossy(&buf[..n]));
            if seen.contains("ping") {
                break;
            }
        }
        assert!(seen.contains("ping"), "got: {seen:?}");
    }
}
