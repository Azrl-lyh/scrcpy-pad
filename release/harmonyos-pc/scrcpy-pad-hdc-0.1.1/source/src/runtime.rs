use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use crate::backend::InputBackend;
use crate::keymap::{Action, Aim, Mapper, Profile, TempMode, WheelMode, swipe_points};

const MAX_CONCURRENT_KEYS: usize = 8;
const DEVICE_MAX_POINTERS: usize = 10;
const AIM_PID: u64 = 1000;

#[derive(Default)]
struct AimState {
    down: bool,
    ox: f32,
    oy: f32,
    current: (i32, i32),
    anchor_used: (i32, i32),
    last_motion: Option<Instant>,
}

fn aim_anchor(mapper: &Mapper, aim: &Aim) -> (i32, i32) {
    let (x, y) = mapper.point(aim.anchor_x, aim.anchor_y);
    (
        x.clamp(0, (mapper.w as i32 - 1).max(0)),
        y.clamp(0, (mapper.h as i32 - 1).max(0)),
    )
}

fn aim_target(mapper: &Mapper, aim: &Aim, state: &AimState) -> (i32, i32, bool) {
    let (anchor_x, anchor_y) = aim_anchor(mapper, aim);
    let ax = anchor_x as f32;
    let ay = anchor_y as f32;
    let max_ox = (mapper.w - 1.0 - ax).max(0.0);
    let max_oy = (mapper.h - 1.0 - ay).max(0.0);
    let offset_x = state.ox.clamp(-ax.max(0.0), max_ox);
    let offset_y = state.oy.clamp(-ay.max(0.0), max_oy);
    let hit_edge = offset_x != state.ox || offset_y != state.oy;
    (
        (ax + offset_x).round() as i32,
        (ay + offset_y).round() as i32,
        hit_edge,
    )
}

#[derive(Default)]
struct FingerPool {
    down: Vec<bool>,
    count: usize,
}

impl FingerPool {
    fn align(&mut self, size: usize) {
        if self.down.len() < size {
            self.down.resize(size, false);
        }
    }

    fn is_down(&self, index: usize) -> bool {
        self.down.get(index).copied().unwrap_or(false)
    }

    fn try_down(&mut self, index: usize, others: usize) -> bool {
        if self.is_down(index)
            || self.count >= MAX_CONCURRENT_KEYS
            || self.count + others >= DEVICE_MAX_POINTERS
        {
            return false;
        }
        self.align(index + 1);
        self.down[index] = true;
        self.count += 1;
        true
    }

    fn release(&mut self, index: usize) -> bool {
        if !self.is_down(index) {
            return false;
        }
        self.down[index] = false;
        self.count = self.count.saturating_sub(1);
        true
    }

    fn free_all(&mut self) {
        self.down.fill(false);
        self.count = 0;
    }
}

#[derive(Default)]
struct WheelState {
    pressed: [bool; 4],
    press_seq: [u64; 4],
    next_seq: u64,
    active: bool,
    down: bool,
    last_x: i32,
    last_y: i32,
}

impl WheelState {
    fn new() -> Self {
        Self {
            active: true,
            ..Self::default()
        }
    }
}

pub fn wheel_axis_value(
    mode: WheelMode,
    neg_down: bool,
    pos_down: bool,
    neg_seq: u64,
    pos_seq: u64,
) -> i32 {
    match (neg_down, pos_down) {
        (false, false) => 0,
        (true, false) => -1,
        (false, true) => 1,
        (true, true) => match mode {
            WheelMode::Classic => 0,
            WheelMode::Sensitive => {
                if pos_seq > neg_seq {
                    1
                } else {
                    -1
                }
            }
        },
    }
}

pub struct MappingRuntime {
    backend: Arc<dyn InputBackend>,
    profile: Profile,
    held: HashSet<u16>,
    normal_fingers: FingerPool,
    fps_fingers: FingerPool,
    wheels: Vec<WheelState>,
    tap_toggle_down: Vec<bool>,
    aim: AimState,
    enabled: bool,
    fps_active: bool,
    fps_suspended: bool,
    viewport: (u32, u32),
}

