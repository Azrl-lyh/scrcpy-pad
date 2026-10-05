use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui;
use egui::{Align, Color32, CornerRadius, Frame, Layout, Margin, RichText, Stroke};
use scrcpy_pad_hdc::capture;
use scrcpy_pad_hdc::config::ProfileStore;
use scrcpy_pad_hdc::editor;
use scrcpy_pad_hdc::hdc::Hdc;
use scrcpy_pad_hdc::input;
use scrcpy_pad_hdc::keymap::{ConfigFile, Profile};
use scrcpy_pad_hdc::service::MappingService;
use scrcpy_pad_hdc::ui_font::install_cjk_font;

const APP_ICON_PNG: &[u8] = include_bytes!("../../../icons/scrcpy-pad.png");
const BG: Color32 = Color32::from_rgb(244, 247, 251);
const PANEL: Color32 = Color32::from_rgb(255, 255, 255);
const LINE: Color32 = Color32::from_rgb(222, 229, 238);
const INK: Color32 = Color32::from_rgb(24, 34, 48);
const MUTED: Color32 = Color32::from_rgb(105, 119, 139);
const ACCENT: Color32 = Color32::from_rgb(35, 132, 255);
const OK: Color32 = Color32::from_rgb(22, 163, 74);
const WARN: Color32 = Color32::from_rgb(217, 119, 6);
const DANGER: Color32 = Color32::from_rgb(220, 38, 38);

fn main() -> eframe::Result<()> {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1240.0, 800.0])
            .with_min_inner_size([980.0, 650.0])
            .with_icon(load_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "scrcpy-pad HarmonyOS HDC",
        native_options,
        Box::new(|cc| {
            install_cjk_font(&cc.egui_ctx);
            configure_style(&cc.egui_ctx);
            Ok(Box::new(HdcPadApp::new()))
        }),
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Connect,
    Keys,
    Wheels,
    Fps,
    Test,
}

struct HdcPadApp {
    hdc: Hdc,
    devices: Vec<String>,
    selected: String,
    status: String,
    diagnostics: Vec<String>,
    screenshot: Option<egui::TextureHandle>,
    screenshot_size: (u32, u32),
    screenshot_path: PathBuf,
    auto_capture: bool,
    last_capture: Instant,
    x: i32,
    y: i32,
    x2: i32,
    y2: i32,
    duration_ms: i32,
    mapping: Option<MappingService>,
    mapping_status: String,
    pointer_captured_prev: bool,
    store: ProfileStore,
    config: ConfigFile,
    editor_state: editor::EditorState,
    dirty: bool,
    tab: Tab,
}

impl HdcPadApp {
    fn new() -> Self {
        scrcpy_pad_hdc::input_capture::set_cursor_visible_from_ui(true);
        let hdc = Hdc::locate().expect("HDC discovery failed");
        let store = ProfileStore::discover();
        let config = store.load_or_default();
        let mut app = Self {
            hdc,
            devices: Vec::new(),
            selected: String::new(),
            status: "就绪".to_string(),
            diagnostics: Vec::new(),
            screenshot: None,
            screenshot_size: (0, 0),
            screenshot_path: std::env::temp_dir().join("scrcpy-pad-hdc-screen.png"),
            auto_capture: false,
            last_capture: Instant::now(),
            x: 612,
            y: 1388,
            x2: 900,
            y2: 1700,
            duration_ms: 300,
            mapping: None,
            mapping_status: "映射未启动".to_string(),
            pointer_captured_prev: false,
            store,
            config,
            editor_state: editor::EditorState::default(),
            dirty: false,
            tab: Tab::Connect,
        };
        app.refresh_devices();
        if std::env::var_os("SCRCPY_PAD_HDC_AUTOSTART").is_some() && !app.selected.is_empty() {
            app.start_mapping();
        }
        app
    }

    fn active_profile(&self) -> &Profile {
        self.config
            .active_profile()
            .expect("ConfigFile must contain at least one scheme")
    }

    fn active_profile_mut(&mut self) -> &mut Profile {
        self.config.normalize();
        let index = self
            .config
            .active
            .min(self.config.schemes.len().saturating_sub(1));
        &mut self.config.schemes[index]
    }

    fn profile_changed(&mut self) {
        self.dirty = true;
        self.sync_runtime_profile();
    }

