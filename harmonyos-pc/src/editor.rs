use eframe::egui::{self, PointerButton};

use crate::keymap::{
    Action, DEFAULT_RADIUS, DEFAULT_TAP_DURATION_MS, DEFAULT_WHEEL_RADIUS, DEFAULT_WHEEL_SCOPE,
    Easing, KeyBind, Mapper, Profile, RecenterMode, Swipe, SwipePath, TempMode, TempWheel, Wheel,
    WheelMode,
};

/// Transient state for the "press a key" controls. It intentionally stays out
/// of `Profile`, so a pending capture can never leak into the saved YAML.
#[derive(Default)]
pub struct EditorState {
    capture: Option<CaptureField>,
    pick: Option<PickField>,
}

impl EditorState {
    pub fn cancel_capture(&mut self) {
        self.capture = None;
        self.pick = None;
    }

    pub fn cancel_pick(&mut self) {
        self.pick = None;
    }

    pub fn is_picking(&self) -> bool {
        self.pick.is_some()
    }

    pub fn pick_hint(&self) -> Option<String> {
        self.pick.map(|field| match field {
            PickField::BindPoint(index) => format!("请在截图中点击：按键 {} 的落点", index + 1),
            PickField::SwipeStart(index) => {
                format!("请在截图中点击：按键 {} 的滑动起点", index + 1)
            }
            PickField::SwipeEnd(index) => format!("请在截图中点击：按键 {} 的滑动终点", index + 1),
            PickField::SwipeAngle(index) => {
                format!("请在截图中点击：按键 {} 的圆周方向", index + 1)
            }
            PickField::WheelCenter(index) => format!("请在截图中点击：轮盘 {} 的圆心", index + 1),
            PickField::FpsAnchor => "请在截图中点击：FPS 瞄准锚点".to_string(),
        })
    }