impl MappingRuntime {
    pub fn new(backend: Arc<dyn InputBackend>, profile: Profile, viewport: (u32, u32)) -> Self {
        let mut runtime = Self {
            backend,
            profile,
            held: HashSet::new(),
            normal_fingers: FingerPool::default(),
            fps_fingers: FingerPool::default(),
            wheels: Vec::new(),
            tap_toggle_down: Vec::new(),
            aim: AimState::default(),
            enabled: false,
            fps_active: false,
            fps_suspended: false,
            viewport,
        };
        runtime.align_state();
        runtime
    }

    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    pub fn profile_mut(&mut self) -> &mut Profile {
        &mut self.profile
    }

    pub fn set_viewport(&mut self, width: u32, height: u32) {
        self.viewport = (width.max(1), height.max(1));
        self.reconcile_all();
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        if self.enabled == enabled {
            return;
        }
        self.enabled = enabled;
        if enabled {
            self.reconcile_all();
        } else {
            self.release_normal_binds();
            self.release_wheels();
        }
    }

    pub fn is_fps_active(&self) -> bool {
        self.fps_active && self.fps_running()
    }

    /// True when the system pointer should be captured/hidden. Suspending FPS
    /// or turning off pointer hiding immediately returns the cursor to the
    /// desktop while keeping the FPS mode armed.
    pub fn pointer_should_be_hidden(&self) -> bool {
        self.fps_running() && self.profile.aim.capture_mouse
    }

    pub fn handle_motion(&mut self, dx: f32, dy: f32) -> bool {
        if !self.aim_active() {
            self.aim_lift();
            return false;
        }

        let aim = self.profile.aim.clone();
        if !self.aim.down {
            let reserved =
                self.wheel_pointers() + self.normal_fingers.count + self.fps_fingers.count;
            if reserved >= DEVICE_MAX_POINTERS {
                return true;
            }
        }
        let mapper = Mapper::new(self.profile.coord_unit(), self.viewport);
        let anchor = aim_anchor(&mapper, &aim);
        if self.aim.anchor_used != anchor {
            self.aim_lift();
            self.aim.anchor_used = anchor;
        }

        self.aim.ox += dx * aim.sensitivity_x;
        let sy = if aim.invert_y {
            -aim.sensitivity_y
        } else {
            aim.sensitivity_y
        };
        self.aim.oy += dy * sy;
        self.aim.last_motion = Some(Instant::now());

        let (x, y, hit_edge) = aim_target(&mapper, &aim, &self.aim);
        let result = if self.aim.down {
            let (from_x, from_y) = self.aim.current;
            self.backend.touch_move(AIM_PID, from_x, from_y, x, y, 0)
        } else {
            self.backend.touch_down(AIM_PID, x, y)
        };
        let _ = result;
        self.aim.down = true;
        self.aim.current = (x, y);

        let threshold = aim.recenter_threshold.max(1) as f32;
        let threshold_hit = aim.recenter == crate::keymap::RecenterMode::Threshold
            && (self.aim.ox.abs() >= threshold || self.aim.oy.abs() >= threshold);
        let edge_hit = hit_edge && aim.recenter != crate::keymap::RecenterMode::Never;
        if threshold_hit || edge_hit {
            self.aim_recenter();
        }
        true
    }

    pub fn tick_aim(&mut self) {
        if !self.aim_active() {
            self.aim_lift();
            return;
        }
        if !self.aim.down || self.profile.aim.recenter != crate::keymap::RecenterMode::Idle {
            return;
        }
        let idle_ms = self.profile.aim.recenter_idle_ms.max(16) as u128;
        let paused = self
            .aim
            .last_motion
            .map(|time| time.elapsed().as_millis() >= idle_ms)
            .unwrap_or(false);
        if paused && (self.aim.ox != 0.0 || self.aim.oy != 0.0) {
            self.aim_recenter();
        }
    }

    fn aim_active(&self) -> bool {
        self.fps_running()
            && self.profile.aim.anchor_set()
            && (self.profile.aim.hold_key == 0 || self.held.contains(&self.profile.aim.hold_key))
    }

