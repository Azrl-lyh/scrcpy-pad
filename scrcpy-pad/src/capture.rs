//! 全局输入捕获层(平台无关接口,平台实现见下方 cfg 分支)。
//! Linux  : evdev 读取 /dev/input/event*(Wayland/X11 皆可),需 input 组权限;
//!          grab 模式用于映射开启时屏蔽原始按键,鼠标 grab 用于 FPS 瞄准时冻结/隐藏光标。
//! Windows: rdev 低级键盘/鼠标钩子(WH_KEYBOARD_LL / WH_MOUSE_LL),无需管理员权限;
//!          rdev 的 grab 在 Windows 未实现,鼠标改用"光标回中"实现等效捕获。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use anyhow::Result;

/// 统一的输入事件。
///
/// 键盘按键与鼠标按键共用同一 evdev 码空间(鼠标左/右/中键 = BTN_LEFT/RIGHT/MIDDLE,
/// 即 272/273/274),因此键位绑定流程无需区分二者;
/// 鼠标位移单独作为 Motion 事件,供 FPS 瞄准子系统使用。
#[derive(Debug, Clone, Copy)]
pub enum CaptureEvent {
    /// 按键状态变化(键盘按键或鼠标按键)
    Button { code: u16, pressed: bool },
    /// 鼠标相对位移(设备计数,非像素)
    Motion { dx: f32, dy: f32 },
}

impl CaptureEvent {
    /// 用于"按任意键"绑定捕获:只取按下的按键
    pub fn pressed_code(&self) -> Option<u16> {
        match *self {
            CaptureEvent::Button { code, pressed: true } => Some(code),
            _ => None,
        }
    }
}

