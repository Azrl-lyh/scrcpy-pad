//! 主题层:语义色、界面尺寸常量、配色/几何预设与背景图设置。
//!
//! 约定(为后续主题化/控件化布局打地基):
//!   1. 界面里不再直接写 `Color32::YELLOW` 这类硬编码颜色,
//!      一律取 [`Theme`] 的语义色(ok/warn/danger/...),换主题时一处生效;
//!   2. 常用尺寸(描边宽度、浮层字号、圆心半径等)集中在本模块的 [`size`];
//!   3. 主题设置存在配置里([`Look`]),因此"选用配置"会一并切换外观。

use egui::{Color32, FontFamily, FontId, TextStyle};
use serde::{Deserialize, Serialize};

// ============================ 界面风格(主题) ============================
//
// "风格"与"配色"是两个维度:
//   - 配色(Preset):深色/浅色/Nord/Catppuccin,四种配色在所有风格下都保留;
//   - 风格(UiStyle):整体布局与观感 —— 默认(现有界面)/可视化(虚拟键盘)。
//
// 风格改动牵动整体布局(按键布局、信息展示都会变),无法像配色那样每帧热应用,
// 因此切换风格需要**关闭再重开 UI**(见 app::request_style_restart / main.rs 的重启循环),
// 重启后从 look.json 读回上一次的风格 —— "用户上一次设的主题,下次启动还在"。

/// 界面风格(主题)。新增风格时:补枚举 + label + apply_style 里的分支 + 界面选择器。
///
/// 历史上还有第三种"鸿蒙"风格(仿 `harmonyos-pc` 的独立卡片式界面),2026-10-06
/// 按用户决策(D1-部分C)删除:可视化 ≈ 默认 + 显示键位,维护代价小得多,
/// 而鸿蒙是一整套独立布局(它才是 UI 分支爆炸的主要来源)。
/// **老配置兼容**:旧的 `style: 鸿蒙/Harmony` 会被下面的 alias 静默降级为默认风格,
/// 绝不因为删了一个枚举值让老用户的配置读不出来。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum UiStyle {
    /// 默认:现有界面布局(左配置+日志 / 右四标签页)
    #[default]
    #[serde(alias = "Harmony", alias = "鸿蒙")]
    Default,
    /// 可视化:虚拟键盘/鼠标取键 + 键位亮起的编辑界面
    Visual,
}