    fn aim_lift(&mut self) {
        if self.aim.down {
            let (x, y) = self.aim.current;
            let _ = self.backend.touch_up(AIM_PID, x, y);
        }
        self.aim.down = false;
        self.aim.ox = 0.0;
        self.aim.oy = 0.0;
        self.aim.last_motion = None;
    }

    fn aim_recenter(&mut self) {
        if !self.aim.down {
            self.aim.ox = 0.0;
            self.aim.oy = 0.0;
            return;
        }
        let (x, y) = self.aim.current;
        let _ = self.backend.touch_up(AIM_PID, x, y);
        self.aim.ox = 0.0;
        self.aim.oy = 0.0;
        let mapper = Mapper::new(self.profile.coord_unit(), self.viewport);
        let anchor = aim_anchor(&mapper, &self.profile.aim);
        let _ = self.backend.touch_down(AIM_PID, anchor.0, anchor.1);
        self.aim.current = anchor;
        self.aim.last_motion = Some(Instant::now());
    }

    pub fn set_fps_enabled(&mut self, enabled: bool) {
        self.profile.aim.enabled = enabled;
        if !enabled {
            self.fps_active = false;
            self.fps_suspended = false;
            self.aim_lift();
            self.release_fps_binds();
        } else {
            self.reconcile_all();
        }
    }

    pub fn set_fps_toggle_key(&mut self, key: u16) {
        self.profile.aim.toggle_key = key;
    }

    pub fn set_fps_suspend_key(&mut self, key: u16) {
        self.profile.aim.suspend_key = key;
    }

    pub fn set_all_wheel_modes(&mut self, mode: WheelMode) {
        for wheel in &mut self.profile.wheels {
            wheel.mode = mode;
        }
        self.release_wheels();
        self.reconcile_all();
    }

    pub fn notify_profile_changed(&mut self) {
        self.aim_lift();
        self.release_normal_binds();
        self.release_fps_binds();
        self.release_wheels();
        self.normal_fingers = FingerPool::default();
        self.fps_fingers = FingerPool::default();
        self.tap_toggle_down.clear();
        self.align_state();
        self.reconcile_all();
    }

    pub fn handle_key_event(&mut self, code: u16, down: bool, pressed_codes: &[u16]) -> bool {
        let was_down = self.held.contains(&code);
        let fresh_press = down && !was_down;
        self.reconcile_held(code, down, pressed_codes);

        let mut consumed = false;
        if code == self.profile.toggle_key {
            consumed = true;
            if fresh_press {
                self.set_enabled(!self.enabled);
            }
        }

        let fps_toggle = self.profile.aim.toggle_key;
        if self.profile.aim.enabled && fps_toggle != 0 && code == fps_toggle {
            consumed = true;
            if fresh_press {
                self.fps_active = !self.fps_active;
                if self.fps_active {
                    self.fps_suspended = false;
                }
                self.reconcile_all();
            }
        }

        let suspend_key = self.profile.aim.suspend_key;
        if self.profile.aim.enabled && suspend_key != 0 && code == suspend_key {
            consumed = true;
        }
        if self.profile.aim.enabled && suspend_key != 0 {
            let want_suspend = self.held.contains(&suspend_key);
            if want_suspend != self.fps_suspended {
                self.fps_suspended = want_suspend;
                self.reconcile_all();
            }
        }

        if self.handle_temporary_wheel_toggle(code, fresh_press) {
            consumed = true;
        }
        self.reconcile_wheel_directions();
        if self.is_wheel_direction_key(code) || self.is_key_owned_by_wheel(code) {
            consumed = true;
        }

        self.reconcile_normal_binds();
        if self.fps_running() {
            self.reconcile_fps_binds();
        } else {
            self.release_fps_binds();
        }

        if fresh_press && self.fire_one_shot(code) {
            consumed = true;
        }

        self.reconcile_normal_binds();
        if self.fps_running() {
            self.reconcile_fps_binds();
        } else {
            self.release_fps_binds();
        }
        self.tick_aim();
        consumed
    }

