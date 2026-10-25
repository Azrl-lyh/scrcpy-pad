//! 蒙版模式(批次 0 探雷,2026-10-11)。
//!
//! # 它解决什么
//!
//! 我们依赖外部 scrcpy 窗口出画面,于是**同一次点击/拖动会被投递两份**:一份给
//! scrcpy(它自己再转发一套触控),一份给本程序的键鼠映射。既有的「鼠标独占(隔膜)」
//! 是在钩子层把消息吞掉;蒙版则是**在窗口层**解决同一件事 ——
//! 一块贴在投屏区域上的透明窗口,鼠标事件落在它身上,底下的 scrcpy 窗口**收不到**,
//! 双份输入从根上消失;而它自己不透明的地方只有画出来的圈与字,**画面照常透出来**。
//!
//! # 为什么不用 egui 视口
//!
//! egui 0.36.1 确实有 `with_transparent` / `with_mouse_passthrough`,但那条路要
//! 取决于 eframe + glow 后端在 Windows 上的逐像素透明实现,不确定性在**框架**里。
//! 这里改用 `capture.rs` 早就跑通的那套 Win32 基建(注册窗口类 + `wndproc` + 消息泵,
//! 见 `capture.rs` 的 Raw Input 接收窗口):加 `WS_EX_LAYERED` + `LWA_COLORKEY`,
//! 用"键色"当背景 —— **键色像素被系统置为完全透明**,其余像素原样显示。
//! 确定性全在自己手里,且不需要动 eframe。
//!
//! 关于点击穿透:MSDN 对 `WS_EX_TRANSPARENT` 的定义是**绘制顺序**
//! ("在下面的兄弟窗口绘制之后再绘制本窗口"),它不是点击穿透开关。
//! 真正让鼠标消息落到下层窗口的是 `WM_NCHITTEST` 返回 `HTTRANSPARENT` —— 本模块
//! 就是这么做的(见 [`platform`] 的 `nchittest`)。
//!
//! # 平台
//!
//! 仅 Windows 有实现;其他平台是空实现([`available`] 返回 false),界面据此隐藏入口。

#![allow(dead_code)]

use std::sync::Arc;

#[cfg(windows)]
#[path = "mask/win.rs"]
mod platform;
#[cfg(not(windows))]
#[path = "mask/other.rs"]
mod platform;

// 批次 0 只交付"窗口层本身 + 自检",界面接线在批次 5 —— 在此之前
// `Mask` / `available` / `description` 会被自检与后续接线用到,先按未使用放行。
#[allow(unused_imports)]
pub use platform::{
    Mask, available, client_area_of_pid, description, selftest, window_rect_of_pid,
};
/// 蒙版窗口在屏幕上的矩形(物理像素,与 `SetWindowPos` 同一坐标系)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaskRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Default for MaskRect {
    fn default() -> Self {
        Self {
            x: 0,
            y: 0,
            w: 800,
            h: 600,
        }
    }
}

impl MaskRect {
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub fn center(&self) -> (i32, i32) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }

    /// 宽高至少 32px:零尺寸窗口会连"画在哪"都看不出来,也会让命中测试失去意义。
    pub fn normalized(self) -> Self {
        Self {
            x: self.x,
            y: self.y,
            w: self.w.max(32),
            h: self.h.max(32),
        }
    }
}

/// 画在蒙版上的东西的类别(决定颜色)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeKind {
    /// 普通键位
    Bind,
    /// 仅 FPS 键位
    Fps,
    /// 轮盘(圆心 + 半径)
    Wheel,
    /// 瞄准锚点
    Aim,
    /// 纯文字标签(不画圈)
    Label,
    /// **滑动轨迹**(用户 2026-10-13,V2-3):折线,不是圆。
    ///
    /// 单独一个类别而不是复用 `Bind`:遮板要按类别选颜色,轨迹用键位绿
    /// 会与"这里有个按键"混为一谈。
    Swipe,
}

/// 一条要画的**折线**(滑动轨迹)。
///
/// 与 [`Shape`](圆)分开成两个类型而不是给 `Shape` 加可选字段:圆的语义是
/// "圆心 + 半径 + 标签",折线的语义是"一串点 + 方向标 + 标签",共用一份结构
/// 只会让两边都带着用不上的字段(而且 `r` 对折线毫无意义)。
///
/// 坐标一律是**蒙版窗口客户区像素**(与 [`Shape`] 同一坐标系,调用方换算好)。
#[derive(Debug, Clone)]
pub struct Polyline {
    pub points: Vec<(i32, i32)>,
    /// 是否画方向标(箭头):录制/回放的折返轨迹靠它才读得出走向
    pub arrows: bool,
    pub label: String,
}

impl Polyline {
    pub fn new(points: Vec<(i32, i32)>, arrows: bool, label: impl Into<String>) -> Self {
        Self {
            points,
            arrows,
            label: label.into(),
        }
    }
}

/// 一个要画的圆(可带标签)。
#[derive(Debug, Clone)]
pub struct Shape {
    pub x: i32,
    pub y: i32,
    pub r: i32,
    pub kind: ShapeKind,
    pub label: String,
}

impl Shape {
    pub fn new(kind: ShapeKind, x: i32, y: i32, r: i32, label: impl Into<String>) -> Self {
        Self {
            x,
            y,
            r,
            kind,
            label: label.into(),
        }
    }
}

/// 一帧要提交给蒙版窗口的全部内容:几何 + 穿透 + 要画的东西。
#[derive(Debug, Clone, Default)]
pub struct Frame {
    pub rect: MaskRect,
    pub shapes: Vec<Shape>,
    /// **滑动轨迹折线**(用户 2026-10-13,V2-3)。与 `shapes` 并列而不是混在一起:
    /// 圆的绘制是 `Ellipse`,折线是 `Polyline`,两类几何各走各的分支,
    /// 混在一个 `Vec` 里只会让绘制处到处 `match`。
    pub tracks: Vec<Polyline>,
    /// true = 鼠标穿透(点得到底下的窗口);false = 吃掉鼠标事件(默认)
    pub passthrough: bool,
}

impl Frame {
    pub fn new(rect: MaskRect) -> Self {
        Self {
            rect,
            shapes: Vec::new(),
            tracks: Vec::new(),
            passthrough: false,
        }
    }
}

/// 蒙版句柄的共享状态别名(实现内部用;放在这里是为了让平台实现与测试都能引用)。
pub(crate) type SharedFrame = Arc<std::sync::Mutex<Frame>>;
