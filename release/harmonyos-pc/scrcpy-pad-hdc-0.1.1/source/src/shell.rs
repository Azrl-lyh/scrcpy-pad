use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};

use anyhow::{Context, Result};

use crate::hdc::Hdc;

/// A long-lived `hdc shell` process.
///
/// It avoids paying the Windows process-start and USB-session setup cost for
/// every touch move. Commands are written in order to the device shell's stdin;
/// a drain thread consumes output so the child cannot block on a full pipe.
#[derive(Debug)]
pub struct HdcShell {
    tx: Sender<String>,
    alive: Arc<AtomicBool>,
}

impl HdcShell {
    pub fn spawn(hdc: &Hdc, serial: &str) -> Result<Self> {
        let mut child = Command::new(hdc.executable())
            .args(["-t", serial, "shell"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawn persistent hdc shell for {serial}"))?;

        let mut stdin = child.stdin.take().context("take hdc shell stdin")?;
        let mut stdout = child.stdout.take().context("take hdc shell stdout")?;
        let mut stderr = child.stderr.take().context("take hdc shell stderr")?;
        let alive = Arc::new(AtomicBool::new(true));
        let (tx, rx) = channel::<String>();

        let writer_alive = alive.clone();
        std::thread::spawn(move || {
            while let Ok(command) = rx.recv() {
                if stdin.write_all(command.as_bytes()).is_err() || stdin.flush().is_err() {
                    writer_alive.store(false, Ordering::Release);
                    break;
                }
            }
            let _ = child.kill();
            let _ = child.wait();
            writer_alive.store(false, Ordering::Release);
        });

        std::thread::spawn(move || {
            let mut sink = [0_u8; 4096];
            while stdout.read(&mut sink).map(|n| n > 0).unwrap_or(false) {}
        });
        std::thread::spawn(move || {
            let mut sink = [0_u8; 4096];
            while stderr.read(&mut sink).map(|n| n > 0).unwrap_or(false) {}
        });

        Ok(Self { tx, alive })
    }

    pub fn send(&self, command: &str) -> Result<()> {
        if !self.alive.load(Ordering::Acquire) {
            anyhow::bail!("persistent hdc shell is closed");
        }
        self.tx
            .send(format!("{command}\n"))
            .context("send command to persistent hdc shell")
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
}