    pub fn apply_picked(
        &mut self,
        profile: &mut Profile,
        x_rel: f32,
        y_rel: f32,
        viewport: (u32, u32),
    ) -> bool {
        let Some(field) = self.pick.take() else {
            return false;
        };
        let x_rel = x_rel.clamp(0.0, 1.0);
        let y_rel = y_rel.clamp(0.0, 1.0);
        match field {
            PickField::BindPoint(index) => {
                let Some(bind) = profile.binds.get_mut(index) else {
                    return false;
                };
                match &mut bind.action {
                    Action::Tap { x, y, .. } | Action::Hold { x, y, .. } => {
                        *x = x_rel;
                        *y = y_rel;
                    }
                    _ => return false,
                }
            }
            PickField::SwipeStart(index) => {
                let Some(Action::Swipe(swipe)) =
                    profile.binds.get_mut(index).map(|bind| &mut bind.action)
                else {
                    return false;
                };
                swipe.start = (x_rel, y_rel);
            }
            PickField::SwipeEnd(index) => {
                let Some(Action::Swipe(swipe)) =
                    profile.binds.get_mut(index).map(|bind| &mut bind.action)
                else {
                    return false;
                };
                swipe.end = (x_rel, y_rel);
            }
            PickField::SwipeAngle(index) => {
                let coord_unit = profile.coord_unit();
                let Some(Action::Swipe(swipe)) =
                    profile.binds.get_mut(index).map(|bind| &mut bind.action)
                else {
                    return false;
                };
                let mapper = Mapper::new(coord_unit, viewport);
                let start = mapper.point(swipe.start.0, swipe.start.1);
                let end = mapper.point(swipe.end.0, swipe.end.1);
                set_circle_angle(
                    &mut swipe.path,
                    start,
                    end,
                    (x_rel * viewport.0.max(1) as f32).round() as i32,
                    (y_rel * viewport.1.max(1) as f32).round() as i32,
                );
            }
            PickField::WheelCenter(index) => {
                let Some(wheel) = profile.wheels.get_mut(index) else {
                    return false;
                };
                wheel.cx = x_rel;
                wheel.cy = y_rel;
            }
            PickField::FpsAnchor => {
                profile.aim.anchor_x = x_rel;
                profile.aim.anchor_y = y_rel;
            }
        }
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptureField {
    BindKey(usize),
    WheelDirection { wheel: usize, direction: usize },
    WheelTempKey(usize),
    FpsToggle,
    FpsSuspend,
    FpsHold,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PickField {
    BindPoint(usize),
    SwipeStart(usize),
    SwipeEnd(usize),
    SwipeAngle(usize),
    WheelCenter(usize),
    FpsAnchor,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActionKind {
    Tap,
    Hold,
    Swipe,
    System,
}

impl ActionKind {
    fn of(action: &Action) -> Self {
        match action {
            Action::Tap { .. } => Self::Tap,
            Action::Hold { .. } => Self::Hold,
            Action::Swipe(_) => Self::Swipe,
            Action::AndroidKey { .. } => Self::System,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Tap => "点按",
            Self::Hold => "长按",
            Self::Swipe => "滑动",
            Self::System => "系统键",
        }
    }

    fn default_action(self) -> Action {
        match self {
            Self::Tap => Action::Tap {
                x: 0.5,
                y: 0.5,
                duration_ms: DEFAULT_TAP_DURATION_MS,
                radius: DEFAULT_RADIUS,
            },
            Self::Hold => Action::Hold {
                x: 0.5,
                y: 0.5,
                radius: DEFAULT_RADIUS,
            },
            Self::Swipe => Action::Swipe(Swipe {
                start: (0.5, 0.7),
                end: (0.5, 0.3),
                duration_ms: 300,
                easing: Easing::Linear,
                path: SwipePath::Line,
            }),
            Self::System => Action::AndroidKey { keycode: 4 },
        }
    }
}

/// Applies a physical key captured by the focused GUI window.
///
/// Keyboard capture deliberately runs before the page UI is drawn. The button
/// that arms capture is clicked during one frame, so the next frame is the
/// first frame that can observe a real key press.
pub fn capture_next_key(
    ctx: &egui::Context,
    state: &mut EditorState,
    profile: &mut Profile,
) -> bool {
    let Some(target) = state.capture else {
        return false;
    };
    let Some(code) = captured_key(ctx) else {
        return false;
    };
    state.capture = None;
    apply_capture(profile, target, code)
}

fn captured_key(ctx: &egui::Context) -> Option<u16> {
    ctx.input(|input| {
        for event in &input.events {
            if let egui::Event::Key {
                key,
                physical_key,
                pressed: true,
                repeat: false,
                ..
            } = event
                && let Some(code) = key_to_evdev(physical_key.unwrap_or(*key))
            {
                return Some(code);
            }
        }
        [
            (PointerButton::Primary, 272_u16),
            (PointerButton::Secondary, 273),
            (PointerButton::Middle, 274),
            (PointerButton::Extra1, 275),
            (PointerButton::Extra2, 276),
        ]
        .into_iter()
        .find_map(|(button, code)| input.pointer.button_pressed(button).then_some(code))
    })
}

fn apply_capture(profile: &mut Profile, target: CaptureField, code: u16) -> bool {
    let mut changed = false;
    match target {
        CaptureField::BindKey(index) => {
            if let Some(bind) = profile.binds.get_mut(index) {
                bind.key = code;
                changed = true;
            }
        }
        CaptureField::WheelDirection { wheel, direction } => {
            if let Some(wheel) = profile.wheels.get_mut(wheel) {
                match direction {
                    0 => wheel.up = code,
                    1 => wheel.down = code,
                    2 => wheel.left = code,
                    3 => wheel.right = code,
                    _ => return false,
                }
                changed = true;
            }
        }
        CaptureField::WheelTempKey(index) => {
            if let Some(temp) = profile
                .wheels
                .get_mut(index)
                .and_then(|wheel| wheel.temp.as_mut())
            {
                temp.key = code;
                changed = true;
            }
        }
        CaptureField::FpsToggle => {
            profile.aim.toggle_key = code;
            changed = true;
        }
        CaptureField::FpsSuspend => {
            profile.aim.suspend_key = code;
            changed = true;
        }
        CaptureField::FpsHold => {
            profile.aim.hold_key = code;
            changed = true;
        }
    }
    changed
}

fn key_to_evdev(key: egui::Key) -> Option<u16> {
    use egui::Key;
    Some(match key {
        Key::ArrowDown => 108,
        Key::ArrowLeft => 105,
        Key::ArrowRight => 106,
        Key::ArrowUp => 103,
        Key::Escape => 1,
        Key::Tab => 15,
        Key::Backspace => 14,
        Key::Enter => 28,
        Key::Space => 57,
        Key::Insert => 110,
        Key::Delete => 111,
        Key::Home => 102,
        Key::End => 107,
        Key::PageUp => 104,
        Key::PageDown => 109,
        Key::Colon | Key::Semicolon => 39,
        Key::Comma => 51,
        Key::Backslash | Key::Pipe => 43,
        Key::Slash | Key::Questionmark => 53,
        Key::OpenBracket | Key::OpenCurlyBracket => 26,
        Key::CloseBracket | Key::CloseCurlyBracket => 27,
        Key::Backtick => 41,
        Key::Minus => 12,
        Key::Period => 52,
        Key::Plus | Key::Equals => 13,
        Key::Quote => 40,
        Key::Num0 => 11,
        Key::Num1 | Key::Exclamationmark => 2,
        Key::Num2 => 3,
        Key::Num3 => 4,
        Key::Num4 => 5,
        Key::Num5 => 6,
        Key::Num6 => 7,
        Key::Num7 => 8,
        Key::Num8 => 9,
        Key::Num9 => 10,
        Key::A => 30,
        Key::B => 48,
        Key::C => 46,
        Key::D => 32,
        Key::E => 18,
        Key::F => 33,
        Key::G => 34,
        Key::H => 35,
        Key::I => 23,
        Key::J => 36,
        Key::K => 37,
        Key::L => 38,
        Key::M => 50,
        Key::N => 49,
        Key::O => 24,
        Key::P => 25,
        Key::Q => 16,
        Key::R => 19,
        Key::S => 31,
        Key::T => 20,
        Key::U => 22,
        Key::V => 47,
        Key::W => 17,
        Key::X => 45,
        Key::Y => 21,
        Key::Z => 44,
        Key::F1 => 59,
        Key::F2 => 60,
        Key::F3 => 61,
        Key::F4 => 62,
        Key::F5 => 63,
        Key::F6 => 64,
        Key::F7 => 65,
        Key::F8 => 66,
        Key::F9 => 67,
        Key::F10 => 68,
        Key::F11 => 87,
        Key::F12 => 88,
        Key::F13 => 183,
        Key::F14 => 184,
        Key::F15 => 185,
        Key::F16 => 186,
        Key::F17 => 187,
        Key::F18 => 188,
        Key::F19 => 189,
        Key::F20 => 190,
        Key::F21 => 191,
        Key::F22 => 192,
        Key::F23 => 193,
        Key::F24 => 194,
        Key::BrowserBack => 158,
        Key::ShiftLeft => 42,
        Key::ShiftRight => 54,
        Key::ControlLeft => 29,
        Key::ControlRight => 97,
        Key::AltLeft => 56,
        Key::AltRight => 100,
        Key::SuperLeft => 125,
        Key::SuperRight => 126,
        Key::IntlBackslash => 86,
        _ => return None,
    })
}

fn key_button(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    field: CaptureField,
    enabled: bool,
) -> bool {
    let armed = state.capture == Some(field);
    if armed {
        ui.add_enabled(false, egui::Button::new("等待按键..."));
        if ui.small_button("取消").clicked() {
            state.capture = None;
        }
    } else if ui
        .add_enabled(enabled, egui::Button::new("捕获"))
        .on_hover_text(if enabled {
            "点击后按下要绑定的键盘键或鼠标键"
        } else {
            "先停止映射运行，避免捕获时把按键注入手机"
        })
        .clicked()
    {
        state.capture = Some(field);
    }
    armed
}

fn key_code_editor(
    ui: &mut egui::Ui,
    key: &mut u16,
    state: &mut EditorState,
    field: CaptureField,
    enabled: bool,
) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        let armed = key_button(ui, state, field, enabled);
        changed |= ui
            .add_enabled(!armed, egui::DragValue::new(key).range(0..=u16::MAX))
            .changed();
        ui.label(egui::RichText::new(display_key_name(*key)).monospace());
    });
    changed
}

fn display_key_name(code: u16) -> String {
    #[cfg(windows)]
    {
        crate::input_capture::win_key_name(code)
    }
    #[cfg(not(windows))]
    {
        crate::keymap::key_name(code)
    }
}

pub fn bindings_ui(
    ui: &mut egui::Ui,
    profile: &mut Profile,
    state: &mut EditorState,
    capture_enabled: bool,
) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.heading("按键映射");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("＋ 新增点按").clicked() {
                profile.binds.push(KeyBind {
                    key: 57,
                    action: ActionKind::Tap.default_action(),
                    fps_only: false,
                });
                changed = true;
            }
        });
    });
    ui.label("每一行代表一个物理键到手机动作的映射。“仅 FPS”同键会覆盖普通映射。");
    if !capture_enabled {
        ui.label(
            egui::RichText::new("映射运行中：停止后可使用“捕获”和“截图取点”。")
                .color(egui::Color32::from_rgb(217, 119, 6)),
        );
    }
    ui.add_space(8.0);

    let mut remove = None;
    for index in 0..profile.binds.len() {
        let mut bind = profile.binds[index].clone();
        let mut bind_changed = false;
        let mut copy_requested = false;
        let title = format!(
            "{}  {}  {}",
            if bind.fps_only { "FPS" } else { "普通" },
            display_key_name(bind.key),
            bind.action.kind_name()
        );
        egui::CollapsingHeader::new(title)
            .id_salt(("bind", index))
            .default_open(false)
            .show(ui, |ui| {
                egui::Grid::new(("bind_grid", index))
                    .num_columns(2)
                    .spacing([12.0, 7.0])
                    .show(ui, |ui| {
                        ui.label("物理键码");
                        if key_code_editor(
                            ui,
                            &mut bind.key,
                            state,
                            CaptureField::BindKey(index),
                            capture_enabled,
                        ) {
                            changed = true;
                        }
                        ui.end_row();
                        ui.label("作用范围");
                        if ui.checkbox(&mut bind.fps_only, "仅 FPS").changed() {
                            bind_changed = true;
                        }
                        ui.end_row();
                        ui.label("动作");
                        let mut kind = ActionKind::of(&bind.action);
                        egui::ComboBox::from_id_salt(("bind_kind", index))
                            .selected_text(kind.label())
                            .show_ui(ui, |ui| {
                                for candidate in [
                                    ActionKind::Tap,
                                    ActionKind::Hold,
                                    ActionKind::Swipe,
                                    ActionKind::System,
                                ] {
                                    ui.selectable_value(&mut kind, candidate, candidate.label());
                                }
                            });
                        if kind != ActionKind::of(&bind.action) {
                            bind.action = kind.default_action();
                            bind_changed = true;
                        }
                        ui.end_row();
                    });

                match &mut bind.action {
                    Action::Tap {
                        x,
                        y,
                        duration_ms,
                        radius,
                    } => {
                        bind_changed |= point_editor(
                            ui,
                            "落点",
                            x,
                            y,
                            state,
                            PickField::BindPoint(index),
                            capture_enabled,
                        );
                        ui.horizontal(|ui| {
                            ui.label("持续");
                            bind_changed |= ui
                                .add(
                                    egui::DragValue::new(duration_ms)
                                        .range(0..=10_000)
                                        .suffix(" ms"),
                                )
                                .changed();
                            ui.label("范围");
                            bind_changed |= ui
                                .add(egui::DragValue::new(radius).speed(0.001).range(0.005..=0.5))
                                .changed();
                        });
                    }
                    Action::Hold { x, y, radius } => {
                        bind_changed |= point_editor(
                            ui,
                            "落点",
                            x,
                            y,
                            state,
                            PickField::BindPoint(index),
                            capture_enabled,
                        );
                        ui.horizontal(|ui| {
                            ui.label("范围");
                            bind_changed |= ui
                                .add(egui::DragValue::new(radius).speed(0.001).range(0.005..=0.5))
                                .changed();
                        });
                    }
                    Action::Swipe(swipe) => {
                        bind_changed |= swipe_editor(ui, swipe, state, index, capture_enabled);
                    }
                    Action::AndroidKey { keycode } => {
                        ui.horizontal(|ui| {
                            ui.label("系统键码");
                            bind_changed |= ui
                                .add(egui::DragValue::new(keycode).range(0..=u32::MAX))
                                .changed();
                        });
                    }
                }

                ui.horizontal(|ui| {
                    if ui.button("复制").clicked() {
                        copy_requested = true;
                    }
                    if ui.button("删除").clicked() {
                        remove = Some(index);
                    }
                });
            });
        if bind_changed {
            profile.binds[index] = bind.clone();
            changed = true;
        }
        if copy_requested {
            profile.binds.push(bind);
            changed = true;
        }
    }
    if let Some(index) = remove {
        profile.binds.remove(index);
        state.capture = None;
        changed = true;
    }
    changed
}

