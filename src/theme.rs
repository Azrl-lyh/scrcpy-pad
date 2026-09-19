//! 主题层:语义色、界面尺寸常量、配色/几何预设与背景图设置。
//!
//! 约定(为后续主题化/控件化布局打地基):
//!   1. 界面里不再直接写 `Color32::YELLOW` 这类硬编码颜色,
//!      一律取 [`Theme`] 的语义色(ok/warn/danger/...),换主题时一处生效;
//!   2. 常用尺寸(描边宽度、浮层字号、圆心半径等)集中在本模块的 [`size`];
//!   3. 主题设置存在配置里([`Look`]),因此"选用配置"会一并切换外观。

use egui::{Color32, FontFamily, FontId, TextStyle};
use serde::{Deserialize, Serialize};

/// 浮层/控件常用尺寸(默认档)
pub mod size {
    /// 键位圆圈描边宽度
    pub const KEY_STROKE: f32 = 2.0;
    /// 浮层标注文字字号
    pub const LABEL_FONT: f32 = 12.0;
    /// 浮层小字字号
    pub const SMALL_FONT: f32 = 11.0;
    /// 键位圆心文字字号
    pub const KEY_FONT: f32 = 12.0;
    /// 锚点示意圆半径
    pub const AIM_RING: f32 = 16.0;
    /// 锚点十字/虚线标注的臂长
    pub const AIM_ARM: f32 = 28.0;
    /// 轮盘示意圆环半径
    pub const WHEEL_RING: f32 = 18.0;
}

/// 语义色:按含义取用,不直接写死具体颜色
#[derive(Clone, Copy)]
pub struct Theme {
    /// 正常/已就绪/已连接
    pub ok: Color32,
    /// 提醒/等待用户操作/参数未设置
    pub warn: Color32,
    /// 错误/不可用
    pub danger: Color32,
    /// 次要说明文字
    pub muted: Color32,
    /// 强调色(链接、选中)
    pub accent: Color32,

    /// 点按键(圆圈描边 + 填充)
    pub key_tap: Color32,
    pub key_tap_fill: Color32,
    /// 长按键
    pub key_hold: Color32,
    pub key_hold_fill: Color32,
    /// 正在修改响应范围的键
    pub key_resize: Color32,
    pub key_resize_fill: Color32,
    /// 新增键位草稿
    pub draft: Color32,
    /// 滑动键轨迹
    pub swipe: Color32,
    /// 永久轮盘 / 临时轮盘
    pub wheel_perm: Color32,
    pub wheel_temp: Color32,
    /// FPS 瞄准锚点
    pub aim: Color32,
}

impl Theme {
    /// 深色(默认):沿用程序原有的配色,保证老用户观感不变
    pub fn dark() -> Self {
        Self {
            ok: Color32::LIGHT_GREEN,
            warn: Color32::from_rgb(255, 170, 0),
            danger: Color32::from_rgb(255, 120, 120),
            muted: Color32::GRAY,
            accent: Color32::from_rgb(120, 170, 255),
            key_tap: Color32::GREEN,
            key_tap_fill: Color32::from_rgba_unmultiplied(0, 200, 0, 60),
            key_hold: Color32::ORANGE,
            key_hold_fill: Color32::from_rgba_unmultiplied(255, 165, 0, 60),
            key_resize: Color32::YELLOW,
            key_resize_fill: Color32::from_rgba_unmultiplied(255, 230, 0, 70),
            draft: Color32::YELLOW,
            swipe: Color32::LIGHT_BLUE,
            wheel_perm: Color32::from_rgb(255, 90, 220),
            wheel_temp: Color32::from_rgb(0, 200, 255),
            aim: Color32::from_rgb(255, 96, 0),
        }
    }

    /// 浅色:白底面板,浮层语义色相应加深
    pub fn light() -> Self {
        Self {
            ok: Color32::from_rgb(0, 130, 60),
            warn: Color32::from_rgb(180, 105, 0),
            danger: Color32::from_rgb(190, 40, 40),
            muted: Color32::from_rgb(110, 110, 110),
            accent: Color32::from_rgb(30, 100, 200),
            key_tap: Color32::from_rgb(0, 140, 60),
            key_tap_fill: Color32::from_rgba_unmultiplied(0, 170, 70, 60),
            key_hold: Color32::from_rgb(200, 110, 0),
            key_hold_fill: Color32::from_rgba_unmultiplied(230, 140, 0, 70),
            key_resize: Color32::from_rgb(180, 150, 0),
            key_resize_fill: Color32::from_rgba_unmultiplied(220, 190, 0, 80),
            draft: Color32::from_rgb(150, 120, 0),
            swipe: Color32::from_rgb(0, 110, 200),
            wheel_perm: Color32::from_rgb(180, 30, 150),
            wheel_temp: Color32::from_rgb(0, 140, 180),
            aim: Color32::from_rgb(210, 70, 0),
        }
    }