    pub fn cancel_all(&mut self) {
        self.aim_lift();
        self.normal_fingers.free_all();
        self.fps_fingers.free_all();
        self.held.clear();
        self.release_wheels();
        self.reconcile_all();
    }

    fn reconcile_held(&mut self, code: u16, down: bool, pressed_codes: &[u16]) {
        if !pressed_codes.is_empty() {
            let current: HashSet<u16> = pressed_codes.iter().copied().collect();
            self.held.retain(|held| current.contains(held));
        }
        if down {
            self.held.insert(code);
        } else {
            self.held.remove(&code);
        }
    }

    fn fps_running(&self) -> bool {
        self.profile.aim.enabled && self.fps_active && !self.fps_suspended
    }

    fn align_state(&mut self) {
        self.normal_fingers.align(self.profile.binds.len());
        self.fps_fingers.align(self.profile.binds.len());
        self.tap_toggle_down.resize(self.profile.binds.len(), false);
        while self.wheels.len() < self.profile.wheels.len() {
            self.wheels.push(WheelState::new());
        }
    }

    fn handle_temporary_wheel_toggle(&mut self, code: u16, fresh_press: bool) -> bool {
        let mut consumed = false;
        for (index, wheel) in self.profile.wheels.iter().enumerate() {
            let Some(temp) = &wheel.temp else {
                continue;
            };
            if temp.key != code {
                continue;
            }
            consumed = true;
            if fresh_press && temp.mode == TempMode::Toggle {
                self.wheels[index].active = !self.wheels[index].active;
            }
        }
        consumed
    }

    fn reconcile_temporary_wheels(&mut self) {
        for (index, wheel) in self.profile.wheels.iter().enumerate() {
            match &wheel.temp {
                None => self.wheels[index].active = true,
                Some(temp) if temp.mode == TempMode::Hold => {
                    self.wheels[index].active = self.held.contains(&temp.key);
                }
                Some(_) => {}
            }
        }
    }

    fn reconcile_all(&mut self) {
        self.align_state();
        self.reconcile_temporary_wheels();
        self.reconcile_wheel_directions();
        self.reconcile_normal_binds();
        if self.fps_running() {
            self.reconcile_fps_binds();
        } else {
            self.release_fps_binds();
        }
        if !self.aim_active() {
            self.aim_lift();
        }
    }

    fn reconcile_wheel_directions(&mut self) {
        let wheels = self.profile.wheels.clone();
        for (index, wheel) in wheels.iter().enumerate() {
            let state = &mut self.wheels[index];
            let engaged = wheel.temp.is_none() || state.active;
            let keys = [wheel.up, wheel.down, wheel.left, wheel.right];
            for (direction, key) in keys.iter().enumerate() {
                let want = engaged && self.held.contains(key);
                if want && !state.pressed[direction] {
                    state.next_seq = state.next_seq.wrapping_add(1);
                    state.press_seq[direction] = state.next_seq;
                }
                state.pressed[direction] = want;
            }
            self.update_wheel(index);
        }
    }

    fn update_wheel(&mut self, index: usize) {
        let wheel = &self.profile.wheels[index];
        let state = &mut self.wheels[index];
        let dx = wheel_axis_value(
            wheel.mode,
            state.pressed[2],
            state.pressed[3],
            state.press_seq[2],
            state.press_seq[3],
        );
        let dy = wheel_axis_value(
            wheel.mode,
            state.pressed[0],
            state.pressed[1],
            state.press_seq[0],
            state.press_seq[1],
        );
        let mapper = Mapper::new(self.profile.coord_unit(), self.viewport);
        let (center_x, center_y) = mapper.point(wheel.cx, wheel.cy);
        let push = wheel.push_px(&mapper);
        if dx == 0 && dy == 0 {
            if state.down {
                let _ = self.backend.touch_up(0, state.last_x, state.last_y);
                state.down = false;
            }
            return;
        }

        let (mut fx, mut fy) = (dx as f32, dy as f32);
        if dx != 0 && dy != 0 {
            let inv = 1.0 / 2.0_f32.sqrt();
            fx *= inv;
            fy *= inv;
        }
        let target_x = center_x + (fx * push).round() as i32;
        let target_y = center_y + (fy * push).round() as i32;
        if !state.down {
            let _ = self.backend.touch_down(0, center_x, center_y);
            let _ = self
                .backend
                .touch_move(0, center_x, center_y, target_x, target_y, 0);
            state.down = true;
        } else if (state.last_x, state.last_y) != (target_x, target_y) {
            let _ = self
                .backend
                .touch_move(0, state.last_x, state.last_y, target_x, target_y, 0);
        }
        state.last_x = target_x;
        state.last_y = target_y;
    }

