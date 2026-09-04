//! 映射引擎:消费 evdev 按键事件,驱动控制通道。
//! 动作语义:
//!   Tap   - 按下 -> 触点落下,40ms 后抬起
//!   Hold  - 按下 -> 触点落下,松开 -> 抬起
//!   Swipe - 按下 -> 沿折线匀速滑动
//!   Wheel - 方向键组合 -> 虚拟摇杆(圆心按下 + 向方向移动 + 松开回中抬起)

use crate::capture::CaptureKey;
use crate::control::ControlClient;
use crate::keymap::{Action, Profile};
use std::collections::HashSet;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
}

pub fn run(
    shared: SharedState,
    rx: Receiver<CaptureKey>,
    gui_tx: Sender<CaptureKey>,
) {
    let mut scheduled: Vec<(Instant, SchedAct)> = Vec::new();
    let mut active_holds: HashSet<usize> = HashSet::new();
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
                            SchedAct::Up { pid, x, y } => c.touch_up(pid, x, y),
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
                // 关闭时释放所有触点
                if let Some(c) = g.control.as_ref() {
                    for idx in active_holds.drain() {
                        let (x, y) = bind_point(&g.profile, idx);
                        c.touch_up(bind_pid(idx), x, y);
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
                        ws.pressed = [false; 4];
                    }
                } else {
                    active_holds.clear();
                    active_android_keys.clear();
                    for ws in wheels.iter_mut() {
                        ws.down = false;
                        ws.pressed = [false; 4];
                    }
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

        // ---- 轮盘方向键 ----
        let mut handled = false;
        for (j, w) in profile.wheels.iter().enumerate() {
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

        // ---- 普通绑定 ----
        for (idx, bind) in profile.binds.iter().enumerate() {
            if bind.key != ev.code {
                continue;
            }
            let pid = bind_pid(idx);
            match &bind.action {
                Action::Tap { x, y } if ev.pressed => {
                    ctl.touch_down(pid, *x, *y);
                    scheduled.push((
                        Instant::now() + Duration::from_millis(40),
                        SchedAct::Up { pid, x: *x, y: *y },
                    ));
                }
                Action::Hold { x, y } => {
                    if ev.pressed {
                        ctl.touch_down(pid, *x, *y);
                        active_holds.insert(idx);
                    } else if active_holds.remove(&idx) {
                        ctl.touch_up(pid, *x, *y);
                    }
                }
                Action::Swipe {
                    points,
                    duration_ms,
                } if ev.pressed && points.len() >= 2 => {
                    let (x0, y0) = points[0];
                    ctl.touch_down(pid, x0, y0);
                    let n = points.len();
                    let start = Instant::now();
                    for (k, &(x, y)) in points.iter().enumerate().skip(1) {
                        let t = start
                            + Duration::from_millis(
                                (*duration_ms as u64) * k as u64 / (n as u64 - 1),
                            );
                        scheduled.push((t, SchedAct::Move { pid, x, y }));
                    }
                    let (xe, ye) = points[n - 1];
                    scheduled.push((
                        start + Duration::from_millis(*duration_ms as u64 + 20),
                        SchedAct::Up { pid, x: xe, y: ye },
                    ));
                }
                Action::AndroidKey { keycode } => {
                    if ev.pressed {
                        ctl.key(true, *keycode);
                        active_android_keys.insert(*keycode as u16);
                    } else if active_android_keys.remove(&(*keycode as u16)) {
                        ctl.key(false, *keycode);
                    }
                }
                _ => {}
            }
        }
    }
}

fn bind_pid(idx: usize) -> u64 {
    1000 + idx as u64
}

fn wheel_pid(idx: usize) -> u64 {
    2000 + idx as u64
}

fn bind_point(profile: &Profile, idx: usize) -> (i32, i32) {
    match profile.binds.get(idx).map(|b| &b.action) {
        Some(Action::Hold { x, y }) | Some(Action::Tap { x, y }) => (*x, *y),
        _ => (0, 0),
    }
}

fn update_wheel(ctl: &ControlClient, j: usize, w: &crate::keymap::Wheel, st: &mut WheelState) {
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
