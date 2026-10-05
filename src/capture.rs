//! 全局输入捕获层(平台无关接口,平台实现见下方 cfg 模块)。
//!
//! Linux  : evdev 读取 `/dev/input/event*`(Wayland/X11 皆可),需 input 组权限;
//!          grab 模式用于映射开启时屏蔽原始按键,鼠标 grab 用于 FPS 瞄准时冻结光标。
//! Windows: 自己装的低级键盘/鼠标钩子(WH_KEYBOARD_LL / WH_MOUSE_LL),无需管理员权限。
//!
//! # 为什么不用 rdev(Windows)
//!
//! 原先 Windows 侧用 rdev 0.5.3 的低级钩子。它有个致命开销:它的回调在**调用我们的
//! 闭包之前**,会对每个 `KeyPress` 调一次 `Keyboard::get_name()`,而那里做了
//! `GetForegroundWindow` + `AttachThreadInput(当前线程, 前台窗口线程)` +
//! `ToUnicodeEx`。`AttachThreadInput` 会同步附着到**别的线程**的输入队列上,
//! 对方不泵消息就会一直阻塞。Windows 对低级钩子有硬时限
//! (`LowLevelHooksTimeout`,默认 300ms):超时的事件被**静默丢弃**,
//! 反复超时还会把整个钩子**静默摘掉** —— 用户侧的表现就是"按了没反应,
//! 而且切过窗口之后尤其容易触发"(切窗口正是 `GetForegroundWindow` 变化的时刻)。
//!
//! 讽刺的是 `event.name` 我们从未使用过。自己装钩子之后,回调里只做
//! "读 vkCode → 查表 → 塞进队列"这三件事,一次系统调用都不做,超时风险随之消失。
//!
//! # 两条平台共用的可靠性机制
//!
//! 1. **热插拔重扫**:设备只在启动时枚举一次是不够的(外接键盘挂起唤醒、拔插、
//!    USB 复位都会换节点号,旧 fd 直接作废且不会报错)。两边都有后台线程定期重扫。
//! 2. **丢失 release 的自愈**:底层一旦漏掉一个 KeyRelease,引擎侧那个键就会
//!    永远留在"按下"状态(幻影触点 + 上升沿不再产生)。所以捕获层自己维护一份
//!    "已上报为按下"的集合,并定期与**权威真值**比对,把多出来的补发一次抬起:
//!      * Linux  : `EVIOCGKEY`,直接问内核"此刻哪些键真的按着";
//!      * Windows: `GetAsyncKeyState`,逐个问系统。

use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Sender;

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
            CaptureEvent::Button {
                code,
                pressed: true,
            } => Some(code),
            _ => None,
        }
    }
}

pub struct Capture {
    /// 键盘抓取(Linux 专用;映射开启时置 true,原始按键不再传给其它程序)
    pub grab: Arc<AtomicBool>,
    /// 鼠标抓取(FPS 瞄准开启时置 true;Linux 走 EVIOCGRAB,Windows 走光标回中)
    pub mouse_grab: Arc<AtomicBool>,
    /// 独立的光标消隐请求(不影响鼠标事件抓取/回中;Windows 使用透明系统光标)
    pub cursor_hide: Arc<AtomicBool>,
    /// 是否检测到鼠标设备(FPS 瞄准的前提;用于界面诊断)
    pub mouse_found: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    /// Windows:消息循环所在线程 id,退出时 `PostThreadMessage(WM_QUIT)` 唤醒它
    /// (`GetMessage` 会阻塞,光置 stop 标志它看不到)。Linux 侧恒为 None。
    #[cfg_attr(not(windows), allow(dead_code))]
    wake_thread: Option<Arc<AtomicU32>>,
    /// Windows hook 线程句柄。退出时必须等它真正卸载钩子后再返回，
    /// 避免低层鼠标钩子在进程退出瞬间仍被系统调用。
    #[cfg(windows)]
    hook_thread: Option<std::thread::JoinHandle<()>>,
}

impl Capture {
    pub fn start(tx: Sender<CaptureEvent>) -> Result<Self> {
        let grab = Arc::new(AtomicBool::new(false));
        let mouse_grab = Arc::new(AtomicBool::new(false));
        let cursor_hide = Arc::new(AtomicBool::new(false));
        let mouse_found = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));

        #[cfg(target_os = "linux")]
        let wake_thread = linux::start(&grab, &mouse_grab, &cursor_hide, &mouse_found, &stop, tx)?;
        #[cfg(windows)]
        let (wake_thread, hook_thread) =
            windows::start(&grab, &mouse_grab, &cursor_hide, &mouse_found, &stop, tx)?;

        Ok(Self {
            grab,
            mouse_grab,
            cursor_hide,
            mouse_found,
            stop,
            wake_thread,
            #[cfg(windows)]
            hook_thread: Some(hook_thread),
        })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.cursor_hide.store(false, Ordering::Relaxed);
        #[cfg(windows)]
        windows::set_cursor_visible(true);
        // Windows 的消息循环阻塞在 GetMessage 上,必须显式叫醒；随后等待
        // hook 线程完成 UnhookWindowsHookEx，保证低层鼠标钩子不会拖到进程
        // 退出阶段才被系统异步清理。
        #[cfg(windows)]
        {
            if let Some(tid) = &self.wake_thread {
                let tid = tid.load(Ordering::Relaxed);
                if tid != 0 {
                    windows::post_quit(tid);
                }
            }
            if let Some(handle) = self.hook_thread.take() {
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
                while !handle.is_finished() && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                if handle.is_finished() {
                    let _ = handle.join();
                }
            }
        }
    }
}

/// Called from the GUI thread when capture starts/stops.  The Windows backend
/// installs/restores transparent system cursor images, so the cursor disappears
/// over scrcpy and any other focused window too.
#[cfg(windows)]
pub fn set_cursor_visible_from_ui(visible: bool) {
    windows::set_cursor_visible(visible);
}

#[cfg(not(windows))]
pub fn set_cursor_visible_from_ui(_visible: bool) {}

/// Re-apply a NULL cursor shape after egui/window painting.  The transparent
/// system cursor remains the global fallback across other applications.
#[cfg(windows)]
pub fn hide_cursor_shape_from_ui() {
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::SetCursor(std::ptr::null_mut());
    }
}

#[cfg(not(windows))]
pub fn hide_cursor_shape_from_ui() {}

// ===================================================================
//  Windows 虚拟键码 <-> evdev 键码(平台无关的纯数据 + 纯函数)
// ===================================================================
//
// 为什么放在 cfg 之外:这张表是**纯数据映射**,不依赖任何 Win32 调用,
// 因此可以在 Linux 上直接跑单元测试。跨平台配置能不能通用、小键盘与多媒体键
// 能不能绑上,全靠它 —— 这种表最容易抄错一个码,必须有测试兜着。

#[cfg(any(windows, test))]
mod vktable {
    use std::sync::OnceLock;