impl UiStyle {
    pub fn label(self) -> &'static str {
        match self {
            UiStyle::Default => "默认",
            UiStyle::Visual => "可视化",
        }
    }

    pub const ALL: [UiStyle; 2] = [UiStyle::Default, UiStyle::Visual];
}

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
    /// 仅 FPS 生效的键(截图浮层使用独立色,与普通点按/长按区分)
    pub key_fps: Color32,
    pub key_fps_fill: Color32,
    /// 宏触发键（与普通键位、FPS 键均使用不同语义色）
    pub key_macro: Color32,
    pub key_macro_fill: Color32,
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
    /// 临时轮盘启用键（与方向键区分，避免误认成同类键位）
    pub wheel_enable: Color32,
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
            key_fps: Color32::from_rgb(214, 104, 255),
            key_fps_fill: Color32::from_rgba_unmultiplied(180, 70, 255, 70),
            key_macro: Color32::from_rgb(90, 210, 190),
            key_macro_fill: Color32::from_rgba_unmultiplied(90, 210, 190, 70),
            key_resize: Color32::YELLOW,
            key_resize_fill: Color32::from_rgba_unmultiplied(255, 230, 0, 70),
            draft: Color32::YELLOW,
            swipe: Color32::LIGHT_BLUE,
            wheel_perm: Color32::from_rgb(255, 90, 220),
            wheel_temp: Color32::from_rgb(0, 200, 255),
            wheel_enable: Color32::from_rgb(255, 230, 90),
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
            key_fps: Color32::from_rgb(145, 45, 190),
            key_fps_fill: Color32::from_rgba_unmultiplied(145, 45, 190, 82),
            key_macro: Color32::from_rgb(0, 135, 120),
            key_macro_fill: Color32::from_rgba_unmultiplied(0, 135, 120, 82),
            key_resize: Color32::from_rgb(180, 150, 0),
            key_resize_fill: Color32::from_rgba_unmultiplied(220, 190, 0, 80),
            draft: Color32::from_rgb(150, 120, 0),
            swipe: Color32::from_rgb(0, 110, 200),
            wheel_perm: Color32::from_rgb(180, 30, 150),
            wheel_temp: Color32::from_rgb(0, 140, 180),
            wheel_enable: Color32::from_rgb(175, 125, 0),
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
            key_fps: Color32::from_rgb(191, 97, 210),
            key_fps_fill: Color32::from_rgba_unmultiplied(191, 97, 210, 72),
            key_macro: Color32::from_rgb(94, 129, 172),
            key_macro_fill: Color32::from_rgba_unmultiplied(94, 129, 172, 74),
            key_resize: Color32::from_rgb(235, 203, 139),
            key_resize_fill: Color32::from_rgba_unmultiplied(235, 203, 139, 76),
            draft: Color32::from_rgb(235, 203, 139),
            swipe: Color32::from_rgb(136, 192, 208),
            wheel_perm: Color32::from_rgb(180, 142, 173),
            wheel_temp: Color32::from_rgb(143, 188, 187),
            wheel_enable: Color32::from_rgb(235, 203, 139),
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
            key_fps: Color32::from_rgb(203, 166, 247),
            key_fps_fill: Color32::from_rgba_unmultiplied(203, 166, 247, 72),
            key_macro: Color32::from_rgb(148, 226, 213),
            key_macro_fill: Color32::from_rgba_unmultiplied(148, 226, 213, 72),
            key_resize: Color32::from_rgb(249, 226, 175),
            key_resize_fill: Color32::from_rgba_unmultiplied(249, 226, 175, 76),
            draft: Color32::from_rgb(249, 226, 175),
            swipe: Color32::from_rgb(137, 220, 235),
            wheel_perm: Color32::from_rgb(245, 194, 231),
            wheel_temp: Color32::from_rgb(137, 220, 235),
            wheel_enable: Color32::from_rgb(249, 226, 175),
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
    /// 界面风格(默认/可视化)。切换需重启 UI,见 [`UiStyle`]。
    /// 老配置没有该字段,缺省 [`UiStyle::Default`] —— 与旧版观感一致;
    /// 写着已删除的 `Harmony`/`鸿蒙` 也会落到 [`UiStyle::Default`](见那里的 alias)。
    #[serde(default)]
    pub style: UiStyle,
    #[serde(default)]
    pub preset: Preset,
    #[serde(default)]
    pub density: Density,
    /// 面板不透明度(255=完全不透明;有背景图时才会透出)
    ///
    /// 注:背景图本身(`bg_path`/`bg_fit`/`bg_dim`)**不在这个结构里**,它从 2026-10-26 起是
    /// **程序级**设置,存在 settings.json(见 `crate::settings::Settings`)。
    /// 原因与 `mouse_capture` 当初搬走时**完全同型**(P22 事故,见 settings.rs 那段注释):
    /// 放在这里会跟着「按键组合」走 —— 换一套配置(导入别人的 YAML)就整体被替换,
    /// 用户实测症状:「只有第一个组合继承默认设置的图片,导入后连图片都变成虚无缥缈的东西」。
    /// 换键位不该换图片:背景图是这台机器的界面偏好,不是键位方案的一部分。
    #[serde(default = "default_panel_alpha")]
    pub panel_alpha: u8,
    /// 键位浮层(键位/摇杆的响应范围圈与标注)的显示亮度档位,见 [`tone_color`]。
    /// 0 = 默认,与旧版观感逐一致;>0 变浅、<0 加深。老配置没有这个字段,缺省 0。
    /// 默认情况下它是"微调":实际档位 = 自动对比(按截图采样)+ 本值。
    #[serde(default)]
    pub overlay_tone: f32,
    /// 是否按截图**自动对比**(采样标注底下的明暗,自动决定用深色还是浅色)。
    /// 老配置没有这个字段,缺省 true —— 用户反馈"背景一亮就看不清键位",
    /// 自动对比正是为此;想回到"固定配色"可以关掉它。
    #[serde(default = "default_true")]
    pub overlay_auto: bool,
}

/// 鼠标接管的实现方式(用户 2026-10-13 指定:入口在「外观」,支持热切换)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum MouseCaptureMode {
    /// **默认**:钩子层隔膜 —— 与从前逐位一致,不改变任何既有体验。
    #[default]
    Shield,
    /// 窗口层遮板:一块透明置顶窗贴在投屏区域上,鼠标事件落在它身上,
    /// 底下的投屏窗口收不到;画上去的圈与字照常显示,其余像素完全透明。
    Mask,
}

impl MouseCaptureMode {
    pub fn label(self) -> &'static str {
        match self {
            MouseCaptureMode::Shield => "隔膜",
            MouseCaptureMode::Mask => "遮板",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            MouseCaptureMode::Shield => shield_hint(cfg!(windows)),
            MouseCaptureMode::Mask => mask_hint(crate::mask::available()),
        }
    }

    pub const ALL: [MouseCaptureMode; 2] = [MouseCaptureMode::Shield, MouseCaptureMode::Mask];
}

/// 隔膜一档的说明文案(按「本平台**能不能真吞鼠标消息**」分叉;用户 2026-10-26 第2/3条)。
///
/// 为什么平台判断做成**参数**而不是在文案里内联 cfg:这段字是**给用户看的承诺**,
/// 两个分支都必须能在同一台机器上被测到(§3.3 阳/阴对照) —— 在 Windows 上跑测试
/// 也能验证「Linux 那句『不支持』确实存在且没被改掉」。
pub fn shield_hint(supported: bool) -> &'static str {
    if supported {
        "在**钩子层**吞掉鼠标消息(默认,与从前行为一致)。\n\
         优点:不新增窗口、不依赖投屏窗口位置。\n\
         缺点:本界面在独占期间也点不了(键盘不受影响)。"
    } else {
        "本平台**不支持**隔膜:它靠 Windows 低级鼠标钩子实现,Linux 没有对应能力,\n\
         选它不会有任何效果 —— 鼠标事件照旧同时进本程序和投屏窗口。\n\
         这一档保留只为让 Windows 的配置能导入;详见帮助「Linux 上做不到的几项」。"
    }
}

