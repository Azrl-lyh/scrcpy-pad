//! 映射引擎:消费 evdev 按键事件,驱动控制通道。
//! 动作语义:
//!   Tap   - 按下 -> 触点落下,40ms 后抬起
//!   Hold  - 按下 -> 触点落下,松开 -> 抬起
//!   Swipe - 按下 -> 沿折线匀速滑动
//!   Wheel - 方向键组合 -> 虚拟摇杆(圆心按下 + 向方向移动 + 松开回中抬起)
//!          永久轮盘始终生效;临时轮盘仅在启用期间生效(Hold=按住启用键,
//!          Toggle=按一下开/再按关),启用期间方向键归摇杆、同键位的其它绑定失效。
//! 并发:普通键位(点按/长按/滑动)各自独立跟踪按下/抬起(见 Fingers),最多
//!       MAX_CONCURRENT_KEYS 个键可同时按下,互不干扰,便于战斗中放组合技。

use crate::capture::CaptureEvent;
use crate::control::ControlClient;
use crate::keymap::{
    Action, Aim, Mapper, Profile, RecenterMode, TempMode, Wheel, easing_apply, swipe_points,
};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 普通键位(点按/长按/滑动)可同时按下的数量上限。
/// 设备端 PointersState.MAX_POINTERS = 10;瞄准指针与轮盘指针另计,
/// 因此这里留出余量,便于 FPS 场景同时按住开火/蹲/跳等多个键。
const MAX_CONCURRENT_KEYS: usize = 8;

/// 瞄准指针的固定 id(与普通绑定 1000+、轮盘 2000+ 区分开)
const AIM_PID: u64 = 3000;

/// Ctrl / Alt 的 evdev 键码,用于识别"Ctrl+Alt 交还鼠标"这一组合
const KEY_LEFTCTRL: u16 = 29;
const KEY_RIGHTCTRL: u16 = 97;
const KEY_LEFTALT: u16 = 56;
const KEY_RIGHTALT: u16 = 100;

fn is_ctrl(code: u16) -> bool {
    code == KEY_LEFTCTRL || code == KEY_RIGHTCTRL
}

fn is_alt(code: u16) -> bool {
    code == KEY_LEFTALT || code == KEY_RIGHTALT
}

/// Ctrl+Alt 组合的触发判定:两者都按住时,按下的那一方触发一次切换。
/// 这样一次组合只会切换一次,按住不放也不会连发。
fn ctrl_alt_chord(code: u16, pressed: bool, ctrl_down: bool, alt_down: bool) -> bool {
    pressed && ctrl_down && alt_down && (is_ctrl(code) || is_alt(code))
}

/// 引擎内部使用的按键事件:与 capture::CaptureEvent::Button 同构,
/// 保留 code/pressed 字段名以便下方既有逻辑直接复用。
#[derive(Clone, Copy)]
struct CaptureKey {
    code: u16,
    pressed: bool,
}

/// 普通键位并发跟踪器:每个绑定索引独立记录"是否已按下",
/// 保证任意键的按下/抬起严格成对、只作用于自己的 pid,互不干扰。
///  - 同键未抬起时忽略重复按下(全局钩子自动重复/抖动),避免向 scrcpy
///    反复注入同一 pid 的 DOWN,把 server 的多点指针池搅乱;
///  - 没有按下记录的抬起一律忽略,绝不误抬其它键;
///  - 同时按下的键数不超过 MAX_CONCURRENT_KEYS(超出忽略新按下)。
#[derive(Default)]
struct Fingers {
    /// 绑定索引 -> 是否已按下
    down: Vec<bool>,
    /// 当前按下中的键数
    count: usize,
}

impl Fingers {
    /// 绑定列表长度变化时补长(只增不减,删除绑定产生的残留由 free_all 兜底)
    fn align(&mut self, n: usize) {
        if self.down.len() < n {
            self.down.resize(n, false);
        }
    }

    fn is_down(&self, idx: usize) -> bool {
        self.down.get(idx).copied().unwrap_or(false)
    }

    /// 登记一次按下;已按下或并发已达上限时返回 false(调用方不注入 DOWN)
    fn try_down(&mut self, idx: usize) -> bool {
        if self.is_down(idx) || self.count >= MAX_CONCURRENT_KEYS {
            return false;
        }
        self.down[idx] = true;
        self.count += 1;
        true
    }

    /// 登记一次抬起;仅当确有按下记录时返回 true(调用方据此注入 UP)
    fn release(&mut self, idx: usize) -> bool {
        if self.is_down(idx) {
            self.down[idx] = false;
            self.count = self.count.saturating_sub(1);
            return true;
        }
        false
    }

