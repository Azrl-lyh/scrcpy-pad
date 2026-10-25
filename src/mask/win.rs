//! 蒙版窗口的 Windows 实现(批次 0)。
//!
//! # 三层保证,每一层都有自检
//!
//! 1. **透明**:`WS_EX_LAYERED` + `SetLayeredWindowAttributes(..., LWA_COLORKEY)`。
//!    窗口背景整片填成 [`KEY_COLOR`](键色),系统把这些像素置为**完全透明**,
//!    只有画上去的圈和字留下 —— 底下的投屏画面原样透出来。
//! 2. **置顶**:`WS_EX_TOPMOST` + `SetWindowPos(HWND_TOPMOST, SWP_NOACTIVATE)`,
//!    且带 `WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE`:不占任务栏、不抢焦点。
//! 3. **吃掉鼠标**:窗口**不设** `WS_EX_TRANSPARENT`,并在 `WM_NCHITTEST` 里
//!    按 [`Frame::passthrough`] 回答 `HTCLIENT`(吃)或 `HTTRANSPARENT`(放行)。
//!    —— MSDN 的 `WS_EX_TRANSPARENT` 讲的是**绘制顺序**,真正决定"鼠标消息
//!    落到哪个窗口"的是 `WM_NCHITTEST` 的返回值,这里按后者做。
//!
//! [`selftest`] 把这三件事变成可执行的断言:它在自己身后放一个纯白背景窗,
//! 然后**采屏幕上的像素**看白底有没有透出来、圈有没有画上、`WindowFromPoint`
//! 到底指向谁 —— 不需要人眼确认。

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, ClientToScreen, CreatePen, CreateSolidBrush, DeleteObject, Ellipse, EndPaint,
    FillRect, GetDC, GetPixel, GetStockObject, HDC, HGDIOBJ, HOLLOW_BRUSH, InvalidateRect, LineTo,
    MoveToEx, PAINTSTRUCT, PS_SOLID, Polyline, ReleaseDC, SelectObject, SetBkMode, SetTextColor,
    TRANSPARENT, TextOutW, UpdateWindow,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, EnumWindows, GWL_EXSTYLE,
    GetClientRect, GetForegroundWindow, GetLayeredWindowAttributes, GetMessageW, GetWindowLongW,
    GetWindowRect, GetWindowThreadProcessId, HTCLIENT, HTTRANSPARENT, HWND_TOPMOST, IsIconic,
    IsWindow, IsWindowVisible, LWA_ALPHA, LWA_COLORKEY, MSG, PostMessageW, PostQuitMessage,
    RegisterClassW, SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SetLayeredWindowAttributes,
    SetWindowPos, ShowWindow, TranslateMessage, UnregisterClassW, WM_CLOSE, WM_DESTROY,
    WM_ERASEBKGND, WM_NCHITTEST, WM_PAINT, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WindowFromPoint,
};

use super::{Frame, MaskRect, Polyline, Shape, ShapeKind};

/// 键色:窗口背景填成这个颜色 ⇒ 被系统置为完全透明。
///
/// 取洋红是因为它在界面里几乎不可能自然出现;万一用户的主题真给出同色,
/// [`safe`] 会把它改掉一个通道位,保证"要画的东西不会被自己抠掉"。
const KEY_COLOR: u32 = rgb(255, 0, 255);

/// `COLORREF` 是 `0x00BBGGRR`(低字节是红)。
const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

/// 撞到键色的绘制颜色移一位(见 [`KEY_COLOR`])。
const fn safe(c: u32) -> u32 {
    if c == KEY_COLOR { c ^ 0x0000_0100 } else { c }
}

fn color_for(kind: ShapeKind) -> u32 {
    safe(match kind {
        ShapeKind::Bind => rgb(80, 220, 120),
        ShapeKind::Fps => rgb(255, 150, 60),
        ShapeKind::Wheel => rgb(90, 200, 255),
        ShapeKind::Aim => rgb(255, 90, 90),
        ShapeKind::Label => rgb(230, 230, 230),
        // 轨迹用亮青:与"键位绿/轮盘蓝"都能分开,且压在游戏画面上仍然醒目
        ShapeKind::Swipe => rgb(120, 230, 255),
    })
}

/// 蒙版刷新消息(自定义)。
const WM_MASK_REFRESH: u32 = 0x8000 + 41; // WM_APP + 41

/// 窗口类名(注册与注销同一份)。
fn class_name() -> Vec<u16> {
    "scrcpy_pad_mask\0".encode_utf16().collect()
}

/// 蒙版的全部共享状态。`wndproc` 是自由函数,靠 [`CTL`] 找回它。
struct Ctl {
    frame: Mutex<Frame>,
    hwnd: AtomicU64,
    stop: AtomicBool,
    /// 已经应用到窗口上的矩形(与请求比较,避免每帧 `SetWindowPos`)。
    applied_rect: Mutex<MaskRect>,
    applied_passthrough: AtomicBool,
}