pub fn wheels_ui(
    ui: &mut egui::Ui,
    profile: &mut Profile,
    state: &mut EditorState,
    capture_enabled: bool,
) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.heading("虚拟轮盘");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("＋ 新增轮盘").clicked() {
                let (cx, cy) = crate::keymap::next_wheel_spot(&profile.wheels);
                profile.wheels.push(Wheel {
                    up: 17,
                    down: 31,
                    left: 30,
                    right: 32,
                    cx,
                    cy,
                    radius: DEFAULT_WHEEL_RADIUS,
                    scope: DEFAULT_WHEEL_SCOPE,
                    mode: WheelMode::Classic,
                    temp: None,
                });
                changed = true;
            }
        });
    });
    ui.label("方向键可重复使用；灵敏模式按轴采用最后按下的方向。");
    if !capture_enabled {
        ui.label(
            egui::RichText::new("映射运行中：停止后可使用“捕获”和“截图取点”。")
                .color(egui::Color32::from_rgb(217, 119, 6)),
        );
    }
    ui.add_space(8.0);

    let mut remove = None;
    for index in 0..profile.wheels.len() {
        let wheel = &mut profile.wheels[index];
        egui::CollapsingHeader::new(format!("轮盘 {}  ·  {}", index + 1, wheel.mode.label()))
            .id_salt(("wheel", index))
            .default_open(index == 0)
            .show(ui, |ui| {
                egui::Grid::new(("wheel_grid", index))
                    .num_columns(2)
                    .spacing([10.0, 7.0])
                    .show(ui, |ui| {
                        let directions = [
                            ("上", 0, &mut wheel.up),
                            ("下", 1, &mut wheel.down),
                            ("左", 2, &mut wheel.left),
                            ("右", 3, &mut wheel.right),
                        ];
                        for (label, direction, value) in directions {
                            ui.label(label);
                            if key_code_editor(
                                ui,
                                value,
                                state,
                                CaptureField::WheelDirection {
                                    wheel: index,
                                    direction,
                                },
                                capture_enabled,
                            ) {
                                changed = true;
                            }
                            ui.end_row();
                        }
                    });
                changed |= point_editor(
                    ui,
                    "圆心",
                    &mut wheel.cx,
                    &mut wheel.cy,
                    state,
                    PickField::WheelCenter(index),
                    capture_enabled,
                );
                ui.horizontal(|ui| {
                    ui.label("半径");
                    changed |= ui
                        .add(
                            egui::DragValue::new(&mut wheel.radius)
                                .speed(0.001)
                                .range(0.01..=0.5),
                        )
                        .changed();
                    ui.label("力度");
                    changed |= ui
                        .add(
                            egui::DragValue::new(&mut wheel.scope)
                                .speed(0.02)
                                .range(0.2..=4.0),
                        )
                        .changed();
                });
                ui.horizontal(|ui| {
                    ui.label("模式");
                    let mut mode = wheel.mode;
                    egui::ComboBox::from_id_salt(("wheel_mode", index))
                        .selected_text(mode.label())
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut mode,
                                WheelMode::Classic,
                                WheelMode::Classic.label(),
                            );
                            ui.selectable_value(
                                &mut mode,
                                WheelMode::Sensitive,
                                WheelMode::Sensitive.label(),
                            );
                        });
                    if mode != wheel.mode {
                        wheel.mode = mode;
                        changed = true;
                    }
                });
                ui.separator();
                ui.horizontal(|ui| {
                    let mut has_temp = wheel.temp.is_some();
                    if ui.checkbox(&mut has_temp, "临时轮盘").changed() {
                        wheel.temp = has_temp.then_some(TempWheel {
                            key: 18,
                            mode: TempMode::Hold,
                        });
                        changed = true;
                    }
                });
                if let Some(temp) = &mut wheel.temp {
                    ui.horizontal(|ui| {
                        ui.label("启用键");
                        if key_code_editor(
                            ui,
                            &mut temp.key,
                            state,
                            CaptureField::WheelTempKey(index),
                            capture_enabled,
                        ) {
                            changed = true;
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("触发");
                        let mut mode = temp.mode;
                        egui::ComboBox::from_id_salt(("temp_mode", index))
                            .selected_text(match mode {
                                TempMode::Hold => "按住启用",
                                TempMode::Toggle => "按一下切换",
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut mode, TempMode::Hold, "按住启用");
                                ui.selectable_value(&mut mode, TempMode::Toggle, "按一下切换");
                            });
                        if mode != temp.mode {
                            temp.mode = mode;
                            changed = true;
                        }
                    });
                }
                if ui.button("删除轮盘").clicked() {
                    remove = Some(index);
                }
            });
    }
    if let Some(index) = remove {
        profile.wheels.remove(index);
        state.capture = None;
        changed = true;
    }
    changed
}