    fn profile_changed_if(&mut self, changed: bool) {
        if changed {
            self.profile_changed();
        }
    }

    fn sync_runtime_profile(&mut self) {
        let profile = self.active_profile().clone();
        if let Some(service) = &self.mapping {
            let runtime = service.runtime();
            let mut runtime = runtime.lock().unwrap();
            *runtime.profile_mut() = profile;
            runtime.notify_profile_changed();
            drop(runtime);
            service.sync_state();
        }
    }

    fn save_config(&mut self) {
        match self.store.save(&self.config) {
            Ok(()) => {
                self.dirty = false;
                self.status = format!("配置已保存：{}", self.store.path().display());
            }
            Err(error) => self.status = format!("配置保存失败：{error:#}"),
        }
    }

    fn refresh_devices(&mut self) {
        match self.hdc.targets() {
            Ok(devices) => {
                self.devices = devices;
                if self.selected.is_empty() {
                    self.selected = self.devices.first().cloned().unwrap_or_default();
                }
                self.status = if self.devices.is_empty() {
                    "未发现设备".to_string()
                } else {
                    format!("发现 {} 台设备", self.devices.len())
                };
            }
            Err(error) => self.status = format!("设备枚举失败：{error:#}"),
        }
    }

    fn capture_now(&mut self, ctx: &egui::Context) {
        if self.selected.is_empty() {
            self.status = "没有选择设备".to_string();
            return;
        }
        match capture::capture(
            &self.hdc,
            &self.selected,
            &self.screenshot_path.display().to_string(),
        ) {
            Ok(path) => match image::open(&path) {
                Ok(image) => {
                    let rgba = image.to_rgba8();
                    let previous_size = self.screenshot_size;
                    self.screenshot_size = rgba.dimensions();
                    let color_image = egui::ColorImage::from_rgba_unmultiplied(
                        [
                            self.screenshot_size.0 as usize,
                            self.screenshot_size.1 as usize,
                        ],
                        rgba.as_raw(),
                    );
                    self.screenshot = Some(ctx.load_texture(
                        "harmonyos-screen",
                        color_image,
                        egui::TextureOptions::LINEAR,
                    ));
                    self.status = format!(
                        "截图成功 {}×{}",
                        self.screenshot_size.0, self.screenshot_size.1
                    );
                    if previous_size != self.screenshot_size
                        && let Some(service) = &self.mapping
                    {
                        service
                            .runtime()
                            .lock()
                            .unwrap()
                            .set_viewport(self.screenshot_size.0, self.screenshot_size.1);
                        service.sync_state();
                    }
                    self.last_capture = Instant::now();
                }
                Err(error) => self.status = format!("截图解码失败：{error:#}"),
            },
            Err(error) => self.status = format!("截图失败：{error:#}"),
        }
    }

    fn run_diagnostics(&mut self) {
        self.diagnostics.clear();
        if self.selected.is_empty() {
            self.status = "没有选择设备".to_string();
            return;
        }
        for command in [
            vec!["param", "get", "const.product.software.version"],
            vec!["param", "get", "const.ohos.apiversion"],
            vec!["which", "uinput"],
            vec!["which", "uitest"],
            vec!["which", "snapshot_display"],
        ] {
            let label = command.join(" ");
            match self.hdc.shell(&self.selected, command) {
                Ok(output) => self
                    .diagnostics
                    .push(format!("{label} → {}", output.trim())),
                Err(error) => self.diagnostics.push(format!("{label} → ERROR {error:#}")),
            }
        }
        self.status = "能力检测完成".to_string();
    }

    fn send(&mut self, result: anyhow::Result<String>) {
        self.status = match result {
            Ok(output) if output.trim().is_empty() => "命令执行成功".to_string(),
            Ok(output) => output.trim().to_string(),
            Err(error) => format!("命令失败：{error:#}"),
        };
    }