    fn is_wheel_direction_key(&self, code: u16) -> bool {
        self.profile
            .wheels
            .iter()
            .enumerate()
            .any(|(index, wheel)| {
                let engaged = wheel.temp.is_none() || self.wheels[index].active;
                engaged
                    && (wheel.up == code
                        || wheel.down == code
                        || wheel.left == code
                        || wheel.right == code)
            })
    }

    fn is_key_owned_by_wheel(&self, code: u16) -> bool {
        self.profile
            .wheels
            .iter()
            .enumerate()
            .any(|(index, wheel)| {
                if wheel.temp.as_ref().is_some_and(|temp| temp.key == code) {
                    return true;
                }
                let engaged = wheel.temp.is_none() || self.wheels[index].active;
                engaged
                    && (wheel.up == code
                        || wheel.down == code
                        || wheel.left == code
                        || wheel.right == code)
            })
    }

    fn fire_one_shot(&mut self, code: u16) -> bool {
        let mut consumed = false;
        let fps_running = self.fps_running();
        for (index, bind) in self.profile.binds.iter().enumerate() {
            let lane_active = if bind.fps_only {
                fps_running
            } else {
                self.enabled
            };
            if !lane_active || bind.key != code || self.is_key_owned_by_wheel(code) {
                continue;
            }
            match &bind.action {
                Action::Tap {
                    x, y, duration_ms, ..
                } => {
                    consumed = true;
                    let mapper = Mapper::new(self.profile.coord_unit(), self.viewport);
                    let (px, py) = mapper.point(*x, *y);
                    if *duration_ms == 0 {
                        self.tap_toggle_down[index] = !self.tap_toggle_down[index];
                        let result = if self.tap_toggle_down[index] {
                            self.backend.touch_down(index as u64, px, py).map(|_| ())
                        } else {
                            self.backend.touch_up(index as u64, px, py).map(|_| ())
                        };
                        let _ = result;
                    } else {
                        let _ = self.backend.tap(px, py, *duration_ms as i32);
                    }
                }
                Action::Swipe(swipe) => {
                    consumed = true;
                    let mapper = Mapper::new(self.profile.coord_unit(), self.viewport);
                    let start = mapper.point(swipe.start.0, swipe.start.1);
                    let end = mapper.point(swipe.end.0, swipe.end.1);
                    let points = swipe_points(swipe.path, start, end, 32);
                    if let Some(first) = points.first() {
                        let _ = self.backend.touch_down(index as u64, first.0, first.1);
                        let mut previous = *first;
                        let mut elapsed = 0_u64;
                        for point in points.iter().skip(1) {
                            let step = if points.len() > 1 {
                                swipe.duration_ms as u64 / (points.len() - 1) as u64
                            } else {
                                0
                            };
                            elapsed = elapsed.saturating_add(step);
                            let _ = self.backend.touch_move(
                                index as u64,
                                previous.0,
                                previous.1,
                                point.0,
                                point.1,
                                step as i32,
                            );
                            previous = *point;
                        }
                        let _ = elapsed;
                        let _ = self.backend.touch_up(index as u64, previous.0, previous.1);
                    }
                }
                _ => {}
            }
        }
        consumed
    }

    fn reconcile_normal_binds(&mut self) {
        self.reconcile_binds(false);
    }

    fn reconcile_fps_binds(&mut self) {
        self.reconcile_binds(true);
    }