pub fn fps_ui(
    ui: &mut egui::Ui,
    profile: &mut Profile,
    state: &mut EditorState,
    capture_enabled: bool,
) -> bool {
    let aim = &mut profile.aim;
    let mut changed = false;
    ui.heading("FPS 模式");
    ui.label("FPS 专用键会覆盖同键普通映射；“按住才退出”期间恢复普通映射。");
    if !capture_enabled {
        ui.label(
            egui::RichText::new("映射运行中：停止后可使用“捕获”和“截图取点”。")
                .color(egui::Color32::from_rgb(217, 119, 6)),
        );
    }
    ui.add_space(8.0);
    changed |= ui.checkbox(&mut aim.enabled, "启用 FPS 模式").changed();
    changed |= point_editor(
        ui,
        "锚点",
        &mut aim.anchor_x,
        &mut aim.anchor_y,
        state,
        PickField::FpsAnchor,
        capture_enabled,
    );

    egui::Grid::new("fps_grid")
        .num_columns(2)
        .spacing([12.0, 8.0])
        .show(ui, |ui| {
            ui.label("灵敏度 X");
            changed |= ui
                .add(egui::DragValue::new(&mut aim.sensitivity_x).range(0.1..=20.0))
                .changed();
            ui.end_row();
            ui.label("灵敏度 Y");
            changed |= ui
                .add(egui::DragValue::new(&mut aim.sensitivity_y).range(0.1..=20.0))
                .changed();
            ui.end_row();
            ui.label("FPS 开关键");
            changed |= key_code_editor(
                ui,
                &mut aim.toggle_key,
                state,
                CaptureField::FpsToggle,
                capture_enabled,
            );
            ui.end_row();
            ui.label("按住才退出");
            changed |= key_code_editor(
                ui,
                &mut aim.suspend_key,
                state,
                CaptureField::FpsSuspend,
                capture_enabled,
            );
            ui.end_row();
            ui.label("按住才瞄准");
            changed |= key_code_editor(
                ui,
                &mut aim.hold_key,
                state,
                CaptureField::FpsHold,
                capture_enabled,
            );
            ui.end_row();
        });
    changed |= ui.checkbox(&mut aim.invert_y, "反转 Y").changed();
    changed |= ui
        .checkbox(&mut aim.capture_mouse, "指针消隐（默认开启）")
        .changed();

    ui.separator();
    ui.horizontal(|ui| {
        ui.label("归中");
        let mut mode = aim.recenter;
        egui::ComboBox::from_id_salt("recenter_mode")
            .selected_text(mode.label())
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut mode, RecenterMode::Idle, RecenterMode::Idle.label());
                ui.selectable_value(
                    &mut mode,
                    RecenterMode::Threshold,
                    RecenterMode::Threshold.label(),
                );
                ui.selectable_value(&mut mode, RecenterMode::Never, RecenterMode::Never.label());
            });
        if mode != aim.recenter {
            aim.recenter = mode;
            changed = true;
        }
    });
    match aim.recenter {
        RecenterMode::Idle => {
            ui.horizontal(|ui| {
                ui.label("静止");
                changed |= ui
                    .add(
                        egui::DragValue::new(&mut aim.recenter_idle_ms)
                            .range(20..=2000)
                            .suffix(" ms"),
                    )
                    .changed();
            });
        }
        RecenterMode::Threshold => {
            ui.horizontal(|ui| {
                ui.label("阈值");
                changed |= ui
                    .add(
                        egui::DragValue::new(&mut aim.recenter_threshold)
                            .range(20..=4000)
                            .suffix(" px"),
                    )
                    .changed();
            });
        }
        RecenterMode::Never => {}
    }
    changed
}