    fn start_mapping(&mut self) {
        if self.selected.is_empty() {
            self.mapping_status = "没有选择设备".to_string();
            return;
        }
        self.editor_state.cancel_capture();
        let viewport = if self.screenshot_size.0 > 0 {
            self.screenshot_size
        } else {
            (1224, 2776)
        };
        match MappingService::start(
            self.selected.clone(),
            viewport,
            self.active_profile().clone(),
        ) {
            Ok(service) => {
                service.runtime().lock().unwrap().set_enabled(true);
                service.sync_state();
                self.mapping = Some(service);
                self.mapping_status = "映射已开启，按键即可控制手机；F8 可暂时关闭".to_string();
            }
            Err(error) => self.mapping_status = format!("启动失败：{error:#}"),
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("●").size(18.0).color(ACCENT));
            ui.heading(RichText::new("scrcpy-pad").strong().color(INK));
            ui.label(RichText::new("HarmonyOS HDC").color(MUTED));
            ui.separator();
            egui::ComboBox::from_id_salt("device_top")
                .selected_text(if self.selected.is_empty() {
                    "无设备"
                } else {
                    &self.selected
                })
                .show_ui(ui, |ui| {
                    for device in &self.devices {
                        ui.selectable_value(&mut self.selected, device.clone(), device);
                    }
                });
            let connected = !self.selected.is_empty();
            pill(
                ui,
                if connected {
                    "USB 已连接"
                } else {
                    "未连接"
                },
                if connected { OK } else { DANGER },
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add_enabled(
                        self.dirty,
                        egui::Button::new("保存配置")
                            .fill(ACCENT)
                            .corner_radius(8.0),
                    )
                    .clicked()
                {
                    self.save_config();
                }
                if ui.button("刷新设备").clicked() {
                    self.refresh_devices();
                }
                let mapping_enabled = self
                    .mapping
                    .as_ref()
                    .map(|service| service.runtime().lock().unwrap().is_enabled())
                    .unwrap_or(false);
                if self.mapping.is_some() {
                    let text = if mapping_enabled {
                        "映射：开"
                    } else {
                        "映射：关"
                    };
                    pill(ui, text, if mapping_enabled { OK } else { MUTED });
                }
            });
        });
    }

    fn nav(&mut self, ui: &mut egui::Ui) {
        ui.add_space(10.0);
        for (tab, icon, label) in [
            (Tab::Connect, "⌁", "连接与设备"),
            (Tab::Keys, "⌨", "按键映射"),
            (Tab::Wheels, "◎", "虚拟轮盘"),
            (Tab::Fps, "⊕", "FPS 模式"),
            (Tab::Test, "⌘", "设备测试"),
        ] {
            let selected = self.tab == tab;
            let text = RichText::new(format!("{icon}  {label}"))
                .size(15.0)
                .color(if selected { ACCENT } else { INK });
            if ui.selectable_label(selected, text).clicked() {
                self.tab = tab;
            }
            ui.add_space(2.0);
        }
        ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
            ui.add_space(12.0);
            ui.label(RichText::new("HDC · USB · uinput").size(11.0).color(MUTED));
        });
    }

    fn content(&mut self, ui: &mut egui::Ui) {
        self.config.normalize();
        let profile_index = self
            .config
            .active
            .min(self.config.schemes.len().saturating_sub(1));
        let capture_enabled = self.mapping.is_none();
        egui::ScrollArea::vertical().show(ui, |ui| match self.tab {
            Tab::Connect => self.connect_page(ui),
            Tab::Keys => {
                let changed = editor::bindings_ui(
                    ui,
                    &mut self.config.schemes[profile_index],
                    &mut self.editor_state,
                    capture_enabled,
                );
                self.profile_changed_if(changed);
            }
            Tab::Wheels => {
                let changed = editor::wheels_ui(
                    ui,
                    &mut self.config.schemes[profile_index],
                    &mut self.editor_state,
                    capture_enabled,
                );
                self.profile_changed_if(changed);
            }
            Tab::Fps => {
                let changed = editor::fps_ui(
                    ui,
                    &mut self.config.schemes[profile_index],
                    &mut self.editor_state,
                    capture_enabled,
                );
                self.profile_changed_if(changed);
            }
            Tab::Test => self.test_page(ui),
        });
    }

    fn connect_page(&mut self, ui: &mut egui::Ui) {
        card(ui, "设备连接", |ui| {
            ui.horizontal(|ui| {
                if ui.button("刷新设备").clicked() {
                    self.refresh_devices();
                }
                egui::ComboBox::from_id_salt("device_page")
                    .selected_text(if self.selected.is_empty() {
                        "无设备"
                    } else {
                        &self.selected
                    })
                    .show_ui(ui, |ui| {
                        for device in &self.devices {
                            ui.selectable_value(&mut self.selected, device.clone(), device);
                        }
                    });
                if ui.button("能力检测").clicked() {
                    self.run_diagnostics();
                }
            });
            ui.label(RichText::new(&self.status).color(MUTED));
            if !self.diagnostics.is_empty() {
                ui.add_space(6.0);
                for line in &self.diagnostics {
                    ui.monospace(line);
                }
            }
        });

        card(ui, "映射运行时", |ui| {
            ui.horizontal(|ui| {
                if self.mapping.is_none() {
                    if ui
                        .add(
                            egui::Button::new("启动并开启映射")
                                .fill(ACCENT)
                                .corner_radius(8.0),
                        )
                        .clicked()
                    {
                        self.start_mapping();
                    }
                } else {
                    let runtime = self.mapping.as_ref().unwrap().runtime();
                    let enabled = runtime.lock().unwrap().is_enabled();
                    if ui
                        .button(if enabled {
                            "关闭映射"
                        } else {
                            "开启映射"
                        })
                        .clicked()
                    {
                        runtime.lock().unwrap().set_enabled(!enabled);
                        self.mapping.as_ref().unwrap().sync_state();
                    }
                    if ui.button("停止运行时").clicked() {
                        self.mapping = None;
                        self.mapping_status = "映射已停止".to_string();
                    }
                }
            });
            ui.label(RichText::new(&self.mapping_status).color(MUTED));
            ui.label("默认 F8 开关。FPS 模式拥有独立开关，不依赖普通映射总开关。");
        });

        card(ui, "配置组合", |ui| {
            ui.horizontal(|ui| {
                let current = self.active_profile().name.clone();
                egui::ComboBox::from_id_salt("scheme_select")
                    .selected_text(&current)
                    .show_ui(ui, |ui| {
                        for index in 0..self.config.schemes.len() {
                            let name = self.config.schemes[index].name.clone();
                            if ui
                                .selectable_label(self.config.active == index, name.clone())
                                .clicked()
                            {
                                self.config.active = index;
                                self.editor_state.cancel_capture();
                                self.sync_runtime_profile();
                            }
                        }
                    });
                if ui.button("新建组合").clicked() {
                    let mut copy = self.active_profile().clone();
                    copy.name = format!("组合 {}", self.config.schemes.len() + 1);
                    self.config.schemes.push(copy);
                    self.config.active = self.config.schemes.len() - 1;
                    self.editor_state.cancel_capture();
                    self.profile_changed();
                }
                if self.config.schemes.len() > 1 && ui.button("删除当前").clicked() {
                    let index = self.config.active;
                    self.config.schemes.remove(index);
                    self.config.active = index.min(self.config.schemes.len() - 1);
                    self.editor_state.cancel_capture();
                    self.profile_changed();
                }
            });
            let name = &mut self.active_profile_mut().name;
            if ui.text_edit_singleline(name).changed() {
                self.dirty = true;
            }
        });
    }

    fn test_page(&mut self, ui: &mut egui::Ui) {
        card(ui, "触摸注入", |ui| {
            egui::Grid::new("test_coords")
                .num_columns(4)
                .spacing([10.0, 8.0])
                .show(ui, |ui| {
                    ui.label("起点 X");
                    ui.add(egui::DragValue::new(&mut self.x).range(0..=10000));
                    ui.label("Y");
                    ui.add(egui::DragValue::new(&mut self.y).range(0..=10000));
                    ui.end_row();
                    ui.label("终点 X");
                    ui.add(egui::DragValue::new(&mut self.x2).range(0..=10000));
                    ui.label("Y");
                    ui.add(egui::DragValue::new(&mut self.y2).range(0..=10000));
                    ui.end_row();
                });
            ui.add(egui::Slider::new(&mut self.duration_ms, 30..=2000).text("时长 ms"));
            ui.horizontal(|ui| {
                if ui.button("点击").clicked() {
                    let result = input::tap(&self.hdc, &self.selected, self.x, self.y);
                    self.send(result);
                }
                if ui.button("滑动").clicked() {
                    let result = input::swipe(
                        &self.hdc,
                        &self.selected,
                        self.x,
                        self.y,
                        self.x2,
                        self.y2,
                        self.duration_ms,
                    );
                    self.send(result);
                }
                if ui.button("长按").clicked() {
                    let result =
                        input::hold(&self.hdc, &self.selected, self.x, self.y, self.duration_ms);
                    self.send(result);
                }
            });
        });
    }

    fn preview(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("屏幕预览");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add(
                        egui::Button::new("立即抓帧")
                            .fill(ACCENT)
                            .corner_radius(8.0),
                    )
                    .clicked()
                {
                    let ctx = ctx.clone();
                    self.capture_now(&ctx);
                }
            });
        });
        ui.checkbox(&mut self.auto_capture, "自动抓帧（约 2 FPS）");
        if let Some(hint) = self.editor_state.pick_hint() {
            ui.horizontal(|ui| {
                ui.label(RichText::new(hint).color(ACCENT));
                if ui.button("取消取点").clicked() {
                    self.editor_state.cancel_pick();
                }
            });
        }
        ui.separator();
        if let Some(texture) = self.screenshot.clone() {
            let available = ui.available_size();
            let image_size =
                egui::vec2(self.screenshot_size.0 as f32, self.screenshot_size.1 as f32);
            let scale = (available.x / image_size.x)
                .min(available.y / image_size.y)
                .min(1.0)
                .max(0.05);
            let desired = image_size * scale;
            let (rect, response) = ui.allocate_exact_size(desired, egui::Sense::click());
            egui::Image::new(&texture)
                .fit_to_exact_size(desired)
                .maintain_aspect_ratio(true)
                .paint_at(ui, rect);
            let response = if self.editor_state.is_picking() {
                response.on_hover_cursor(egui::CursorIcon::Crosshair)
            } else {
                response
            };
            if response.clicked()
                && let Some(pos) = response.interact_pointer_pos()
            {
                let x_rel = ((pos.x - rect.min.x) / rect.width()).clamp(0.0, 1.0);
                let y_rel = ((pos.y - rect.min.y) / rect.height()).clamp(0.0, 1.0);
                let index = self
                    .config
                    .active
                    .min(self.config.schemes.len().saturating_sub(1));
                if self.editor_state.apply_picked(
                    &mut self.config.schemes[index],
                    x_rel,
                    y_rel,
                    self.screenshot_size,
                ) {
                    self.profile_changed();
                    let px = (x_rel * self.screenshot_size.0.max(1) as f32).round() as i32;
                    let py = (y_rel * self.screenshot_size.1.max(1) as f32).round() as i32;
                    self.status = format!("取点完成：({px}, {py})");
                }
            }
        } else {
            Frame::default()
                .fill(Color32::from_rgb(239, 244, 250))
                .corner_radius(14.0)
                .inner_margin(Margin::same(24))
                .show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(80.0);
                        ui.label(RichText::new("尚未抓取屏幕").size(18.0).color(MUTED));
                        ui.label("连接设备后点击“立即抓帧”");
                    });
                });
        }
        ui.add_space(10.0);
        ui.label(
            RichText::new(format!(
                "{} × {}",
                self.screenshot_size.0, self.screenshot_size.1
            ))
            .color(MUTED),
        );
    }
}