    /// Nord
    pub fn nord() -> Self {
        Self {
            ok: Color32::from_rgb(163, 190, 140),
            warn: Color32::from_rgb(235, 203, 139),
            danger: Color32::from_rgb(191, 97, 106),
            muted: Color32::from_rgb(129, 161, 193),
            accent: Color32::from_rgb(136, 192, 208),
            key_tap: Color32::from_rgb(163, 190, 140),
            key_tap_fill: Color32::from_rgba_unmultiplied(163, 190, 140, 64),
            key_hold: Color32::from_rgb(208, 135, 112),
            key_hold_fill: Color32::from_rgba_unmultiplied(208, 135, 112, 64),
            key_resize: Color32::from_rgb(235, 203, 139),
            key_resize_fill: Color32::from_rgba_unmultiplied(235, 203, 139, 76),
            draft: Color32::from_rgb(235, 203, 139),
            swipe: Color32::from_rgb(136, 192, 208),
            wheel_perm: Color32::from_rgb(180, 142, 173),
            wheel_temp: Color32::from_rgb(143, 188, 187),
            aim: Color32::from_rgb(208, 135, 112),
        }
    }

    /// Catppuccin (Mocha)
    pub fn catppuccin() -> Self {
        Self {
            ok: Color32::from_rgb(166, 227, 161),
            warn: Color32::from_rgb(249, 226, 175),
            danger: Color32::from_rgb(243, 139, 168),
            muted: Color32::from_rgb(166, 173, 200),
            accent: Color32::from_rgb(137, 180, 250),
            key_tap: Color32::from_rgb(166, 227, 161),
            key_tap_fill: Color32::from_rgba_unmultiplied(166, 227, 161, 64),
            key_hold: Color32::from_rgb(250, 179, 135),
            key_hold_fill: Color32::from_rgba_unmultiplied(250, 179, 135, 64),
            key_resize: Color32::from_rgb(249, 226, 175),
            key_resize_fill: Color32::from_rgba_unmultiplied(249, 226, 175, 76),
            draft: Color32::from_rgb(249, 226, 175),
            swipe: Color32::from_rgb(137, 220, 235),
            wheel_perm: Color32::from_rgb(245, 194, 231),
            wheel_temp: Color32::from_rgb(137, 220, 235),
            aim: Color32::from_rgb(250, 179, 135),
        }
    }
}

/// 配色预设
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Preset {
    #[default]
    Dark,
    Light,
    Nord,
    Catppuccin,
}

impl Preset {
    pub fn label(self) -> &'static str {
        match self {
            Preset::Dark => "深色(默认)",
            Preset::Light => "浅色",
            Preset::Nord => "Nord",
            Preset::Catppuccin => "Catppuccin",
        }
    }

    pub fn theme(self) -> Theme {
        match self {
            Preset::Dark => Theme::dark(),
            Preset::Light => Theme::light(),
            Preset::Nord => Theme::nord(),
            Preset::Catppuccin => Theme::catppuccin(),
        }
    }

    pub fn is_dark(self) -> bool {
        !matches!(self, Preset::Light)
    }

    fn panel(self) -> Color32 {
        match self {
            Preset::Dark => Color32::from_rgb(27, 27, 27),
            Preset::Light => Color32::from_rgb(248, 248, 248),
            Preset::Nord => Color32::from_rgb(46, 52, 64),
            Preset::Catppuccin => Color32::from_rgb(30, 30, 46),
        }
    }

    fn window(self) -> Color32 {
        match self {
            Preset::Dark => Color32::from_rgb(32, 32, 32),
            Preset::Light => Color32::from_rgb(255, 255, 255),
            Preset::Nord => Color32::from_rgb(59, 66, 82),
            Preset::Catppuccin => Color32::from_rgb(24, 24, 37),
        }
    }
}

/// 控件密度:只保留两档,避免维护成本失控
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Density {
    Compact,
    #[default]
    Standard,
    Loose,
}

impl Density {
    pub fn label(self) -> &'static str {
        match self {
            Density::Compact => "紧凑",
            Density::Standard => "标准",
            Density::Loose => "宽松",
        }
    }

    /// (行高倍数, 控件间距倍数)
    fn factors(self) -> (f32, f32) {
        match self {
            Density::Compact => (0.88, 0.7),
            Density::Standard => (1.0, 1.0),
            Density::Loose => (1.12, 1.35),
        }
    }
}

/// 背景图填充方式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum BgFit {
    #[default]
    Cover,
    Contain,
    Tile,
}