fn point_editor(
    ui: &mut egui::Ui,
    label: &str,
    x: &mut f32,
    y: &mut f32,
    state: &mut EditorState,
    field: PickField,
    enabled: bool,
) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(label);
        ui.label("X");
        changed |= ui
            .add(egui::DragValue::new(x).speed(0.005).range(0.0..=1.0))
            .changed();
        ui.label("Y");
        changed |= ui
            .add(egui::DragValue::new(y).speed(0.005).range(0.0..=1.0))
            .changed();
        let waiting = state.pick == Some(field);
        if enabled {
            if ui
                .button(if waiting {
                    "点击截图..."
                } else {
                    "截图取点"
                })
                .clicked()
            {
                state.capture = None;
                state.pick = Some(field);
            }
        } else {
            ui.add_enabled(false, egui::Button::new("截图取点"));
        }
    });
    changed
}

fn swipe_editor(
    ui: &mut egui::Ui,
    swipe: &mut Swipe,
    state: &mut EditorState,
    index: usize,
    enabled: bool,
) -> bool {
    let mut changed = false;
    changed |= point_editor(
        ui,
        "起点",
        &mut swipe.start.0,
        &mut swipe.start.1,
        state,
        PickField::SwipeStart(index),
        enabled,
    );
    changed |= point_editor(
        ui,
        "终点",
        &mut swipe.end.0,
        &mut swipe.end.1,
        state,
        PickField::SwipeEnd(index),
        enabled,
    );
    ui.horizontal(|ui| {
        ui.label("时长");
        changed |= ui
            .add(
                egui::DragValue::new(&mut swipe.duration_ms)
                    .range(20..=5000)
                    .suffix(" ms"),
            )
            .changed();
    });
    ui.horizontal(|ui| {
        ui.label("轨迹");
        let mut path = swipe.path;
        egui::ComboBox::from_id_salt("swipe_path")
            .selected_text(path.label())
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut path, SwipePath::Line, "直线");
                ui.selectable_value(&mut path, SwipePath::Rect, "矩形");
                let circle = SwipePath::Circle {
                    as_diameter: false,
                    start_angle: 0.0,
                };
                ui.selectable_value(&mut path, circle, "圆形");
            });
        if path != swipe.path {
            swipe.path = path;
            changed = true;
        }
    });
    if matches!(swipe.path, SwipePath::Circle { .. }) {
        let waiting = state.pick == Some(PickField::SwipeAngle(index));
        if enabled {
            if ui
                .button(if waiting {
                    "点击截图取圆周方向..."
                } else {
                    "截图取圆周方向"
                })
                .clicked()
            {
                state.capture = None;
                state.pick = Some(PickField::SwipeAngle(index));
            }
        } else {
            ui.add_enabled(false, egui::Button::new("截图取圆周方向"));
        }
    }
    changed
}