impl eframe::App for HdcPadApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.config.normalize();
        let profile_index = self
            .config
            .active
            .min(self.config.schemes.len().saturating_sub(1));
        if editor::capture_next_key(
            &ctx,
            &mut self.editor_state,
            &mut self.config.schemes[profile_index],
        ) {
            self.profile_changed();
        }
        egui::Panel::top("top")
            .frame(
                Frame::default()
                    .fill(PANEL)
                    .inner_margin(Margin::symmetric(16, 10))
                    .stroke(Stroke::new(1.0, LINE)),
            )
            .show(ui, |ui| self.top_bar(ui));
        egui::Panel::bottom("status")
            .frame(
                Frame::default()
                    .fill(PANEL)
                    .inner_margin(Margin::symmetric(14, 7))
                    .stroke(Stroke::new(1.0, LINE)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&self.status).color(MUTED));
                    if self.dirty {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(RichText::new("配置尚未保存").color(WARN));
                        });
                    }
                });
            });
        egui::Panel::left("nav")
            .resizable(false)
            .default_size(190.0)
            .frame(
                Frame::default()
                    .fill(PANEL)
                    .inner_margin(Margin::symmetric(12, 16))
                    .stroke(Stroke::new(1.0, LINE)),
            )
            .show(ui, |ui| self.nav(ui));
        egui::Panel::right("preview")
            .resizable(true)
            .default_size(470.0)
            .min_size(330.0)
            .frame(
                Frame::default()
                    .fill(PANEL)
                    .inner_margin(Margin::same(16))
                    .stroke(Stroke::new(1.0, LINE)),
            )
            .show(ui, |ui| self.preview(&ctx, ui));
        egui::CentralPanel::default()
            .frame(Frame::default().fill(BG).inner_margin(Margin::same(18)))
            .show(ui, |ui| self.content(ui));

        if self.auto_capture
            && !self.selected.is_empty()
            && self.last_capture.elapsed() >= Duration::from_millis(500)
        {
            self.capture_now(&ctx);
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        let pointer_captured = self
            .mapping
            .as_ref()
            .map(|service| {
                service
                    .mouse_grab()
                    .load(std::sync::atomic::Ordering::Relaxed)
            })
            .unwrap_or(false);
        if pointer_captured != self.pointer_captured_prev {
            self.pointer_captured_prev = pointer_captured;
            scrcpy_pad_hdc::input_capture::set_cursor_visible_from_ui(!pointer_captured);
        }
        if pointer_captured {
            scrcpy_pad_hdc::input_capture::hide_cursor_shape_from_ui();
        }
    }
}