    fn free_all(&mut self) {
        for v in &mut self.down {
            *v = false;
        }
        self.count = 0;
    }
}

// ============================ FPS 鼠标瞄准 ============================
//
// 手机 FPS 的视角靠"手指在屏幕上拖动"实现,而鼠标给的是相对位移,
// 因此这里维护一个独立的手指触点:
//   鼠标位移 -> 累积偏移 -> 落点 = 锚点 + 偏移 -> 注入 touch_move
// 偏移接近屏幕边界时无法继续转向,所以需要「抬指 + 在锚点重按」(=归中)。

/// 瞄准运行状态(供界面显示,用来确认"偏移到底有没有在累积、触点有没有落下")
#[derive(Default, Clone, Copy)]
pub struct AimLive {
    /// 收到的鼠标位移事件数
    pub motions: u64,
    /// 最近一次位移量
    pub last_dx: f32,
    pub last_dy: f32,
    /// 当前累积偏移
    pub ox: f32,
    pub oy: f32,
    /// 瞄准触点当前是否按在屏幕上
    pub down: bool,
    /// 当前是否满足瞄准生效条件
    pub active: bool,
    /// 已注入的瞄准触点消息数(down/move/up 合计)
    pub sent: u64,
}

/// 瞄准子系统状态
#[derive(Default)]
struct AimState {
    /// 手指当前是否按在屏幕上
    down: bool,
    /// 累积偏移(设备像素,相对锚点)
    ox: f32,
    oy: f32,
    /// 当前落点(抬指时需要给出正确坐标)
    cur: (i32, i32),
    /// 门控键是否按住(hold_key == 0 时忽略)
    gate_down: bool,
    /// 用户按 Ctrl+Alt 临时把鼠标交还给系统:此时不瞄准、也不捕获光标
    released: bool,
    /// 最近一次鼠标位移时间(静止归中的依据)
    last_motion: Option<Instant>,
    /// 当前拖动使用的锚点;锚点被改动时重置拖动
    anchor_used: (i32, i32),
    /// 已注入的触点消息数(仅诊断用)
    sent: u64,
}

/// 瞄准自身是否具备生效条件(是否映射开启由调用方判断)
fn aim_active(aim: &Aim, st: &AimState) -> bool {
    aim.enabled
        && aim.anchor_set()
        && !st.released
        && (aim.hold_key == 0 || st.gate_down)
}

/// 在当前位置抬起瞄准触点并清空偏移
fn aim_lift(ctl: &ControlClient, st: &mut AimState) {
    if st.down {
        let (x, y) = st.cur;
        ctl.touch_up(AIM_PID, x, y);
        st.sent += 1;
        st.down = false;
    }
    st.ox = 0.0;
    st.oy = 0.0;
    st.last_motion = None;
}

/// 没有控制通道时只清理本地状态
fn aim_release_local(st: &mut AimState) {
    st.down = false;
    st.ox = 0.0;
    st.oy = 0.0;
    st.last_motion = None;
}

/// 锚点换算成像素并钳进当前屏幕内。锚点可能是在别的屏幕方向下取的
/// (例如竖屏取的锚点,横屏时 y 会超出屏幕高度),而落在屏幕外的触摸
/// 会被系统整条丢弃,表现为"鼠标怎么动都没有反应",因此注入前必须钳制。
fn aim_anchor(m: &Mapper, aim: &Aim) -> (i32, i32) {
    let (x, y) = m.point(aim.anchor_x, aim.anchor_y);
    (
        x.clamp(0, (m.w as i32 - 1).max(0)),
        y.clamp(0, (m.h as i32 - 1).max(0)),
    )
}

/// 落点 = 锚点 + 累积偏移,并钳制在屏幕内;第三个返回值表示是否触到边界
fn aim_target(m: &Mapper, aim: &Aim, st: &AimState) -> (i32, i32, bool) {
    let (ax, ay) = aim_anchor(m, aim);
    let ax = ax as f32;
    let ay = ay as f32;
    let max_ox = (m.w - 1.0 - ax).max(0.0);
    let max_oy = (m.h - 1.0 - ay).max(0.0);
    let cx = st.ox.clamp(-ax.max(0.0), max_ox);
    let cy = st.oy.clamp(-ay.max(0.0), max_oy);
    let hit_edge = cx != st.ox || cy != st.oy;
    ((ax + cx).round() as i32, (ay + cy).round() as i32, hit_edge)
}