    /// (Windows 虚拟键码, evdev 键码, 可读名)
    ///
    /// 名称沿用 evdev 的叫法(`KEY_A` / `BTN_LEFT`),这样 Windows 与 Linux
    /// 上看到的名字一致 —— 两个平台共用同一份配置,名字不该是两套。
    ///
    /// 表中**特意包含**了 rdev 那份枚举没有覆盖的部分:小键盘(`VK_NUMPAD*`、
    /// 四则运算符)、多媒体/浏览器键。旧实现里这些一律落到 `Key::Unknown`
    /// 然后被 `_ => return None` 静默丢弃,用户按小键盘改键会发现"怎么按都没反应"。
    pub const VK_TABLE: &[(u16, u16, &str)] = &[
        // ---- 主键区 ----
        (0x08, 14, "KEY_BACKSPACE"),
        (0x09, 15, "KEY_TAB"),
        (0x0D, 28, "KEY_ENTER"),
        (0x1B, 1, "KEY_ESC"),
        (0x20, 57, "KEY_SPACE"),
        (0x14, 58, "KEY_CAPSLOCK"),
        // ---- 修饰键:虚拟键码可能报"通用"也可能是"左右具体" ----
        (0x10, 42, "KEY_LEFTSHIFT"),
        (0x11, 29, "KEY_LEFTCTRL"),
        (0x12, 56, "KEY_LEFTALT"),
        (0xA0, 42, "KEY_LEFTSHIFT"),
        (0xA1, 54, "KEY_RIGHTSHIFT"),
        (0xA2, 29, "KEY_LEFTCTRL"),
        (0xA3, 97, "KEY_RIGHTCTRL"),
        (0xA4, 56, "KEY_LEFTALT"),
        (0xA5, 100, "KEY_RIGHTALT"),
        (0x5B, 125, "KEY_LEFTMETA"),
        (0x5C, 126, "KEY_RIGHTMETA"),
        (0x5D, 127, "KEY_COMPOSE"),
        // ---- 数字行 ----
        (0x30, 11, "KEY_0"),
        (0x31, 2, "KEY_1"),
        (0x32, 3, "KEY_2"),
        (0x33, 4, "KEY_3"),
        (0x34, 5, "KEY_4"),
        (0x35, 6, "KEY_5"),
        (0x36, 7, "KEY_6"),
        (0x37, 8, "KEY_7"),
        (0x38, 9, "KEY_8"),
        (0x39, 10, "KEY_9"),
        // ---- 字母区 ----
        (0x41, 30, "KEY_A"),
        (0x42, 48, "KEY_B"),
        (0x43, 46, "KEY_C"),
        (0x44, 32, "KEY_D"),
        (0x45, 18, "KEY_E"),
        (0x46, 33, "KEY_F"),
        (0x47, 34, "KEY_G"),
        (0x48, 35, "KEY_H"),
        (0x49, 23, "KEY_I"),
        (0x4A, 36, "KEY_J"),
        (0x4B, 37, "KEY_K"),
        (0x4C, 38, "KEY_L"),
        (0x4D, 50, "KEY_M"),
        (0x4E, 49, "KEY_N"),
        (0x4F, 24, "KEY_O"),
        (0x50, 25, "KEY_P"),
        (0x51, 16, "KEY_Q"),
        (0x52, 19, "KEY_R"),
        (0x53, 31, "KEY_S"),
        (0x54, 20, "KEY_T"),
        (0x55, 22, "KEY_U"),
        (0x56, 47, "KEY_V"),
        (0x57, 17, "KEY_W"),
        (0x58, 45, "KEY_X"),
        (0x59, 21, "KEY_Y"),
        (0x5A, 44, "KEY_Z"),
        // ---- 标点(OEM 键位随键盘布局变化,但物理位置固定) ----
        (0xBA, 39, "KEY_SEMICOLON"),
        (0xBB, 13, "KEY_EQUAL"),
        (0xBC, 51, "KEY_COMMA"),
        (0xBD, 12, "KEY_MINUS"),
        (0xBE, 52, "KEY_DOT"),
        (0xBF, 53, "KEY_SLASH"),
        (0xC0, 41, "KEY_GRAVE"),
        (0xDB, 26, "KEY_LEFTBRACE"),
        (0xDC, 43, "KEY_BACKSLASH"),
        (0xDD, 27, "KEY_RIGHTBRACE"),
        (0xDE, 40, "KEY_APOSTROPHE"),
        (0xE2, 86, "KEY_102ND"),
        // ---- 功能键 ----
        (0x70, 59, "KEY_F1"),
        (0x71, 60, "KEY_F2"),
        (0x72, 61, "KEY_F3"),
        (0x73, 62, "KEY_F4"),
        (0x74, 63, "KEY_F5"),
        (0x75, 64, "KEY_F6"),
        (0x76, 65, "KEY_F7"),
        (0x77, 66, "KEY_F8"),
        (0x78, 67, "KEY_F9"),
        (0x79, 68, "KEY_F10"),
        (0x7A, 87, "KEY_F11"),
        (0x7B, 88, "KEY_F12"),
        // ---- 编辑/导航 ----
        (0x2C, 99, "KEY_SYSRQ"),
        (0x91, 70, "KEY_SCROLLLOCK"),
        (0x13, 119, "KEY_PAUSE"),
        (0x2D, 110, "KEY_INSERT"),
        (0x2E, 111, "KEY_DELETE"),
        (0x24, 102, "KEY_HOME"),
        (0x23, 107, "KEY_END"),
        (0x21, 104, "KEY_PAGEUP"),
        (0x22, 109, "KEY_PAGEDOWN"),
        (0x25, 105, "KEY_LEFT"),
        (0x26, 103, "KEY_UP"),
        (0x27, 106, "KEY_RIGHT"),
        (0x28, 108, "KEY_DOWN"),
        // ---- 小键盘(旧实现完全没有,用户根本绑不上) ----
        (0x90, 69, "KEY_NUMLOCK"),
        (0x60, 82, "KEY_KP0"),
        (0x61, 79, "KEY_KP1"),
        (0x62, 80, "KEY_KP2"),
        (0x63, 81, "KEY_KP3"),
        (0x64, 75, "KEY_KP4"),
        (0x65, 76, "KEY_KP5"),
        (0x66, 77, "KEY_KP6"),
        (0x67, 71, "KEY_KP7"),
        (0x68, 72, "KEY_KP8"),
        (0x69, 73, "KEY_KP9"),
        (0x6A, 55, "KEY_KPASTERISK"),
        (0x6B, 78, "KEY_KPPLUS"),
        (0x6C, 121, "KEY_KPCOMMA"),
        (0x6D, 74, "KEY_KPMINUS"),
        (0x6E, 83, "KEY_KPDOT"),
        (0x6F, 98, "KEY_KPSLASH"),
        // ---- 多媒体 ----
        (0xAD, 113, "KEY_MUTE"),
        (0xAE, 114, "KEY_VOLUMEDOWN"),
        (0xAF, 115, "KEY_VOLUMEUP"),
        (0xB0, 163, "KEY_NEXTSONG"),
        (0xB1, 165, "KEY_PREVIOUSSONG"),
        (0xB2, 166, "KEY_STOPCD"),
        (0xB3, 164, "KEY_PLAYPAUSE"),
        // ---- 浏览器 ----
        (0xA6, 158, "KEY_BACK"),
        (0xA7, 159, "KEY_FORWARD"),
        (0xA8, 173, "KEY_REFRESH"),
        (0xAA, 217, "KEY_SEARCH"),
        (0xAC, 172, "KEY_HOMEPAGE"),
    ];

    /// 鼠标按键的 evdev 码 -> Windows 虚拟键码。
    ///
    /// 为什么要这张小表:`GetAsyncKeyState` 认的是 VK_* 而不是 evdev 码,
    /// 而鼠标键也需要参与"丢失 release"的对账 —— 卡住的右键比卡住的键盘键更常见。
    pub const MOUSE_VK: &[(u16, u16)] = &[
        (272, 0x01), // BTN_LEFT   -> VK_LBUTTON
        (273, 0x02), // BTN_RIGHT  -> VK_RBUTTON
        (274, 0x04), // BTN_MIDDLE -> VK_MBUTTON
        (275, 0x05), // BTN_SIDE   -> VK_XBUTTON1
        (276, 0x06), // BTN_EXTRA  -> VK_XBUTTON2
    ];

    /// vk 索引 -> evdev 码(0 = 未映射)。
    ///
    /// 低级钩子每个事件都要查一次,所以用 O(1) 的数组而不是遍历:
    /// 回调里省下的每一纳秒,都是在远离那个 300ms 的超时上限。
    static VK_TO_EVDEV: OnceLock<Box<[u16; 256]>> = OnceLock::new();
    /// evdev 码 -> vk(只在对账时用,查表次数少,线性找即可)
    static EVDEV_TO_VK: OnceLock<Vec<(u16, u16)>> = OnceLock::new();