impl Drop for HdcPadApp {
    fn drop(&mut self) {
        scrcpy_pad_hdc::input_capture::set_cursor_visible_from_ui(true);
    }
}

fn card(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    Frame::default()
        .fill(PANEL)
        .stroke(Stroke::new(1.0, LINE))
        .corner_radius(14.0)
        .inner_margin(Margin::same(16))
        .outer_margin(Margin::symmetric(0, 7))
        .show(ui, |ui| {
            ui.label(RichText::new(title).size(17.0).strong().color(INK));
            ui.add_space(8.0);
            add(ui);
        });
}

fn pill(ui: &mut egui::Ui, text: &str, color: Color32) {
    Frame::default()
        .fill(color.gamma_multiply(0.12))
        .corner_radius(CornerRadius::same(20))
        .inner_margin(Margin::symmetric(10, 5))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(12.0).color(color));
        });
}

fn configure_style(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Light);
    let mut style = (*ctx.style_of(egui::Theme::Light)).clone();
    let mut visuals = egui::Visuals::light();
    visuals.panel_fill = BG;
    visuals.window_fill = PANEL;
    visuals.extreme_bg_color = Color32::from_rgb(236, 241, 247);
    visuals.selection.bg_fill = ACCENT.gamma_multiply(0.3);
    visuals.hyperlink_color = ACCENT;
    visuals.widgets.active.bg_fill = ACCENT.gamma_multiply(0.5);
    visuals.widgets.hovered.bg_fill = ACCENT.gamma_multiply(0.15);
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(238, 243, 249);
    visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(238, 243, 249);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, INK);
    let round = CornerRadius::same(8);
    visuals.widgets.noninteractive.corner_radius = round;
    visuals.widgets.inactive.corner_radius = round;
    visuals.widgets.hovered.corner_radius = round;
    visuals.widgets.active.corner_radius = round;
    style.visuals = visuals;
    style.spacing.item_spacing = egui::vec2(9.0, 8.0);
    style.spacing.button_padding = egui::vec2(12.0, 7.0);
    style.spacing.interact_size.y = 30.0;
    style.spacing.scroll.bar_width = 7.0;
    ctx.set_style_of(egui::Theme::Light, style);
}

fn load_icon() -> egui::IconData {
    if let Ok(image) = image::load_from_memory(APP_ICON_PNG) {
        let rgba = image.to_rgba8();
        let (width, height) = rgba.dimensions();
        egui::IconData {
            rgba: rgba.into_raw(),
            width,
            height,
        }
    } else {
        egui::IconData {
            rgba: vec![0, 0, 0, 255],
            width: 1,
            height: 1,
        }
    }
}