/// 归中:在当前位置抬指,回到锚点重新按下。
/// 这一轮拖动产生的转向已经生效,玩家侧只是看到视角继续转动。
fn aim_recenter(ctl: &ControlClient, m: &Mapper, aim: &Aim, st: &mut AimState) {
    if !st.down {
        st.ox = 0.0;
        st.oy = 0.0;
        return;
    }
    let (cx, cy) = st.cur;
    ctl.touch_up(AIM_PID, cx, cy);
    st.sent += 1;
    st.ox = 0.0;
    st.oy = 0.0;
    let (ax, ay) = aim_anchor(m, aim);
    ctl.touch_down(AIM_PID, ax, ay);
    st.sent += 1;
    st.cur = (ax, ay);
    // 刚归中,重置静止计时,避免立刻再次归中
    st.last_motion = Some(Instant::now());
}

/// 鼠标相对位移 -> 手机上的拖动
fn aim_on_motion(ctl: &ControlClient, m: &Mapper, aim: &Aim, st: &mut AimState, dx: f32, dy: f32) {
    if !aim_active(aim, st) {
        return;
    }
    let anchor = aim_anchor(m, aim);
    // 锚点被改动过(或屏幕方向变了):结束旧拖动,从新锚点重新开始
    if st.anchor_used != anchor {
        aim_lift(ctl, st);
        st.anchor_used = anchor;
    }

    st.ox += dx * aim.sensitivity_x;
    let sy = if aim.invert_y {
        -aim.sensitivity_y
    } else {
        aim.sensitivity_y
    };
    st.oy += dy * sy;
    st.last_motion = Some(Instant::now());

    let (tx, ty, hit_edge) = aim_target(m, aim, st);
    if st.down {
        ctl.touch_move(AIM_PID, tx, ty);
    } else {
        ctl.touch_down(AIM_PID, tx, ty);
        st.down = true;
    }
    st.sent += 1;
    st.cur = (tx, ty);

    // 阈值归中:偏移超过设定值;或已顶到边界(否则无法继续同向转向)
    let th = aim.recenter_threshold.max(1) as f32;
    let threshold_hit =
        aim.recenter == RecenterMode::Threshold && (st.ox.abs() >= th || st.oy.abs() >= th);
    let edge_hit = hit_edge && aim.recenter != RecenterMode::Never;
    if threshold_hit || edge_hit {
        aim_recenter(ctl, m, aim, st);
    }
}

/// 定期维护:映射关闭/门控松开 -> 收手;静止归中 -> 回锚点
fn aim_tick(ctl: &ControlClient, m: &Mapper, aim: &Aim, mapping_enabled: bool, st: &mut AimState) {
    if !mapping_enabled || !aim_active(aim, st) {
        aim_lift(ctl, st);
        return;
    }
    if !st.down || aim.recenter != RecenterMode::Idle {
        return;
    }
    let idle_ms = aim.recenter_idle_ms.max(16) as u128;
    let paused = st
        .last_motion
        .map(|t| t.elapsed().as_millis() >= idle_ms)
        .unwrap_or(false);
    if paused && (st.ox != 0.0 || st.oy != 0.0) {
        aim_recenter(ctl, m, aim, st);
    }
}

pub struct Shared {
    pub profile: Profile,
    pub enabled: bool,
    pub control: Option<ControlClient>,
    /// 瞄准运行状态(界面诊断显示)
    pub aim_live: AimLive,
}

pub type SharedState = Arc<Mutex<Shared>>;

#[derive(Debug, Clone)]
enum SchedAct {
    Up { pid: u64, x: i32, y: i32 },
    Move { pid: u64, x: i32, y: i32 },
}

#[derive(Default, Clone)]
struct WheelState {
    pressed: [bool; 4], // up down left right
    down: bool,
    last: (i32, i32),
    /// 临时轮盘当前是否处于启用状态(永久轮盘恒为 true)
    active: bool,
}