/// 遮板一档的说明文案(同上:按「本平台支持遮板吗」分叉)。
pub fn mask_hint(mask_available: bool) -> &'static str {
    if mask_available {
        "在**窗口层**挡鼠标:一块透明置顶窗贴在投屏区域上,事件落在它身上,\n\
         底下的投屏窗口收不到 ⇒ 双份输入从根上消失;本界面照常可点。\n\
         它还会把键位/轮盘/锚点画在画面上(不透明的地方只有圈和字)。\n\
         注意:需要能定位到投屏窗口的位置;定位不到时会**自动退回隔膜**。"
    } else {
        "本平台**不支持**遮板:它要在别的进程的窗口上方开一块自己的窗口,\n\
         Linux(Wayland)不允许客户端定位或覆盖其他程序的 surface,\n\
         所以选它只会**自动退回隔膜**(界面上会红字提示)。\n\
         这一档保留只为让 Windows 的配置能导入;详见帮助「Linux 上做不到的几项」。"
    }
}
/// 「隔膜此刻该不该开」的**纯判定**(用户 2026-10-13;便于单测)。
///
/// 输入:
/// - `mode` = 外观页选的接管方式;
/// - `mask_available` = 本平台支持遮板吗;
/// - `mask_active` = 遮板此刻**真的在工作**吗(窗口已起 + 定位到了投屏窗口);
/// - `shield_wanted` = 隔膜那条路自己算出来的结果(见 `keymap::mouse_shield_on`)。
///
/// 规则(两条,都是"只能有一个生效"):
/// 1. 选了遮板**且**遮板真的在工作 ⇒ 隔膜必须关(`false`);
/// 2. 其余情况 ⇒ 隔膜按它自己的判据走。
///
/// 为什么"遮板没起来就退回隔膜"而不是"什么都不开":后者会让用户以为接管生效了,
/// 实际双份输入照旧 —— 退回隔膜至少行为与从前一致。
pub fn shield_effective(
    mode: MouseCaptureMode,
    mask_available: bool,
    mask_active: bool,
    shield_wanted: bool,
) -> bool {
    if mode == MouseCaptureMode::Mask && mask_available && mask_active {
        return false;
    }
    shield_wanted
}

fn default_panel_alpha() -> u8 {
    // 150/255:面板半透明,背景图能透出来(255=完全不透明,就看不到图了)
    150
}

/// serde 缺省:老配置里没有"自动对比"字段时按开启处理
fn default_true() -> bool {
    true
}

impl Default for Look {
    fn default() -> Self {
        Self {
            style: UiStyle::default(),
            preset: Preset::default(),
            density: Density::default(),
            panel_alpha: default_panel_alpha(),
            overlay_tone: 0.0,
            overlay_auto: true,
        }
    }
}

impl Look {
    pub fn theme(&self) -> Theme {
        self.preset.theme()
    }
}

/// 把配色 + 几何应用到 egui 样式。
/// `bg_active` 为真时面板半透明,让背景图透出来(仍受 `panel_alpha` 控制)。
pub fn apply_style(style: &mut egui::Style, look: &Look, bg_active: bool) {
    let mut visuals = if !look.preset.is_dark() {
        egui::Visuals::light()
    } else {
        egui::Visuals::dark()
    };
    let theme = look.theme();

    // ---- 界面风格:面板/弹窗底色与圆角 ----
    // 沿用四种配色各自的 panel/window;语义色(键位圈/成功/警告等)由 Look::theme 给出。
    // (历史上这里还有一条"鸿蒙"分支,用一套独立的浅色卡片配色,已按用户决策删除。)
    let round = if look.density == Density::Compact {
        2.0
    } else {
        4.0
    };
    let (panel, window) = (look.preset.panel(), look.preset.window());

    visuals.panel_fill = if bg_active {
        with_alpha(panel, look.panel_alpha)
    } else {
        panel
    };
    visuals.window_fill = if bg_active {
        with_alpha(window, popup_alpha(look.panel_alpha))
    } else {
        window
    };
    visuals.extreme_bg_color = if bg_active {
        with_alpha(panel, look.panel_alpha)
    } else {
        panel
    };
    let accent = theme.accent;
    visuals.selection.bg_fill = accent.gamma_multiply(0.45);
    visuals.hyperlink_color = accent;
    visuals.widgets.active.bg_fill = accent.gamma_multiply(0.55);
    visuals.widgets.hovered.bg_fill = accent.gamma_multiply(0.28);

    let (row, gap) = look.density.factors();
    style.visuals = visuals;
    style.spacing.item_spacing = egui::vec2(6.0 * gap, 4.0 * gap);
    style.spacing.button_padding = egui::vec2(6.0 * gap, 3.0 * row);
    style.spacing.interact_size.y = 20.0 * row;
    style.spacing.scroll.bar_width = 8.0 * gap;
    // 几何:控件圆角随密度
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
        (
            TextStyle::Button,
            FontId::new(base, FontFamily::Proportional),
        ),
        (
            TextStyle::Small,
            FontId::new(base * 0.82, FontFamily::Proportional),
        ),
        (
            TextStyle::Monospace,
            FontId::new(base, FontFamily::Monospace),
        ),
    ]
    .into();
}