    fn reconcile_binds(&mut self, fps_lane: bool) {
        let lane_active = if fps_lane {
            self.fps_running()
        } else {
            self.enabled
        };
        let wheel_pointers = self.wheel_pointers();
        let fps_count = self.fps_fingers.count;
        let normal_count = self.normal_fingers.count;
        let aim_pointers = usize::from(self.aim.down);
        for index in 0..self.profile.binds.len() {
            let bind = &self.profile.binds[index];
            if bind.fps_only != fps_lane {
                continue;
            }
            if !matches!(bind.action, Action::Hold { .. } | Action::AndroidKey { .. }) {
                continue;
            }
            let want = lane_active
                && self.held.contains(&bind.key)
                && !self.is_key_owned_by_wheel(bind.key);
            let others =
                wheel_pointers + aim_pointers + if fps_lane { normal_count } else { fps_count };
            let pool = if fps_lane {
                &mut self.fps_fingers
            } else {
                &mut self.normal_fingers
            };
            if want {
                if !pool.is_down(index) {
                    if pool.try_down(index, others) {
                        let action = self.profile.binds[index].action.clone();
                        let mapper = Mapper::new(self.profile.coord_unit(), self.viewport);
                        if let Action::Hold { x, y, .. } = action {
                            let (px, py) = mapper.point(x, y);
                            let _ = self.backend.touch_down(index as u64, px, py);
                        } else if let Action::AndroidKey { keycode } = action {
                            let _ = self.backend.key(true, keycode as i32);
                        }
                    }
                }
            } else if pool.release(index) {
                let action = self.profile.binds[index].action.clone();
                let mapper = Mapper::new(self.profile.coord_unit(), self.viewport);
                if let Action::Hold { x, y, .. } = action {
                    let (px, py) = mapper.point(x, y);
                    let _ = self.backend.touch_up(index as u64, px, py);
                } else if let Action::AndroidKey { keycode } = action {
                    let _ = self.backend.key(false, keycode as i32);
                }
            }
        }
    }

    fn release_binds(&mut self, fps_lane: bool) {
        let mapper = Mapper::new(self.profile.coord_unit(), self.viewport);
        let binds = self.profile.binds.clone();
        let pool = if fps_lane {
            &mut self.fps_fingers
        } else {
            &mut self.normal_fingers
        };
        for (index, bind) in binds.iter().enumerate() {
            if bind.fps_only != fps_lane || !pool.release(index) {
                continue;
            }
            match bind.action {
                Action::Hold { x, y, .. } => {
                    let (px, py) = mapper.point(x, y);
                    let _ = self.backend.touch_up(index as u64, px, py);
                }
                Action::AndroidKey { keycode } => {
                    let _ = self.backend.key(false, keycode as i32);
                }
                _ => {}
            }
        }
    }

    fn release_normal_binds(&mut self) {
        self.release_binds(false);
    }

    fn release_fps_binds(&mut self) {
        self.release_binds(true);
    }

    fn release_wheels(&mut self) {
        for state in &mut self.wheels {
            if state.down {
                let _ = self.backend.touch_up(0, state.last_x, state.last_y);
            }
            state.down = false;
            state.pressed = [false; 4];
        }
    }

