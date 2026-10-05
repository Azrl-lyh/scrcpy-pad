use anyhow::Result;

use crate::hdc::Hdc;

/// Abstraction used by the mapping runtime.
///
/// The current implementation uses the shell-accessible `uinput` tool. A future
/// companion-HAP transport can implement the same trait with an authorized
/// native injection channel and real pointer IDs.
pub trait InputBackend: Send + Sync {
    fn tap(&self, x: i32, y: i32, duration_ms: i32) -> Result<()>;
    fn touch_down(&self, pointer_id: u64, x: i32, y: i32) -> Result<()>;
    fn touch_move(
        &self,
        pointer_id: u64,
        from_x: i32,
        from_y: i32,
        to_x: i32,
        to_y: i32,
        smooth_ms: i32,
    ) -> Result<()>;
    fn touch_up(&self, pointer_id: u64, x: i32, y: i32) -> Result<()>;
    fn key(&self, down: bool, key_code: i32) -> Result<()>;
    fn mouse_move(&self, dx: i32, dy: i32) -> Result<()>;
    fn mouse_button(&self, button: i32, down: bool) -> Result<()>;
    fn mouse_scroll(&self, amount: i32) -> Result<()>;
}

#[derive(Debug, Clone)]
pub struct HdcInputBackend {
    hdc: Hdc,
    serial: String,
}

impl HdcInputBackend {
    pub fn new(hdc: Hdc, serial: impl Into<String>) -> Self {
        let serial = serial.into();
        Self { hdc, serial }
    }

    fn send(&self, command: String) -> Result<()> {
        if std::env::var_os("SCRCPY_PAD_HDC_TRACE").is_some() {
            eprintln!("[hdc-input] {command}");
        }
        let result = self
            .hdc
            .shell(&self.serial, command.split_whitespace())
            .map(|_| ());
        if let Err(error) = &result {
            eprintln!("[hdc-input] {command}: {error:#}");
        }
        result
    }
}

impl InputBackend for HdcInputBackend {
    fn tap(&self, x: i32, y: i32, duration_ms: i32) -> Result<()> {
        let _ = duration_ms;
        self.send(format!("uinput -T -c {x} {y}"))
    }

    fn touch_down(&self, _pointer_id: u64, x: i32, y: i32) -> Result<()> {
        self.send(format!("uinput -T -d {x} {y}"))
    }

    fn touch_move(
        &self,
        _pointer_id: u64,
        from_x: i32,
        from_y: i32,
        to_x: i32,
        to_y: i32,
        smooth_ms: i32,
    ) -> Result<()> {
        self.send(format!(
            "uinput -T -m {from_x} {from_y} {to_x} {to_y} {smooth_ms}"
        ))
    }

    fn touch_up(&self, _pointer_id: u64, x: i32, y: i32) -> Result<()> {
        self.send(format!("uinput -T -u {x} {y}"))
    }

    fn key(&self, down: bool, key_code: i32) -> Result<()> {
        let action = if down { "-d" } else { "-u" };
        self.send(format!("uinput -K {action} {key_code}"))
    }

    fn mouse_move(&self, dx: i32, dy: i32) -> Result<()> {
        self.send(format!("uinput -M -m {dx} {dy}"))
    }

    fn mouse_button(&self, button: i32, down: bool) -> Result<()> {
        let action = if down { "-d" } else { "-u" };
        self.send(format!("uinput -M {action} {button}"))
    }

    fn mouse_scroll(&self, amount: i32) -> Result<()> {
        self.send(format!("uinput -M -s {amount}"))
    }
}