pub fn run(
    shared: SharedState,
    rx: Receiver<CaptureEvent>,
    gui_tx: Sender<CaptureEvent>,
    mouse_grab: Arc<AtomicBool>,
) {
    let mut scheduled: Vec<(Instant, SchedAct)> = Vec::new();
    let mut fingers = Fingers::default();
    let mut active_android_keys: HashSet<u16> = HashSet::new();
    let mut wheels: Vec<WheelState> = Vec::new();
    let mut wheel_count = usize::MAX; // 触发重建
    let mut aim = AimState::default();
    // Ctrl+Alt 组合:按下即把鼠标交还给系统,再按一次收回(需用全局钩子判定,
    // 因为开打时焦点通常在 scrcpy 窗口,主程序收不到按键)
    let mut ctrl_down = false;
    let mut alt_down = false;

    loop {
        // 处理到期的计划动作
        let now = Instant::now();
        let mut i = 0;
        while i < scheduled.len() {
            if scheduled[i].0 <= now {
                let (_, act) = scheduled.swap_remove(i);
                let ctl = { shared.lock().unwrap().control.as_ref().map(|_| ()) };
                if ctl.is_some() {
                    let guard = shared.lock().unwrap();
                    if let Some(c) = guard.control.as_ref() {
                        match act {
                            SchedAct::Up { pid, x, y } => {
                                c.touch_up(pid, x, y);
                                // 定时抬起(点按 40ms / 滑动终点):该键本次按下结束,清除按下状态
                                if pid >= 1000 {
                                    fingers.release((pid - 1000) as usize);
                                }
                            }
                            SchedAct::Move { pid, x, y } => c.touch_move(pid, x, y),
                        }
                    }
                }
            } else {
                i += 1;
            }
        }

        // 瞄准定期维护 + 同步鼠标捕获状态
        {
            let mut g = shared.lock().unwrap();
            let s: &mut Shared = &mut g;
            let cfg = s.profile.aim.clone();
            let enabled = s.enabled;
            let connected = s
                .control
                .as_ref()
                .map(|c| c.is_connected())
                .unwrap_or(false);
            // 仅当映射开启、控制通道在线、瞄准启用且未被 Ctrl+Alt 释放时才捕获光标,
            // 避免出现"光标被冻结但什么都做不了";
            // 绑了"按住才瞄准"时只在按住期间捕获,松手即把光标还给系统
            mouse_grab.store(
                enabled
                    && connected
                    && cfg.enabled
                    && cfg.capture_mouse
                    && cfg.anchor_set()
                    && (cfg.hold_key == 0 || aim.gate_down)
                    && !aim.released,
                Ordering::Relaxed,
            );
            match s.control.as_ref() {
                Some(ctl) if connected => {
                    let m = s.profile.mapper((ctl.screen_w, ctl.screen_h));
                    aim_tick(ctl, &m, &cfg, enabled, &mut aim);
                }
                _ => aim_release_local(&mut aim),
            }
            let l = &mut s.aim_live;
            l.ox = aim.ox;
            l.oy = aim.oy;
            l.down = aim.down;
            l.active = aim_active(&cfg, &aim);
            l.sent = aim.sent;
        }

        // 等待下一个事件。有未到期的计划动作时,精确等到最早那个到期为止,
        // 使点按的定时抬起/滑动的每一步都能落在预定时刻(误差由 ~4ms 降到 ~1ms);
        // 没有计划动作时仍按 4ms 轮询,保证瞄准维护与捕获状态及时刷新。
        let wait = scheduled
            .iter()
            .map(|(t, _)| t.saturating_duration_since(Instant::now()))
            .min()
            .map(|d| d.min(Duration::from_millis(4)))
            .unwrap_or(Duration::from_millis(4));
        let raw = match rx.recv_timeout(wait) {
            Ok(ev) => ev,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };

        // 鼠标相对位移:交给瞄准子系统,不参与按键绑定流程
        let ev = match raw {
            CaptureEvent::Button { code, pressed } => CaptureKey { code, pressed },
            CaptureEvent::Motion { dx, dy } => {
                let mut g = shared.lock().unwrap();
                let s: &mut Shared = &mut g;
                s.aim_live.motions += 1;
                s.aim_live.last_dx = dx;
                s.aim_live.last_dy = dy;
                if s.enabled {
                    let cfg = s.profile.aim.clone();
                    if let Some(ctl) = s.control.as_ref() {
                        if ctl.is_connected() {
                            let m = s.profile.mapper((ctl.screen_w, ctl.screen_h));
                            aim_on_motion(ctl, &m, &cfg, &mut aim, dx, dy);
                        }
                    }
                }
                let l = &mut s.aim_live;
                l.ox = aim.ox;
                l.oy = aim.oy;
                l.down = aim.down;
                l.sent = aim.sent;
                continue;
            }
        };

        // 转发给 GUI(按键捕获绑定用;鼠标位移不需要)
        if ev.pressed {
            let _ = gui_tx.send(CaptureEvent::Button {
                code: ev.code,
                pressed: true,
            });
        }

        let (enabled, toggle_key, aim_hold_key, aim_captured) = {
            let g = shared.lock().unwrap();
            let aim = &g.profile.aim;
            (
                g.enabled,
                g.profile.toggle_key,
                aim.hold_key,
                aim.enabled && aim.capture_mouse && aim.anchor_set(),
            )
        };

        // 瞄准门控键(如鼠标右键=开镜)的按下/松开
        if aim_hold_key != 0 && ev.code == aim_hold_key {
            aim.gate_down = ev.pressed;
        }

        // Ctrl+Alt:临时把鼠标交还给系统,再按一次收回。
        // 只在瞄准确实会捕获鼠标时才有意义,避免误触改状态。
        if is_ctrl(ev.code) {
            ctrl_down = ev.pressed;
        } else if is_alt(ev.code) {
            alt_down = ev.pressed;
        }
        if aim_captured && ctrl_alt_chord(ev.code, ev.pressed, ctrl_down, alt_down) {
            aim.released = !aim.released;
        }

        // 总开关键:任何时候都生效
        if ev.code == toggle_key && ev.pressed {
            let mut g = shared.lock().unwrap();
            g.enabled = !g.enabled;
            let now_enabled = g.enabled;
            if !now_enabled {
                // 关闭时释放所有按下中的触点与系统键,并停用全部临时轮盘
                if let Some(c) = g.control.as_ref() {
                    let m = g.profile.mapper((c.screen_w, c.screen_h));
                    // 瞄准触点一并抬起(否则松手后仍按在屏幕上)
                    aim_lift(c, &mut aim);
                    for idx in 0..g.profile.binds.len() {
                        if fingers.release(idx) {
                            let (x, y) = bind_point(&g.profile, &m, idx);
                            c.touch_up(bind_pid(idx), x, y);
                        }
                    }
                    for kc in active_android_keys.drain() {
                        c.key(false, kc as u32);
                    }
                    for (j, ws) in wheels.iter_mut().enumerate() {
                        if ws.down {
                            let w = &g.profile.wheels[j];
                            let (cx, cy) = m.point(w.cx, w.cy);
                            c.touch_move(wheel_pid(j), cx, cy);
                            c.touch_up(wheel_pid(j), cx, cy);
                            ws.down = false;
                        }
                    }
                } else {
                    active_android_keys.clear();
                    aim_release_local(&mut aim);
                }
                fingers.free_all();
                for ws in wheels.iter_mut() {
                    ws.down = false;
                    ws.pressed = [false; 4];
                    ws.active = false;
                }
            } else {
                // 每次重新开打都恢复捕获,避免上一局的"交还鼠标"状态带过来
                aim.released = false;
            }
            continue;
        }

        if !enabled {
            continue;
        }

        let g = shared.lock().unwrap();
        let Some(ctl) = g.control.as_ref() else {
            continue;
        };
        if !ctl.is_connected() {
            continue;
        }
        let profile = &g.profile;
        // 坐标换算器:配置里的相对坐标 -> 当前屏幕像素(唯一换算入口)
        let m = profile.mapper((ctl.screen_w, ctl.screen_h));

        // 轮盘状态数量对齐(配置可能被编辑)
        if wheel_count != profile.wheels.len() {
            wheels = vec![WheelState::default(); profile.wheels.len()];
            wheel_count = profile.wheels.len();
        }
        // 普通绑定并发跟踪对齐
        fingers.align(profile.binds.len());

        // ---- 临时轮盘启用键 ----
        let mut consumed = false;
        for (j, w) in profile.wheels.iter().enumerate() {
            let Some(t) = &w.temp else { continue };
            if ev.code != t.key {
                continue;
            }
            match t.mode {
                TempMode::Hold => {
                    if ev.pressed {
                        wheels[j].active = true;
                        // 启用瞬间,释放与方向键冲突的普通绑定,避免触点卡死
                        release_conflicting_binds(
                            ctl,
                            &m,
                            profile,
                            &mut fingers,
                            &mut active_android_keys,
                            w,
                        );
                    } else {
                        deactivate_wheel(ctl, &m, j, w, &mut wheels[j]);
                    }
                }
                TempMode::Toggle => {
                    if ev.pressed {
                        if wheels[j].active {
                            deactivate_wheel(ctl, &m, j, w, &mut wheels[j]);
                        } else {
                            wheels[j].active = true;
                            release_conflicting_binds(
                                ctl,
                                &m,
                                profile,
                                &mut fingers,
                                &mut active_android_keys,
                                w,
                            );
                        }
                    }
                }
            }
            consumed = true;
        }
        if consumed {
            continue;
        }

        // ---- 轮盘方向键(仅生效中的轮盘:永久 或 已启用的临时) ----
        let mut handled = false;
        for (j, w) in profile.wheels.iter().enumerate() {
            let engaged = w.temp.is_none() || wheels[j].active;
            if !engaged {
                continue;
            }
            let dir_idx = if ev.code == w.up {
                Some(0)
            } else if ev.code == w.down {
                Some(1)
            } else if ev.code == w.left {
                Some(2)
            } else if ev.code == w.right {
                Some(3)
            } else {
                None
            };
            if let Some(d) = dir_idx {
                wheels[j].pressed[d] = ev.pressed;
                update_wheel(ctl, &m, j, w, &mut wheels[j]);
                handled = true;
            }
        }
        if handled {
            continue;
        }

        // ---- 普通绑定(每键独立按/抬状态,最多 MAX_CONCURRENT_KEYS 并发) ----
        for (idx, bind) in profile.binds.iter().enumerate() {
            if bind.key != ev.code {
                continue;
            }
            let pid = bind_pid(idx);
            match &bind.action {
                Action::Tap {
                    x,
                    y,
                    duration_ms,
                    ..
                } => {
                    let (px, py) = m.point(*x, *y);
                    if *duration_ms == 0 {
                        // 按住切换:按下不松手,直到再次按下同一键才抬起
                        if ev.pressed {
                            if fingers.is_down(idx) {
                                ctl.touch_up(pid, px, py);
                                fingers.release(idx);
                            } else if fingers.try_down(idx) {
                                ctl.touch_down(pid, px, py);
                            }
                        }
                    } else if ev.pressed {
                        // 点按:按下注入 DOWN,持续 duration_ms(默认 40ms)后定时抬起。
                        // 若上一击尚未自动抬起又再次按下(极快连点/组合技排序),
                        // 先立即结束旧触点再开新一轮,保证每次点按都完整触发、不丢键。
                        if fingers.is_down(idx) {
                            cancel_up_for(pid, &mut scheduled);
                            ctl.touch_up(pid, px, py);
                            fingers.release(idx);
                        }
                        if fingers.try_down(idx) {
                            ctl.touch_down(pid, px, py);
                            scheduled.push((
                                Instant::now()
                                    + Duration::from_millis((*duration_ms as u64).max(5)),
                                SchedAct::Up { pid, x: px, y: py },
                            ));
                        }
                    }
                    // 松开不在此处理,统一由定时抬起收尾
                }
                Action::Hold { x, y, .. } => {
                    let (px, py) = m.point(*x, *y);
                    if ev.pressed {
                        if fingers.try_down(idx) {
                            ctl.touch_down(pid, px, py);
                        }
                    } else if fingers.release(idx) {
                        ctl.touch_up(pid, px, py);
                    }
                }
                Action::Swipe(s) => {
                    // 按下触发滑动;中途松手不打断,滑到终点由定时抬起清理按下状态
                    if ev.pressed {
                        if fingers.try_down(idx) {
                            // 相对坐标 -> 像素后再生成轨迹采样点
                            let start_px = m.point(s.start.0, s.start.1);
                            let end_px = m.point(s.end.0, s.end.1);
                            let points = swipe_points(s.path, start_px, end_px, 64);
                            if points.len() >= 2 {
                                let (x0, y0) = points[0];
                                ctl.touch_down(pid, x0, y0);
                                let n = points.len();
                                let start = Instant::now();
                                let steps = (n - 1) as u64;
                                for k in 1..n {
                                    let t = k as f32 / steps as f32;
                                    // 曲线把时间映射为沿路径的进度,再做弧长插值
                                    let prog = easing_apply(s.easing, t);
                                    let (x, y) = point_at_progress(&points, prog);
                                    let time = start
                                        + Duration::from_millis(
                                            (s.duration_ms as u64) * k as u64 / steps,
                                        );
                                    scheduled.push((time, SchedAct::Move { pid, x, y }));
                                }
                                let (xe, ye) = points[n - 1];
                                scheduled.push((
                                    start
                                        + Duration::from_millis(s.duration_ms as u64 + 20),
                                    SchedAct::Up { pid, x: xe, y: ye },
                                ));
                            }
                        }
                    }
                }
                Action::AndroidKey { keycode } => {
                    let kc = *keycode as u16;
                    if ev.pressed {
                        // 同键未抬起(钩子自动重复/抖动)时不重复按下
                        if active_android_keys.insert(kc) {
                            ctl.key(true, *keycode);
                        }
                    } else if active_android_keys.remove(&kc) {
                        ctl.key(false, *keycode);
                    }
                }
            }
        }
    }
}