    fn vk_to_evdev() -> &'static [u16; 256] {
        VK_TO_EVDEV.get_or_init(|| {
            let mut t = Box::new([0u16; 256]);
            // 同一个 vk 出现两次时以**先出现**的为准:表中"通用修饰键"排在
            // "左右具体"之前,而真实低级钩子报的是具体的左右键,两者都映射到
            // 同一个 evdev 码(见 VK_TABLE 的分组注释)。
            for &(vk, code, _) in VK_TABLE {
                let i = vk as usize;
                if i < 256 && t[i] == 0 {
                    t[i] = code;
                }
            }
            t
        })
    }

    /// Windows 虚拟键码 -> evdev 键码
    pub fn map_vk(vk: u16) -> Option<u16> {
        let c = vk_to_evdev()[vk as usize & 0xFF];
        (c != 0).then_some(c)
    }

    /// evdev 键码 -> Windows 虚拟键码(对账时问系统"这个键还按着吗")
    pub fn vk_for_evdev(code: u16) -> Option<u16> {
        if let Some(&(_, vk)) = MOUSE_VK.iter().find(|(c, _)| *c == code) {
            return Some(vk);
        }
        let table = EVDEV_TO_VK.get_or_init(|| {
            let mut v: Vec<(u16, u16)> = VK_TABLE.iter().map(|&(vk, c, _)| (c, vk)).collect();
            // 同一个 evdev 码对应多个 vk 时,取**最小**的那个:它们是等价的
            // (例如 err 左右 Shift 都映射到 KEY_LEFTSHIFT 之外的错误),取小值稳定可复现
            v.sort_unstable();
            v.dedup_by_key(|(c, _)| *c);
            v
        });
        table
            .binary_search_by_key(&code, |&(c, _)| c)
            .ok()
            .map(|i| table[i].1)
    }

    /// evdev 键码 -> 可读名(Windows 侧 `keymap::key_name` 用它)
    pub fn evdev_name(code: u16) -> String {
        if let Some(&(_, _, name)) = VK_TABLE.iter().find(|&&(_, c, _)| c == code) {
            return name.to_string();
        }
        if let Some(&(c, _)) = MOUSE_VK.iter().find(|&&(c, _)| c == code) {
            return match c {
                272 => "BTN_LEFT".into(),
                273 => "BTN_RIGHT".into(),
                274 => "BTN_MIDDLE".into(),
                275 => "BTN_SIDE".into(),
                _ => "BTN_EXTRA".into(),
            };
        }
        format!("Key({code})")
    }

    /// 未被映射的虚拟键码只提示一次(用位图去重,避免按住某个未知键时刷屏)
    static UNKNOWN_SEEN: OnceLock<Box<[AtomicBool; 256]>> = OnceLock::new();

    /// 记录一个"我们没有映射"的虚拟键码;首次见到时返回 true(调用方据此打日志)。
    ///
    /// 为什么值得记:用户报"某个键改键时怎么按都没反应"时,日志里直接就能看到
    /// 是哪个 vkCode 没覆盖,补一行表就能修好 —— 否则只能靠猜。
    pub fn note_unknown_vk(vk: u16) -> bool {
        let seen =
            UNKNOWN_SEEN.get_or_init(|| Box::new(std::array::from_fn(|_| AtomicBool::new(false))));
        !seen[vk as usize & 0xFF].swap(true, Ordering::Relaxed)
    }

    use std::sync::atomic::{AtomicBool, Ordering};

    #[cfg(test)]
    mod tests {
        use super::*;

        /// 表里不能有"同一个 vk 映射到两个不同 evdev 码"的矛盾项 ——
        /// 那会让行为取决于遍历顺序,极难排查
        #[test]
        fn no_conflicting_vk_mappings() {
            let mut seen: Vec<(u16, u16)> = Vec::new();
            for &(vk, code, _) in VK_TABLE {
                if let Some(&(_, prev)) = seen.iter().find(|&&(v, _)| v == vk) {
                    assert_eq!(prev, code, "vk {vk:#04x} 同时映射到 {prev} 与 {code}");
                } else {
                    seen.push((vk, code));
                }
            }
        }

        /// 名称要**按 evdev 码**唯一:两个 vk 指向同一个 evdev 码是合法的
        /// (通用修饰键 VK_SHIFT 与具体的 VK_LSHIFT 都落到 KEY_LEFTSHIFT),
        /// 但它们必须给出同一个名字 —— 否则同一个键在不同来源下显示成两个名字。
        #[test]
        fn names_are_unique_per_evdev_code() {
            let mut by_code: Vec<(u16, &str)> = Vec::new();
            for &(_, code, name) in VK_TABLE {
                if let Some(&(_, prev)) = by_code.iter().find(|&&(c, _)| c == code) {
                    assert_eq!(prev, name, "evdev 码 {code} 有两个名字: {prev} / {name}");
                } else {
                    by_code.push((code, name));
                }
            }
        }

        /// 抽样核对关键映射 —— 这些码抄错一位就会"按 A 出 B",而且很难发现
        #[test]
        fn key_positions_match_the_linux_table() {
            assert_eq!(map_vk(0x41), Some(30), "VK_A -> KEY_A");
            assert_eq!(map_vk(0x44), Some(32), "VK_D -> KEY_D");
            assert_eq!(map_vk(0x57), Some(17), "VK_W -> KEY_W");
            assert_eq!(map_vk(0x53), Some(31), "VK_S -> KEY_S");
            assert_eq!(map_vk(0x31), Some(2), "VK_1 -> KEY_1");
            assert_eq!(map_vk(0x30), Some(11), "VK_0 -> KEY_0");
            assert_eq!(map_vk(0x1B), Some(1), "VK_ESCAPE -> KEY_ESC");
            assert_eq!(map_vk(0x0D), Some(28), "VK_RETURN -> KEY_ENTER");
            assert_eq!(map_vk(0x20), Some(57), "VK_SPACE -> KEY_SPACE");
            assert_eq!(map_vk(0x77), Some(66), "VK_F8 -> KEY_F8(默认总开关键)");
            assert_eq!(map_vk(0x7B), Some(88), "VK_F12 -> KEY_F12");
            assert_eq!(map_vk(0x26), Some(103), "VK_UP -> KEY_UP");
            assert_eq!(map_vk(0x28), Some(108), "VK_DOWN -> KEY_DOWN");
        }

        /// 小键盘必须能绑上(旧实现里它整个落到 Unknown 被丢弃)
        #[test]
        fn numpad_and_media_keys_are_covered() {
            assert_eq!(map_vk(0x60), Some(82), "VK_NUMPAD0 -> KEY_KP0");
            assert_eq!(map_vk(0x69), Some(73), "VK_NUMPAD9 -> KEY_KP9");
            assert_eq!(map_vk(0x6A), Some(55), "VK_MULTIPLY -> KEY_KPASTERISK");
            assert_eq!(map_vk(0x6B), Some(78), "VK_ADD -> KEY_KPPLUS");
            assert_eq!(map_vk(0x6D), Some(74), "VK_SUBTRACT -> KEY_KPMINUS");
            assert_eq!(map_vk(0x6F), Some(98), "VK_DIVIDE -> KEY_KPSLASH");
            assert_eq!(map_vk(0xAF), Some(115), "VK_VOLUME_UP -> KEY_VOLUMEUP");
            assert_eq!(
                map_vk(0xB3),
                Some(164),
                "VK_MEDIA_PLAY_PAUSE -> KEY_PLAYPAUSE"
            );
        }

        /// 左右修饰键各归各位:左右混用会让"按住才瞄准"这类门控键时灵时不灵
        #[test]
        fn left_and_right_modifiers_are_distinct() {
            assert_eq!(map_vk(0xA0), Some(42), "左 Shift");
            assert_eq!(map_vk(0xA1), Some(54), "右 Shift");
            assert_eq!(map_vk(0xA2), Some(29), "左 Ctrl");
            assert_eq!(map_vk(0xA3), Some(97), "右 Ctrl");
            assert_eq!(map_vk(0xA4), Some(56), "左 Alt");
            assert_eq!(map_vk(0xA5), Some(100), "右 Alt");
        }

        /// 反查(evdev -> vk)必须能找回原值;对账靠它问系统"还按着吗"
        #[test]
        fn reverse_lookup_round_trips() {
            for &(vk, code, name) in VK_TABLE {
                let got = vk_for_evdev(code);
                assert!(got.is_some(), "{name}({code}) 反查不到 vk");
                // 同一个 evdev 码可能有多个 vk(见表),反查回来的必须仍然映射到同一 evdev 码
                let back = map_vk(got.unwrap());
                assert_eq!(
                    back,
                    Some(code),
                    "{name}: vk {vk:#04x} 反查后映射到了 {back:?}"
                );
            }
        }

        /// 鼠标键的 vk 映射:BUTTON 系列与键盘共用同一套对账流程
        #[test]
        fn mouse_buttons_have_vk_codes() {
            assert_eq!(vk_for_evdev(272), Some(0x01));
            assert_eq!(vk_for_evdev(273), Some(0x02));
            assert_eq!(vk_for_evdev(274), Some(0x04));
            assert_eq!(vk_for_evdev(275), Some(0x05), "侧键 1:旧实现会错位成 276");
            assert_eq!(vk_for_evdev(276), Some(0x06), "侧键 2");
        }

        /// 未映射的 vk 要能稳定返回 None,并且"只提示一次"要真的只提示一次
        #[test]
        fn unknown_vk_is_reported_once() {
            assert_eq!(map_vk(0xFF), None);
            assert_eq!(map_vk(0x00), None);
            let vk = 0xF7; // 随便挑一个表里没有的
            assert!(note_unknown_vk(vk), "首次应当提示");
            assert!(!note_unknown_vk(vk), "第二次不应再提示");
        }

        /// 名称查询:已知码出名字,未知码兜底成 Key(N) 而不是空串
        #[test]
        fn name_lookup_falls_back_gracefully() {
            assert_eq!(evdev_name(30), "KEY_A");
            assert_eq!(evdev_name(272), "BTN_LEFT");
            assert_eq!(evdev_name(276), "BTN_EXTRA");
            assert_eq!(evdev_name(9999), "Key(9999)");
        }
    }
}

#[cfg(windows)]
pub fn win_key_name(code: u16) -> String {
    vktable::evdev_name(code)
}

// ============================ Linux 实现 ============================

#[cfg(target_os = "linux")]
mod linux {
    use super::CaptureEvent;
    use crate::{diag_debug, diag_info, diag_warn};
    use anyhow::Result;
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::Sender;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// 后台线程心跳(热插拔重扫与按键对账都按它的倍数计)
    const HEARTBEAT_MS: u64 = 100;
    /// 重扫 `/dev/input` 的间隔(心跳数):≈2s。插上键盘到能用之间最多等这么久。
    const RESCAN_TICKS: u32 = 20;
    /// 与内核对账按键状态的间隔(心跳数):≈400ms。
    ///
    /// 为什么不需要更快:对账只处理"release 丢了"这种异常,而异常多等 400ms
    /// 没有任何影响;但它会 open/ioctl 设备,太快反而增加无谓的系统调用。
    const RECONCILE_TICKS: u32 = 4;

    /// 单个已打开设备在共享状态里的登记项
    struct Slot {
        /// 该设备当前线程的唯一令牌。线程退出时只注销**属于自己的**那一份,
        /// 否则"设备拔掉 → 新设备复用同一节点名"时会把新线程的登记也一起抹掉。
        token: u64,
        keyboard: bool,
        mouse: bool,
    }

    #[derive(Default)]
    struct State {
        alive: HashMap<PathBuf, Slot>,
        /// 已判定为"既不像键盘也不像鼠标"的节点。
        ///
        /// 重扫每 2 秒跑一次,而一台笔记本上这类节点有十几个(电源键、合盖开关、
        /// 视频总线、HDMI 音频…)。不记住它们的话,每次重扫都要把它们全部
        /// `open` 一遍再判定一次,并且往日志里刷一屏 DEBUG —— 既浪费又淹没重点。
        ///
        /// 节点从 `/dev/input` 消失时会把对应条目清掉:拔掉再插上可能复用同一个
        /// 节点名而设备身份已经变了,那时必须重新判定。
        ignored: HashSet<PathBuf>,
        next_token: u64,
        /// 我们已经**上报为按下**的键码集合(含鼠标键)。
        /// 对账就是拿它减去内核真值,差集就是"我们以为按着、内核说没有"的键。
        delivered: HashSet<u16>,
        /// 对账时复用的设备句柄(同一节点第二次 open,只为读 EVIOCGKEY)。
        /// 缓存是为了避免每 400ms 重新 open 一遍;读失败就丢掉,下次重开。
        truth_devs: HashMap<PathBuf, evdev::Device>,
        /// 上次报告过的"漏掉的按下",用于抑制重复日志
        missed_seen: HashSet<u16>,
    }

    type Shared = Arc<Mutex<State>>;

