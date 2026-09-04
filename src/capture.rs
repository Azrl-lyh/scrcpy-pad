//! 全局键盘捕获层(平台无关接口,平台实现见下方 cfg 分支)。
//! Linux  : evdev 读取 /dev/input/event*(Wayland/X11 皆可),需 input 组权限;
//!          grab 模式用于映射开启时屏蔽原始按键。
//! Windows: rdev 低级键盘钩子(WH_KEYBOARD_LL),无需管理员权限;
//!          rdev 的 grab 在 Windows 未实现,故 grab 开关在该平台不生效(原键不会被屏蔽)。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use anyhow::Result;

#[derive(Debug, Clone, Copy)]
pub struct CaptureKey {
    pub code: u16,
    pub pressed: bool,
}

pub struct Capture {
    /// 是否抓取键盘(Linux 专用;映射开启时置 true,原始按键不再传给其它程序)
    pub grab: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl Capture {
    pub fn start(tx: Sender<CaptureKey>) -> Result<Self> {
        let grab = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        platform_start(&grab, &stop, tx)?;
        Ok(Self { grab, stop })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

// ============================ Linux 实现 ============================

#[cfg(target_os = "linux")]
fn platform_start(
    grab: &Arc<AtomicBool>,
    stop: &Arc<AtomicBool>,
    tx: Sender<CaptureKey>,
) -> Result<()> {
    use anyhow::bail;

    let mut opened = 0usize;
    for (path, device) in evdev::enumerate() {
        if !is_keyboard(&device) {
            continue;
        }
        if let Err(e) = device.set_nonblocking(true) {
            log_line(format!("设置非阻塞失败 {}: {e}", path.display()));
            continue;
        }
        opened += 1;
        let tx = tx.clone();
        let grab = grab.clone();
        let stop = stop.clone();
        std::thread::spawn(move || device_loop(device, tx, grab, stop));
    }

    if opened == 0 {
        // enumerate() 会静默跳过打不开或不是键盘的设备,
        // 需直接探测 /dev/input 来区分"没有设备"与"没有权限"
        let mut denied = 0usize;
        if let Ok(rd) = std::fs::read_dir("/dev/input") {
            for entry in rd.flatten() {
                if !entry.file_name().to_string_lossy().starts_with("event") {
                    continue;
                }
                if std::fs::File::open(entry.path()).is_err() {
                    denied += 1;
                }
            }
        }
        if denied > 0 {
            bail!(
                "无权限读取输入设备(/dev/input/event*)。\
                 请确认已执行 sudo usermod -aG input $USER 并【重新登录】,\
                 且本程序是在重新登录后启动的"
            );
        }
        bail!("未找到键盘设备");
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn is_keyboard(device: &evdev::Device) -> bool {
    device
        .supported_keys()
        .map(|k| {
            k.contains(evdev::KeyCode::KEY_A)
                && k.contains(evdev::KeyCode::KEY_Z)
                && k.contains(evdev::KeyCode::KEY_ENTER)
        })
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn device_loop(
    mut device: evdev::Device,
    tx: Sender<CaptureKey>,
    grab_flag: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) {
    use std::time::Duration;

    let mut grabbed = false;
    while !stop.load(Ordering::Relaxed) {
        // 同步 grab 状态
        let want = grab_flag.load(Ordering::Relaxed);
        if want != grabbed {
            let r = if want { device.grab() } else { device.ungrab() };
            if r.is_ok() {
                grabbed = want;
            }
        }

        match device.fetch_events() {
            Ok(events) => {
                for ev in events {
                    if let evdev::EventSummary::Key(_, key, value) = ev.destructure() {
                        // value: 0=抬起 1=按下 2=自动重复(忽略)
                        if value == 2 {
                            continue;
                        }
                        let _ = tx.send(CaptureKey {
                            code: key.0,
                            pressed: value == 1,
                        });
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(_) => {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    if grabbed {
        let _ = device.ungrab();
    }
}

#[cfg(target_os = "linux")]
fn log_line(s: String) {
    eprintln!("[capture] {s}");
}

// ============================ Windows 实现 ============================

#[cfg(windows)]
fn platform_start(
    grab: &Arc<AtomicBool>,
    stop: &Arc<AtomicBool>,
    tx: Sender<CaptureKey>,
) -> Result<()> {
    // rdev 的 grab 在 Windows 平台未实现,grab/stop 标志保留但不生效
    let _ = (grab, stop);

    let tx = std::sync::Mutex::new(tx);
    std::thread::spawn(move || {
        if let Err(e) = rdev::listen(move |event| match event.event_type {
            rdev::EventType::KeyPress(k) => {
                if let Some(c) = map_win_key(k) {
                    if let Ok(tx) = tx.lock() {
                        let _ = tx.send(CaptureKey { code: c, pressed: true });
                    }
                }
            }
            rdev::EventType::KeyRelease(k) => {
                if let Some(c) = map_win_key(k) {
                    if let Ok(tx) = tx.lock() {
                        let _ = tx.send(CaptureKey { code: c, pressed: false });
                    }
                }
            }
            _ => {}
        }) {
            eprintln!("[capture] rdev 监听失败: {e:?}");
        }
    });
    Ok(())
}

/// rdev::Key -> Linux evdev 键码(统一码空间)
#[cfg(windows)]
fn map_win_key(k: rdev::Key) -> Option<u16> {
    use rdev::Key as K;
    Some(match k {
        K::KeyA => 30,
        K::KeyB => 48,
        K::KeyC => 46,
        K::KeyD => 32,
        K::KeyE => 18,
        K::KeyF => 33,
        K::KeyG => 34,
        K::KeyH => 35,
        K::KeyI => 23,
        K::KeyJ => 36,
        K::KeyK => 37,
        K::KeyL => 38,
        K::KeyM => 50,
        K::KeyN => 49,
        K::KeyO => 24,
        K::KeyP => 25,
        K::KeyQ => 16,
        K::KeyR => 19,
        K::KeyS => 31,
        K::KeyT => 20,
        K::KeyU => 22,
        K::KeyV => 47,
        K::KeyW => 17,
        K::KeyX => 45,
        K::KeyY => 21,
        K::KeyZ => 44,
        K::Num1 => 2,
        K::Num2 => 3,
        K::Num3 => 4,
        K::Num4 => 5,
        K::Num5 => 6,
        K::Num6 => 7,
        K::Num7 => 8,
        K::Num8 => 9,
        K::Num9 => 10,
        K::Num0 => 11,
        K::F1 => 59,
        K::F2 => 60,
        K::F3 => 61,
        K::F4 => 62,
        K::F5 => 63,
        K::F6 => 64,
        K::F7 => 65,
        K::F8 => 66,
        K::F9 => 67,
        K::F10 => 68,
        K::F11 => 87,
        K::F12 => 88,
        K::Escape => 1,
        K::Return => 28,
        K::Space => 57,
        K::Tab => 15,
        K::Backspace => 14,
        K::UpArrow => 103,
        K::DownArrow => 108,
        K::LeftArrow => 105,
        K::RightArrow => 106,
        K::CapsLock => 58,
        K::ShiftLeft => 42,
        K::ShiftRight => 54,
        K::ControlLeft => 29,
        K::ControlRight => 97,
        K::Alt => 56,
        K::AltGr => 100,
        K::MetaLeft => 125,
        K::MetaRight => 126,
        K::Minus => 12,
        K::Equal => 13,
        K::LeftBracket => 26,
        K::RightBracket => 27,
        K::SemiColon => 39,
        K::Quote => 40,
        K::BackSlash => 43,
        K::Comma => 51,
        K::Dot => 52,
        K::Slash => 53,
        K::BackQuote => 41,
        K::Delete => 111,
        K::Home => 102,
        K::End => 107,
        K::PageUp => 104,
        K::PageDown => 109,
        K::Insert => 110,
        K::PrintScreen => 99,
        K::ScrollLock => 70,
        K::Pause => 119,
        K::NumLock => 69,
        _ => return None,
    })
}

/// 键码 -> 可读名称(Windows 侧反向映射)
#[cfg(windows)]
pub fn win_key_name(code: u16) -> String {
    use rdev::Key as K;
    let k = match code {
        30 => K::KeyA,
        48 => K::KeyB,
        46 => K::KeyC,
        32 => K::KeyD,
        18 => K::KeyE,
        33 => K::KeyF,
        34 => K::KeyG,
        35 => K::KeyH,
        23 => K::KeyI,
        36 => K::KeyJ,
        37 => K::KeyK,
        38 => K::KeyL,
        50 => K::KeyM,
        49 => K::KeyN,
        24 => K::KeyO,
        25 => K::KeyP,
        16 => K::KeyQ,
        19 => K::KeyR,
        31 => K::KeyS,
        20 => K::KeyT,
        22 => K::KeyU,
        47 => K::KeyV,
        17 => K::KeyW,
        45 => K::KeyX,
        21 => K::KeyY,
        44 => K::KeyZ,
        2 => K::Num1,
        3 => K::Num2,
        4 => K::Num3,
        5 => K::Num4,
        6 => K::Num5,
        7 => K::Num6,
        8 => K::Num7,
        9 => K::Num8,
        10 => K::Num9,
        11 => K::Num0,
        59 => K::F1,
        60 => K::F2,
        61 => K::F3,
        62 => K::F4,
        63 => K::F5,
        64 => K::F6,
        65 => K::F7,
        66 => K::F8,
        67 => K::F9,
        68 => K::F10,
        87 => K::F11,
        88 => K::F12,
        1 => K::Escape,
        28 => K::Return,
        57 => K::Space,
        15 => K::Tab,
        14 => K::Backspace,
        103 => K::UpArrow,
        108 => K::DownArrow,
        105 => K::LeftArrow,
        106 => K::RightArrow,
        58 => K::CapsLock,
        42 => K::ShiftLeft,
        54 => K::ShiftRight,
        29 => K::ControlLeft,
        97 => K::ControlRight,
        56 => K::Alt,
        100 => K::AltGr,
        125 => K::MetaLeft,
        126 => K::MetaRight,
        12 => K::Minus,
        13 => K::Equal,
        26 => K::LeftBracket,
        27 => K::RightBracket,
        39 => K::SemiColon,
        40 => K::Quote,
        43 => K::BackSlash,
        51 => K::Comma,
        52 => K::Dot,
        53 => K::Slash,
        41 => K::BackQuote,
        111 => K::Delete,
        102 => K::Home,
        107 => K::End,
        104 => K::PageUp,
        109 => K::PageDown,
        110 => K::Insert,
        99 => K::PrintScreen,
        70 => K::ScrollLock,
        119 => K::Pause,
        69 => K::NumLock,
        _ => return format!("Key({code})"),
    };
    format!("{k:?}")
}
