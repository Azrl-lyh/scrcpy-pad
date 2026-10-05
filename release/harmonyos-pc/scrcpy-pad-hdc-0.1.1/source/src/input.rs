use anyhow::Result;

use crate::hdc::Hdc;

pub fn touch_down(hdc: &Hdc, serial: &str, x: i32, y: i32) -> Result<String> {
    hdc.shell(
        serial,
        ["uinput", "-T", "-d", &x.to_string(), &y.to_string()],
    )
}

pub fn touch_move(
    hdc: &Hdc,
    serial: &str,
    x1: i32,
    y1: i32,
    x2: i32,
    y2: i32,
    smooth_ms: i32,
) -> Result<String> {
    hdc.shell(
        serial,
        [
            "uinput",
            "-T",
            "-m",
            &x1.to_string(),
            &y1.to_string(),
            &x2.to_string(),
            &y2.to_string(),
            &smooth_ms.to_string(),
        ],
    )
}

pub fn touch_up(hdc: &Hdc, serial: &str, x: i32, y: i32) -> Result<String> {
    hdc.shell(
        serial,
        ["uinput", "-T", "-u", &x.to_string(), &y.to_string()],
    )
}

pub fn tap(hdc: &Hdc, serial: &str, x: i32, y: i32) -> Result<String> {
    hdc.shell(
        serial,
        ["uinput", "-T", "-c", &x.to_string(), &y.to_string()],
    )
}

pub fn swipe(
    hdc: &Hdc,
    serial: &str,
    x1: i32,
    y1: i32,
    x2: i32,
    y2: i32,
    duration_ms: i32,
) -> Result<String> {
    hdc.shell(
        serial,
        [
            "uinput",
            "-T",
            "-m",
            &x1.to_string(),
            &y1.to_string(),
            &x2.to_string(),
            &y2.to_string(),
            &duration_ms.to_string(),
        ],
    )
}

pub fn key_tap(hdc: &Hdc, serial: &str, key_code: i32) -> Result<String> {
    let down = hdc.shell(serial, ["uinput", "-K", "-d", &key_code.to_string()])?;
    std::thread::sleep(std::time::Duration::from_millis(45));
    let up = hdc.shell(serial, ["uinput", "-K", "-u", &key_code.to_string()])?;
    Ok(format!("{down}{up}"))
}

pub fn hold(hdc: &Hdc, serial: &str, x: i32, y: i32, duration_ms: i32) -> Result<String> {
    let down = touch_down(hdc, serial, x, y)?;
    std::thread::sleep(std::time::Duration::from_millis(duration_ms.max(1) as u64));
    let up = touch_up(hdc, serial, x, y)?;
    Ok(format!("{down}{up}"))
}

pub fn key_down(hdc: &Hdc, serial: &str, key_code: i32) -> Result<String> {
    hdc.shell(serial, ["uinput", "-K", "-d", &key_code.to_string()])
}

pub fn key_up(hdc: &Hdc, serial: &str, key_code: i32) -> Result<String> {
    hdc.shell(serial, ["uinput", "-K", "-u", &key_code.to_string()])
}

pub fn mouse_move(hdc: &Hdc, serial: &str, dx: i32, dy: i32) -> Result<String> {
    hdc.shell(
        serial,
        ["uinput", "-M", "-m", &dx.to_string(), &dy.to_string()],
    )
}

pub fn mouse_button(hdc: &Hdc, serial: &str, button: i32, down: bool) -> Result<String> {
    hdc.shell(
        serial,
        [
            "uinput",
            "-M",
            if down { "-d" } else { "-u" },
            &button.to_string(),
        ],
    )
}

pub fn mouse_scroll(hdc: &Hdc, serial: &str, amount: i32) -> Result<String> {
    hdc.shell(serial, ["uinput", "-M", "-s", &amount.to_string()])
}