fn set_circle_angle(path: &mut SwipePath, start: (i32, i32), end: (i32, i32), x: i32, y: i32) {
    let SwipePath::Circle {
        as_diameter,
        start_angle,
    } = path
    else {
        return;
    };
    let (cx, cy) = if *as_diameter {
        (
            (start.0 + end.0) as f32 / 2.0,
            (start.1 + end.1) as f32 / 2.0,
        )
    } else {
        (start.0 as f32, start.1 as f32)
    };
    *start_angle = (y as f32 - cy).atan2(x as f32 - cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gui_keys_map_to_the_shared_evdev_codes() {
        assert_eq!(key_to_evdev(egui::Key::W), Some(17));
        assert_eq!(key_to_evdev(egui::Key::A), Some(30));
        assert_eq!(key_to_evdev(egui::Key::F8), Some(66));
        assert_eq!(key_to_evdev(egui::Key::ControlLeft), Some(29));
    }

    #[test]
    fn capture_updates_bind_wheel_and_fps_without_stale_panics() {
        let mut profile = Profile::default();
        profile.binds.push(KeyBind {
            key: 57,
            action: ActionKind::Tap.default_action(),
            fps_only: false,
        });

        assert!(apply_capture(&mut profile, CaptureField::BindKey(0), 17));
        assert_eq!(profile.binds[0].key, 17);

        assert!(apply_capture(
            &mut profile,
            CaptureField::WheelDirection {
                wheel: 0,
                direction: 2,
            },
            30,
        ));
        assert_eq!(profile.wheels[0].left, 30);

        assert!(apply_capture(&mut profile, CaptureField::FpsSuspend, 18));
        assert_eq!(profile.aim.suspend_key, 18);

        assert!(!apply_capture(&mut profile, CaptureField::BindKey(99), 44));
        assert!(!apply_capture(
            &mut profile,
            CaptureField::WheelDirection {
                wheel: 99,
                direction: 0,
            },
            44,
        ));
    }

    #[test]
    fn capture_next_key_reads_the_gui_event_stream() {
        let ctx = egui::Context::default();
        let mut state = EditorState {
            capture: Some(CaptureField::FpsToggle),
            ..Default::default()
        };
        let mut profile = Profile::default();
        let input = egui::RawInput {
            events: vec![egui::Event::Key {
                key: egui::Key::K,
                physical_key: Some(egui::Key::K),
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            }],
            ..Default::default()
        };

        let mut output = ctx.run_ui(input, |ctx| {
            assert!(capture_next_key(ctx, &mut state, &mut profile));
        });
        output.textures_delta.clear();
        assert_eq!(profile.aim.toggle_key, 37);
        assert!(state.capture.is_none());
    }

    #[test]
    fn screenshot_pick_writes_bind_swipe_wheel_and_fps_coordinates() {
        let mut profile = Profile::default();
        profile.binds.push(KeyBind {
            key: 57,
            action: ActionKind::Tap.default_action(),
            fps_only: false,
        });
        profile.binds.push(KeyBind {
            key: 17,
            action: ActionKind::Swipe.default_action(),
            fps_only: false,
        });
        let mut state = EditorState {
            pick: Some(PickField::BindPoint(0)),
            ..Default::default()
        };

        assert!(state.apply_picked(&mut profile, 0.25, 0.75, (1000, 2000)));
        assert!(matches!(
            profile.binds[0].action,
            Action::Tap {
                x: 0.25,
                y: 0.75,
                ..
            }
        ));

        state.pick = Some(PickField::SwipeStart(1));
        assert!(state.apply_picked(&mut profile, 0.1, 0.2, (1000, 2000)));
        state.pick = Some(PickField::SwipeEnd(1));
        assert!(state.apply_picked(&mut profile, 0.8, 0.9, (1000, 2000)));
        let Action::Swipe(swipe) = &profile.binds[1].action else {
            unreachable!()
        };
        assert_eq!(swipe.start, (0.1, 0.2));
        assert_eq!(swipe.end, (0.8, 0.9));

        state.pick = Some(PickField::WheelCenter(0));
        assert!(state.apply_picked(&mut profile, 0.3, 0.4, (1000, 2000)));
        assert_eq!((profile.wheels[0].cx, profile.wheels[0].cy), (0.3, 0.4));

        state.pick = Some(PickField::FpsAnchor);
        assert!(state.apply_picked(&mut profile, 0.6, 0.7, (1000, 2000)));
        assert_eq!((profile.aim.anchor_x, profile.aim.anchor_y), (0.6, 0.7));

        state.pick = Some(PickField::BindPoint(99));
        assert!(!state.apply_picked(&mut profile, 0.5, 0.5, (1000, 2000)));
    }
}