pub struct Capture {
    /// 键盘抓取(Linux 专用;映射开启时置 true,原始按键不再传给其它程序)
    pub grab: Arc<AtomicBool>,
    /// 鼠标抓取(FPS 瞄准开启时置 true;Linux 走 EVIOCGRAB,Windows 走光标回中)
    pub mouse_grab: Arc<AtomicBool>,
    /// 是否检测到鼠标设备(FPS 瞄准的前提;用于界面诊断)
    pub mouse_found: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl Capture {
    pub fn start(tx: Sender<CaptureEvent>) -> Result<Self> {
        let grab = Arc::new(AtomicBool::new(false));
        let mouse_grab = Arc::new(AtomicBool::new(false));
        let mouse_found = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        platform_start(&grab, &mouse_grab, &mouse_found, &stop, tx)?;
        Ok(Self {
            grab,
            mouse_grab,
            mouse_found,
            stop,
        })
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
    mouse_grab: &Arc<AtomicBool>,
    mouse_found: &Arc<AtomicBool>,
    stop: &Arc<AtomicBool>,
    tx: Sender<CaptureEvent>,
) -> Result<()> {
    use anyhow::bail;

    let mut opened = 0usize;
    let mut mice = 0usize;
    for (path, device) in evdev::enumerate() {
        let keyboard = is_keyboard(&device);
        let mouse = is_mouse(&device);
        if !keyboard && !mouse {
            continue;
        }
        if let Err(e) = device.set_nonblocking(true) {
            log_line(format!("设置非阻塞失败 {}: {e}", path.display()));
            continue;
        }
        opened += 1;
        if mouse {
            mice += 1;
        }
        let tx = tx.clone();
        // 键盘设备用键盘抓取标志;鼠标设备用鼠标抓取标志
        let grab_flag = if keyboard {
            grab.clone()
        } else {
            mouse_grab.clone()
        };
        let stop = stop.clone();
        std::thread::spawn(move || device_loop(device, tx, grab_flag, stop, mouse));
    }
    mouse_found.store(mice > 0, Ordering::Relaxed);

    if opened == 0 {
        // enumerate() 会静默跳过打不开或不是输入设备的设备,
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

/// 具备 REL_X / REL_Y 相对轴的设备按鼠标处理
#[cfg(target_os = "linux")]
fn is_mouse(device: &evdev::Device) -> bool {
    device
        .supported_relative_axes()
        .map(|a| {
            a.contains(evdev::RelativeAxisCode::REL_X) && a.contains(evdev::RelativeAxisCode::REL_Y)
        })
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn device_loop(
    mut device: evdev::Device,
    tx: Sender<CaptureEvent>,
    grab_flag: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    mouse: bool,
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
                // 鼠标同一次读取内的 REL_X/REL_Y 合并为一个 Motion,减少消息数
                let mut dx = 0f32;
                let mut dy = 0f32;
                for ev in events {
                    match ev.destructure() {
                        evdev::EventSummary::Key(_, key, value) => {
                            // value: 0=抬起 1=按下 2=自动重复(忽略)
                            if value == 2 {
                                continue;
                            }
                            let _ = tx.send(CaptureEvent::Button {
                                code: key.0,
                                pressed: value == 1,
                            });
                        }
                        evdev::EventSummary::RelativeAxis(_, axis, value) if mouse => {
                            if axis == evdev::RelativeAxisCode::REL_X {
                                dx += value as f32;
                            } else if axis == evdev::RelativeAxisCode::REL_Y {
                                dy += value as f32;
                            }
                        }
                        _ => {}
                    }
                }
                if dx != 0.0 || dy != 0.0 {
                    let _ = tx.send(CaptureEvent::Motion { dx, dy });
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
    mouse_grab: &Arc<AtomicBool>,
    mouse_found: &Arc<AtomicBool>,
    stop: &Arc<AtomicBool>,
    tx: Sender<CaptureEvent>,
) -> Result<()> {
    // rdev 的 grab 在 Windows 未实现:键盘抓取标志保留但不生效;
    // 鼠标改用"每次位移后把光标拉回屏幕中心"实现等效捕获,使位移可持续。
    let _ = (grab, stop);
    // rdev 的钩子同时覆盖鼠标,平台本身总有鼠标可用
    mouse_found.store(true, Ordering::Relaxed);

    let tx = std::sync::Mutex::new(tx);
    let mouse_grab = mouse_grab.clone();
    std::thread::spawn(move || {
        let mut center = cursor_center();
        let mut last: Option<(f64, f64)> = None;
        let mut hiding = false;
        let mut skip_next = false;

        let r = rdev::listen(move |event| {
            let want_grab = mouse_grab.load(Ordering::Relaxed);
            if want_grab != hiding {
                // ShowCursor 内部是引用计数,必须成对调用
                unsafe { set_cursor_visible(!want_grab) };
                hiding = want_grab;
                if want_grab {
                    // 进入抓取:重新取一次屏幕中心(分辨率/显示器可能已经变了),
                    // 再把光标归中,后续位移以中心为基准持续累积;
                    // 归中自身会再产生一次鼠标事件,跳过它
                    center = cursor_center();
                    move_cursor(center.0, center.1);
                    last = Some((center.0 as f64, center.1 as f64));
                    skip_next = true;
                }
            }

            let send = |ev: CaptureEvent| {
                if let Ok(tx) = tx.lock() {
                    let _ = tx.send(ev);
                }
            };

            match event.event_type {
                rdev::EventType::KeyPress(k) => {
                    if let Some(c) = map_win_key(k) {
                        send(CaptureEvent::Button {
                            code: c,
                            pressed: true,
                        });
                    }
                }
                rdev::EventType::KeyRelease(k) => {
                    if let Some(c) = map_win_key(k) {
                        send(CaptureEvent::Button {
                            code: c,
                            pressed: false,
                        });
                    }
                }
                rdev::EventType::ButtonPress(b) => {
                    if let Some(c) = map_win_button(b) {
                        send(CaptureEvent::Button {
                            code: c,
                            pressed: true,
                        });
                    }
                }
                rdev::EventType::ButtonRelease(b) => {
                    if let Some(c) = map_win_button(b) {
                        send(CaptureEvent::Button {
                            code: c,
                            pressed: false,
                        });
                    }
                }
                rdev::EventType::MouseMove { x, y } => {
                    // 位移统一按"上一次位置 -> 本次位置"求差(抓取与否都要发,供 FPS 瞄准使用)
                    let d = match last {
                        Some((px, py)) => (x - px, y - py),
                        None => (0.0, 0.0),
                    };
                    last = Some((x, y));
                    if skip_next {
                        skip_next = false;
                    } else if d.0 != 0.0 || d.1 != 0.0 {
                        send(CaptureEvent::Motion {
                            dx: d.0 as f32,
                            dy: d.1 as f32,
                        });
                    }
                    // 抓取中:光标离屏幕中心超过阈值才拉回中心。
                    // 每次拉回都会再触发一次鼠标事件(回调开销翻倍),按阈值回中可
                    // 把回中次数降一个数量级,同时光标始终远离屏幕边缘,位移不会丢。
                    if want_grab
                        && ((x - center.0 as f64).abs() > CURSOR_RECENTER_PX
                            || (y - center.1 as f64).abs() > CURSOR_RECENTER_PX)
                    {
                        // 注意把回中自身触发的那次 MouseMove 也标记为跳过,
                        // 否则它会以 (x,y)->中心 的形式被当成真实位移上报,
                        // 表现为视角突然反向跳一下。
                        move_cursor(center.0, center.1);
                        last = Some((center.0 as f64, center.1 as f64));
                        skip_next = true;
                    }
                }
                _ => {}
            }
        });
        if let Err(e) = r {
            eprintln!("[capture] rdev 监听失败: {e:?}");
        }
    });
    Ok(())
}

/// 抓取鼠标时,光标离屏幕中心超过该像素数才拉回中心。
/// 拉回(SetCursorPos)本身会再触发一次鼠标事件,使钩子回调量翻倍;
/// 按阈值回中可把回中次数降低一个数量级,同时光标始终远离屏幕边缘,
/// 不会出现"顶到边缘后位移丢失"。正常甩动一次也不会越过屏幕边界。
#[cfg(windows)]
const CURSOR_RECENTER_PX: f64 = 64.0;

/// 屏幕中心(像素)
#[cfg(windows)]
fn cursor_center() -> (i32, i32) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
    unsafe { (GetSystemMetrics(SM_CXSCREEN) / 2, GetSystemMetrics(SM_CYSCREEN) / 2) }
}

#[cfg(windows)]
fn move_cursor(x: i32, y: i32) {
    use windows_sys::Win32::UI::WindowsAndMessaging::SetCursorPos;
    unsafe {
        SetCursorPos(x, y);
    }
}

#[cfg(windows)]
unsafe fn set_cursor_visible(visible: bool) {
    use windows_sys::Win32::UI::WindowsAndMessaging::ShowCursor;
    unsafe {
        ShowCursor(if visible { 1 } else { 0 });
    }
}

/// rdev 鼠标按键 -> Linux evdev 键码(与平台无关的同一码空间)
#[cfg(windows)]
fn map_win_button(b: rdev::Button) -> Option<u16> {
    use rdev::Button as B;
    Some(match b {
        B::Left => 272,               // BTN_LEFT
        B::Right => 273,              // BTN_RIGHT
        B::Middle => 274,             // BTN_MIDDLE
        B::Unknown(n) => 275 + n as u16, // 其它键位依次排开,避免与已知码冲突
    })
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
    // 鼠标按键沿用 Linux 侧的名称,保证两平台配置互通
    match code {
        272 => return "BTN_LEFT".into(),
        273 => return "BTN_RIGHT".into(),
        274 => return "BTN_MIDDLE".into(),
        _ => {}
    }
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