    /// 带容错的加锁:某处 panic 让锁中毒之后,输入捕获必须还能继续工作
    fn lock(st: &Shared) -> std::sync::MutexGuard<'_, State> {
        st.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn start(
        grab: &Arc<AtomicBool>,
        mouse_grab: &Arc<AtomicBool>,
        _cursor_hide: &Arc<AtomicBool>,
        mouse_found: &Arc<AtomicBool>,
        stop: &Arc<AtomicBool>,
        tx: Sender<CaptureEvent>,
    ) -> Result<Option<Arc<std::sync::atomic::AtomicU32>>> {
        let st: Shared = Arc::new(Mutex::new(State::default()));

        // 首次扫描是**同步**的:必须在这一步就把权限问题暴露给调用方,
        // 而不是丢给后台线程慢慢发现(那时用户已经看到一个"看起来正常但按键全无反应"的界面)
        let (kb, mice, denied) = scan(&st, grab, mouse_grab, stop, &tx);
        mouse_found.store(mice > 0, Ordering::Relaxed);

        if kb == 0 && mice == 0 {
            if denied > 0 {
                anyhow::bail!(
                    "无权限读取输入设备(/dev/input/event*)。\
                     请确认已执行 sudo usermod -aG input $USER 并【重新登录】,\
                     且本程序是在重新登录后启动的(共 {} 个节点打不开,详见诊断日志)",
                    denied
                );
            }
            anyhow::bail!("未找到键盘设备(详见诊断日志里的设备清单)");
        }

        // 后台:热插拔重扫 + 丢失 release 的自愈
        {
            let st = st.clone();
            let grab = grab.clone();
            let mouse_grab = mouse_grab.clone();
            let mouse_found = mouse_found.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                supervisor(st, grab, mouse_grab, mouse_found, stop, tx);
            });
        }
        Ok(None)
    }

    /// 扫描 `/dev/input` 并接管所有"新出现"的键盘/鼠标设备。
    ///
    /// 返回 (键盘数, 鼠标数, 打不开的节点数)。**三个数都要**,因为
    /// "一个都没打开"与"部分打不开"是两种完全不同的故障,日志要能区分。
    fn scan(
        st: &Shared,
        grab: &Arc<AtomicBool>,
        mouse_grab: &Arc<AtomicBool>,
        stop: &Arc<AtomicBool>,
        tx: &Sender<CaptureEvent>,
    ) -> (usize, usize, usize) {
        let mut keyboards = 0usize;
        let mut mice = 0usize;
        let mut denied = 0usize;

        let mut paths = event_paths();
        paths.sort();
        // 先把"已经不存在的忽略项"清掉,再决定本轮的跳过集合
        {
            let present: HashSet<&PathBuf> = paths.iter().collect();
            lock(st).ignored.retain(|p| present.contains(p));
        }
        for path in paths {
            // 已接管:只统计(设备身份不变时不要重复开线程,否则一次按键会被上报两遍)
            let known = {
                let s = lock(st);
                if s.ignored.contains(&path) {
                    continue;
                }
                s.alive.get(&path).map(|slot| (slot.keyboard, slot.mouse))
            };
            if let Some((k, m)) = known {
                keyboards += usize::from(k);
                mice += usize::from(m);
                continue;
            }

            let device = match evdev::Device::open(&path) {
                Ok(d) => d,
                Err(e) => {
                    denied += 1;
                    // 权限与"设备已消失"要分开说:前者要用户去改组,后者是正常竞态
                    let hint = match e.kind() {
                        std::io::ErrorKind::PermissionDenied => {
                            " → 当前用户不在 input 组,或未重新登录"
                        }
                        _ => "",
                    };
                    diag_warn!(
                        "capture",
                        "打不开 {}: {e} (errno {:?}){hint}",
                        path.display(),
                        e.raw_os_error()
                    );
                    continue;
                }
            };

            let name = device.name().unwrap_or("(无名)").to_string();
            let id = device.input_id();
            let is_kb = is_keyboard(&device);
            let is_mouse_dev = is_mouse(&device);
            let key_count = device
                .supported_keys()
                .map(|k| k.iter().count())
                .unwrap_or(0);

            if !is_kb && !is_mouse_dev {
                // 只有排查时才需要看这些"路过"的设备;记进忽略集合,
                // 后续重扫不再重复 open 与记录
                diag_debug!(
                    "capture",
                    "跳过 {} 「{name}」{id:?} 键数={key_count} (既不像键盘也不像鼠标)",
                    path.display()
                );
                lock(st).ignored.insert(path.clone());
                continue;
            }
            if let Err(e) = device.set_nonblocking(true) {
                diag_warn!("capture", "设置非阻塞失败 {}: {e}", path.display());
                continue;
            }

            let token = {
                let mut s = lock(st);
                // 先把令牌算出来再借用 `alive`:直接在字段初始化式里读
                // `s.next_token` 会与外层的可变借用冲突
                let token = s.next_token + 1;
                s.next_token = token;
                s.alive.insert(
                    path.clone(),
                    Slot {
                        token,
                        keyboard: is_kb,
                        mouse: is_mouse_dev,
                    },
                );
                token
            };
            diag_info!(
                "capture",
                "已接管 {} 「{name}」{id:?} 键数={key_count} 键盘={is_kb} 鼠标={is_mouse_dev}",
                path.display()
            );
            if is_kb {
                keyboards += 1;
            }
            if is_mouse_dev {
                mice += 1;
                // 热插鼠标必须让 FPS 面板立刻知道,否则用户只能重启程序
                // (界面上的"检测到鼠标设备"直接读这个标志)
            }

            let grab_flag = if is_kb {
                grab.clone()
            } else {
                mouse_grab.clone()
            };
            let st2 = st.clone();
            let tx2 = tx.clone();
            let stop2 = stop.clone();
            std::thread::spawn(move || {
                // evdev 读取线程 = Linux 版"钩子线程":每台设备一条,事件即读即转。
                crate::priority::boost(crate::priority::Class::Highest);
                device_loop(
                    device,
                    path,
                    token,
                    tx2,
                    grab_flag,
                    stop2,
                    st2,
                    is_mouse_dev,
                );
            });
        }
        (keyboards, mice, denied)
    }

    /// `/dev/input` 下的所有 event 节点
    fn event_paths() -> Vec<PathBuf> {
        let Ok(rd) = std::fs::read_dir("/dev/input") else {
            return Vec::new();
        };
        rd.flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().starts_with("event"))
                    .unwrap_or(false)
            })
            .collect()
    }

    /// 判定"这个节点是键盘"。
    ///
    /// 主判据(字母区 A/Z + 回车)沿用旧实现:一把键盘会同时产生好几个
    /// input 节点(主键区、多媒体、系统控制…),而按键矩阵只落在其中一个上,
    /// 靠"有没有字母键"能把那个节点挑出来。
    ///
    /// 兜底判据是为少数可编程/宏键盘加的:它们可能把字母区拆到不同节点。
    /// 要求同时具备 A + 空格 + 回车 + 足够多的键 —— 遥控器、数字小键盘、
    /// 手柄(BTN_*)都不会同时满足。
    fn is_keyboard(device: &evdev::Device) -> bool {
        let Some(k) = device.supported_keys() else {
            return false;
        };
        if k.contains(evdev::KeyCode::KEY_A)
            && k.contains(evdev::KeyCode::KEY_Z)
            && k.contains(evdev::KeyCode::KEY_ENTER)
        {
            return true;
        }
        k.iter().count() >= 30
            && k.contains(evdev::KeyCode::KEY_A)
            && k.contains(evdev::KeyCode::KEY_SPACE)
            && k.contains(evdev::KeyCode::KEY_ENTER)
    }

    /// 具备 REL_X / REL_Y 相对轴的设备按鼠标处理。
    ///
    /// 注意:现代笔记本触摸板走 ABS 多点触控协议,**没有相对轴**,
    /// 因此不会被认作鼠标(FPS 瞄准用不了)。这是有意的:触摸板的绝对坐标
    /// 需要另一套"手指位移 -> 视角"的模型,不能直接套用相对位移的灵敏度。
    fn is_mouse(device: &evdev::Device) -> bool {
        device
            .supported_relative_axes()
            .map(|a| {
                a.contains(evdev::RelativeAxisCode::REL_X)
                    && a.contains(evdev::RelativeAxisCode::REL_Y)
            })
            .unwrap_or(false)
    }

    /// 单设备读取循环
    #[allow(clippy::too_many_arguments)]
    fn device_loop(
        mut device: evdev::Device,
        path: PathBuf,
        token: u64,
        tx: Sender<CaptureEvent>,
        grab_flag: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
        st: Shared,
        mouse: bool,
    ) {
        let mut grabbed = false;
        // 设备名先取成 owned 字符串:`fetch_events` 期间 `device` 是可变借用的,
        // 那时再调 `device.name()`(不可变借用)会冲突。
        let dev_name = device.name().unwrap_or("(无名)").to_string();
        // 连续多少次"非 WouldBlock"的失败后放弃这个设备。
        // 不能一失败就退出:USB 短暂复位后同一个 fd 有可能恢复;
        // 但也不能永远空转 —— 旧实现就是在这里 50ms 一次无限循环,
        // 设备早没了却还占着线程,而且一声不吭。
        let mut hard_errors = 0u32;
        while !stop.load(Ordering::Relaxed) {
            if token != lock(&st).alive.get(&path).map(|s| s.token).unwrap_or(0) {
                break; // 已被更新的线程接管
            }
            let want = grab_flag.load(Ordering::Relaxed);
            if want != grabbed {
                let r = if want { device.grab() } else { device.ungrab() };
                if r.is_ok() {
                    grabbed = want;
                    diag_info!(
                        "capture",
                        "{} {} 「{}」",
                        if want { "已独占" } else { "已放开" },
                        path.display(),
                        &dev_name
                    );
                }
            }
            match device.fetch_events() {
                Ok(events) => {
                    hard_errors = 0;
                    let mut dx = 0f32;
                    let mut dy = 0f32;
                    for ev in events {
                        match ev.destructure() {
                            evdev::EventSummary::Key(_, key, value) => {
                                // value: 0=抬起 1=按下 2=自动重复
                                if value == 2 {
                                    // 自动重复不转发(引擎用上升沿收口),但它证明
                                    // "这个键此刻确实按着",是天然的心跳,留着无坏处
                                    continue;
                                }
                                let pressed = value == 1;
                                {
                                    let mut s = lock(&st);
                                    if pressed {
                                        s.delivered.insert(key.0);
                                    } else {
                                        s.delivered.remove(&key.0);
                                    }
                                }
                                let _ = tx.send(CaptureEvent::Button {
                                    code: key.0,
                                    pressed,
                                });
                            }
                            evdev::EventSummary::RelativeAxis(_, axis, value) if mouse => {
                                if axis == evdev::RelativeAxisCode::REL_X {
                                    dx += value as f32;
                                } else if axis == evdev::RelativeAxisCode::REL_Y {
                                    dy += value as f32;
                                } else if axis == evdev::RelativeAxisCode::REL_WHEEL
                                    || axis == evdev::RelativeAxisCode::REL_HWHEEL
                                {
                                    if value != 0 {
                                        let code = match (axis, value.is_positive()) {
                                            (evdev::RelativeAxisCode::REL_WHEEL, true) => {
                                                crate::keymap::BTN_WHEEL_UP
                                            }
                                            (evdev::RelativeAxisCode::REL_WHEEL, false) => {
                                                crate::keymap::BTN_WHEEL_DOWN
                                            }
                                            (evdev::RelativeAxisCode::REL_HWHEEL, true) => {
                                                crate::keymap::BTN_WHEEL_RIGHT
                                            }
                                            _ => crate::keymap::BTN_WHEEL_LEFT,
                                        };
                                        for _ in 0..value.unsigned_abs().min(10) {
                                            let _ = tx.send(CaptureEvent::Button {
                                                code,
                                                pressed: true,
                                            });
                                            let _ = tx.send(CaptureEvent::Button {
                                                code,
                                                pressed: false,
                                            });
                                        }
                                    }
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
                Err(e) => {
                    hard_errors += 1;
                    if hard_errors == 1 || hard_errors == 20 {
                        diag_warn!(
                            "capture",
                            "读取 {} 出错(第 {} 次): {e} (errno {:?})",
                            path.display(),
                            hard_errors,
                            e.raw_os_error()
                        );
                    }
                    if hard_errors >= 20 {
                        // 设备确实没了:登记注销,让重扫有机会重新接管。
                        // 注销时校验令牌,避免把复用同一节点名的新设备登记抹掉。
                        {
                            let mut s = lock(&st);
                            if s.alive.get(&path).map(|x| x.token) == Some(token) {
                                s.alive.remove(&path);
                                s.truth_devs.remove(&path);
                            }
                        }
                        diag_warn!(
                            "capture",
                            "放弃 {} 「{}」(连续 {hard_errors} 次读取失败;它被拔掉或复位了,重扫会重新接管)",
                            path.display(),
                            &dev_name
                        );
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
        if grabbed {
            let _ = device.ungrab();
        }
    }

    /// 后台监管线程:热插拔重扫 + 丢失 release 的自愈
    fn supervisor(
        st: Shared,
        grab: Arc<AtomicBool>,
        mouse_grab: Arc<AtomicBool>,
        mouse_found: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
        tx: Sender<CaptureEvent>,
    ) {
        let mut tick: u32 = 0;
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(HEARTBEAT_MS));
            if stop.load(Ordering::Relaxed) {
                break;
            }
            tick = tick.wrapping_add(1);
            if tick % RECONCILE_TICKS == 0 {
                reconcile_keys(&st, &tx);
            }
            if tick % RESCAN_TICKS == 0 {
                let (kb, mice, denied) = scan(&st, &grab, &mouse_grab, &stop, &tx);
                let had = mouse_found.load(Ordering::Relaxed);
                mouse_found.store(mice > 0, Ordering::Relaxed);
                if had && mice == 0 {
                    diag_warn!(
                        "capture",
                        "鼠标设备已全部消失(FPS 瞄准将不可用),等待重新插入"
                    );
                } else if !had && mice > 0 {
                    diag_info!("capture", "检测到鼠标设备({mice} 个),FPS 瞄准现在可用");
                }
                if denied > 0 && kb == 0 && mice == 0 {
                    diag_warn!("capture", "当前没有任何可用输入设备({denied} 个节点打不开)");
                }
            }
        }
        diag_info!("capture", "捕获层监管线程退出");
    }

    /// 与内核真值对账:把"我们以为按着、内核说已经松开"的键补发一次抬起。
    ///
    /// 为什么必须做:底层一旦漏掉一个 KeyRelease(设备被拔、USB 复位、
    /// 事件缓冲区溢出…),引擎侧那个键就会永远留在按下状态 ——
    /// 既占着一个永远抬不起来的幻影触点(设备端触点池一共只有 10 个),
    /// 又让"上升沿"永远不再产生(开关型点按/滑动彻底失效)。
    /// 用户看到的就是"过一会儿某个键就没反应了,非得重新开一次映射"。
    ///
    /// 只补发**抬起**,不补发按下:补发按下会让一次性动作(点按、滑动)
    /// 在用户没碰键盘的时候凭空触发。而"漏掉的按下"我们只报告不动作 ——
    /// 下一次真实按键会正常补上。
    fn reconcile_keys(st: &Shared, tx: &Sender<CaptureEvent>) {
        let (paths, delivered, missed_before) = {
            let s = lock(st);
            if s.delivered.is_empty() && s.missed_seen.is_empty() {
                return; // 没有任何"我们认为按着"的键,无需打扰内核
            }
            (
                s.alive.keys().cloned().collect::<Vec<_>>(),
                s.delivered.clone(),
                s.missed_seen.clone(),
            )
        };

        // 一个设备都没有 = 全被拔了:真值就是"什么都没按着"。
        // 注意"列表非空但都打不开"要区分对待 —— 那是暂时的读失败,不能据此清空。
        if paths.is_empty() {
            if !delivered.is_empty() {
                diag_warn!(
                    "capture",
                    "所有输入设备均已消失,补发 {} 个抬起以防卡键: {}",
                    delivered.len(),
                    names(delivered.iter().copied())
                );
                release_all(st, tx, delivered.iter().copied());
            }
            return;
        }

        let mut truth: HashSet<u16> = HashSet::new();
        let mut read_ok = false;
        {
            let mut s = lock(st);
            for path in &paths {
                // 先确保有句柄(不存在则开一个),再取不可变引用去读。
                // 不能边持有引用边 `entry().or_insert()` —— 同一字段的
                // 不可变借用与可变借用会冲突。
                if !s.truth_devs.contains_key(path) {
                    match evdev::Device::open(path) {
                        Ok(d) => {
                            s.truth_devs.insert(path.clone(), d);
                        }
                        Err(e) => {
                            diag_debug!("capture", "对账时打不开 {}: {e}", path.display());
                            continue;
                        }
                    }
                }
                let mut drop_handle = false;
                {
                    let Some(dev) = s.truth_devs.get(path) else {
                        continue;
                    };
                    match dev.get_key_state() {
                        Ok(keys) => {
                            read_ok = true;
                            for k in keys.iter() {
                                truth.insert(k.0);
                            }
                        }
                        Err(e) => {
                            diag_debug!(
                                "capture",
                                "读 {} 的按键状态失败({e}),丢弃句柄待下次重开",
                                path.display()
                            );
                            drop_handle = true;
                        }
                    }
                }
                if drop_handle {
                    s.truth_devs.remove(path);
                }
            }
        }
        if !read_ok {
            return; // 一个都读不到:宁可不动作,也不要凭空抬起一堆键
        }

        let lost: Vec<u16> = delivered
            .iter()
            .copied()
            .filter(|c| !truth.contains(c))
            .collect();
        if !lost.is_empty() {
            diag_warn!(
                "capture",
                "权威对账:内核认为这些键已抬起,但我们从未收到 release —— 补发抬起以防卡键: {}",
                names(lost.iter().copied())
            );
            release_all(st, tx, lost.iter().copied());
        }

        // 反向(内核说按着、我们没记录)只报告不动作,且只在集合变化时报告一次,
        // 否则会每 400ms 刷一行。
        let missed: HashSet<u16> = truth
            .iter()
            .copied()
            .filter(|c| !delivered.contains(c))
            .collect();
        if missed != missed_before {
            if !missed.is_empty() {
                diag_warn!(
                    "capture",
                    "权威对账:这些键内核认为正按着,但我们没收到按下事件(该次按下已丢失): {}",
                    names(missed.iter().copied())
                );
            }
            lock(st).missed_seen = missed;
        }
    }

    /// 补发抬起并同步本地"已上报"集合
    fn release_all<I: IntoIterator<Item = u16>>(st: &Shared, tx: &Sender<CaptureEvent>, codes: I) {
        for code in codes {
            let _ = tx.send(CaptureEvent::Button {
                code,
                pressed: false,
            });
            lock(st).delivered.remove(&code);
        }
    }

    /// 键码列表 -> 可读名(日志里"17 31"远不如"KEY_W KEY_S"好认)
    fn names<I: IntoIterator<Item = u16>>(codes: I) -> String {
        let mut v: Vec<String> = codes.into_iter().map(crate::keymap::key_name).collect();
        v.sort();
        v.join(" ")
    }
}

// ============================ Windows 实现 ============================

#[cfg(windows)]
mod windows {
    use super::{CaptureEvent, vktable};
    use crate::keymap::{BTN_WHEEL_DOWN, BTN_WHEEL_LEFT, BTN_WHEEL_RIGHT, BTN_WHEEL_UP};
    use crate::{diag_debug, diag_error, diag_info, diag_warn};
    use anyhow::Result;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};
    use std::sync::mpsc::{Sender, channel};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{LPARAM, LRESULT, POINT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
    };
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, CreateCursor, GetCursorPos, GetMessageW, GetSystemMetrics, HC_ACTION,
        IDC_ARROW, KBDLLHOOKSTRUCT, LoadCursorW, MSG, MSLLHOOKSTRUCT, OCR_APPSTARTING, OCR_CROSS,
        OCR_HAND, OCR_HELP, OCR_IBEAM, OCR_NO, OCR_NORMAL, OCR_SIZEALL, OCR_SIZENESW, OCR_SIZENS,
        OCR_SIZENWSE, OCR_SIZEWE, OCR_UP, OCR_WAIT, PostThreadMessageW, SM_CXSCREEN, SM_CYSCREEN,
        SPI_SETCURSORS, SPIF_SENDCHANGE, SYSTEM_CURSOR_ID, SetCursor, SetCursorPos,
        SetSystemCursor, SetWindowsHookExW, SystemParametersInfoW, UnhookWindowsHookEx,
        WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP,
        WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN,
        WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
    };

    /// 抓取鼠标时,光标离屏幕中心超过该比例(相对显示器短边)才拉回中心。
    ///
    /// 旧实现是固定 64px —— 在 1080p 上,1600 DPI 的鼠标一次快速甩动就能走
    /// 50~100px,于是几乎**每个事件**都触发回中,而每次回中都会吃掉/反转
    /// 一次真实位移,表现就是"视角能动但幅度极小、一卡一卡"。
    const RECENTER_RATIO: i32 = 4; // 短边的 1/4
    /// 回中阈值的下限(小显示器/小窗口时别退化成"每帧回中")
    const RECENTER_MIN_PX: i32 = 96;

    /// 丢弃 release 的对账间隔
    const RECONCILE_INTERVAL: Duration = Duration::from_millis(400);
    /// 松键误判的宽限期:release 事件在队列中可能晚于 GetAsyncKeyState 的
    /// 状态变化到达。宽限期内即使系统说已抬起,也不补发,避免把真实按住
    /// (尤其是自动重复中的键)误判成漏抬,再被下一次重复按下重新触发。
    const RECONCILE_GRACE_MS: u64 = 1500;
    /// 回调耗时统计的汇总间隔
    const TIMING_REPORT_INTERVAL: Duration = Duration::from_secs(5);

    /// 回调耗时直方图(按 2 的幂分桶)。
    ///
    /// 为什么需要:Windows 对低级钩子有 `LowLevelHooksTimeout`(默认 300ms),
    /// 超时的事件会被静默丢弃、反复超时还会静默摘掉整个钩子。所以"回调里花了
    /// 多久"是这个平台最关键的指标 —— 有它才能判断"我到底离危险线还有多远"。
    struct Timing {
        buckets: [AtomicU64; 24],
        count: AtomicU64,
        max: AtomicU64,
    }

    impl Default for Timing {
        fn default() -> Self {
            Self {
                buckets: std::array::from_fn(|_| AtomicU64::new(0)),
                count: AtomicU64::new(0),
                max: AtomicU64::new(0),
            }
        }
    }

    impl Timing {
        fn record(&self, nanos: u64) {
            self.count.fetch_add(1, Ordering::Relaxed);
            let idx = if nanos == 0 {
                0
            } else {
                (u64::BITS - nanos.leading_zeros()) as usize
            }
            .min(self.buckets.len() - 1);
            self.buckets[idx].fetch_add(1, Ordering::Relaxed);
            self.max.fetch_max(nanos, Ordering::Relaxed);
        }

        /// 按分桶上界估算分位数(足够回答"有没有逼近超时")
        fn quantile_nanos(&self, q: f64) -> u64 {
            let total = self.count.load(Ordering::Relaxed);
            if total == 0 {
                return 0;
            }
            let want = (total as f64 * q).ceil() as u64;
            let mut acc = 0u64;
            for (i, b) in self.buckets.iter().enumerate() {
                acc += b.load(Ordering::Relaxed);
                if acc >= want {
                    return if i == 0 { 1 } else { 1u64 << i };
                }
            }
            1u64 << (self.buckets.len() - 1)
        }

        /// 取走本轮的统计并清零(用于周期性汇总)
        fn take_report(&self) -> String {
            let count = self.count.swap(0, Ordering::Relaxed);
            let max = self.max.swap(0, Ordering::Relaxed);
            let p50 = self.quantile_nanos(0.50);
            let p99 = self.quantile_nanos(0.99);
            for b in &self.buckets {
                b.store(0, Ordering::Relaxed);
            }
            format!(
                "钩子回调 {} 次,耗时 p50≈{:.1}µs p99≈{:.1}µs 最大 {:.1}µs(超时线 300ms)",
                count,
                p50 as f64 / 1000.0,
                p99 as f64 / 1000.0,
                max as f64 / 1000.0
            )
        }
    }

    /// 队列里的事件:绝对坐标由消费线程做差分。
    ///
    /// 为什么不直接在回调里算位移:算位移要读/写"上一次位置"、还要在需要时
    /// `SetCursorPos` 回中 —— 那些都是**会阻塞的 Win32 调用**。低级钩子回调里
    /// 只允许做"读字段、查表、塞队列"这三件事。
    enum RawEvent {
        Key { code: u16, pressed: bool },
        Move { x: f64, y: f64 },
    }

    struct WinShared {
        raw_tx: Sender<RawEvent>,
        /// 每个键码最后一次事件的时刻(ms since start);0 表示"该键不在按下状态"
        last_seen: Box<[AtomicU64]>,
        timing: Timing,
        start: Instant,
    }

    /// 钩子回调无法捕获环境,共享状态必须放静态里。这里不能用 `OnceLock`:
    /// 主题切换会让 eframe 在**同一进程内关闭并重新打开窗口**,从而第二次
    /// 调用 Capture::start；OnceLock 会保留上一次已经断开的 raw_tx,新钩子
    /// 仍然把事件送进死队列,表现就是“映射完全失效”。
    /// 改成原子指针；每次启动换入新的 Box。旧 Box 故意不释放,避免退出中的
    /// 旧钩子线程仍可能读到它。
    static SHARED_PTR: AtomicPtr<WinShared> = AtomicPtr::new(std::ptr::null_mut());

    fn shared() -> Option<&'static WinShared> {
        let ptr = SHARED_PTR.load(Ordering::Acquire);
        if ptr.is_null() {
            None
        } else {
            Some(unsafe { &*ptr })
        }
    }

    fn install_shared(raw_tx: Sender<RawEvent>) -> *mut WinShared {
        let shared = Box::into_raw(Box::new(WinShared {
            raw_tx,
            last_seen: (0..512).map(|_| AtomicU64::new(0)).collect(),
            timing: Timing::default(),
            start: Instant::now(),
        }));
        SHARED_PTR.store(shared, Ordering::Release);
        shared
    }
    /// 两个钩子的句柄分开存 —— rdev 当年用一个 `static mut HOOK` 装了两把,
    /// 后装的把先装的覆盖掉,于是键盘钩子永远卸不下来。
    static HOOK_KB: AtomicU64 = AtomicU64::new(0);
    static HOOK_MS: AtomicU64 = AtomicU64::new(0);

    fn now_ms() -> u64 {
        shared()
            .map(|s| s.start.elapsed().as_millis() as u64)
            .unwrap_or(0)
    }

    pub fn start(
        grab: &Arc<AtomicBool>,
        mouse_grab: &Arc<AtomicBool>,
        cursor_hide: &Arc<AtomicBool>,
        mouse_found: &Arc<AtomicBool>,
        stop: &Arc<AtomicBool>,
        tx: Sender<CaptureEvent>,
    ) -> Result<(Option<Arc<AtomicU32>>, std::thread::JoinHandle<()>)> {
        // 键盘抓取在 Windows 上无法实现(低级钩子只能观察,不能拦截),
        // 鼠标抓取走"隐藏光标 + 回中",下面由消费线程执行
        let _ = grab;
        let (raw_tx, raw_rx) = channel::<RawEvent>();
        let _ = install_shared(raw_tx);

        let wake = Arc::new(AtomicU32::new(0));

        // 消费线程:算位移、维护光标、定期对账
        {
            let mouse_grab = mouse_grab.clone();
            let cursor_hide = cursor_hide.clone();
            let mouse_found = mouse_found.clone();
            let stop = stop.clone();
            let start = Instant::now();
            let txt = tx.clone();
            std::thread::spawn(move || {
                // 钩子→引擎的中转线程:轻量但要及时。
                crate::priority::boost(crate::priority::Class::AboveNormal);
                consumer(
                    raw_rx,
                    txt,
                    mouse_grab,
                    cursor_hide,
                    mouse_found,
                    stop,
                    start,
                );
            });
        }

        // 钩子线程:装钩子 + 消息循环。低级钩子必须装在跑消息循环的那个线程上。
        let hook_thread = {
            let wake = wake.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                // 低级钩子回调跑在这个线程上,系统对钩子投递有低延迟预期:
                // 提到最高优先级,满载机器上也能第一时间处理输入。
                crate::priority::boost(crate::priority::Class::Highest);
                install_and_pump(wake, stop);
            })
        };

        Ok((Some(wake), hook_thread))
    }

    /// 唤醒钩子线程(退出时用;`GetMessage` 阻塞,光置标志它看不见)
    pub fn post_quit(tid: u32) {
        const WM_QUIT: u32 = 0x0012;
        unsafe {
            PostThreadMessageW(tid, WM_QUIT, 0, 0);
        }
    }

    fn install_and_pump(wake: Arc<AtomicU32>, stop: Arc<AtomicBool>) {
        let tid = unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() };
        wake.store(tid, Ordering::Relaxed);

        unsafe {
            let h = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), std::ptr::null_mut(), 0);
            if h.is_null() {
                diag_error!(
                    "capture",
                    "安装键盘钩子失败(GetLastError={});按键映射将完全不可用",
                    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
                );
            } else {
                HOOK_KB.store(h as u64, Ordering::Relaxed);
                diag_info!("capture", "键盘低级钩子已安装");
            }
            let h = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), std::ptr::null_mut(), 0);
            if h.is_null() {
                diag_error!(
                    "capture",
                    "安装鼠标钩子失败(GetLastError={});鼠标按键与 FPS 瞄准将不可用",
                    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
                );
            } else {
                HOOK_MS.store(h as u64, Ordering::Relaxed);
                diag_info!("capture", "鼠标低级钩子已安装");
            }
        }

        // 消息循环。注意:`GetMessageW` 在没有消息时阻塞,这是**必须**的
        // (低级钩子靠系统调用宿主线程来处理事件),不能改成轮询。
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        loop {
            let r = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
            if r <= 0 {
                break; // 0 = WM_QUIT,-1 = 出错
            }
            if stop.load(Ordering::Relaxed) {
                break;
            }
        }

        unsafe {
            let kb = HOOK_KB.swap(0, Ordering::Relaxed);
            if kb != 0 {
                UnhookWindowsHookEx(kb as _);
            }
            let ms = HOOK_MS.swap(0, Ordering::Relaxed);
            if ms != 0 {
                UnhookWindowsHookEx(ms as _);
            }
        }
        diag_info!("capture", "Windows 钩子线程已退出(钩子已卸载)");
    }

    /// 记一次事件时刻(供丢失 release 的对账使用),并记录回调耗时。
    ///
    /// 这里只有两次 relaxed 原子操作 —— 相对"附着到前台线程"那种开销,
    /// 可以认为不花时间。绝不在此加锁、绝不调用任何可能阻塞的 Win32 API。
    fn mark(shared: &WinShared, code: u16, pressed: bool, began: Instant) {
        if (code as usize) < shared.last_seen.len() {
            let v = if pressed { now_ms().max(1) } else { 0 };
            shared.last_seen[code as usize].store(v, Ordering::Relaxed);
        }
        shared.timing.record(began.elapsed().as_nanos() as u64);
    }

    unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        let began = Instant::now();
        if code == HC_ACTION as i32 {
            if let Some(shared) = shared() {
                let kb = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
                let pressed = matches!(wparam as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
                let released = matches!(wparam as u32, WM_KEYUP | WM_SYSKEYUP);
                if pressed || released {
                    match vktable::map_vk(kb.vkCode as u16) {
                        Some(ev) => {
                            mark(shared, ev, pressed, began);
                            let _ = shared.raw_tx.send(RawEvent::Key { code: ev, pressed });
                        }
                        None => {
                            // 首次遇到未映射的键码时记一条:用户报"这个键绑不上"时,
                            // 日志里直接就有答案,补一行表即可
                            if vktable::note_unknown_vk(kb.vkCode as u16) {
                                diag_debug!(
                                    "capture",
                                    "未映射的虚拟键码 {} (0x{:02X}),该键当前无法绑定",
                                    kb.vkCode,
                                    kb.vkCode
                                );
                            }
                        }
                    }
                }
            }
        }
        // 永远放行:Windows 上无法通过低级钩子屏蔽输入(这也是"独占键盘"
        // 在 Windows 不生效的原因),这里不做任何拦截。
        unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
    }

    unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        let began = Instant::now();
        if code == HC_ACTION as i32 {
            if let Some(shared) = shared() {
                let msg = wparam as u32;
                if msg == WM_MOUSEWHEEL || msg == WM_MOUSEHWHEEL {
                    let ms = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
                    let delta = ((ms.mouseData >> 16) & 0xFFFF) as u16 as i16;
                    if delta != 0 {
                        let code = if msg == WM_MOUSEWHEEL {
                            if delta > 0 {
                                BTN_WHEEL_UP
                            } else {
                                BTN_WHEEL_DOWN
                            }
                        } else if delta > 0 {
                            BTN_WHEEL_RIGHT
                        } else {
                            BTN_WHEEL_LEFT
                        };
                        mark(shared, code, true, began);
                        let _ = shared.raw_tx.send(RawEvent::Key {
                            code,
                            pressed: true,
                        });
                        mark(shared, code, false, began);
                        let _ = shared.raw_tx.send(RawEvent::Key {
                            code,
                            pressed: false,
                        });
                    }
                    return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
                }
                let ev_code = match wparam as u32 {
                    WM_LBUTTONDOWN | WM_LBUTTONUP => Some(272u16), // BTN_LEFT
                    WM_RBUTTONDOWN | WM_RBUTTONUP => Some(273u16), // BTN_RIGHT
                    WM_MBUTTONDOWN | WM_MBUTTONUP => Some(274u16), // BTN_MIDDLE
                    WM_XBUTTONDOWN | WM_XBUTTONUP => {
                        // XBUTTON1/2 -> BTN_SIDE/BTN_EXTRA。
                        // 旧实现写成 275+n(得到 276/277),与 Linux 的
                        // BTN_SIDE=275/BTN_EXTRA=276 差一位,同一颗物理侧键
                        // 在两个平台上会绑成不同的键码,配置无法互通。
                        let ms = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
                        let which = ((ms.mouseData >> 16) & 0xFFFF) as u16;
                        Some(if which == 1 { 275 } else { 276 })
                    }
                    WM_MOUSEMOVE => None,
                    _ => None,
                };
                if let Some(c) = ev_code {
                    let pressed = matches!(
                        wparam as u32,
                        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
                    );
                    mark(shared, c, pressed, began);
                    let _ = shared.raw_tx.send(RawEvent::Key { code: c, pressed });
                } else if wparam as u32 == WM_MOUSEMOVE {
                    let ms = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
                    let _ = shared.raw_tx.send(RawEvent::Move {
                        x: ms.pt.x as f64,
                        y: ms.pt.y as f64,
                    });
                    shared.timing.record(began.elapsed().as_nanos() as u64);
                }
            }
        }
        unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
    }

    /// 光标所在显示器的中心与回中阈值
    fn monitor_center() -> ((i32, i32), i32) {
        unsafe {
            let mut pt: POINT = std::mem::zeroed();
            let mon = if GetCursorPos(&mut pt) != 0 {
                MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST)
            } else {
                std::ptr::null_mut()
            };
            let mut mi: MONITORINFO = std::mem::zeroed();
            mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            if !mon.is_null() && GetMonitorInfoW(mon, &mut mi) != 0 {
                let r = mi.rcMonitor;
                let (w, h) = (r.right - r.left, r.bottom - r.top);
                let thr = (w.min(h) / RECENTER_RATIO).max(RECENTER_MIN_PX);
                return (((r.left + r.right) / 2, (r.top + r.bottom) / 2), thr);
            }
            // 兜底:主屏中心(多显示器下不理想,但好过不用)
            (
                (
                    GetSystemMetrics(SM_CXSCREEN) / 2,
                    GetSystemMetrics(SM_CYSCREEN) / 2,
                ),
                RECENTER_MIN_PX,
            )
        }
    }

    static CURSOR_HIDDEN: AtomicBool = AtomicBool::new(false);
    static CURSOR_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    pub(super) fn set_cursor_visible(visible: bool) {
        let _guard = CURSOR_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        if visible {
            // Always restore the user's configured cursor scheme.  This also
            // repairs a stale blank cursor left by an abruptly killed run.
            let restored = unsafe {
                let ok =
                    SystemParametersInfoW(SPI_SETCURSORS, 0, std::ptr::null_mut(), SPIF_SENDCHANGE);
                SetCursor(LoadCursorW(std::ptr::null_mut(), IDC_ARROW));
                ok
            };
            if restored == 0 {
                diag_warn!(
                    "capture",
                    "恢复系统光标方案失败: {}",
                    std::io::Error::last_os_error()
                );
            }
            CURSOR_HIDDEN.store(false, Ordering::Release);
            return;
        }
        if CURSOR_HIDDEN.load(Ordering::Acquire) {
            return;
        }

        // ShowCursor only controls the calling thread's visible window, which
        // is why scrcpy's window kept painting its own arrow.  Replace the
        // system cursor images with a transparent cursor instead; this is
        // global and does not depend on which application has focus.
        let ids: [SYSTEM_CURSOR_ID; 14] = [
            OCR_NORMAL,
            OCR_IBEAM,
            OCR_WAIT,
            OCR_CROSS,
            OCR_UP,
            OCR_SIZENWSE,
            OCR_SIZENESW,
            OCR_SIZEWE,
            OCR_SIZENS,
            OCR_SIZEALL,
            OCR_NO,
            OCR_HAND,
            OCR_APPSTARTING,
            OCR_HELP,
        ];
        let and_mask = [0xFF_u8, 0xFF];
        let xor_mask = [0_u8, 0];
        let mut installed = false;
        unsafe {
            for id in ids {
                let cursor = CreateCursor(
                    std::ptr::null_mut(),
                    0,
                    0,
                    1,
                    1,
                    and_mask.as_ptr().cast(),
                    xor_mask.as_ptr().cast(),
                );
                if !cursor.is_null() && SetSystemCursor(cursor, id) != 0 {
                    installed = true;
                }
            }
            SetCursor(std::ptr::null_mut());
        }
        CURSOR_HIDDEN.store(installed, Ordering::Release);
        if !installed {
            diag_warn!("capture", "无法安装透明系统光标，指针消隐可能无效");
        }
    }

    fn move_cursor(x: i32, y: i32) {
        unsafe {
            SetCursorPos(x, y);
        }
    }

    /// 鼠标位移与光标状态(只被消费线程碰,因此无需加锁)
    struct Motion {
        last: Option<(f64, f64)>,
        center: (i32, i32),
        threshold: i32,
        /// "我们自己刚把光标挪到过这里"的预期回声。
        ///
        /// 旧实现用一个布尔 `skip_next`:只要置了它就无条件丢弃**下一条**
        /// 鼠标事件。问题在于回声到达的时机没有保证 —— 真实移动可能抢在回声
        /// 之前到达,于是那条真实位移被丢掉;随后回声到达,反而被当成
        /// "从新位置回到中心"的反向位移发出去。快速甩动时每个事件都回中,
        /// 于是每条位移都要经历一次"丢一条 + 发一条反向"。
        /// 改成记录**预期的坐标**:只有位置真的落在那里才判定为回声。
        /// 真位移一来就把预期清掉(那条位移绝不丢失),回声没来也不再赖着。
        echo: Option<(i32, i32)>,
        hiding: bool,
    }

    impl Motion {
        fn new() -> Self {
            let (c, t) = monitor_center();
            Self {
                last: None,
                center: c,
                threshold: t,
                echo: None,
                hiding: false,
            }
        }
    }

    fn consumer(
        rx: std::sync::mpsc::Receiver<RawEvent>,
        tx: Sender<CaptureEvent>,
        mouse_grab: Arc<AtomicBool>,
        cursor_hide: Arc<AtomicBool>,
        mouse_found: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
        _start: Instant,
    ) {
        // Windows 上 rdev 时代假定的"总有鼠标"在纯钩子模式下依然成立:
        // 鼠标钩子一装上就能收到事件。这里仍保留一个标志,便于界面自检显示。
        mouse_found.store(true, Ordering::Relaxed);

        let mut motion = Motion::new();
        let mut pressed: HashSet<u16> = HashSet::new();
        let mut last_reconcile = Instant::now();
        let mut last_report = Instant::now();
        let mut last_fg: isize = 0; // 前台窗口句柄:变化即"切换了焦点"

        while !stop.load(Ordering::Relaxed) {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(RawEvent::Key {
                    code,
                    pressed: down,
                }) => {
                    if down {
                        // 不断重复补发,并让真正的自动重复事件重新触发一次性动作。
                        // 关键:补发之后同步本地按下集合。否则下一次对账仍会看到这个幻影键,
                        pressed.remove(&code);
                    }
                    let _ = tx.send(CaptureEvent::Button {
                        code,
                        pressed: down,
                    });
                }
                Ok(RawEvent::Move { x, y }) => {
                    handle_move(&mut motion, &mouse_grab, &cursor_hide, x, y, &tx);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }

            // 焦点切换后光标可能落在别的显示器上:重新取一次中心,
            // 否则"回中"会把光标反复拽向一个错误的点,位移几乎全被吃掉。
            let fg = foreground_handle();
            if fg != last_fg {
                last_fg = fg;
                let (c, t) = monitor_center();
                motion.center = c;
                motion.threshold = t;
                motion.last = None;
                motion.echo = None;
                diag_debug!("capture", "前台窗口变化,重新取显示器中心 {:?}", c);
            }

            let grabbing = mouse_grab.load(Ordering::Relaxed);
            let hide_cursor = grabbing || cursor_hide.load(Ordering::Relaxed);
            if hide_cursor != motion.hiding {
                set_cursor_visible(!hide_cursor);
                motion.hiding = hide_cursor;
                if grabbing {
                    let (c, t) = monitor_center();
                    motion.center = c;
                    motion.threshold = t;
                    move_cursor(c.0, c.1);
                    motion.last = Some((c.0 as f64, c.1 as f64));
                    motion.echo = Some(c);
                    diag_info!("capture", "鼠标已捕获(光标隐去并按中心回中)");
                } else if hide_cursor {
                    diag_info!("capture", "系统光标已由全局消隐开关隐藏");
                } else {
                    diag_info!("capture", "系统光标已恢复显示");
                }
            }

            if last_reconcile.elapsed() >= RECONCILE_INTERVAL {
                last_reconcile = Instant::now();
                if let Some(s) = shared() {
                    reconcile(&mut pressed, &tx, &s.last_seen);
                }
            }
            if last_report.elapsed() >= TIMING_REPORT_INTERVAL {
                last_report = Instant::now();
                if let Some(s) = shared() {
                    let r = s.timing.take_report();
                    // 只在不健康时才值得占用一行日志:正常时 p99 是微秒级
                    let p99 = s.timing.quantile_nanos(0.99);
                    if p99 > 5_000_000 {
                        diag_warn!("capture", "{r}");
                    } else {
                        diag_debug!("capture", "{r}");
                    }
                }
            }
        }
        if motion.hiding {
            set_cursor_visible(true);
        }
        diag_info!("capture", "Windows 捕获消费线程退出");
    }

    /// 处理一次绝对坐标移动:差分 -> 回声识别 -> 必要时回中
    fn handle_move(
        motion: &mut Motion,
        mouse_grab: &Arc<AtomicBool>,
        cursor_hide: &Arc<AtomicBool>,
        x: f64,
        y: f64,
        tx: &Sender<CaptureEvent>,
    ) {
        if mouse_grab.load(Ordering::Relaxed) || cursor_hide.load(Ordering::Relaxed) {
            unsafe {
                SetCursor(std::ptr::null_mut());
            }
        }
        let pos = (x, y);
        let mut suppress = false;
        if let Some(e) = motion.echo {
            if (x.round() as i32, y.round() as i32) == e {
                // 这正是我们自己 SetCursorPos 触发的回声:丢弃,但把 last 对齐到它
                motion.echo = None;
                suppress = true;
            } else {
                // 回声没来(被系统合并/丢弃),或抢在了真位移后面:
                // 无论哪种,这条都是真实位移,必须发出去。
                motion.echo = None;
            }
        }
        let d = match motion.last {
            Some((px, py)) => (x - px, y - py),
            None => (0.0, 0.0),
        };
        motion.last = Some(pos);
        if !suppress && (d.0 != 0.0 || d.1 != 0.0) {
            let _ = tx.send(CaptureEvent::Motion {
                dx: d.0 as f32,
                dy: d.1 as f32,
            });
        }

        if mouse_grab.load(Ordering::Relaxed)
            && ((x - motion.center.0 as f64).abs() > motion.threshold as f64
                || (y - motion.center.1 as f64).abs() > motion.threshold as f64)
        {
            let c = motion.center;
            move_cursor(c.0, c.1);
            motion.last = Some((c.0 as f64, c.1 as f64));
            motion.echo = Some(c);
        }
    }

    /// 当前前台窗口的句柄(转成 isize 便于比较)。
    ///
    /// 句柄变化 ≈ 用户切换了程序/窗口焦点。为什么要盯着它:切换之后光标很可能
    /// 落在**另一个显示器**上,而回中用的中心点还是旧的那个 —— 于是回中条件
    /// 几乎每个事件都成立,表现就是"视角能动但幅度极小、一卡一卡"。
    /// 用窗口句柄而不是桌面会话 id 是为了少依赖一个 Win32 API(会话 id 只在
    /// 多用户/远程桌面场景才有区别,对焦点切换这个用途毫无增益)。
    fn foreground_handle() -> isize {
        use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
        unsafe { GetForegroundWindow() as isize }
    }

    /// 与系统对账:只有"键还在本地按下表 + 系统也明确抬起 + 距最后事件已过宽限期"
    /// 三条同时成立,才认定 release 真丢了。
    ///
    /// 旧实现只看 `GetAsyncKeyState`,会在真实 KeyRelease 还在队列里排队时就
    /// 抢先把键判死:随后向引擎补发一个 UP,而键盘自动重复的下一帧 DOWN 又被
    /// 当成全新按下,于是 Hold 触点会被反复重按。A+U+K 这类"一个持续方向 +
    /// 多个 Hold 技能"的组合正好能持续供给重复 DOWN,所以会卡成死循环。
    fn reconcile(pressed: &mut HashSet<u16>, tx: &Sender<CaptureEvent>, last_seen: &[AtomicU64]) {
        if pressed.is_empty() {
            return;
        }
        let now = now_ms();
        let mut lost: Vec<u16> = Vec::new();
        for &code in pressed.iter() {
            let Some(vk) = vktable::vk_for_evdev(code) else {
                continue;
            };
            // 高位为 1 表示此刻按着
            let down = unsafe { GetAsyncKeyState(vk as i32) } < 0;
            let seen = last_seen
                .get(code as usize)
                .map(|v| v.load(Ordering::Relaxed))
                .unwrap_or(0);
            if release_is_lost(down, seen, now, RECONCILE_GRACE_MS) {
                lost.push(code);
            }
        }
        if lost.is_empty() {
            return;
        }
        let names: Vec<String> = lost.iter().map(|&c| crate::keymap::key_name(c)).collect();
        diag_warn!(
            "capture",
            "权威对账:宽限期内没有任何后续事件且系统确认已抬起,补发 KeyRelease 以防卡键: {}",
            names.join(" ")
        );
        for code in lost {
            pressed.remove(&code);
            let _ = tx.send(CaptureEvent::Button {
                code,
                pressed: false,
            });
        }
    }

    /// `seen == 0` 表示我们从未记录到按下心跳,宁可不动,避免凭空补抬。
    /// 判定公式抽出来便于单元测试:系统抬起、且最后一次事件已超过宽限期。
    pub(super) fn release_is_lost(
        system_down: bool,
        seen_ms: u64,
        now_ms: u64,
        grace_ms: u64,
    ) -> bool {
        !system_down && seen_ms != 0 && now_ms.saturating_sub(seen_ms) >= grace_ms
    }

    #[cfg(test)]
    mod tests {
        use super::{RawEvent, WinShared, install_shared, release_is_lost, shared};
        use std::sync::mpsc::channel;

        #[test]
        fn restart_replaces_windows_hook_shared_transport() {
            let (tx1, _rx1) = channel::<RawEvent>();
            let first = install_shared(tx1);
            let (tx2, _rx2) = channel::<RawEvent>();
            let second = install_shared(tx2);
            assert_ne!(first, second);
            assert_eq!(
                shared().map(|s| s as *const WinShared),
                Some(second as *const WinShared)
            );
            shared()
                .unwrap()
                .raw_tx
                .send(RawEvent::Key {
                    code: 17,
                    pressed: true,
                })
                .unwrap();
            assert!(matches!(
                _rx2.recv_timeout(std::time::Duration::from_millis(100)),
                Ok(RawEvent::Key {
                    code: 17,
                    pressed: true
                })
            ));
        }

        #[test]
        fn lost_release_requires_system_up_and_grace_period() {
            assert!(
                !release_is_lost(true, 100, 10_000, 1_500),
                "系统仍按住时不能补抬"
            );
            assert!(
                !release_is_lost(false, 9_500, 10_000, 1_500),
                "宽限期内不能抢在真实事件前补抬"
            );
            assert!(
                release_is_lost(false, 8_000, 10_000, 1_500),
                "系统抬起且超过宽限期才可补抬"
            );
            assert!(
                !release_is_lost(false, 0, 10_000, 1_500),
                "没有按下心跳时不能凭空补抬"
            );
        }
    }
}