impl BgFit {
    pub fn label(self) -> &'static str {
        match self {
            BgFit::Cover => "铺满(裁切)",
            BgFit::Contain => "完整显示",
            BgFit::Tile => "平铺",
        }
    }
}

/// 外观设置(随配置保存;缺省即保持原来的深色观感)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Look {
    #[serde(default)]
    pub preset: Preset,
    #[serde(default)]
    pub density: Density,
    /// 背景图路径(空=不启用)
    #[serde(default)]
    pub bg_path: String,
    #[serde(default)]
    pub bg_fit: BgFit,
    /// 背景图上的暗化遮罩不透明度(0=不遮,保证可读性的兜底)
    #[serde(default = "default_bg_dim")]
    pub bg_dim: u8,
    /// 面板不透明度(255=完全不透明;有背景图时才会透出)
    #[serde(default = "default_panel_alpha")]
    pub panel_alpha: u8,
}

fn default_bg_dim() -> u8 {
    // 压暗到 120/255:背景图清晰可见,同时保证文字可读
    120
}

fn default_panel_alpha() -> u8 {
    // 150/255:面板半透明,背景图能透出来(255=完全不透明,就看不到图了)
    150
}

impl Default for Look {
    fn default() -> Self {
        Self {
            preset: Preset::default(),
            density: Density::default(),
            bg_path: String::new(),
            bg_fit: BgFit::default(),
            bg_dim: default_bg_dim(),
            panel_alpha: default_panel_alpha(),
        }
    }
}

impl Look {
    pub fn theme(&self) -> Theme {
        self.preset.theme()
    }

    /// 是否配置了背景图
    pub fn has_bg(&self) -> bool {
        !self.bg_path.is_empty()
    }

    /// 清空背景图(保留其它外观设置)
    pub fn clear_bg(&mut self) {
        self.bg_path.clear();
    }
}

/// 把配色 + 几何应用到 egui 样式。
/// `bg_active` 为真时面板半透明,让背景图透出来(仍受 `panel_alpha` 控制)。
pub fn apply_style(style: &mut egui::Style, look: &Look, bg_active: bool) {
    let mut visuals = if look.preset.is_dark() {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    let theme = look.theme();
    let panel = look.preset.panel();
    let window = look.preset.window();

    visuals.panel_fill = if bg_active {
        with_alpha(panel, look.panel_alpha)
    } else {
        panel
    };
    visuals.window_fill = if bg_active {
        with_alpha(window, look.panel_alpha)
    } else {
        window
    };
    visuals.extreme_bg_color = if bg_active {
        with_alpha(panel, look.panel_alpha)
    } else {
        panel
    };
    visuals.selection.bg_fill = theme.accent.gamma_multiply(0.45);
    visuals.hyperlink_color = theme.accent;
    visuals.widgets.active.bg_fill = theme.accent.gamma_multiply(0.55);
    visuals.widgets.hovered.bg_fill = theme.accent.gamma_multiply(0.28);

    let (row, gap) = look.density.factors();
    style.visuals = visuals;
    style.spacing.item_spacing = egui::vec2(6.0 * gap, 4.0 * gap);
    style.spacing.button_padding = egui::vec2(6.0 * gap, 3.0 * row);
    style.spacing.interact_size.y = 20.0 * row;
    style.spacing.scroll.bar_width = 8.0 * gap;
    // 几何:圆角
    let round = if look.density == Density::Compact { 2.0 } else { 4.0 };
    let cr = egui::CornerRadius::from(round);
    style.visuals.widgets.noninteractive.corner_radius = cr;
    style.visuals.widgets.inactive.corner_radius = cr;
    style.visuals.widgets.hovered.corner_radius = cr;
    style.visuals.widgets.active.corner_radius = cr;
    style.visuals.window_corner_radius = egui::CornerRadius::from(round + 2.0);
    // 字号:按密度整体缩放
    let base = 14.0 * row;
    style.text_styles = [
        (
            TextStyle::Heading,
            FontId::new(base * 1.35, FontFamily::Proportional),
        ),
        (TextStyle::Body, FontId::new(base, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(base, FontFamily::Proportional)),
        (
            TextStyle::Small,
            FontId::new(base * 0.82, FontFamily::Proportional),
        ),
        (TextStyle::Monospace, FontId::new(base, FontFamily::Monospace)),
    ]
    .into();
}

/// 给颜色套一个不透明度
pub fn with_alpha(c: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

/// 浮层标注文字:白色(在任意截图上都可读)
pub fn outline_text() -> Color32 {
    Color32::WHITE
}

/// 浮层用的白色描边
pub fn outline_stroke(width: f32) -> egui::Stroke {
    egui::Stroke::new(width, Color32::WHITE)
}