fn bind_pid(idx: usize) -> u64 {
    1000 + idx as u64
}

/// 取消指定 pid 尚未执行的定时抬起(点按连发抢占旧点击用),
/// 避免旧排程把新一撃提前抬起。
fn cancel_up_for(pid: u64, scheduled: &mut Vec<(Instant, SchedAct)>) {
    let target = pid;
    scheduled.retain(|(_, act)| !matches!(act, SchedAct::Up { pid: p, .. } if *p == target));
}

fn wheel_pid(idx: usize) -> u64 {
    2000 + idx as u64
}

/// 绑定落点(相对坐标 -> 当前像素)
fn bind_point(profile: &Profile, m: &Mapper, idx: usize) -> (i32, i32) {
    match profile.binds.get(idx).map(|b| &b.action) {
        Some(Action::Hold { x, y, .. }) | Some(Action::Tap { x, y, .. }) => m.point(*x, *y),
        Some(Action::Swipe(s)) => m.point(s.start.0, s.start.1),
        _ => (0, 0),
    }
}

/// 在折线上按弧长进度(0..1)插值取点,使曲线缓动沿路径均匀分布。
fn point_at_progress(points: &[(i32, i32)], progress: f32) -> (i32, i32) {
    if points.is_empty() {
        return (0, 0);
    }
    if points.len() == 1 {
        return points[0];
    }
    let prog = progress.clamp(0.0, 1.0);
    let mut cum: Vec<f32> = Vec::with_capacity(points.len());
    cum.push(0.0);
    let mut total = 0.0f32;
    for w in points.windows(2) {
        let dx = (w[1].0 - w[0].0) as f32;
        let dy = (w[1].1 - w[0].1) as f32;
        total += (dx * dx + dy * dy).sqrt();
        cum.push(total);
    }
    if total <= 1e-6 {
        return points[0];
    }
    let target = prog * total;
    for i in 0..points.len() - 1 {
        if target <= cum[i + 1] {
            let seg = cum[i + 1] - cum[i];
            let frac = if seg <= 1e-6 {
                0.0
            } else {
                (target - cum[i]) / seg
            }
            .clamp(0.0, 1.0);
            let x = points[i].0 as f32 + (points[i + 1].0 - points[i].0) as f32 * frac;
            let y = points[i].1 as f32 + (points[i + 1].1 - points[i].1) as f32 * frac;
            return (x.round() as i32, y.round() as i32);
        }
    }
    points[points.len() - 1]
}

