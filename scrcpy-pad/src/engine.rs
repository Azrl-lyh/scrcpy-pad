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

use crate::capture::CaptureKey;
use crate::control::ControlClient;
use crate::keymap::{Action, Profile, TempMode, Wheel, easing_apply, swipe_points};
use std::collections::HashSet;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 普通键位(点按/长按/滑动)可同时按下的数量上限。
/// 模拟器多点触控一般支持 5 指,超出上限的新按下会被忽略,已按下键不受影响。
const MAX_CONCURRENT_KEYS: usize = 5;

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

pub struct Shared {
    pub profile: Profile,
    pub enabled: bool,
    pub control: Option<ControlClient>,
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
    rx: Receiver<CaptureKey>,
    gui_tx: Sender<CaptureKey>,
) {
    let mut scheduled: Vec<(Instant, SchedAct)> = Vec::new();
    let mut fingers = Fingers::default();
    let mut active_android_keys: HashSet<u16> = HashSet::new();
    let mut wheels: Vec<WheelState> = Vec::new();
    let mut wheel_count = usize::MAX; // 触发重建

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

        let ev = match rx.recv_timeout(Duration::from_millis(4)) {
            Ok(ev) => ev,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };

        // 转发给 GUI(按键捕获绑定用)
        if ev.pressed {
            let _ = gui_tx.send(ev);
        }

        let (enabled, toggle_key) = {
            let g = shared.lock().unwrap();
            (g.enabled, g.profile.toggle_key)
        };

        // 总开关键:任何时候都生效
        if ev.code == toggle_key && ev.pressed {
            let mut g = shared.lock().unwrap();
            g.enabled = !g.enabled;
            let now_enabled = g.enabled;
            if !now_enabled {
                // 关闭时释放所有按下中的触点与系统键,并停用全部临时轮盘
                if let Some(c) = g.control.as_ref() {
                    for idx in 0..g.profile.binds.len() {
                        if fingers.release(idx) {
                            let (x, y) = bind_point(&g.profile, idx);
                            c.touch_up(bind_pid(idx), x, y);
                        }
                    }
                    for kc in active_android_keys.drain() {
                        c.key(false, kc as u32);
                    }
                    for (j, ws) in wheels.iter_mut().enumerate() {
                        if ws.down {
                            let w = &g.profile.wheels[j];
                            c.touch_move(wheel_pid(j), w.cx, w.cy);
                            c.touch_up(wheel_pid(j), w.cx, w.cy);
                            ws.down = false;
                        }
                    }
                } else {
                    active_android_keys.clear();
                }
                fingers.free_all();
                for ws in wheels.iter_mut() {
                    ws.down = false;
                    ws.pressed = [false; 4];
                    ws.active = false;
                }
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
                            profile,
                            &mut fingers,
                            &mut active_android_keys,
                            w,
                        );
                    } else {
                        deactivate_wheel(ctl, j, w, &mut wheels[j]);
                    }
                }
                TempMode::Toggle => {
                    if ev.pressed {
                        if wheels[j].active {
                            deactivate_wheel(ctl, j, w, &mut wheels[j]);
                        } else {
                            wheels[j].active = true;
                            release_conflicting_binds(
                                ctl,
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
                update_wheel(ctl, j, w, &mut wheels[j]);
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
                    if *duration_ms == 0 {
                        // 按住切换:按下不松手,直到再次按下同一键才抬起
                        if ev.pressed {
                            if fingers.is_down(idx) {
                                ctl.touch_up(pid, *x, *y);
                                fingers.release(idx);
                            } else if fingers.try_down(idx) {
                                ctl.touch_down(pid, *x, *y);
                            }
                        }
                    } else if ev.pressed {
                        // 点按:按下注入 DOWN,持续 duration_ms(默认 40ms)后定时抬起。
                        // 若上一击尚未自动抬起又再次按下(极快连点/组合技排序),
                        // 先立即结束旧触点再开新一轮,保证每次点按都完整触发、不丢键。
                        if fingers.is_down(idx) {
                            cancel_up_for(pid, &mut scheduled);
                            ctl.touch_up(pid, *x, *y);
                            fingers.release(idx);
                        }
                        if fingers.try_down(idx) {
                            ctl.touch_down(pid, *x, *y);
                            scheduled.push((
                                Instant::now()
                                    + Duration::from_millis((*duration_ms as u64).max(5)),
                                SchedAct::Up { pid, x: *x, y: *y },
                            ));
                        }
                    }
                    // 松开不在此处理,统一由定时抬起收尾
                }
                Action::Hold { x, y, .. } => {
                    if ev.pressed {
                        if fingers.try_down(idx) {
                            ctl.touch_down(pid, *x, *y);
                        }
                    } else if fingers.release(idx) {
                        ctl.touch_up(pid, *x, *y);
                    }
                }
                Action::Swipe(s) => {
                    // 按下触发滑动;中途松手不打断,滑到终点由定时抬起清理按下状态
                    if ev.pressed {
                        if fingers.try_down(idx) {
                            let points = swipe_points(s.path, s.start, s.end, 64);
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

fn bind_point(profile: &Profile, idx: usize) -> (i32, i32) {
    match profile.binds.get(idx).map(|b| &b.action) {
        Some(Action::Hold { x, y, .. }) | Some(Action::Tap { x, y, .. }) => (*x, *y),
        Some(Action::Swipe(s)) => s.start,
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
fn deactivate_wheel(ctl: &ControlClient, j: usize, w: &Wheel, st: &mut WheelState) {
    st.active = false;
    st.pressed = [false; 4];
    if st.down {
        let pid = wheel_pid(j);
        ctl.touch_move(pid, w.cx, w.cy);
        ctl.touch_up(pid, w.cx, w.cy);
        st.down = false;
    }
}

/// 临时轮盘启用的瞬间,释放所有与该轮盘方向键冲突的普通绑定
/// (仍按住的点按/长按/滑动触点与系统键),避免方向输入被抢占或触点卡死。
fn release_conflicting_binds(
    ctl: &ControlClient,
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
                    let (x, y) = bind_point(profile, i);
                    ctl.touch_up(bind_pid(i), x, y);
                }
            }
        }
    }
}

fn update_wheel(ctl: &ControlClient, j: usize, w: &Wheel, st: &mut WheelState) {
    let pid = wheel_pid(j);
    let dx = st.pressed[3] as i32 - st.pressed[2] as i32; // right - left
    let dy = st.pressed[1] as i32 - st.pressed[0] as i32; // down - up

    if dx == 0 && dy == 0 {
        if st.down {
            // 回中后抬起
            ctl.touch_move(pid, w.cx, w.cy);
            ctl.touch_up(pid, w.cx, w.cy);
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
    let tx = w.cx + (fx * w.radius as f64).round() as i32;
    let ty = w.cy + (fy * w.radius as f64).round() as i32;

    if !st.down {
        ctl.touch_down(pid, w.cx, w.cy);
        ctl.touch_move(pid, tx, ty);
        st.down = true;
    } else if st.last != (tx, ty) {
        ctl.touch_move(pid, tx, ty);
    }
    st.last = (tx, ty);
}
