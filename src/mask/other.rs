//! 非 Windows 平台:蒙版是空实现。
//!
//! 为什么不做 Linux 版:那边的"鼠标独占"由 evdev 的 `EVIOCGRAB` 承担,而蒙版的
//! 真实价值是**压住投屏窗口、吃掉鼠标事件**;Linux 上要以 X11/Wayland 两套
//! override-redirect / layer-shell 各写一份,收益与风险都不划算。
//! 接口保持同形,界面据此直接隐藏入口(见 [`available`])。

use super::{Frame, MaskRect};

pub fn available() -> bool {
    false
}

/// 非 Windows 没有"按 pid 找窗口矩形"这套(那要靠 X11/Wayland 各自的协议)。
/// 返回 `None` ⇒ 调用方按既有约定**退回隔膜**。
pub fn window_rect_of_pid(_pid: u32) -> Option<MaskRect> {
    None
}

/// 非 Windows 没有客户区概念(那要靠 X11/Wayland 各自的协议)。
/// 返回 `None` ⇒ 调用方退回用整窗矩形。
pub fn client_area_of_pid(_pid: u32) -> Option<(i32, i32, i32, i32)> {
    None
}

pub fn description() -> Vec<String> {
    vec!["蒙版窗口:本平台不支持(仅 Windows)".to_string()]
}

/// 无界面自检:本平台没有可验证的东西,直接返回 0(不算失败)。
pub fn selftest() -> i32 {
    println!("[SKIP] 蒙版自检:仅 Windows 有效");
    0
}

/// 空句柄:所有方法都是无操作,存在只是为了让调用点不必到处 `cfg`。
pub struct Mask;

impl Mask {
    pub fn start(_rect: MaskRect) -> Result<Self, String> {
        Err("本平台不支持蒙版窗口".to_string())
    }

    pub fn set_frame(&self, _frame: Frame) {}

    pub fn hwnd(&self) -> u64 {
        0
    }

    pub fn alive(&self) -> bool {
        false
    }
}