/// 停用一个轮盘:释放触点、清方向状态(临时轮盘专用,但通用无害)
fn deactivate_wheel(ctl: &ControlClient, m: &Mapper, j: usize, w: &Wheel, st: &mut WheelState) {
    st.active = false;
    st.pressed = [false; 4];
    if st.down {
        let pid = wheel_pid(j);
        let (cx, cy) = m.point(w.cx, w.cy);
        ctl.touch_move(pid, cx, cy);
        ctl.touch_up(pid, cx, cy);
        st.down = false;
    }
}

/// 临时轮盘启用的瞬间,释放所有与该轮盘方向键冲突的普通绑定
/// (仍按住的点按/长按/滑动触点与系统键),避免方向输入被抢占或触点卡死。
fn release_conflicting_binds(
    ctl: &ControlClient,
    m: &Mapper,
    profile: &Profile,
    fingers: &mut Fingers,
    active_android_keys: &mut HashSet<u16>,
    w: &Wheel,
) {
    let dirs = [w.up, w.down, w.left, w.right];
    for (i, bind) in profile.binds.iter().enumerate() {
        if !dirs.contains(&bind.key) {
            continue;
        }
        match &bind.action {
            Action::AndroidKey { keycode } => {
                let kc = *keycode as u16;
                if active_android_keys.remove(&kc) {
                    ctl.key(false, *keycode);
                }
            }
            _ => {
                if fingers.release(i) {
                    let (x, y) = bind_point(profile, m, i);
                    ctl.touch_up(bind_pid(i), x, y);
                }
            }
        }
    }
}