/// 单实例:蒙版是"盖在投屏上的一块",同时开两块没有意义。
static CTL: Mutex<Option<Arc<Ctl>>> = Mutex::new(None);

fn ctl() -> Option<Arc<Ctl>> {
    CTL.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// 蒙版句柄。`Drop` 负责销毁窗口并等线程退出。
pub struct Mask {
    ctl: Arc<Ctl>,
    join: Option<JoinHandle<()>>,
}

impl Mask {
    /// 起一块蒙版。失败原因会带 `GetLastError`。
    pub fn start(rect: MaskRect) -> Result<Self, String> {
        {
            let guard = CTL.lock().unwrap_or_else(|e| e.into_inner());
            if guard.is_some() {
                return Err("蒙版已经在运行(单实例)".to_string());
            }
        }
        let rect = rect.normalized();
        let ctl = Arc::new(Ctl {
            frame: Mutex::new(Frame::new(rect)),
            hwnd: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            applied_rect: Mutex::new(rect),
            applied_passthrough: AtomicBool::new(false),
        });
        *CTL.lock().unwrap_or_else(|e| e.into_inner()) = Some(ctl.clone());

        let thread_ctl = ctl.clone();
        let join = thread::spawn(move || run_window(thread_ctl));

        // 等窗口真的建起来:调用方拿到 Err 比拿到一个"永远不显示的句柄"有用。
        let deadline = Instant::now() + Duration::from_millis(3000);
        while ctl.hwnd.load(Ordering::Relaxed) == 0 {
            if Instant::now() > deadline {
                *CTL.lock().unwrap_or_else(|e| e.into_inner()) = None;
                return Err("创建蒙版窗口超时".to_string());
            }
            thread::sleep(Duration::from_millis(5));
        }
        Ok(Self {
            ctl,
            join: Some(join),
        })
    }

    /// 提交一帧(几何 + 穿透 + 要画的东西),并立刻请求重绘。
    pub fn set_frame(&self, frame: Frame) {
        let rect = frame.rect.normalized();
        {
            let mut f = self.ctl.frame.lock().unwrap_or_else(|e| e.into_inner());
            *f = Frame { rect, ..frame };
        }
        let hwnd = self.ctl.hwnd.load(Ordering::Relaxed);
        if hwnd != 0 {
            unsafe { PostMessageW(hwnd as HWND, WM_MASK_REFRESH, 0, 0) };
        }
    }

    pub fn hwnd(&self) -> u64 {
        self.ctl.hwnd.load(Ordering::Relaxed)
    }

    pub fn alive(&self) -> bool {
        let hwnd = self.hwnd();
        hwnd != 0 && unsafe { IsWindow(hwnd as HWND) } != 0
    }
}

impl Drop for Mask {
    fn drop(&mut self) {
        self.ctl.stop.store(true, Ordering::Relaxed);
        let hwnd = self.ctl.hwnd.load(Ordering::Relaxed);
        if hwnd != 0 {
            unsafe { PostMessageW(hwnd as HWND, WM_CLOSE, 0, 0) };
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        *CTL.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// 窗口线程:注册类 → 建窗 → 消息泵 → 注销类。
fn run_window(ctl: Arc<Ctl>) {
    unsafe {
        let hinst = GetModuleHandleW(std::ptr::null());
        let class = class_name();
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
        };
        // 失败可能只是"类已注册"(同一进程里重建蒙版),继续建窗即可。
        let _ = RegisterClassW(&wc);

        let title: Vec<u16> = "scrcpy-pad 蒙版\0".encode_utf16().collect();
        let rect = *ctl.applied_rect.lock().unwrap_or_else(|e| e.into_inner());
        let hwnd = CreateWindowExW(
            // 分层 + 置顶 + 不进任务栏 + 不抢焦点。**不设** WS_EX_TRANSPARENT:
            // 那是"绘制顺序",不是点击穿透(见模块注释)。
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            rect.x,
            rect.y,
            rect.w,
            rect.h,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            return;
        }
        ctl.hwnd.store(hwnd as u64, Ordering::Relaxed);
        // 键色透明:背景整片是 KEY_COLOR 的地方被系统抠成完全透明。
        SetLayeredWindowAttributes(hwnd, KEY_COLOR, 0, LWA_COLORKEY);
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            rect.x,
            rect.y,
            rect.w,
            rect.h,
            SWP_NOACTIVATE,
        );
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);

        let mut msg: MSG = std::mem::zeroed();
        loop {
            let r = GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0);
            if r <= 0 {
                break;
            }
            if ctl.stop.load(Ordering::Relaxed) {
                break;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = UnregisterClassW(class.as_ptr(), hinst);
    }
}

/// 从共享帧里取出当前几何,把变化应用到窗口上。
unsafe fn apply_frame(hwnd: HWND, ctl: &Ctl) {
    let (rect, passthrough) = {
        let f = ctl.frame.lock().unwrap_or_else(|e| e.into_inner());
        (f.rect.normalized(), f.passthrough)
    };
    let changed = {
        let mut applied = ctl.applied_rect.lock().unwrap_or_else(|e| e.into_inner());
        if *applied != rect {
            *applied = rect;
            true
        } else {
            false
        }
    };
    if changed {
        unsafe {
            SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                rect.x,
                rect.y,
                rect.w,
                rect.h,
                SWP_NOACTIVATE,
            )
        };
    }
    if ctl.applied_passthrough.swap(passthrough, Ordering::Relaxed) != passthrough {
        // 穿透由 WM_NCHITTEST 决定,这里只记状态(见 `nchittest`)。
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let Some(ctl) = ctl() else {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    };
    match msg {
        WM_MASK_REFRESH => {
            unsafe { apply_frame(hwnd, &ctl) };
            unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            0
        }
        WM_ERASEBKGND => 1, // 整片自己画,别让系统先刷一遍(闪烁源)
        WM_NCHITTEST => {
            // 这个返回值就是"鼠标消息归谁":HTCLIENT = 归蒙版(底下的投屏窗口
            // 收不到 ⇒ 双份输入消失),HTTRANSPARENT = 交给下层窗口(用户要自己点
            // 投屏窗口时用)。
            let pass = ctl.applied_passthrough.load(Ordering::Relaxed);
            if pass {
                HTTRANSPARENT as LRESULT
            } else {
                HTCLIENT as LRESULT
            }
        }
        WM_PAINT => {
            unsafe { paint(hwnd, &ctl) };
            0
        }
        WM_CLOSE => {
            unsafe { DestroyWindow(hwnd) };
            0
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// 一次重绘:先用键色铺满(⇒ 透明),再画圈、轨迹与标签。
unsafe fn paint(hwnd: HWND, ctl: &Ctl) {
    let (shapes, tracks): (Vec<Shape>, Vec<Polyline>) = {
        let f = ctl.frame.lock().unwrap_or_else(|e| e.into_inner());
        (f.shapes.clone(), f.tracks.clone())
    };
    unsafe {
        let mut ps: PAINTSTRUCT = std::mem::zeroed();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut rc: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut rc);
        // ① 背景 = 键色(被抠成透明)
        let key_brush = CreateSolidBrush(KEY_COLOR);
        FillRect(hdc, &rc, key_brush);
        DeleteObject(key_brush as HGDIOBJ);
        // ② 圈:空心笔刷 + 彩色描边
        let hollow = GetStockObject(HOLLOW_BRUSH);
        for s in &shapes {
            if s.kind == ShapeKind::Label {
                continue;
            }
            let color = color_for(s.kind);
            let pen = CreatePen(PS_SOLID, 2, color);
            let old_pen = SelectObject(hdc, pen as HGDIOBJ);
            let old_brush = SelectObject(hdc, hollow);
            Ellipse(hdc, s.x - s.r, s.y - s.r, s.x + s.r, s.y + s.r);
            SelectObject(hdc, old_pen);
            SelectObject(hdc, old_brush);
            DeleteObject(pen as HGDIOBJ);
        }
        // ②' 滑动轨迹:折线 + 端点圆点 + 方向标(用户 2026-10-13,V2-3)。
        //
        // 用 `Polyline` 一笔画完整条折线(而不是逐段 `MoveToEx`+`LineTo`):
        // 折返/绕圈的轨迹点很密,逐段画会成倍地切换画笔与坐标转换。
        // 方向标画成"两根短线组成的箭头",与截图浮层上的观感保持一致
        // (蒙版是 GDI、浮层是 egui,只能靠同样的几何规则对齐,不能共用代码)。
        for t in &tracks {
            if t.points.len() < 2 {
                continue;
            }
            let color = color_for(ShapeKind::Swipe);
            // GDI 的 POINT 与我们的 (i32,i32) 内存布局一致(两个 i32),
            // 但**不依赖**这一点:显式转一份,免得将来换类型时静默画错。
            let pts: Vec<POINT> = t.points.iter().map(|&(x, y)| POINT { x, y }).collect();
            let pen = CreatePen(PS_SOLID, 2, color);
            let old_pen = SelectObject(hdc, pen as HGDIOBJ);
            let old_brush = SelectObject(hdc, hollow);
            // 一笔画完整条折线(而不是逐段 MoveToEx+LineTo):折返/绕圈的轨迹点
            // 很密,一次调用省下成倍的画笔切换与坐标处理。
            Polyline(hdc, pts.as_ptr(), pts.len() as i32);
            // 端点:起点空心圈、终点实心点(与浮层同一套读法)
            let first = pts[0];
            let last = pts[pts.len() - 1];
            Ellipse(hdc, first.x - 6, first.y - 6, first.x + 6, first.y + 6);
            SelectObject(hdc, old_brush);
            SelectObject(hdc, old_pen);
            DeleteObject(pen as HGDIOBJ);
            let dot = CreateSolidBrush(color);
            let old_brush2 = SelectObject(hdc, dot);
            Ellipse(hdc, last.x - 4, last.y - 4, last.x + 4, last.y + 4);
            SelectObject(hdc, old_brush2);
            DeleteObject(dot as HGDIOBJ);
            // 方向标:沿弧长等距取 3 个,画成两根短线组成的箭头
            if t.arrows {
                let marks = crate::keymap::track_arrow_marks(&t.points, 3);
                if !marks.is_empty() {
                    let pen2 = CreatePen(PS_SOLID, 2, color);
                    let old_pen2 = SelectObject(hdc, pen2 as HGDIOBJ);
                    for ((x, y), (dx, dy)) in marks {
                        draw_gdi_arrow(hdc, x, y, dx, dy);
                    }
                    SelectObject(hdc, old_pen2);
                    DeleteObject(pen2 as HGDIOBJ);
                }
            }
        }
        // ③ 标签:深色实心底 + 白字。底板是不透明的 —— 半透明会跟键色混出
        //    边缘杂色(键色抠图对"混合过的像素"无能为力)。
        SetBkMode(hdc, TRANSPARENT as i32);
        for s in &shapes {
            if s.label.is_empty() {
                continue;
            }
            let text: Vec<u16> = s.label.encode_utf16().collect();
            let w = (text.len() as i32) * 8 + 8;
            let plate = RECT {
                left: s.x - w / 2,
                top: s.y + s.r + 2,
                right: s.x + w / 2,
                bottom: s.y + s.r + 20,
            };
            let plate_brush = CreateSolidBrush(rgb(16, 18, 22));
            FillRect(hdc, &plate, plate_brush);
            DeleteObject(plate_brush as HGDIOBJ);
            SetTextColor(hdc, color_for(s.kind));
            TextOutW(
                hdc,
                plate.left + 4,
                plate.top + 3,
                text.as_ptr(),
                text.len() as i32,
            );
        }
        // ③' 轨迹标签:画在**起点**上方(与截图浮层一致:标签跟着起点走)
        for t in &tracks {
            if t.label.is_empty() || t.points.len() < 2 {
                continue;
            }
            let (x, y) = t.points[0];
            let text: Vec<u16> = t.label.encode_utf16().collect();
            let w = (text.len() as i32) * 8 + 8;
            let plate = RECT {
                left: x - w / 2,
                top: y - 26,
                right: x + w / 2,
                bottom: y - 8,
            };
            let plate_brush = CreateSolidBrush(rgb(16, 18, 22));
            FillRect(hdc, &plate, plate_brush);
            DeleteObject(plate_brush as HGDIOBJ);
            SetTextColor(hdc, color_for(ShapeKind::Swipe));
            TextOutW(
                hdc,
                plate.left + 4,
                plate.top + 3,
                text.as_ptr(),
                text.len() as i32,
            );
        }
        EndPaint(hwnd, &ps);
    }
}

/// 在 `(x, y)` 处按单位方向 `(dx, dy)` 画一个箭头(两根短线)。
///
/// 角度固定 150°、长度 10px:蒙版上的轨迹点很密,箭头太大就会糊成一片,
/// 太小又读不出方向。两支箭头共用一个顶点 = 标准"V 形"箭头。
unsafe fn draw_gdi_arrow(hdc: HDC, x: i32, y: i32, dx: f32, dy: f32) {
    const LEN: f32 = 10.0;
    // 箭头两支相对主方向张开 ±150°
    for sign in [1.0f32, -1.0] {
        let a = 150.0f32.to_radians() * sign;
        let (s, c) = a.sin_cos();
        // 旋转 (dx, dy) 得到支线方向(注意:支线是"向后"张开的)
        let bx = -(dx * c - dy * s);
        let by = -(dx * s + dy * c);
        unsafe {
            MoveToEx(hdc, x, y, std::ptr::null_mut());
            LineTo(hdc, x + (bx * LEN) as i32, y + (by * LEN) as i32);
        }
    }
}

/// 本平台是否可用(界面据此决定显不显示入口)。
pub fn available() -> bool {
    true
}

/// 找出属于 `pid` 的**主窗口**在屏幕上的矩形(物理像素)。
///
/// 用途:遮板要贴在投屏窗口上,就必须知道它在哪。判据:
/// 该 pid 的**可见、非最小化、非工具窗**的顶层窗口里,**面积最大**的那个
/// (投屏窗口就是它);一个都没有 ⇒ `None`(调用方据此**退回隔膜**)。
///
/// 为什么用"面积最大"而不是"第一个":一个进程可能同时有隐藏窗、消息窗、
/// 输入法窗等若干顶层窗,枚举顺序不保证;投屏窗口是其中最大的可见窗。
///
/// **最小化为什么不贴**(用户 2026-10-13 选定):最小化时 `GetWindowRect` 返回
/// `(-21333,-21333)` 这类**哨兵坐标**(真机实测,投屏窗口 class=`SDL_app`),
/// 照它贴遮板会把窗口放到屏幕外或乱位置。所以最小化 = 不贴 = 退回隔膜;
/// 用户恢复窗口后,下一轮重新定位(节流 500ms)会自动贴上。
///
/// 这是**只读**查询,不改动任何窗口。
pub fn window_rect_of_pid(pid: u32) -> Option<MaskRect> {
    if pid == 0 {
        return None;
    }
    unsafe {
        let mut best: Option<MaskRect> = None;
        let mut best_area: i64 = 0;
        // 回调是 C ABI 自由函数,状态只能靠 lparam 传;这里传**栈上局部量的裸指针**,
        // 回调在 `EnumWindows` 返回前同步执行完,所以这个指针的生命周期是安全的。
        let mut ctx = FindCtx {
            pid,
            best: &mut best as *mut Option<MaskRect>,
            best_area: &mut best_area as *mut i64,
        };
        EnumWindows(Some(enum_collect_pid), &mut ctx as *mut FindCtx as LPARAM);
        best
    }
}

/// `EnumWindows` 回调的上下文(回调是 C ABI 自由函数,状态只能这样传)。
struct FindCtx {
    pid: u32,
    /// 指向 `window_rect_of_pid` 栈上的 `best`;回调同步执行,生命周期安全。
    best: *mut Option<MaskRect>,
    /// 同上,指向栈上的 `best_area`。
    best_area: *mut i64,
}

unsafe extern "system" fn enum_collect_pid(hwnd: HWND, lparam: LPARAM) -> i32 {
    let ctx = unsafe { &mut *(lparam as *mut FindCtx) };
    unsafe {
        if IsWindowVisible(hwnd) == 0 || IsIconic(hwnd) != 0 {
            return 1; // 继续枚举
        }
        // 排除工具窗(输入法/托盘之类)
        let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TOOLWINDOW != 0 {
            return 1;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid != ctx.pid {
            return 1;
        }
        let mut r = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if GetWindowRect(hwnd, &mut r) == 0 {
            return 1;
        }
        let w = r.right - r.left;
        let h = r.bottom - r.top;
        if w < 32 || h < 32 {
            return 1; // 太小:不是投屏窗口
        }
        let area = i64::from(w) * i64::from(h);
        if area > *ctx.best_area {
            *ctx.best_area = area;
            *ctx.best = Some(MaskRect::new(r.left, r.top, w, h));
        }
        1 // 继续枚举完(要比较面积)
    }
}

/// 找出属于 `pid` 的**主窗口客户区**在屏幕上的矩形(物理像素,不含标题栏/边框)。
///
/// 用途(用户 2026-10-13:"圆的位置仍然不太正确"的根因):
/// [`window_rect_of_pid`] 返回的是**整个窗口**(含标题栏与边框),而画面画在
/// **客户区**里。拿整窗矩形去算画面位置,就会整体偏移一个标题栏的高度。
///
/// 实现:`GetWindowRect` 得到整窗屏幕矩形,`GetClientRect` 得到客户区尺寸,
/// 再用 `ClientToScreen` 把客户区原点(0,0)换算到屏幕坐标。
///
/// 找不到窗口 ⇒ `None`(调用方退回用整窗矩形)。
pub fn client_area_of_pid(pid: u32) -> Option<(i32, i32, i32, i32)> {
    if pid == 0 {
        return None;
    }
    unsafe {
        let mut best: Option<(HWND, i64)> = None;
        let mut ctx = FindClientCtx {
            pid,
            best: &mut best as *mut Option<(HWND, i64)>,
        };
        EnumWindows(
            Some(enum_collect_client),
            &mut ctx as *mut FindClientCtx as LPARAM,
        );
        let (hwnd, _) = best?;
        let mut cr = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if GetClientRect(hwnd, &mut cr) == 0 {
            return None;
        }
        let mut origin = POINT { x: 0, y: 0 };
        if ClientToScreen(hwnd, &mut origin) == 0 {
            return None;
        }
        let w = cr.right - cr.left;
        let h = cr.bottom - cr.top;
        if w < 32 || h < 32 {
            return None;
        }
        Some((origin.x, origin.y, w, h))
    }
}

/// `client_area_of_pid` 的枚举上下文(与 [`FindCtx`] 同一套"面积最大"判据)
struct FindClientCtx {
    pid: u32,
    best: *mut Option<(HWND, i64)>,
}

unsafe extern "system" fn enum_collect_client(hwnd: HWND, lparam: LPARAM) -> i32 {
    let ctx = unsafe { &mut *(lparam as *mut FindClientCtx) };
    unsafe {
        if IsWindowVisible(hwnd) == 0 || IsIconic(hwnd) != 0 {
            return 1;
        }
        let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TOOLWINDOW != 0 {
            return 1;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid != ctx.pid {
            return 1;
        }
        let mut cr = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if GetClientRect(hwnd, &mut cr) == 0 {
            return 1;
        }
        let (w, h) = (cr.right - cr.left, cr.bottom - cr.top);
        if w < 32 || h < 32 {
            return 1;
        }
        let area = i64::from(w) * i64::from(h);
        let better = match *ctx.best {
            Some((_, a)) => area > a,
            None => true,
        };
        if better {
            *ctx.best = Some((hwnd, area));
        }
        1
    }
}

/// 一行行"这套实现到底靠什么工作"的说明(进诊断日志 / 界面提示)。
pub fn description() -> Vec<String> {
    vec![
        format!(
            "蒙版窗口:WS_EX_LAYERED|TOPMOST|TOOLWINDOW|NOACTIVATE,键色透明 COLORREF={KEY_COLOR:#010X}"
        ),
        "点击归属:WM_NCHITTEST → HTCLIENT(吃掉鼠标)/ HTTRANSPARENT(放行)".to_string(),
    ]
}

// ============================ 无界面自检 ============================

/// 纯白背景窗(只为自检存在):蒙版盖在它上面,采屏幕像素看白底有没有透出来。
struct BgWindow(HWND);

impl BgWindow {
    fn create(rect: MaskRect) -> Result<Self, String> {
        unsafe {
            let hinst = GetModuleHandleW(std::ptr::null());
            let class: Vec<u16> = "scrcpy_pad_mask_selftest_bg\0".encode_utf16().collect();
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(bg_wnd_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinst,
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: class.as_ptr(),
            };
            let _ = RegisterClassW(&wc);
            let title: Vec<u16> = "scrcpy-pad 蒙版自检底图\0".encode_utf16().collect();
            let hwnd = CreateWindowExW(
                // 底图也要置顶:否则它会被桌面上任何别的窗口(自检运行时正好在前台的
                // 那个)盖住,"透出白底"这条断言的采样点就落在别人身上了。
                WS_EX_TOPMOST,
                class.as_ptr(),
                title.as_ptr(),
                WS_POPUP,
                rect.x,
                rect.y,
                rect.w,
                rect.h,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                hinst,
                std::ptr::null(),
            );
            if hwnd.is_null() {
                return Err(format!(
                    "建底图窗口失败(GetLastError={})",
                    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
                ));
            }
            SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                rect.x,
                rect.y,
                rect.w,
                rect.h,
                SWP_NOACTIVATE,
            );
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            // **必须显式 UpdateWindow**:这个窗口建在主线程上,而主线程从不泵消息,
            // 队列里的 WM_PAINT 永远不会被派发 —— 第一版就是这样:底图是"没画过的
            // 客户区",合成采样读到的是桌面色,把"透明成功"误报成"透明失败"。
            // UpdateWindow 直接同步送一条 WM_PAINT 过去,绕开消息队列。
            UpdateWindow(hwnd);
            Ok(Self(hwnd))
        }
    }
}

impl Drop for BgWindow {
    fn drop(&mut self) {
        unsafe { DestroyWindow(self.0) };
    }
}

unsafe extern "system" fn bg_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            unsafe {
                let mut ps: PAINTSTRUCT = std::mem::zeroed();
                let hdc = BeginPaint(hwnd, &mut ps);
                let mut rc: RECT = std::mem::zeroed();
                GetClientRect(hwnd, &mut rc);
                let brush = CreateSolidBrush(rgb(255, 255, 255));
                FillRect(hdc, &rc, brush);
                DeleteObject(brush as HGDIOBJ);
                EndPaint(hwnd, &ps);
            }
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// 在 (x,y) 周围 `r` 像素内找第一个不是键色的像素(避开"正好差一像素"的脆弱采样)。
unsafe fn sample_shape_pixel(
    hdc: windows_sys::Win32::Graphics::Gdi::HDC,
    x: i32,
    y: i32,
    r: i32,
) -> Option<u32> {
    unsafe {
        for dy in -r..=r {
            for dx in -r..=r {
                let c = GetPixel(hdc, x + dx, y + dy);
                if c != u32::MAX && c != KEY_COLOR {
                    return Some(c);
                }
            }
        }
        None
    }
}

/// 蒙版自检:透明 / 绘制 / 置顶 / 点击归属 / 不留残窗。返回进程退出码。
///
/// 为什么值得写:这三条都是"只能靠肉眼看着像是对"的性质,而它们一旦回归,
/// 表现是"画面被一块灰布盖住"或"点了没反应" —— 有了它,回归在自检里就能
/// 报出来,不必等真机。
pub fn selftest() -> i32 {
    let mut failed = false;
    let mut check = |name: &str, ok: bool, detail: String| {
        println!("[{}] {name} {detail}", if ok { "PASS" } else { "FAIL" });
        if !ok {
            failed = true;
        }
    };

    let rect = MaskRect::new(160, 160, 460, 320);
    let bg = match BgWindow::create(rect) {
        Ok(b) => b,
        Err(e) => {
            check("自检底图窗口", false, e);
            return 1;
        }
    };
    let mask = match Mask::start(rect) {
        Ok(m) => m,
        Err(e) => {
            check("创建蒙版窗口", false, e);
            return 1;
        }
    };
    let hwnd = mask.hwnd() as HWND;
    let (cx, cy) = rect.center();
    let mut frame = Frame::new(rect);
    frame.shapes.push(Shape::new(
        ShapeKind::Bind,
        cx - rect.x,
        cy - rect.y,
        60,
        "自检",
    ));
    // 轨迹折线(用户 2026-10-13,V2-3):一条横贯窗口的折线 + 一个折返。
    // 单独摆一条**远离圆心**的折线,这样"折线上的像素"与"圈上的像素"
    // 互不干扰,两条断言才能各自独立成立。
    // 坐标都是窗口客户区坐标(rect 左上为原点)。
    let track_y = 40;
    frame.tracks.push(Polyline::new(
        vec![
            (40, track_y),
            (rect.w - 40, track_y),
            (rect.w - 40, track_y + 30),
        ],
        true,
        "自检轨迹",
    ));
    mask.set_frame(frame);
    // 等重绘落定(PostMessage → 泵 → WM_PAINT)
    thread::sleep(Duration::from_millis(300));

    // ① 窗口扩展样式:分层/置顶/不进任务栏/不抢焦点
    let ex = unsafe {
        GetWindowLongW(
            hwnd,
            windows_sys::Win32::UI::WindowsAndMessaging::GWL_EXSTYLE,
        )
    } as u32;
    let want = WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
    check(
        "扩展样式(分层/置顶/工具栏/不激活)",
        ex & want == want,
        format!("ex=0x{ex:08X} 需要=0x{want:08X}"),
    );

    // ② 绘制:圆圈描边位置的像素必须是那个颜色;角落必须还是键色(⇒ 透明)
    unsafe {
        let hdc = GetDC(hwnd);
        let left = cx - rect.x - 60;
        let y = cy - rect.y;
        let drawn = sample_shape_pixel(hdc, left, y, 3);
        let corner = GetPixel(hdc, 3, 3);
        // ②' 轨迹折线真的画上了(用户 2026-10-13,V2-3)。
        //
        // 分两处采样,合起来才说明"是一条线"而不是"一个偶然的亮点":
        //   (a) 水平段中段必须有轨迹色;
        //   (b) 折返段(竖着那 30px)也必须有 —— 只画了第一段就断掉的实现
        //       会在这里露馅。
        // 折线在 y=40,圆心 60px 的圈在窗口正中,两处采样互不干扰。
        //
        // **采样必须在 `ReleaseDC` 之前**:DC 一旦释放,`GetPixel` 只返回
        // `CLR_INVALID`,于是"画上了"与"没画上"看起来一模一样 ——
        // 本自检的第一版就踩过这个坑(拿已释放的 DC 采样,得出过
        // "折线画不出来"的错误结论,还差点据此改掉正确实现)。
        let track_color = color_for(ShapeKind::Swipe);
        let on_hline = sample_shape_pixel(hdc, rect.w / 2, track_y, 3);
        let on_vline = sample_shape_pixel(hdc, rect.w - 40, track_y + 15, 3);
        ReleaseDC(hwnd, hdc);
        let hit_color = color_for(ShapeKind::Bind);
        check(
            "GDI 绘制(圈画上了)",
            drawn.is_some_and(|c| c & 0x00FF_FFFF == hit_color),
            format!("圈上像素={drawn:?} 期望={hit_color:#010X}"),
        );
        check(
            "背景是键色(⇒ 会被抠成透明)",
            corner & 0x00FF_FFFF == KEY_COLOR,
            format!("角落像素={corner:#010X} 键色={KEY_COLOR:#010X}"),
        );
        check(
            "GDI 绘制(轨迹折线画上了)",
            on_hline.is_some_and(|c| c & 0x00FF_FFFF == track_color),
            format!("折线中段像素={on_hline:?} 期望={track_color:#010X}"),
        );
        check(
            "GDI 绘制(折返段也画上了)",
            on_vline.is_some_and(|c| c & 0x00FF_FFFF == track_color),
            format!("折返段像素={on_vline:?} 期望={track_color:#010X}"),
        );
    }

    // ③ 合成透明 —— 用**差分**判定,不对桌面环境做任何假设:
    //    (i) 藏起蒙版采一次基准 (ii) 显示蒙版(键色透明)再采 ⇒ 必须逐像素相同
    //    (iii) 反证:把蒙版改成明显半透明 ⇒ 像素必须变,证明它确实盖在这个点上,
    //          (ii) 的"相同"不是因为蒙版跑到别处去了。
    unsafe {
        let screen = GetDC(std::ptr::null_mut());
        ShowWindow(hwnd, SW_HIDE);
        thread::sleep(Duration::from_millis(120));
        let base = GetPixel(screen, cx, cy);
        check(
            "底图可见(透明判定的基准)",
            base & 0x00FF_FFFF == rgb(255, 255, 255),
            format!("基准像素={base:#010X} 期望白"),
        );
        SetLayeredWindowAttributes(hwnd, KEY_COLOR, 0, LWA_COLORKEY);
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        thread::sleep(Duration::from_millis(120));
        let on = GetPixel(screen, cx, cy);
        check(
            "合成透明(与藏起蒙版时逐像素相同)",
            on == base,
            format!("显示后={on:#010X} 基准={base:#010X}"),
        );
        SetLayeredWindowAttributes(hwnd, 0, 160, LWA_ALPHA);
        thread::sleep(Duration::from_millis(120));
        let tinted = GetPixel(screen, cx, cy);
        check(
            "反证:改成半透明后像素确实被改",
            tinted != base,
            format!("半透明后={tinted:#010X} 基准={base:#010X}"),
        );
        // 复原成生产形态
        SetLayeredWindowAttributes(hwnd, KEY_COLOR, 0, LWA_COLORKEY);
        thread::sleep(Duration::from_millis(80));
        ReleaseDC(std::ptr::null_mut(), screen);
    }

    // ④ 分层属性确实挂上了。与"像素看起来对"互为独立证据:即使采样坐标系有偏移,
    //    这一条也照样说明"键色透明"这套机制是武装着的。
    unsafe {
        let mut key: u32 = 0;
        let mut alpha: u8 = 0;
        let mut flags: u32 = 0;
        let ok = GetLayeredWindowAttributes(hwnd, &mut key, &mut alpha, &mut flags);
        check(
            "分层属性已武装(LWA_COLORKEY + 键色)",
            ok != 0 && key == KEY_COLOR && flags & LWA_COLORKEY != 0,
            format!("ok={ok} key={key:#010X} alpha={alpha} flags=0x{flags:X}"),
        );
    }

    // ⑤ 命中归属 —— 本轮探雷**最重要的结论**,单独立一组:
    //    分层窗口的"透明像素"同时也是"点不到的像素",于是"看得见画面"与
    //    "吃掉鼠标"在同一块窗口上**不可兼得**。两种属性各测一遍,让结论有据可查。
    unsafe {
        let pt = POINT { x: cx, y: cy };
        let on_circle = POINT { x: cx - 60, y: cy };
        // (a) 键色透明:整片透明 ⇒ 整片点不到(鼠标落到下层窗口)
        SetLayeredWindowAttributes(hwnd, KEY_COLOR, 0, LWA_COLORKEY);
        thread::sleep(Duration::from_millis(80));
        let at_center = WindowFromPoint(pt);
        let at_circle = WindowFromPoint(on_circle);
        check(
            "键色透明:未绘制区点不到(落到下层)",
            at_center != hwnd,
            format!(
                "中心 WindowFromPoint={at_center:p} 蒙版={hwnd:p} 底图={:p}",
                bg.0
            ),
        );
        check(
            "键色透明:绘制区仍归蒙版",
            at_circle == hwnd,
            format!("圈上 WindowFromPoint={at_circle:p} 蒙版={hwnd:p}"),
        );
        // (b) 整体 alpha=1:肉眼几乎看不出,但像素不是"全透明",看能不能吃到点击
        SetLayeredWindowAttributes(hwnd, 0, 1, LWA_ALPHA);
        thread::sleep(Duration::from_millis(80));
        let at_alpha = WindowFromPoint(pt);
        check(
            "整体 alpha=1:整片归蒙版(可当独立隔膜窗)",
            at_alpha == hwnd,
            format!("中心 WindowFromPoint={at_alpha:p} 蒙版={hwnd:p}"),
        );
        // 复原成生产形态(键色透明)
        SetLayeredWindowAttributes(hwnd, KEY_COLOR, 0, LWA_COLORKEY);
        thread::sleep(Duration::from_millis(80));
        // ⑥ 不抢焦点
        let fg = GetForegroundWindow();
        check("不抢焦点", fg != hwnd, format!("前台={fg:p}"));
    }

    // ⑦ 关掉之后不留残窗
    let hwnd_raw = mask.hwnd();
    drop(mask);
    thread::sleep(Duration::from_millis(100));
    let gone = unsafe { IsWindow(hwnd_raw as HWND) } == 0;
    check("关闭后无残留窗口", gone, format!("IsWindow={}", !gone));

    println!(
        "{}",
        if failed {
            "蒙版自检:存在失败项"
        } else {
            "蒙版自检:全部通过"
        }
    );
    if failed { 1 } else { 0 }
}