/// 给颜色套一个不透明度
pub fn with_alpha(c: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

/// 弹窗背景的不透明度下限(0.8 × 255)。
///
/// 背景图开着时,弹窗(使用说明、scrcpy 参数助手…)原先和主面板用同一个
/// `panel_alpha`,于是文字压在花哨的游戏画面上很难看清。这里把弹窗的
/// **透明度**打折(见 [`popup_alpha`]),但最多只压到八成的实心程度 ——
/// 再实就完全遮盖背景、失去"半透明程序"的味道了。
const POPUP_MIN_ALPHA: u8 = 204;

/// 弹窗透明度相对程序背景"减少"的比例:0.30 = 减少百分之三十。
const POPUP_ALPHA_CUT: f32 = 0.30;

/// 由主面板的不透明度推出弹窗的不透明度(只增不减)。
///
/// 透明度(1-a)先按 [`POPUP_ALPHA_CUT`] 削减,再受 [`POPUP_MIN_ALPHA`] 封顶:
/// 例如主面板 a=190(透明度 65) -> 弹窗 a=209 -> 超过上限,取 204。
/// `a=0` 时结果仍是 [`POPUP_MIN_ALPHA`],但调用方只在背景图生效时才用它,
/// 那时 `panel_alpha` 本身不会小到那种程度。
pub fn popup_alpha(a: u8) -> u8 {
    let a = a as f32;
    let lifted = a + (255.0 - a) * POPUP_ALPHA_CUT;
    (lifted.round() as i32).clamp(0, POPUP_MIN_ALPHA as i32) as u8
}

// 说明:早期这里有 outline_text()/outline_stroke() 两个"恒为白色"的浮层常量。
// 引入"键位显示亮度"后,浮层颜色统一走 tone_color / tone_text / paint_label,
// 那两个常量已无调用点,故删除 —— 免得以后又有人取到"永远白色"的颜色,
// 让亮度档位在某一处失效。

// ============================ 浮层可读性 ============================
//
// 用户诉求:"背景可能很亮,看不清键位"(附了截图:圆圈与文字压在花哨的游戏画面上)。
//
// 只给一个"调亮度"的滑块解决不了这个问题 —— 画面上有亮块也有暗块,
// 同一个档位在亮块上太浅、在暗块上又太深。查了这类"叠加在任意画面上的标注"
// 的成熟做法,通用的是三招,本程序三招全用:
//
//   ① **描边/外套(halo / casing)**:先画一圈对比色的粗线,再画本色。
//      制图学给等高线、地名加 halo,正是为了让线条在任意底图上都看得见
//      (ICC 2013《Guidelines for Consistently Readable Topographic Vectors
//      and Labels with Toggling Backgrounds》);字幕行业同理 —— Netflix 的
//      Timed Text Style Guide 明确要求字幕带描边/阴影,或者加半透明底色块。
//      原理:人眼对**局部对比度**敏感,而不是对绝对亮度敏感 ——
//      标注与背景亮度接近时,加一圈反相描边就能把可读性拉回来。
//   ② **衬底(scrim)**:文字下面垫一块半透明色块,把对比度"局部锁定"住,
//      这是字幕与直播 HUD 最常用的一招。
//   ③ **自动对比**:按标注底下的实际明暗,决定这一块用深色还是浅色。
//      本程序恰好有手机截图,可以直接采样 —— 比一般 HUD 更有条件这么做。
//
// 手动的"键位显示亮度"滑块保留,但降级为**微调**:在自动结果上叠加偏移。
// 自动对比负责"看得清",滑块负责"顺眼"。

/// 亮度档位的取值范围(界面滑块也用这一对常量,避免两处各写一遍)
pub const TONE_MIN: f32 = -1.0;
pub const TONE_MAX: f32 = 1.0;

/// 端点处向白/黑混合的最大比例
const TONE_MIX: f32 = 0.6;

/// 填充透明度的缩放幅度(变浅 ×(1-0.35) / 加深 ×(1+0.35))
const TONE_ALPHA_SWING: f32 = 0.35;

/// 判定"这是浅色"的亮度阈值(0..255)
const LUMA_LIGHT: f32 = 140.0;

/// 把任意输入(含手工编辑的 json)收敛到合法档位
pub fn clamp_tone(t: f32) -> f32 {
    if t.is_finite() {
        t.clamp(TONE_MIN, TONE_MAX)
    } else {
        0.0
    }
}

/// 由「背景亮度(0..1)+ 手动微调」算出这一块该用的有效档位。
///
/// 背景越亮 -> 越往"加深"走(负);背景越暗 -> 越往"变浅"走(正);
/// 中间调 -> 接近 0(用本色)。系数 2.2 让"明显偏亮/偏暗"就能吃到接近端点的档位,
/// 保证自动对比真的起作用,而不是隔靴搔痒。
pub fn auto_tone(bg_luma: f32, manual: f32) -> f32 {
    let luma = if bg_luma.is_finite() {
        bg_luma.clamp(0.0, 1.0)
    } else {
        0.5
    };
    let auto = ((0.5 - luma) * 2.2).clamp(TONE_MIN, TONE_MAX);
    clamp_tone(auto + clamp_tone(manual))
}

/// 一个颜色的感知亮度(0..255)
pub fn luma(c: Color32) -> f32 {
    let [r, g, b, _] = c.to_srgba_unmultiplied();
    0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32
}

/// 按亮度档位调整一个浮层颜色:正值变浅(向白),负值加深(向黑),色相保持不变。
pub fn tone_color(c: Color32, tone: f32) -> Color32 {
    let t = clamp_tone(tone);
    if t == 0.0 {
        return c;
    }
    // 必须走**非预乘**通道:浮层填充色本身是半透明的,
    // 直接改预乘分量再按原 alpha 存回去,会得到一个更暗的颜色(反而不受控)。
    let [r, g, b, a] = c.to_srgba_unmultiplied();
    let k = t.abs() * TONE_MIX;
    let target: f32 = if t > 0.0 { 255.0 } else { 0.0 };
    let mix = |v: u8| {
        (v as f32 + (target - v as f32) * k)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    Color32::from_rgba_unmultiplied(mix(r), mix(g), mix(b), a)
}

/// 按亮度档位缩放填充透明度:变浅更透、加深更实(见本节开头说明)
pub fn tone_alpha(a: u8, tone: f32) -> u8 {
    let t = clamp_tone(tone);
    ((a as f32) * (1.0 - t * TONE_ALPHA_SWING)).clamp(0.0, 255.0) as u8
}

/// 浮层标注文字的颜色:加深档用近黑字、其余用纯白字。
/// 两者都配 [`paint_label`] 的描边与衬底,所以无论背景明暗都能读出来。
pub fn tone_text(tone: f32) -> Color32 {
    if clamp_tone(tone) < -0.02 {
        Color32::from_rgb(16, 16, 16)
    } else {
        Color32::WHITE
    }
}

/// 按亮度档位处理一个"描边 + 半透明填充"配色对 —— 键位圈、摇杆圈、滑动轨迹
/// 全都用它,保证同一次调整在整张浮层上表现一致。
///
/// 两个要点:
///   * `tone == 0` 时原样返回,一个字节都不改 —— "默认档与旧版观感逐一致"是明确的承诺;
///   * 填充必须走**非预乘**通道再按新 alpha 存回去。`Color32` 内部是预乘存储,
///     直接拿它去 `from_rgba_unmultiplied(..., 新alpha)` 会把颜色再预乘一次,
///     得到的结果比预期暗一大截(档位一拉就会莫名发灰)。
pub fn tone_ring_fill(ring: Color32, fill: Color32, tone: f32) -> (Color32, Color32) {
    let t = clamp_tone(tone);
    if t == 0.0 {
        return (ring, fill);
    }
    let [r, g, b, a] = fill.to_srgba_unmultiplied();
    let fill = Color32::from_rgba_unmultiplied(r, g, b, tone_alpha(a, t));
    (tone_color(ring, t), tone_color(fill, t))
}

/// 与描边色配对的"外套色":亮线配暗外套、暗线配亮外套。
/// 画法:先用它画一圈更粗的线,再把本色线画在上面 —— 于是线条在任意底图上都有
/// 一圈反相边界(制图学的 halo / 字幕的描边,见本节开头)。
pub fn casing(ink: Color32) -> Color32 {
    if luma(ink) > LUMA_LIGHT {
        Color32::from_black_alpha(200)
    } else {
        Color32::from_rgba_unmultiplied(255, 255, 255, 205)
    }
}

/// 文字衬底(scrim)的颜色:浅字配深底、深字配浅底
pub fn plate(ink: Color32) -> Color32 {
    if luma(ink) > LUMA_LIGHT {
        Color32::from_black_alpha(170)
    } else {
        Color32::from_rgba_unmultiplied(255, 255, 255, 185)
    }
}

/// 只画文字:先按四个方向各画一层**反相**描边,再画正文(不垫衬底)。
/// 用于已经有整块衬底的场合(例如摇杆信息卡)。
pub fn paint_text(
    painter: &egui::Painter,
    pos: egui::Pos2,
    align: egui::Align2,
    text: &str,
    font: egui::FontId,
    color: Color32,
) {
    let halo = casing(color);
    for (dx, dy) in [(-1.0, 0.0), (1.0, 0.0), (0.0, -1.0), (0.0, 1.0)] {
        painter.text(pos + egui::vec2(dx, dy), align, text, font.clone(), halo);
    }
    painter.text(pos, align, text, font, color);
}

/// 画一条带衬底的浮层标注(键位名、摇杆名、锚点名等)。
///
/// 三招齐上:①按文字亮度取反的半透明衬底把对比度局部锁定;
/// ②字缘一圈反相描边;③颜色本身已经过自动对比(调用方算好 tone)。
/// 尺寸用 `layout_no_wrap` 精确量出来,衬底只比文字大一点点,
/// 尽量少遮挡截图内容(用户明确要求"不影响截图内容的识别")。
pub fn paint_label(
    painter: &egui::Painter,
    pos: egui::Pos2,
    align: egui::Align2,
    text: &str,
    font: egui::FontId,
    color: Color32,
) {
    let main = painter.layout_no_wrap(text.to_owned(), font.clone(), color);
    let size = main.size();
    let rect = align.anchor_size(pos, size);
    painter.rect_filled(rect.expand(3.0), 3.0, plate(color));
    paint_text(
        painter,
        rect.center(),
        egui::Align2::CENTER_CENTER,
        text,
        font,
        color,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 老配置(没有 style 字段)读入后必须是默认风格 —— 观感与旧版逐一致
    #[test]
    fn look_style_defaults_to_default() {
        let look: Look = serde_json::from_str(r#"{ "preset": "Nord" }"#).unwrap();
        assert_eq!(look.style, UiStyle::Default);
        // 新字段能正常序列化/读回(主题记忆依赖它)
        let mut l2 = look.clone();
        l2.style = UiStyle::Visual;
        let text = serde_json::to_string(&l2).unwrap();
        let l3: Look = serde_json::from_str(&text).unwrap();
        assert_eq!(l3.style, UiStyle::Visual);
    }

    /// 老配置里的鸿蒙风格必须**静默降级为默认**(2026-10-06 删除该风格,W2-7):
    /// 老用户的 look.json / 配置 YAML 里可能还写着 `Harmony`,读不出来就等于
    /// "我的配置文件坏了" —— 绝不能因为删一个枚举值发生这种事。
    #[test]
    fn legacy_harmony_style_degrades_to_default() {
        for text in [
            r#"{ "style": "Harmony" }"#,
            r#"{ "style": "鸿蒙" }"#,
            r#"{ "style": "Default" }"#,
        ] {
            let look: Look = serde_json::from_str(text).unwrap_or_else(|e| {
                panic!("老配置 {text} 必须能读出来,却报错: {e}");
            });
            assert_eq!(look.style, UiStyle::Default, "{text} 应降级为默认风格");
        }
        // 落盘时写回的是新枚举名(下次启动不会再出现 Harmony)
        let text = serde_json::to_string(&Look::default()).unwrap();
        assert!(text.contains("\"Default\""), "应写回 Default: {text}");
        assert!(!text.contains("Harmony"));
    }

    /// 每一个风格都要能落盘/读回,且名字互不相同、不为空 ——
    /// "记住上次设的主题"全靠它;漏一个风格就会出现"换了主题,重启又变回去"。
    #[test]
    fn every_ui_style_round_trips_with_unique_label() {
        let mut labels = std::collections::HashSet::new();
        for s in UiStyle::ALL {
            let text = serde_json::to_string(&s).unwrap();
            let back: UiStyle = serde_json::from_str(&text).unwrap();
            assert_eq!(back, s, "风格 {s:?} 序列化后读回不一致");
            assert!(!s.label().is_empty());
            assert!(labels.insert(s.label()), "风格名重复: {}", s.label());
        }
        // 两种风格(默认/可视化)都要在册,新增风格时同步补上
        assert_eq!(UiStyle::ALL.len(), 2);
        assert_eq!(UiStyle::default(), UiStyle::Default);
        assert_eq!(
            UiStyle::ALL,
            [UiStyle::Default, UiStyle::Visual],
            "界面选择器按 ALL 的顺序展示"
        );
    }

    /// 默认风格的几何/面板底必须与旧版一致(4px 圆角 + 预设面板色),
    /// 避免"主题功能"波及不使用它的老用户。
    #[test]
    fn default_style_keeps_legacy_geometry() {
        let mut style = egui::Style::default();
        apply_style(&mut style, &Look::default(), false);
        assert_eq!(style.visuals.panel_fill, Preset::Dark.panel());
        assert_eq!(
            style.visuals.widgets.inactive.corner_radius,
            egui::CornerRadius::same(4)
        );
    }

    /// 可视化风格与默认风格共用同一套几何/底色(它变的是"画什么",不是"怎么配色")——
    /// 删掉鸿蒙之后,这条保证"剩下的两种风格不会各自长出一套配色分支"。
    #[test]
    fn visual_style_shares_default_geometry() {
        let mut a = egui::Style::default();
        let mut b = egui::Style::default();
        apply_style(&mut a, &Look::default(), false);
        apply_style(
            &mut b,
            &Look {
                style: UiStyle::Visual,
                ..Default::default()
            },
            false,
        );
        assert_eq!(a.visuals.panel_fill, b.visuals.panel_fill);
        assert_eq!(
            a.visuals.widgets.inactive.corner_radius,
            b.visuals.widgets.inactive.corner_radius
        );
    }

    /// 老配置(没有 overlay_tone 字段)读入后必须是 0.0 —— 观感与旧版逐一致。
    ///
    /// 同一个断言顺带守**删字段的容忍性**(AGENTS §5「配置字段删除」行):
    /// 2026-10-26 把 `bg_path`/`bg_fit`/`bg_dim` 从外观搬到 settings.json,
    /// 老 look.json 里这三个键还会留着 —— 必须被静默忽略,而不是让整份外观解析失败
    /// (那会把用户调好的配色/密度一起打回默认)。
    #[test]
    fn look_overlay_tone_defaults_to_neutral() {
        let look: Look = serde_json::from_str(
            r#"{ "preset": "Nord", "bg_dim": 135, "bg_path": "D:/pics/x.png", "bg_fit": "Tile" }"#,
        )
        .unwrap();
        assert_eq!(look.overlay_tone, 0.0);
        // 档位为 0 时颜色不做任何改动(连 alpha 也不动)
        let c = Color32::from_rgba_unmultiplied(0, 200, 0, 60);
        assert_eq!(tone_color(c, 0.0), c);
        assert_eq!(tone_alpha(60, 0.0), 60);
    }

    /// 变浅/加深都必须真的改变明度,且**保持色相可辨**(不能变成纯白/纯黑),
    /// 半透明填充的 alpha 也要跟着反向走。
    #[test]
    fn tone_changes_lightness_but_keeps_hue() {
        let green = Color32::from_rgb(0, 160, 0);
        let lighter = tone_color(green, TONE_MAX);
        let darker = tone_color(green, TONE_MIN);
        assert!(lighter.g() > green.g() && lighter.r() > green.r());
        assert!(darker.g() < green.g());
        // 端点不能变成纯白/纯黑:绿色分量仍是最大的那个
        assert!(lighter.g() > lighter.r() && lighter.g() > lighter.b());
        assert_eq!(darker.r(), 0);
        assert!(darker.g() > 0, "加深不能把颜色压成纯黑,否则分不清语义色");

        // 半透明填充:变浅更透、加深更实
        assert!(tone_alpha(60, TONE_MAX) < 60);
        assert!(tone_alpha(60, TONE_MIN) > 60);
        // 加深也不能把填充顶爆(u8 上限),即不会把截图彻底盖住
        assert!((tone_alpha(250, TONE_MIN) as u32) <= 255);

        // 非法输入回落到默认档
        assert_eq!(clamp_tone(f32::NAN), 0.0);
        assert_eq!(clamp_tone(99.0), TONE_MAX);
        assert_eq!(clamp_tone(-99.0), TONE_MIN);
    }

    /// 文字颜色:加深档用暗字、其余用白字(两者都靠 paint_label 的对比描边兜底)
    #[test]
    fn tone_text_switches_for_dark_tone() {
        assert_eq!(tone_text(0.0), Color32::WHITE);
        assert_eq!(tone_text(1.0), Color32::WHITE);
        let dark = tone_text(-1.0);
        assert!(dark.r() < 60 && dark.g() < 60 && dark.b() < 60);
    }

    /// 亮度档位为 0 时"描边 + 填充"必须**逐字节**等于旧版所用的那对颜色,
    /// 否则"默认档与旧版观感一致"这句承诺就不成立(用户明确要求不与既有表现打架)。
    #[test]
    fn tone_ring_fill_is_identity_at_zero() {
        let ring = Color32::from_rgb(0, 160, 0);
        let fill = Color32::from_rgba_unmultiplied(0, 200, 0, 60);
        assert_eq!(tone_ring_fill(ring, fill, 0.0), (ring, fill));

        // 非 0 档:描边变明/变暗,填充的**非预乘**透明度跟着反向走,
        // 且填充不能被"二次预乘"压成灰色 —— 色相(g 分量占优)必须保住
        let (r_light, f_light) = tone_ring_fill(ring, fill, TONE_MAX);
        let (r_dark, f_dark) = tone_ring_fill(ring, fill, TONE_MIN);
        assert!(r_light.g() > ring.g() && r_dark.g() < ring.g());
        assert!(f_light.a() < fill.a() && f_dark.a() > fill.a());
        let [fr, fg, fb, _] = f_dark.to_srgba_unmultiplied();
        assert!(
            fg > 60 && fg > fr && fg > fb,
            "加深档的填充仍应是可辨认的绿色,而不是被二次预乘压成灰: ({fr},{fg},{fb})"
        );
    }

    /// 自动对比:背景越亮越要加深、越暗越要变浅;中间调接近本色;
    /// 手动微调是在自动结果上**叠加**偏移(而不是取代它)。
    #[test]
    fn auto_tone_follows_background_luminance() {
        // 明亮背景(0.85)-> 明显加深
        let bright = auto_tone(0.85, 0.0);
        assert!(bright < -0.5, "亮背景应明显加深,实际 {bright}");
        // 昏暗背景(0.15)-> 明显变浅
        let dark = auto_tone(0.15, 0.0);
        assert!(dark > 0.5, "暗背景应明显变浅,实际 {dark}");
        // 中间调 -> 接近本色
        assert!(auto_tone(0.5, 0.0).abs() < 0.05);
        // 手动微调叠加在自动结果上,并收敛在合法区间
        assert!(auto_tone(0.85, 0.5) > bright);
        assert_eq!(auto_tone(0.0, 1.0), TONE_MAX);
        assert_eq!(auto_tone(1.0, -1.0), TONE_MIN);
        // 非法输入不 panic、按中性处理
        assert!(auto_tone(f32::NAN, 0.0).abs() < 0.05);
    }

    /// 描边外套与文字衬底都必须**与本体反相**:亮线配暗外套、暗字配浅衬底。
    /// 这是"halo / scrim"能起作用的前提 —— 同相就等于没加。
    #[test]
    fn casing_and_plate_are_opposite_to_ink() {
        let light_ink = Color32::WHITE;
        let dark_ink = Color32::from_rgb(16, 16, 16);
        // 亮本体 -> 暗外套 / 暗衬底
        assert!(luma(casing(light_ink)) < 80.0);
        assert!(luma(plate(light_ink)) < 80.0);
        // 暗本体 -> 亮外套 / 亮衬底
        assert!(luma(casing(dark_ink)) > 200.0);
        assert!(luma(plate(dark_ink)) > 200.0);
        // 衬底必须是半透明的(不能把截图糊死)
        assert!(plate(light_ink).a() < 255 && plate(dark_ink).a() < 255);
        assert!(casing(light_ink).a() < 255 && casing(dark_ink).a() < 255);
    }

    /// **鼠标接管二选一**(用户 2026-10-13):「遮板」与「隔膜」只能生效一个。
    ///
    /// 三条必须成立:
    /// ① 遮板真的在工作 ⇒ 隔膜必须关(否则鼠标被吞两次、界面也点不了);
    /// ② 遮板**没起来**(平台不支持 / 定位不到投屏窗口 / 起窗失败)⇒ 退回隔膜
    ///    —— 不能"什么都不开",那会让用户以为接管生效了而实际双份输入照旧;
    /// ③ 选了隔膜 ⇒ 永远按隔膜自己的判据走(默认行为逐位不变)。
    #[test]
    fn mouse_capture_modes_are_mutually_exclusive() {
        use MouseCaptureMode::{Mask, Shield};
        // ① 遮板在工作:隔膜压成 false(无论隔膜自己多想开)
        assert!(
            !shield_effective(Mask, true, true, true),
            "遮板生效时隔膜必须关"
        );
        assert!(!shield_effective(Mask, true, true, false));
        // ② 遮板没起来:退回隔膜(按隔膜自己的判据)
        assert!(
            shield_effective(Mask, true, false, true),
            "遮板未生效 ⇒ 退回隔膜"
        );
        assert!(
            !shield_effective(Mask, true, false, false),
            "隔膜本就不想开"
        );
        assert!(
            shield_effective(Mask, false, true, true),
            "平台不支持遮板 ⇒ 退回隔膜(哪怕 mask_active 谎报)"
        );
        // ③ 选了隔膜:永远按隔膜判据(不受遮板状态影响)
        assert!(shield_effective(Shield, true, true, true));
        assert!(!shield_effective(Shield, true, true, false));
        // ④ 默认值必须是隔膜(老配置没有该字段 ⇒ 与从前逐位一致)
        assert_eq!(
            MouseCaptureMode::default(),
            Shield,
            "缺省必须是隔膜,否则老用户的行为会变"
        );
        // ⑤ 两个选项都要有可读标签与说明(否则界面上一片空白)
        for m in MouseCaptureMode::ALL {
            assert!(!m.label().is_empty(), "{m:?} 必须有标签");
            assert!(!m.hint().is_empty(), "{m:?} 必须有悬停说明");
        }
        assert_eq!(MouseCaptureMode::ALL.len(), 2, "用户要求就两项:遮板 / 隔膜");
    }

    /// 守护:`hint()` 的**平台分叉**必须真的存在且如实(用户 2026-10-26 第2/3条)。
    ///
    /// 用户原话:「蒙版不能在linux上用。修复他。」「linux鼠标消隐只能在scrcpy-pad界面上生效」。
    /// 这两个能力在 Linux 上是**原理性做不到**的,界面若不说明,用户只会以为程序坏了。
    #[test]
    fn mouse_capture_hints_disclose_platform_gaps() {
        // 阳:支持时描述机制,且不得出现「不支持」这种误导
        assert!(super::shield_hint(true).contains("钩子层"));
        assert!(!super::shield_hint(true).contains("不支持"));
        assert!(super::mask_hint(true).contains("窗口层"));
        assert!(!super::mask_hint(true).contains("不支持"));
        // 阴:不支持时必须明说,并给出退路(不能只说「不支持」就完事)
        assert!(super::shield_hint(false).contains("不支持"));
        assert!(super::shield_hint(false).contains("Windows"));
        assert!(super::mask_hint(false).contains("不支持"));
        assert!(super::mask_hint(false).contains("自动退回隔膜"));
        // 接线:真实入口 `hint()` 必须与平台可用性一致(不是另一份写死的文案)
        assert_eq!(
            super::MouseCaptureMode::Shield.hint(),
            super::shield_hint(cfg!(windows)),
            "隔膜说明必须按平台分叉"
        );
        assert_eq!(
            super::MouseCaptureMode::Mask.hint(),
            super::mask_hint(crate::mask::available()),
            "遮板说明必须按遮板可用性分叉"
        );
    }
}