fn update_wheel(ctl: &ControlClient, m: &Mapper, j: usize, w: &Wheel, st: &mut WheelState) {
    let pid = wheel_pid(j);
    let (wx, wy) = m.point(w.cx, w.cy);
    let wr = m.len(w.radius);
    let dx = st.pressed[3] as i32 - st.pressed[2] as i32; // right - left
    let dy = st.pressed[1] as i32 - st.pressed[0] as i32; // down - up

    if dx == 0 && dy == 0 {
        if st.down {
            // 回中后抬起
            ctl.touch_move(pid, wx, wy);
            ctl.touch_up(pid, wx, wy);
            st.down = false;
        }
        return;
    }

    // 斜向归一化
    let (fx, fy) = if dx != 0 && dy != 0 {
        let inv = std::f64::consts::FRAC_1_SQRT_2;
        (dx as f64 * inv, dy as f64 * inv)
    } else {
        (dx as f64, dy as f64)
    };
    let tx = wx + (fx * wr as f64).round() as i32;
    let ty = wy + (fy * wr as f64).round() as i32;

    if !st.down {
        ctl.touch_down(pid, wx, wy);
        ctl.touch_move(pid, tx, ty);
        st.down = true;
    } else if st.last != (tx, ty) {
        ctl.touch_move(pid, tx, ty);
    }
    st.last = (tx, ty);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 相对坐标的锚点(0.1 => 1000x1000 屏幕上的 (100,100))
    fn aim_at(x: f32, y: f32) -> Aim {
        Aim {
            enabled: true,
            anchor_x: x,
            anchor_y: y,
            ..Aim::default()
        }
    }

    /// 测试用换算器:相对坐标 + 1000x1000 屏幕
    fn mapper() -> Mapper {
        Mapper::new(crate::keymap::CoordUnit::Rel, (1000, 1000))
    }

    /// 落点必须始终落在屏幕内,并正确报告"触到边界"
    #[test]
    fn aim_target_clamps_to_screen() {
        let m = mapper();
        let aim = aim_at(0.1, 0.1);

        let st = AimState::default();
        assert_eq!(aim_target(&m, &aim, &st), (100, 100, false));

        let st = AimState {
            ox: -50.0,
            oy: 30.0,
            ..Default::default()
        };
        assert_eq!(aim_target(&m, &aim, &st), (50, 130, false));

        // 正向超界:钳到右下角
        let st = AimState {
            ox: 99999.0,
            oy: 99999.0,
            ..Default::default()
        };
        assert_eq!(aim_target(&m, &aim, &st), (999, 999, true));

        // 负向超界:钳到左上角
        let st = AimState {
            ox: -99999.0,
            oy: -99999.0,
            ..Default::default()
        };
        assert_eq!(aim_target(&m, &aim, &st), (0, 0, true));
    }

    /// 锚点超出当前坐标空间时(例如竖屏取点后转成横屏),
    /// 注入的触摸会落到屏幕外被系统丢弃 —— 必须先钳进屏幕内
    #[test]
    fn aim_target_clamps_anchor_into_screen() {
        // 竖屏(wm size 1280x2772)下自动放置的锚点,横屏实际坐标空间是 2772x1280
        let m = Mapper::new(crate::keymap::CoordUnit::Rel, (2772, 1280));
        let aim = aim_at(960.0 / 2772.0, 1386.0 / 2772.0);
        let st = AimState {
            down: true,
            ox: 40.0,
            oy: 40.0,
            ..Default::default()
        };
        let (x, y, _) = aim_target(&m, &aim, &st);
        assert!((0..2772).contains(&x));
        assert!((0..1280).contains(&y));
    }

    /// 未设锚点不生效;设了门控键则必须先按住
    #[test]
    fn aim_active_needs_anchor_and_gate() {
        let mut st = AimState::default();

        let mut aim = aim_at(0.1, 0.1);
        assert!(aim_active(&aim, &st));

        // 锚点未设置
        aim.anchor_x = 0.0;
        aim.anchor_y = 0.0;
        assert!(!aim_active(&aim, &st));

        // 门控键:按住才生效
        let mut aim = aim_at(0.1, 0.1);
        aim.hold_key = 273; // BTN_RIGHT
        assert!(!aim_active(&aim, &st));
        st.gate_down = true;
        assert!(aim_active(&aim, &st));
    }

    /// Ctrl+Alt:一次组合只切换一次,松开后再按才会再次触发
    #[test]
    fn ctrl_alt_chord_fires_once_per_press() {
        // 只按 Ctrl:不触发
        assert!(!ctrl_alt_chord(KEY_LEFTCTRL, true, true, false));
        // 在按住 Ctrl 的基础上按下 Alt:触发
        assert!(ctrl_alt_chord(KEY_LEFTALT, true, true, true));
        // Alt 保持按住时的重复按下事件:仍然算一次触发(由调用方取反,不会连发)
        assert!(ctrl_alt_chord(KEY_LEFTALT, true, true, true));
        // 松开 Alt:不触发
        assert!(!ctrl_alt_chord(KEY_LEFTALT, false, true, false));

        // 同时按住 Ctrl+Alt 时按其它键:不触发
        assert!(!ctrl_alt_chord(30, true, true, true)); // KEY_A
        // 右键的 Ctrl / Alt 同样识别
        assert!(ctrl_alt_chord(KEY_RIGHTCTRL, true, true, true));
        assert!(ctrl_alt_chord(KEY_RIGHTALT, true, true, true));
    }

    /// 交还鼠标后瞄准不生效
    #[test]
    fn released_blocks_aim() {
        let aim = aim_at(0.1, 0.1);
        let mut st = AimState::default();
        assert!(aim_active(&aim, &st));
        st.released = true;
        assert!(!aim_active(&aim, &st));
    }
}