    fn wheel_pointers(&self) -> usize {
        self.wheels.iter().filter(|state| state.down).count()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use anyhow::Result;

    use super::*;
    use crate::backend::InputBackend;
    use crate::keymap::{Action, KeyBind};

    #[derive(Default)]
    struct RecordingBackend {
        events: Mutex<Vec<String>>,
    }

    impl RecordingBackend {
        fn events(&self) -> Vec<String> {
            self.events.lock().unwrap().clone()
        }

        fn push(&self, event: String) {
            self.events.lock().unwrap().push(event);
        }
    }

    impl InputBackend for RecordingBackend {
        fn tap(&self, x: i32, y: i32, _duration_ms: i32) -> Result<()> {
            self.push(format!("tap:{x},{y}"));
            Ok(())
        }

        fn touch_down(&self, id: u64, x: i32, y: i32) -> Result<()> {
            self.push(format!("down:{id}:{x},{y}"));
            Ok(())
        }

        fn touch_move(
            &self,
            id: u64,
            from_x: i32,
            from_y: i32,
            to_x: i32,
            to_y: i32,
            _smooth_ms: i32,
        ) -> Result<()> {
            self.push(format!("move:{id}:{from_x},{from_y}->{to_x},{to_y}"));
            Ok(())
        }

        fn touch_up(&self, id: u64, x: i32, y: i32) -> Result<()> {
            self.push(format!("up:{id}:{x},{y}"));
            Ok(())
        }

        fn key(&self, down: bool, key_code: i32) -> Result<()> {
            self.push(format!("key:{down}:{key_code}"));
            Ok(())
        }

        fn mouse_move(&self, dx: i32, dy: i32) -> Result<()> {
            self.push(format!("mouse-move:{dx},{dy}"));
            Ok(())
        }

        fn mouse_button(&self, button: i32, down: bool) -> Result<()> {
            self.push(format!("mouse-button:{button}:{down}"));
            Ok(())
        }

        fn mouse_scroll(&self, amount: i32) -> Result<()> {
            self.push(format!("mouse-scroll:{amount}"));
            Ok(())
        }
    }

    fn tap_bind(key: u16, fps_only: bool) -> KeyBind {
        KeyBind {
            key,
            action: Action::Tap {
                x: 0.5,
                y: 0.5,
                duration_ms: 40,
                radius: 0.03,
            },
            fps_only,
        }
    }

    fn hold_bind(key: u16) -> KeyBind {
        KeyBind {
            key,
            action: Action::Hold {
                x: 0.4,
                y: 0.4,
                radius: 0.03,
            },
            fps_only: false,
        }
    }

    #[test]
    fn auto_repeat_does_not_replay_tap() {
        let backend = Arc::new(RecordingBackend::default());
        let mut profile = Profile::default();
        profile.binds = vec![tap_bind(2037, false)];
        let mut runtime = MappingRuntime::new(backend.clone(), profile, (1000, 2000));
        runtime.set_enabled(true);
        runtime.handle_key_event(2037, true, &[2037]);
        runtime.handle_key_event(2037, true, &[2037]);
        assert_eq!(backend.events(), vec!["tap:500,1000"]);
    }

    #[test]
    fn sensitive_wheel_restores_earlier_direction() {
        let backend = Arc::new(RecordingBackend::default());
        let mut profile = Profile::default();
        profile.wheels[0].mode = WheelMode::Sensitive;
        let mut runtime = MappingRuntime::new(backend.clone(), profile, (1000, 2000));
        runtime.set_enabled(true);
        runtime.handle_key_event(30, true, &[30]); // A: left
        runtime.handle_key_event(32, true, &[30, 32]); // D: right overrides
        runtime.handle_key_event(32, false, &[30]); // release D: restore left
        runtime.handle_key_event(30, false, &[]); // release A: center
        let events = backend.events();
        let moves: Vec<&String> = events
            .iter()
            .filter(|event| event.starts_with("move:"))
            .collect();
        assert!(moves.len() >= 3, "{events:?}");
        assert!(moves[0].contains("->139,750"), "{events:?}");
        assert!(moves[1].contains("->417,750"), "{events:?}");
        assert!(moves[2].contains("->139,750"), "{events:?}");
        assert!(events.iter().any(|event| event.starts_with("up:0:")));
    }

    #[test]
    fn fps_bind_works_while_normal_mapping_is_off() {
        let backend = Arc::new(RecordingBackend::default());
        let mut profile = Profile::default();
        profile.binds = vec![tap_bind(2037, true)];
        let mut runtime = MappingRuntime::new(backend.clone(), profile, (1000, 2000));
        runtime.set_fps_enabled(true);
        runtime.set_fps_toggle_key(2098);
        runtime.handle_key_event(2098, true, &[2098]);
        runtime.handle_key_event(2098, false, &[]);
        runtime.handle_key_event(2037, true, &[2037]);
        assert_eq!(backend.events(), vec!["tap:500,1000"]);
    }

    #[test]
    fn fps_pointer_hiding_tracks_suspend_and_exit() {
        let backend = Arc::new(RecordingBackend::default());
        let mut profile = Profile::default();
        profile.aim.enabled = true;
        profile.aim.anchor_x = 0.5;
        profile.aim.anchor_y = 0.5;
        profile.aim.toggle_key = 2037;
        profile.aim.suspend_key = 2038;
        profile.aim.capture_mouse = true;
        let mut runtime = MappingRuntime::new(backend, profile, (1000, 2000));

        runtime.handle_key_event(2037, true, &[2037]);
        runtime.handle_key_event(2037, false, &[]);
        assert!(runtime.pointer_should_be_hidden());

        runtime.handle_key_event(2038, true, &[2038]);
        assert!(!runtime.pointer_should_be_hidden());

        runtime.handle_key_event(2038, false, &[]);
        assert!(runtime.pointer_should_be_hidden());

        runtime.handle_key_event(2037, true, &[2037]);
        assert!(!runtime.pointer_should_be_hidden());
    }

    #[test]
    fn fps_motion_drives_an_aim_touch() {
        let backend = Arc::new(RecordingBackend::default());
        let mut profile = Profile::default();
        profile.aim.enabled = true;
        profile.aim.anchor_x = 0.5;
        profile.aim.anchor_y = 0.5;
        profile.aim.toggle_key = 2037;
        profile.aim.sensitivity_x = 1.0;
        profile.aim.sensitivity_y = 1.0;
        profile.aim.recenter = crate::keymap::RecenterMode::Never;
        let mut runtime = MappingRuntime::new(backend.clone(), profile, (1000, 2000));

        runtime.handle_key_event(2037, true, &[2037]);
        runtime.handle_key_event(2037, false, &[]);
        assert!(runtime.handle_motion(10.0, 0.0));
        assert!(runtime.handle_motion(5.0, 0.0));

        let events = backend.events();
        assert!(
            events.contains(&"down:1000:510,1000".to_string()),
            "{events:?}"
        );
        assert!(
            events.contains(&"move:1000:510,1000->515,1000".to_string()),
            "{events:?}"
        );
    }

    #[test]
    fn fps_hold_key_gates_aim_and_release_lifts_touch() {
        let backend = Arc::new(RecordingBackend::default());
        let mut profile = Profile::default();
        profile.aim.enabled = true;
        profile.aim.anchor_x = 0.5;
        profile.aim.anchor_y = 0.5;
        profile.aim.toggle_key = 2037;
        profile.aim.hold_key = 273; // right mouse button
        profile.aim.recenter = crate::keymap::RecenterMode::Never;
        let mut runtime = MappingRuntime::new(backend.clone(), profile, (1000, 2000));

        runtime.handle_key_event(2037, true, &[2037]);
        runtime.handle_key_event(2037, false, &[]);
        assert!(!runtime.handle_motion(10.0, 0.0));
        assert!(backend.events().is_empty());

        runtime.handle_key_event(273, true, &[273]);
        runtime.handle_motion(10.0, 0.0);
        runtime.handle_key_event(273, false, &[]);
        let events = backend.events();
        assert!(
            events.iter().any(|event| event.starts_with("down:1000:")),
            "{events:?}"
        );
        assert!(
            events.iter().any(|event| event.starts_with("up:1000:")),
            "{events:?}"
        );
    }

    #[test]
    fn authoritative_snapshot_releases_stale_key() {
        let backend = Arc::new(RecordingBackend::default());
        let mut profile = Profile::default();
        profile.binds = vec![hold_bind(2037)];
        let mut runtime = MappingRuntime::new(backend.clone(), profile, (1000, 2000));
        runtime.set_enabled(true);
        runtime.handle_key_event(2037, true, &[2037]);
        runtime.handle_key_event(2027, true, &[2027]); // U release was lost
        let events = backend.events();
        assert!(events.iter().any(|event| event.starts_with("down:")));
        assert!(events.iter().any(|event| event.starts_with("up:")));
    }
}
