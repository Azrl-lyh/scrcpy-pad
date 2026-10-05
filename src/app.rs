use crate::adb::{self, ControlServer};
use crate::capture::{Capture, CaptureEvent};
use crate::control::ControlClient;
use crate::engine::{Shared, SharedState};
use crate::keyboard;
use crate::keymap::{
    Action, ConfigFile, Easing, KeyBind, KeyCombo, MacroAction, MacroInstruction, MacroKeyMode,
    MacroStep, MacroWheelPart, Mapper, Profile, RecenterMode, Swipe, SwipePath, SwitchDirection,
    SwitchKey, TempMode, TempWheel, ViewInputMode, Wheel, WheelKind, WheelMode, key_name,
};
use crate::settings;
use crate::settings::{Settings, SettingsCache};
use crate::theme::{self, BgFit, Density, Preset, Theme};
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

const SCID: u32 = 0x1a2b3c4d;
const LOCAL_PORT: u16 = 28383;
const REPO_URL: &str = "https://github.com/Azrl-lyh/scrcpy-pad";
const AUTHOR: &str = "Azrl-lyh";

/// MIT 许可证全文:编译期嵌入二进制,关于页可直接查看
const LICENSE_TEXT: &str = include_str!("../LICENSE");

/// scrcpy 启动参数的初始(无预设)值
const BASE_SCRCPY_ARGS: &str = "--stay-awake";

/// 新增键位时的动作类型选项,顺序与 `Draft::kind` 一致。
/// 滑动 / 系统键仍在开发中,名字上直接标出来。
const KIND_NAMES: [&str; 4] = ["点按", "长按", "滑动（开发中）", "系统键（开发中）"];

/// 坐标编辑框的取值范围:**允许负值、允许超出屏幕**。
/// 键位本来就允许落在画面外(比如横屏的布局在竖屏下显示、截图尺寸与布局方向不同),
/// 这里不做越界"纠正",免得程序擅自改动用户调好的坐标。
const COORD_RANGE: std::ops::RangeInclusive<i32> = -8192..=8192;

/// 撤销栈深度(步数)。整份配置快照,50 步足以覆盖一次调参过程。
const UNDO_DEPTH: usize = 50;

// ============================ 界面风格切换的重启标志 ============================
//
// 风格(默认/鸿蒙/可视化)牵动整体布局,不能像配色那样每帧热应用
// (见 theme::UiStyle 的说明)。切换流程:
//   1. ui_look 里的风格选择器改动 -> request_style_restart():
//      把新风格写进 shared.profile.look 并立即落盘(look.json),
//      然后置本标志、关闭视口;
//   2. main.rs 的重启循环里 run_native 返回后调 take_style_restart(),
//      为真则用新风格重新打开窗口 —— "关一下 UI 再打开,确保完全切换完毕"。
// 风格存放在 look.json,重启后 PadApp::new 自动读回(L766 的 load_look_cache),
// 于是"用户上一次设的主题,下次启动还在"。

/// 风格切换请求:窗口关闭后 main.rs 检查它决定是否重开
static STYLE_RESTART: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 取出(并复位)风格切换请求。main.rs 在 run_native 返回后调用。
pub fn take_style_restart() -> bool {
    STYLE_RESTART.swap(false, Ordering::SeqCst)
}

/// 启动参数预设下拉选项
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartPreset {
    /// 无:回到初始默认(--stay-awake)
    None,
    /// 分辨率预设:先重置为默认,再叠加
    Uhd2k,
    Uhd4k,
    Fhd1080,
    Hd720,
    /// 音频类:直接追加/移除,不做重置
    NoAudio,
    WithAudio,
}

impl StartPreset {
    fn label(self) -> &'static str {
        match self {
            StartPreset::None => "无(--stay-awake)",
            StartPreset::Uhd2k => "2k 上限(最长边 ≤2560)",
            StartPreset::Uhd4k => "4k 上限(最长边 ≤3840)",
            StartPreset::Fhd1080 => "1k 上限(最长边 ≤1080)",
            StartPreset::Hd720 => "720p 上限(最长边 ≤720)",
            StartPreset::NoAudio => "不使用音频输出(--no-audio)",
            StartPreset::WithAudio => "指定使用音频输出(移除 --no-audio)",
        }
    }
}

/// 参数助手窗口打开期间的内容;每次打开按默认参数重建,关闭即丢弃临时修改
struct ArgHelp {
    entries: Vec<ArgEntry>,
    selected: usize,
    /// 正在被临时编辑的参数下标(双击进入,焦点移走不丢失,关窗丢弃)
    editing: Option<usize>,
}

/// 一条常用 scrcpy 参数说明
struct ArgEntry {
    /// 参数文本(可被临时编辑,仅本窗口会话内生效)
    flag: String,
    /// 参数中文名
    name: &'static str,
    /// 作用的中文解释
    desc: &'static str,
    /// 使用示例
    usage: &'static str,
}

impl ArgHelp {
    fn defaults() -> Self {
        let raw: &[(&str, &str, &str, &str)] = &[
            (
                "--max-size=1920",
                "画面清晰度上限",
                "限制镜像视频的最长边像素,另一条边按设备比例缩放。\n值越大越清晰、越耗带宽;低于设备原始分辨率还能明显降低延迟。\n默认 0(不限制,即设备原始分辨率)。",
                "--max-size=1920\n\n手机是 1080x2400 时加 --max-size=1080 就是 1k;\n模拟器/真机 4k 屏可用 --max-size=3840。",
            ),
            (
                "--max-fps=60",
                "帧率上限",
                "限制屏幕采集帧率。数值越低越省资源,但画面/操作更“肉”。\n部分游戏建议降到 30~60 以获得更稳的延迟。",
                "--max-fps=60",
            ),
            (
                "--video-bit-rate=16M",
                "视频码率",
                "编码码率,直接决定画面细节保留程度。\n网络/串流卡顿时可调低(如 4M),画面发糊时调高(如 20M)。\n默认 8M。",
                "--video-bit-rate=12M",
            ),
            (
                "--video-codec=h265",
                "视频编码器",
                "可选 h264 / h265 / av1 / vp8 / vp9。\n同码率下 h265 比 h264 更清晰(设备需支持);\nav1 画质最好但仅 Android 11+ 且更吃 CPU。默认 h264。",
                "--video-codec=h265",
            ),
            (
                "--no-audio",
                "关闭音频转发",
                "完全关闭音频转发(设备端照常出声)。\n打游戏不需要听声音时能省带宽、更稳。",
                "--no-audio",
            ),
            (
                "--audio-codec=aac",
                "音频编码器",
                "音频编码可选 opus / aac / flac / raw。\n默认 opus;兼容性问题时可换 aac。",
                "--audio-codec=aac",
            ),
            (
                "--turn-screen-off",
                "启动即息屏",
                "启动后立刻关闭设备屏幕(快捷键等效 -S)。\n串流打游戏时能省电并防止误触,画面不受影响。",
                "--turn-screen-off",
            ),
            (
                "-w",
                "保持唤醒",
                "连接期间设备不休眠(stay-awake)。本程序已默认加入,无需重复。",
                "-w 或 --stay-awake",
            ),
            (
                "-f",
                "全屏显示",
                "scrcpy 窗口以全屏模式打开。",
                "-f 或 --fullscreen",
            ),
            (
                "--always-on-top",
                "窗口置顶",
                "让 scrcpy 画面窗口始终显示在最上层,不被其它窗口遮挡。",
                "--always-on-top",
            ),
            (
                "-t",
                "显示触摸提示",
                "开启系统“显示触摸”提示(仅显示物理触摸,非本程序注入)。",
                "-t 或 --show-touches",
            ),
            (
                "--window-title=scrcpy-pad",
                "自定义窗口标题",
                "给 scrcpy 窗口设置自定义标题,便于区分多开实例。",
                "--window-title=打游戏",
            ),
            (
                "--window-width=480",
                "初始窗口宽度",
                "设置 scrcpy 窗口初始宽度(高度按比例自动)。\n值设为 0 表示自动。",
                "--window-width=480",
            ),
            (
                "--crop=1920:1080:0:0",
                "画面裁剪",
                "只显示设备屏幕的一部分(宽:高:x:y),\n适合隐藏通知栏/黑边,裁剪是在设备端完成的。",
                "--crop=2400:1080:0:1320",
            ),
            (
                "--orientation=90",
                "画面旋转",
                "把画面旋转 0/90/180/270 度;也可用 flip 前缀镜像。\n横屏游戏旋转 90 度后,坐标与截图会按旋转后画面算。",
                "--orientation=90",
            ),
        ];
        let entries = raw
            .iter()
            .map(|(flag, name, desc, usage)| ArgEntry {
                flag: flag.to_string(),
                name,
                desc,
                usage,
            })
            .collect();
        Self {
            entries,
            selected: 0,
            editing: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum KeySlot {
    NewBind,
    Bind(usize),
    WheelDir {
        wheel: usize,
        dir: usize,
    }, // dir: 0上 1下 2左 3右
    WheelEnable(usize), // 临时轮盘启用键
    Toggle,
    /// 全局鼠标消隐切换键
    CursorToggle,
    /// FPS 瞄准的门控鼠标键(如右键=开镜)
    AimHold,
    /// FPS 模式独立开关键
    AimToggle,
    /// 按住暂时退出 FPS 并显示鼠标
    AimSuspend,
    /// 切换键位(第 i 行:按下该键即切到它指向的那套组合)
    SwitchKey(usize),
    /// 切换键位的第二个组合键（最多两个）
    SwitchKeySecond(usize),
    /// 组合键中的第 `slot` 个物理键
    ComboKey {
        combo: usize,
        slot: usize,
    },
    /// 宏页：选择宏的触发键
    MacroTrigger,
    /// 宏页：设置宏中某个按键步骤的键码
    MacroInstructionKey(usize),
    /// 宏页：设置宏中某个组合键步骤的第 `slot` 个键
    MacroInstructionComboKey {
        instruction: usize,
        slot: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum CoordSlot {
    NewBind,
    Bind(usize),
    WheelCenter(usize),
    /// 已有滑动的起点
    SwipeStart(usize),
    /// 已有滑动的终点
    SwipeEnd(usize),
    /// 新增滑动的起点
    NewSwipeStart,
    /// 新增滑动的终点
    NewSwipeEnd,
    /// 圆形轨迹的出发点(取点后转成相对圆心的角度)
    CircleAngle(usize),
    /// 新增滑动圆形轨迹的出发点
    NewCircleAngle,
    /// FPS 瞄准锚点
    AimAnchor,
    /// 组合键的点按/长按落点
    ComboPoint(usize),
    /// 组合键滑动的起点/终点/圆形出发点
    ComboSwipeStart(usize),
    ComboSwipeEnd(usize),
    ComboCircleAngle(usize),
}

/// 正在"修改响应范围"的目标。键位的响应圈和轮盘的半径用的是同一套交互
/// (`Ctrl++` / `Ctrl+-` 缩放、在截图上拖动),所以合并成一个状态,
/// 天然保证同一时刻只有一个目标。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum RightTab {
    #[default]
    Keys,
    Macro,
    Wheels,
    Fps,
    Other,
}

impl RightTab {
    fn label(self) -> &'static str {
        match self {
            Self::Keys => "键位映射",
            Self::Macro => "宏",
            Self::Wheels => "摇杆映射",
            Self::Fps => "FPS 功能",
            Self::Other => "其他功能",
        }
    }
}

/// 可视化风格下左栏中部的三张标签页(键位组合 / 外观 / 诊断)。
/// 与右栏的 RightTab 用同一套"浏览器标签页"式的实现(见中央区标签的画法)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum LeftTab {
    /// 按键组合 / 切换键位(用户要求的默认页:启动程序时默认是设置键位)
    #[default]
    Schemes,
    /// 外观(配色/风格/背景图)
    Look,
    /// 诊断
    Diag,
}

impl LeftTab {
    fn label(self) -> &'static str {
        match self {
            Self::Schemes => "键位组合",
            Self::Look => "外观",
            Self::Diag => "诊断",
        }
    }
}

/// 鸿蒙风格的左侧导航页(照 harmonyos-pc 的 HDC UI:图标 + 文字,选中项淡蓝底)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum HPage {
    /// 连接与设备:设备、连接、启动 scrcpy、三件套路径
    #[default]
    Connect,
    /// 按键映射:配置 + 键位列表 + 组合键
    Keys,
    /// 宏
    Macro,
    /// 虚拟轮盘
    Wheels,
    /// FPS 模式(鼠标瞄准)
    Fps,
    /// 外观
    Look,
    /// 诊断(自检 + 日志)
    Diag,
}

impl HPage {
    /// (图标, 标题)。图标用字形代替图形资源,不额外引依赖;
    /// 只挑基础几何/常用符号(U+25A0-25CF 与 ⌨ 在 CJK 字体里都有,不会变方框)。
    fn icon_title(self) -> (&'static str, &'static str) {
        match self {
            Self::Connect => ("●", "连接与设备"),
            Self::Keys => ("⌨", "按键映射"),
            Self::Macro => ("▶", "宏"),
            Self::Wheels => ("◎", "虚拟轮盘"),
            Self::Fps => ("◇", "FPS 模式"),
            Self::Look => ("◆", "外观"),
            Self::Diag => ("■", "诊断"),
        }
    }

    const ALL: [HPage; 7] = [
        Self::Connect,
        Self::Keys,
        Self::Macro,
        Self::Wheels,
        Self::Fps,
        Self::Look,
        Self::Diag,
    ];
}

/// 可视化风格:虚拟键盘"显示哪几类已设置的键"的五个勾选。
/// 键位页与 FPS 页**显示能力完全一样**(共用同一份 `vk_lights` 逻辑),
/// 只是默认值不同 —— FPS 页默认只显示 FPS 独有的键位
/// (普通映射 / 组合键 / 摇杆默认不显示,需要时自己勾上)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VkFilters {
    binds: bool,
    macros: bool,
    combos: bool,
    wheels_perm: bool,
    wheels_temp: bool,
    fps: bool,
}

impl VkFilters {
    /// 两页默认只显示键位、组合键、永久/临时摇杆；宏、FPS 由用户主动打开。
    const KEYS_PAGE: Self = Self {
        binds: true,
        macros: false,
        combos: true,
        wheels_perm: true,
        wheels_temp: true,
        fps: false,
    };
    const FPS_PAGE: Self = Self {
        binds: true,
        macros: false,
        combos: true,
        wheels_perm: true,
        wheels_temp: true,
        fps: false,
    };

    /// 下拉多选：点击选项只切换对勾，不关闭下拉。
    fn ui(&mut self, ui: &mut egui::Ui, id: &str) {
        egui::ComboBox::from_id_salt(id)
            .selected_text("显示项目")
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show_ui(ui, |ui| {
                ui.checkbox(&mut self.binds, "按键映射");
                ui.checkbox(&mut self.combos, "组合键映射");
                ui.checkbox(&mut self.wheels_perm, "永久摇杆");
                ui.checkbox(&mut self.wheels_temp, "临时摇杆");
                ui.checkbox(&mut self.macros, "宏");
                ui.checkbox(&mut self.fps, "FPS");
            });
    }
}

/// 取一个颜色的更暗版本(虚拟键盘上做键帽侧壁/裙边用)
fn darken(c: egui::Color32, sub: u8) -> egui::Color32 {
    egui::Color32::from_rgb(
        c.r().saturating_sub(sub),
        c.g().saturating_sub(sub),
        c.b().saturating_sub(sub),
    )
}

/// 可视化风格下"虚拟键盘中被选中的目标"。
/// 点击虚拟键盘/鼠标上某个键后,这个键:
///   - 已被绑定 -> 选中它,下方操作栏显示对应编辑器(与右栏列表同一套控件);
///   - 空闲 -> 新建草稿并立刻进入截图取点(见 vk_on_key_clicked)。
/// 选中状态存这里,操作栏据此渲染;再点别的键或点[取消]即切换/退出。
#[derive(Debug, Clone, Copy, PartialEq)]
enum VkSel {
    /// 新增草稿(点击空闲键产生)
    New,
    /// 第 i 条键位绑定
    Bind(usize),
    /// 第 i 条宏绑定（与普通键位编辑区分，展开详细动作）
    Macro(usize),
    /// 总开关键 / FPS 三键 / 临时轮盘启用键等待重新分配(点击已绑定的特殊键)
    Toggle,
    CursorToggle,
    AimToggle,
    AimSuspend,
    AimHold,
    /// 摇杆方向键 / 临时轮盘启用键
    WheelDir {
        wheel: usize,
        dir: usize,
    },
    WheelEnable(usize),
    /// 组合键切换键
    SwitchKey(usize),
    /// 组合键切换键的第二个键
    SwitchKeySecond(usize),
    /// 组合键成员
    ComboKey {
        combo: usize,
        slot: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VkAddKind {
    Key,
    Combo,
    Wheel,
    Aim,
}

impl VkAddKind {
    fn label(self) -> &'static str {
        match self {
            Self::Key => "键位",
            Self::Combo => "组合键",
            Self::Wheel => "轮盘",
            Self::Aim => "准星/瞄准锚点",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum ResizeTarget {
    /// 键位的圆形响应范围
    Bind(usize),
    /// 轮盘的半径
    Wheel(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OverlayFilter {
    keys: bool,
    combos: bool,
    macros: bool,
    wheels_perm: bool,
    wheels_temp: bool,
    aim: bool,
}

impl Default for OverlayFilter {
    fn default() -> Self {
        Self {
            keys: true,
            combos: true,
            macros: false,
            wheels_perm: true,
            wheels_temp: true,
            aim: false,
        }
    }
}

impl OverlayFilter {
    fn ui(&mut self, ui: &mut egui::Ui) {
        egui::ComboBox::from_id_salt("overlay_filters")
            .selected_text("显示项目")
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show_ui(ui, |ui| {
                ui.checkbox(&mut self.keys, "按键映射");
                ui.checkbox(&mut self.combos, "组合键映射");
                ui.checkbox(&mut self.wheels_perm, "永久摇杆");
                ui.checkbox(&mut self.wheels_temp, "临时摇杆");
                ui.checkbox(&mut self.macros, "宏");
                ui.checkbox(&mut self.aim, "FPS 锚点");
            });
    }
}

/// 滑动曲线参数编辑的目标(已有键位或新增草稿)
#[derive(Debug, Clone, Copy, PartialEq)]
enum EasingEditTarget {
    Bind(usize),
    Combo(usize),
    New,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum DialogPurpose {
    ScrcpyExe,
    /// 选择 scrcpy 所在目录(发行包解压出来的那个文件夹)
    ScrcpyDir,
    ServerJar,
    AdbExe,
    SaveLog,
    SaveProfileAs,
    /// 选用已有键位 yaml 作为当前配置
    ChooseProfile,
    /// 新建键位 yaml(路径可不存在,选择后写入全新默认配置)
    NewProfile,
    /// 选择背景图片
    PickBackground,
}

#[derive(Clone, Default)]
struct RemoteDebugInfo {
    fps: Option<f32>,
    resolution: Option<(u32, u32)>,
    updated_at: Option<std::time::SystemTime>,
    error: Option<String>,
}

#[derive(Clone)]
struct DebugOverlayData {
    show_fps: bool,
    show_resolution: bool,
    show_aim: bool,
    fps: String,
    resolution: String,
    aim_motions: u64,
    aim_last_dx: f32,
    aim_last_dy: f32,
    updated: String,
    error: Option<String>,
}

struct MacroRecording {
    steps: Vec<MacroStep>,
    /// 已经按下但尚未抬起、已经作为步骤记录过的键。捕获层可能重复上报
    /// KEY_DOWN，这里用来合并自动重复，而不是把它变成连续点击。
    held: HashSet<u16>,
    last_event: Instant,
    /// 最近一条真正写入 steps 的时间；重复 KEY_DOWN 不推进它，因此长按
    /// 的持续时间会准确落在后续抬起步骤上。
    last_step_at: Instant,
    idle_ms: u32,
}

/// 扩展宏编辑窗口的临时状态；窗口关闭/取消时直接丢弃，不触碰原宏。
struct MacroVirtualEditor {
    profile: Profile,
    selected_key: Option<u16>,
    source_scheme: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MacroInstructionKind {
    Key,
    Combo,
    Wheel,
    Fps,
    Click,
    Swipe,
    Delay,
    Macro,
}

impl MacroInstructionKind {
    const ALL: [Self; 8] = [
        Self::Key,
        Self::Combo,
        Self::Wheel,
        Self::Fps,
        Self::Click,
        Self::Swipe,
        Self::Delay,
        Self::Macro,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Key => "按键",
            Self::Combo => "组合键",
            Self::Wheel => "轮盘",
            Self::Fps => "FPS",
            Self::Click => "点击",
            Self::Swipe => "滑动",
            Self::Delay => "间隔",
            Self::Macro => "嵌套宏",
        }
    }

    fn make(self) -> MacroInstruction {
        match self {
            Self::Key => MacroInstruction::Key {
                code: 0,
                mode: MacroKeyMode::Tap,
                duration_ms: crate::keymap::DEFAULT_TAP_DURATION_MS,
                delay_ms: 0,
            },
            Self::Combo => MacroInstruction::Combo {
                keys: vec![0, 0],
                duration_ms: crate::keymap::DEFAULT_TAP_DURATION_MS,
                delay_ms: 0,
            },
            Self::Wheel => MacroInstruction::Wheel {
                wheel: 0,
                part: MacroWheelPart::Up,
                duration_ms: crate::keymap::DEFAULT_TAP_DURATION_MS,
                delay_ms: 0,
            },
            Self::Fps => MacroInstruction::Fps {
                on: true,
                delay_ms: 0,
            },
            Self::Click => MacroInstruction::Click {
                x: 0.5,
                y: 0.5,
                duration_ms: crate::keymap::DEFAULT_TAP_DURATION_MS,
                delay_ms: 0,
            },
            Self::Swipe => MacroInstruction::Swipe {
                start_x: 0.5,
                start_y: 0.7,
                end_x: 0.5,
                end_y: 0.3,
                duration_ms: 300,
                delay_ms: 0,
            },
            Self::Delay => MacroInstruction::Delay { ms: 100 },
            Self::Macro => MacroInstruction::Macro {
                action: Box::new(MacroAction::default()),
                delay_ms: 0,
            },
        }
    }
}

struct DraftBind {
    key: Option<u16>,
    kind: usize, // 0点按 1长按 2滑动 3系统键
    /// 可视化风格的 FPS 页新建的键:只作为"仅 FPS"键位入配置(键位页则相反)
    fps_only: bool,
    x: i32,
    y: i32,
    /// 点按/长按的响应范围(像素;草稿是临时状态,入配置时才换算成相对值)
    radius: f32,
    /// 点按型新键的触点时长(ms);0=按住切换
    tap_duration_ms: u32,
    // 滑动
    swipe_start: (i32, i32),
    swipe_end: (i32, i32),
    swipe_duration_ms: u32,
    swipe_easing: Easing,
    swipe_path: SwipePath,
    keycode: u32,
}

impl Default for DraftBind {
    fn default() -> Self {
        Self {
            key: None,
            kind: 0,
            fps_only: false,
            // 草稿坐标是像素;新建草稿时会按当前屏幕尺寸重置到画面中央
            x: 540,
            y: 1200,
            // 35.2px 与相对默认值(0.0326 × 1080)对应,视觉一致
            radius: crate::keymap::DEFAULT_RADIUS * 1080.0,
            tap_duration_ms: crate::keymap::DEFAULT_TAP_DURATION_MS,
            swipe_start: (540, 1800),
            swipe_end: (540, 600),
            swipe_duration_ms: 300,
            swipe_easing: Easing::Linear,
            swipe_path: SwipePath::Line,
            keycode: 4,
        }
    }
}

/// 截图的"局部亮度网格":浮层自动对比的依据。
///
/// 把截图缩成至多 64×64 的亮度均值(每格覆盖几十像素),于是"这一块底下是亮是暗"
/// 一查就知道。内存可忽略,却能让每个键位/摇杆**按自己底下的画面**选深浅 ——
/// 这是本程序比一般 HUD 多出来的条件:画布底下就是手机截图本身。
#[derive(Default)]
struct LumaGrid {
    /// 网格宽高(格数)
    gw: usize,
    gh: usize,
    /// 每格的平均亮度(0..1)
    cells: Vec<f32>,
    /// 对应的截图尺寸(像素)
    w: u32,
    h: u32,
}

impl LumaGrid {
    /// 由截图建立亮度网格;尺寸非法时返回 None(此时浮层退回手动档位)
    fn new(img: &egui::ColorImage) -> Option<Self> {
        let (w, h) = (img.size[0], img.size[1]);
        if w == 0 || h == 0 || img.pixels.len() < w * h {
            return None;
        }
        // 网格最多 64×64:再细也没意义(浮层圈本身的直径就有几十像素),
        // 而 64×64 的采样在建立时也只是一次线性扫描。
        let gw = w.min(64).max(1);
        let gh = h.min(64).max(1);
        let mut sum = vec![0f32; gw * gh];
        let mut cnt = vec![0f32; gw * gh];
        for y in 0..h {
            let gy = y * gh / h;
            for x in 0..w {
                let gx = x * gw / w;
                let [r, g, b, _] = img.pixels[y * w + x].to_srgba_unmultiplied();
                let l = (0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32) / 255.0;
                let i = gy * gw + gx;
                sum[i] += l;
                cnt[i] += 1.0;
            }
        }
        let cells = sum
            .iter()
            .zip(&cnt)
            .map(|(s, c)| if *c > 0.0 { s / c } else { 0.5 })
            .collect();
        Some(Self {
            gw,
            gh,
            cells,
            w: w as u32,
            h: h as u32,
        })
    }

    /// 以截图像素 (x, y) 为中心的 3×3 格平均亮度。
    /// 取 3×3 而不是单格:单格可能整好压在一条亮边或一小块高光上,
    /// 平均一下更接近"这一块看上去的明暗"。
    fn around(&self, x: i32, y: i32) -> f32 {
        if self.cells.is_empty() || self.w == 0 || self.h == 0 {
            return 0.5;
        }
        let gx = ((x.max(0) as usize) * self.gw / self.w as usize).min(self.gw - 1);
        let gy = ((y.max(0) as usize) * self.gh / self.h as usize).min(self.gh - 1);
        let (mut s, mut n) = (0.0f32, 0.0f32);
        for dy in -1i32..=1 {
            for dx in -1i32..=1 {
                let cx = gx as i32 + dx;
                let cy = gy as i32 + dy;
                if cx < 0 || cy < 0 || cx >= self.gw as i32 || cy >= self.gh as i32 {
                    continue;
                }
                s += self.cells[cy as usize * self.gw + cx as usize];
                n += 1.0;
            }
        }
        if n > 0.0 { s / n } else { 0.5 }
    }
}

pub struct PadApp {
    shared: SharedState,
    grab_flag: Arc<std::sync::atomic::AtomicBool>,
    /// 鼠标抓取开关(FPS 瞄准期间冻结/隐藏系统光标)
    mouse_grab_flag: Arc<std::sync::atomic::AtomicBool>,
    /// 独立的全局鼠标消隐开关(不依赖 FPS，可由快捷键或“其他功能”按钮切换)
    cursor_hide_flag: Arc<std::sync::atomic::AtomicBool>,
    /// 上一帧的鼠标捕获状态,用于状态变化时记录日志
    mouse_captured_prev: bool,
    /// 上一帧的全局鼠标消隐状态，用于状态变化时记录日志
    cursor_hidden_prev: bool,
    /// 是否检测到鼠标设备(FPS 瞄准面板用于提示)
    mouse_found_flag: Arc<std::sync::atomic::AtomicBool>,
    /// 上一帧的映射开关状态:由关到开时重新确认当前屏幕方向(坐标空间)
    enabled_prev: bool,
    /// 后台查询"当前显示器尺寸"的结果(屏幕方向随时会变)
    space_rx: Option<Receiver<(u32, u32)>>,
    /// 背景图纹理缓存(路径, 纹理);仅在路径变化时重新解码
    bg_tex: Option<(String, egui::TextureHandle)>,
    /// 加载失败过的背景图路径(避免每帧重试并刷屏日志)
    bg_failed: Option<String>,
    /// 诊断面板里缓存的日志尾部(点[刷新预览]或首次展开时读一次,不每帧读盘)
    diag_preview: String,
    /// 诊断面板的"显示日志末尾"是否展开
    diag_preview_open: bool,
    /// 外观缓存(与键位配置同目录的 look.json)上次写入的内容;与当前外观不同才落盘
    look_saved: Option<theme::Look>,
    /// 外观缓存写入失败已提示过(只提示一次,避免刷屏)
    look_cache_warned: bool,
    /// 程序级设置(scrcpy 三件套路径等,settings.json)的延迟写盘缓存
    settings_cache: SettingsCache,
    /// 启动时从 settings.json 读到的**原始内容**(未清理)。
    /// 用途:关掉[记住路径]时写回文件的内容要以它为准 —— 只翻开关、
    /// 不丢已记住的路径文本(否则开关来回拨一次路径就没了)。
    settings_loaded: Settings,
    /// 是否记住 scrcpy 路径(界面开关,紧随 settings.json 落盘)
    remember_paths: bool,
    _capture: Option<Capture>,
    capture_err: Option<String>,
    gui_rx: Receiver<CaptureEvent>,

    devices: Vec<String>,
    selected: usize,
    scrcpy_args: String,
    /// scrcpy subprocess stdout/stderr/exit status, shown in the GUI log.
    scrcpy_status_rx: Option<Receiver<String>>,
    /// scrcpy 可执行文件路径(空 = 使用 PATH 中的 scrcpy)
    scrcpy_path: String,
    /// scrcpy 所在目录(空 = 未指定)。可直接指定目录,三件套由它推导补齐 ——
    /// 用户嘴里的"scrcpy 目录"就是发行包解压出来的那个文件夹。
    scrcpy_dir: String,
    server_path: String,
    /// adb 可执行文件路径(空 = 自动寻找:优先 scrcpy 同目录,再 PATH)
    adb_path: String,
    /// 已应用的 (scrcpy, server, adb) 三元组;用于文本改动后自动联动补齐
    suite_synced: (String, String, String, String),
    /// 由[测试]/[自动寻找]检测出的版本,只读显示
    server_version: String,
    test_msg: Option<(bool, String)>,
    /// 上次由用户手工改动路径时的 (scrcpy, server, adb) 值;
    /// 与当前值不同就说明"用户改过路径",此时即便选择器未变也要重新应用路径与刷新设备。
    /// (早期版本用 `suite_synced` 兼做这件事,导致"同一设备换路径要刷新两次"才生效)
    applied_suite: Option<(String, String, String)>,

    server: Option<ControlServer>,
    connect_rx: Option<Receiver<Result<(ControlServer, ControlClient), String>>>,

    waiting_key: Option<KeySlot>,
    picking: Option<CoordSlot>,
    /// 正在修改响应范围的目标(键位圈或轮盘半径);进入后目标显示为黄色
    resizing: Option<ResizeTarget>,
    /// 正在编辑滑动曲线参数的键位(弹窗)
    easing_edit: Option<EasingEditTarget>,
    /// 新增键位草稿是否进行中(决定预览圆圈/轨迹是否显示,并允许取消)
    draft_active: bool,
    shot: Option<(egui::TextureHandle, u32, u32)>,
    /// 截图局部亮度网格(浮层自动对比用);没截图时为 None
    shot_lum: Option<LumaGrid>,
    shot_rx: Option<Receiver<Result<egui::ColorImage, String>>>,
    /// Screenshot preview zoom multiplier (1.0 = fit preview).
    shot_zoom: f32,
    /// 是否仍使用按窗口尺寸计算的自动初始缩放；手动 +/- 后关闭，重置时恢复。
    shot_zoom_auto: bool,
    /// 音频唤醒(注入音量键)任务的回执;None = 没有进行中的唤醒
    audio_rx: Option<Receiver<String>>,
    overlay_filter: OverlayFilter,
    /// Right-hand content tab. Keys is selected by default.
    right_tab: RightTab,
    /// 可视化风格:左栏中部三标签页(键位组合/外观/诊断)的当前页
    left_tab: LeftTab,
    /// **界面风格(主题)**。界面级设置,不随"按键组合切换"改变 ——
    /// 组合切换会用 YAML 里的 look 整体替换 profile(老 YAML 没有 style 字段),
    /// 所以这里单独存一份,并每帧回写共享配置(见 `stamp_style`)。
    style: theme::UiStyle,
    /// 鸿蒙风格:左侧导航当前页(照 harmonyos-pc 的 HDC UI 重新排布)
    h_page: HPage,
    /// 可视化风格:虚拟键盘上方五个显示开关(键位页与 FPS 页各一套,
    /// 两页的**显示能力完全一样**,只是默认值不同:见 VkFilters)
    vk_filters: VkFilters,
    vk_fps_filters: VkFilters,
    /// 可视化风格:当前在键盘下方操作栏里编辑的目标(见 VkSel)
    vk_sel: Option<VkSel>,
    /// 宏录制器（开发中）。
    macro_recording: Option<MacroRecording>,
    /// 宏页：当前录制/待添加的宏步骤。
    macro_page_steps: Vec<MacroStep>,
    /// 宏页：设置宏的语义操作序列。
    macro_page_instructions: Vec<MacroInstruction>,
    /// 宏页：扩展宏的虚拟键位层；None = 普通宏。
    macro_page_virtual_profile: Option<Profile>,
    /// 扩展宏设置窗口的临时副本；取消/点 X 直接丢弃。
    macro_virtual_editor: Option<MacroVirtualEditor>,
    /// 设置宏“接下来添加什么”的下拉选择。
    macro_instruction_kind: MacroInstructionKind,
    /// 宏页：用户为下一个宏选择的触发键。
    macro_page_key: Option<u16>,
    /// 宏页：新宏是否仅 FPS 生效。
    macro_page_fps_only: bool,
    /// 宏页：空闲自动停止时间。
    macro_idle_ms: u32,
    /// 宏页：是否展开显示原始录制事件（默认只显示结果摘要）。
    macro_show_events: bool,
    /// 已录宏列表中展开的宏索引。
    macro_expanded: Option<usize>,
    /// 当前载入编辑区的是哪一条宏；可视化下列表按钮据此变成“取消编辑”。
    macro_loaded_index: Option<usize>,
    /// 新增键位/组合键/轮盘后，下一帧把当前滚动区滚到新增设置处。
    scroll_to_new: bool,
    /// 退出清理是否已经执行。eframe 的 on_exit、Drop、主循环兜底可能多路调用。
    shutdown_done: bool,
    /// 可视化风格：虚拟键盘当前用于新增哪一类目标。
    vk_add_kind: VkAddKind,
    /// 可视化风格：组合键新增时已经选中的物理键（最多两个）。
    vk_combo_pending: Vec<u16>,
    /// 可视化键位列表底部的“新增按键映射”行是否展开。
    vk_bottom_add_active: bool,
    debug_show_fps: bool,
    debug_show_resolution: bool,
    debug_show_aim: bool,
    log_height: f32,
    log_window_open: bool,
    debug_overlay_open: bool,
    debug_info: RemoteDebugInfo,
    debug_rx: Option<Receiver<Result<RemoteDebugInfo, String>>>,
    debug_last_query: Instant,
    /// 在画布上点开的摇杆(显示它的四个方向键);None = 未点开
    wheel_info: Option<usize>,

    draft: DraftBind,
    logs: VecDeque<String>,
    profile_path: PathBuf,
    grab_enabled: bool,

    about_open: bool,
    /// 使用说明窗口是否打开
    help_open: bool,
    /// 使用说明(markdown 渲染 + 章节索引)
    help: crate::help::HelpWindow,
    /// 许可证窗口是否打开
    license_open: bool,
    /// scrcpy 参数助手窗口状态(None=未打开;每次打开重建默认参数)
    args_helper: Option<ArgHelp>,
    dialog: Option<crate::filedialog::FileDialogHandle>,
    dialog_purpose: DialogPurpose,
    loginfo_rx: Option<Receiver<adb::DeviceInfo>>,
    pending_log: Option<String>,

    // 撤销/重做栈(键位配置快照)
    undo_stack: Vec<Profile>,
    redo_stack: Vec<Profile>,
    /// 帧末统一入栈的撤销快照(持锁修改处无法即时压栈)
    pending_undo: Option<Profile>,
    /// 本帧是否已由某个入口显式压过撤销栈(仅用于调试观察,不参与判定)
    undo_frame_marked: bool,
    /// 上次落盘/装载时生效的组合下标。引擎(切换键)自己换组合时界面看不到,
    /// 靠比对它来发现"换车了",于是把新的 `active` 写回文件(相当于记住上次用的那套)。
    scheme_saved: usize,
    /// 组合表/切换键本身被改过(新建、删除、改名、增删切换键位)。
    ///
    /// 为什么单独一个标记而不是"内容变了就写":拖动键位圆圈时**每帧**都在改
    /// 配置,若按内容比对落盘,一次拖动就是几百次写盘。这里只让"组合表结构"
    /// 这类改动自动落盘,键位细节仍由 [保存配置] / Ctrl+S 决定何时写。
    scheme_dirty: bool,
}

/// egui 默认字体不含 CJK,从系统加载中文字体作为回退
fn install_cjk_font(ctx: &egui::Context) {
    const CANDIDATES: &[&str] = &[
        "/usr/share/fonts/google-noto-sans-cjk-vf-fonts/NotoSansCJK-VF.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/wqy-zenhei/wqy-zenhei.ttc",
        "/usr/share/fonts/wqy-microhei/wqy-microhei.ttc",
        "C:\\Windows\\Fonts\\msyh.ttc",
    ];
    let mut fonts = egui::FontDefinitions::default();
    let mut loaded = None;
    for path in CANDIDATES {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert(
                "cjk".into(),
                std::sync::Arc::new(egui::FontData::from_owned(bytes)),
            );
            loaded = Some(*path);
            break;
        }
    }
    // 用户目录下的思源黑体(本机)
    if loaded.is_none() {
        if let Some(home) = std::env::var_os("HOME") {
            let p = PathBuf::from(home).join(".local/share/fonts/SourceHanSans.ttc");
            if let Ok(bytes) = std::fs::read(&p) {
                fonts.font_data.insert(
                    "cjk".into(),
                    std::sync::Arc::new(egui::FontData::from_owned(bytes)),
                );
                loaded = Some("~/.local/share/fonts/SourceHanSans.ttc");
            }
        }
    }
    if let Some(path) = loaded {
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .push("cjk".into());
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .push("cjk".into());
        ctx.set_fonts(fonts);
        // 字体加载情况默认不往控制台输出(release 里不留这行)。要做多语言适配
        // 调试时再把下面两行注释放开:它能直接告诉你当前命中的是哪个字体文件、
        // 或者根本没有可用字体(界面汉字会变方块)。
        // eprintln!("[font] 已加载中文字体: {path}");
        let _ = path;
    } else {
        eprintln!("[font] 未找到中文字体,界面汉字可能显示为方块");
    }
}

/// egui 收到的按键 -> evdev 键码(与全局捕获同一码空间)。
/// 仅供键位绑定等待期使用:主窗口聚焦时 egui 必报按键事件,
/// 不依赖 Windows 全局钩子(受 scrcpy 抢焦点影响不可靠)。
fn egui_key_code(k: egui::Key) -> Option<u16> {
    use egui::Key as K;
    Some(match k {
        K::A => 30,
        K::B => 48,
        K::C => 46,
        K::D => 32,
        K::E => 18,
        K::F => 33,
        K::G => 34,
        K::H => 35,
        K::I => 23,
        K::J => 36,
        K::K => 37,
        K::L => 38,
        K::M => 50,
        K::N => 49,
        K::O => 24,
        K::P => 25,
        K::Q => 16,
        K::R => 19,
        K::S => 31,
        K::T => 20,
        K::U => 22,
        K::V => 47,
        K::W => 17,
        K::X => 45,
        K::Y => 21,
        K::Z => 44,
        K::Num1 => 2,
        K::Num2 => 3,
        K::Num3 => 4,
        K::Num4 => 5,
        K::Num5 => 6,
        K::Num6 => 7,
        K::Num7 => 8,
        K::Num8 => 9,
        K::Num9 => 10,
        K::Num0 => 11,
        K::F1 => 59,
        K::F2 => 60,
        K::F3 => 61,
        K::F4 => 62,
        K::F5 => 63,
        K::F6 => 64,
        K::F7 => 65,
        K::F8 => 66,
        K::F9 => 67,
        K::F10 => 68,
        K::F11 => 87,
        K::F12 => 88,
        k if (K::F13 as u16..=K::F24 as u16).contains(&(k as u16)) => {
            183 + k as u16 - K::F13 as u16
        }
        K::Escape => 1,
        K::Tab => 15,
        K::Backspace => 14,
        K::Enter => 28,
        K::Space => 57,
        K::Insert => 110,
        K::Delete => 111,
        K::Home => 102,
        K::End => 107,
        K::PageUp => 104,
        K::PageDown => 109,
        K::ArrowUp => 103,
        K::ArrowDown => 108,
        K::ArrowLeft => 105,
        K::ArrowRight => 106,
        K::ShiftLeft => 42,
        K::ShiftRight => 54,
        K::ControlLeft => 29,
        K::ControlRight => 97,
        K::AltLeft => 56,
        K::AltRight => 100,
        K::SuperLeft => 125,
        K::SuperRight => 126,
        K::IntlBackslash => 86,
        K::BrowserBack => 158,
        // 符号键: 统一落到标准键盘的物理键位;只有在 physical_key 缺失时
        // 才会以逻辑键名走到这里(全局捕获始终按物理键位上报)
        K::Semicolon | K::Colon => 39,
        K::Quote => 40,
        K::Comma => 51,
        K::Minus => 12,
        K::Period => 52,
        K::Slash | K::Questionmark => 53,
        K::Backslash | K::Pipe => 43,
        K::Equals | K::Plus => 13,
        K::OpenBracket | K::OpenCurlyBracket => 26,
        K::CloseBracket | K::CloseCurlyBracket => 27,
        K::Backtick => 41,
        K::Exclamationmark => 2,
        _ => return None,
    })
}

impl PadApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_cjk_font(&cc.egui_ctx);
        let (cap_tx, cap_rx) = channel::<CaptureEvent>();
        let (gui_tx, gui_rx) = channel::<CaptureEvent>();
        // Repair a stale blank system cursor if a previous process was killed
        // while cursor capture was active.
        crate::capture::set_cursor_visible_from_ui(true);

        // 配置文件里装的是多套"按键组合",`active` 指向上次用的那一套;
        // 引擎始终只认 `Shared::profile`(= 生效中的那套),组合表放在它旁边。
        let mut doc = load_profile().unwrap_or_default();
        doc.normalize();
        let profile_path = profile_path();
        let mut profile = doc.active_profile().cloned().unwrap_or_default();
        // 外观(配色/密度/背景图)另有一份"程序自用"的缓存,与键位配置同目录。
        // 有了它,即使没点过[保存配置],重启后外观也保持上次调好的样子。
        if let Some(cached) = load_look_cache() {
            profile.look = cached;
        }
        // 生效中的那套要与刚装载的 profile 对齐(外观缓存可能刚覆盖过它)
        if let Some(slot) = doc.schemes.get_mut(doc.active) {
            *slot = profile.clone();
        }
        // 界面风格(主题)是**界面级**设置:启动时从外观缓存里取一次,
        // 之后由 PadApp.style 持有并每帧回写(切换按键组合不会把它带走)。
        let ui_style = profile.look.style;

        let shared: SharedState = Arc::new(std::sync::Mutex::new(Shared {
            profile,
            enabled: false,
            control: None,
            aim_live: Default::default(),
            live: Default::default(),
            notices: Vec::new(),
            space_recheck: false,
            toolbar_release: false,
            schemes: doc.schemes.clone(),
            switch_keys: doc.switch_keys.clone(),
            fast_switch_enabled: doc.fast_switch_enabled,
            active_scheme: doc.active,
        }));

        // 输入捕获层(evdev)
        let (capture, capture_err) = match Capture::start(cap_tx) {
            Ok(c) => (Some(c), None),
            Err(e) => (None, Some(format!("{e:#}"))),
        };
        let grab_flag = capture
            .as_ref()
            .map(|c| c.grab.clone())
            .unwrap_or_else(|| Arc::new(false.into()));
        let mouse_grab_flag = capture
            .as_ref()
            .map(|c| c.mouse_grab.clone())
            .unwrap_or_else(|| Arc::new(false.into()));
        let cursor_hide_flag = capture
            .as_ref()
            .map(|c| c.cursor_hide.clone())
            .unwrap_or_else(|| Arc::new(false.into()));
        let mouse_found_flag = capture
            .as_ref()
            .map(|c| c.mouse_found.clone())
            .unwrap_or_else(|| Arc::new(false.into()));

        // 映射引擎线程(鼠标捕获状态由引擎统一维护,避免与 UI 帧率不同步)
        {
            let shared = shared.clone();
            let mouse_grab = mouse_grab_flag.clone();
            let cursor_hide = cursor_hide_flag.clone();
            std::thread::spawn(move || {
                // 事件分发主线程:满载机器上必须优先于普通后台任务拿到 CPU。
                crate::priority::boost(crate::priority::Class::AboveNormal);
                crate::engine::run(shared, cap_rx, gui_tx, mouse_grab, cursor_hide)
            });
        }

        // 程序级设置(scrcpy 三件套路径等):与 profile.json / look.json 同目录的 settings.json。
        // 有了它,scrcpy 目录即使不在程序同级,重启后也仍然记得,不必每次重新寻找。
        let mut saved = settings::load().unwrap_or_default(); // 先留一份"文件里原本写了什么":关掉[记住路径]时写回的内容要以它为准,
        // 而且落盘判定也必须以磁盘上的真实内容为基准(否则"关掉开关"这一动作
        // 永远写不出去,下次启动又变回"记住")。
        let loaded_raw = saved.clone();
        // 日志级别:环境变量优先(临时排查不必改文件),否则用 settings.json 里的设置。
        // 放在这里是因为 diag 已经在 main() 最开始初始化好了,那时还没读到设置文件。
        if std::env::var(crate::diag::ENV_LEVEL).is_err() {
            crate::diag::set_level_from_str(&saved.log_level, "settings.json");
        }
        if !saved.remember_paths {
            // 用户关掉了"记住路径":不采用其中的路径,但仍保留启动参数一类的偏好
            saved.clear_paths();
        }
        let remembered = saved.clone();
        // 记住的路径可能早已被移动/删除:清掉死路径,但**保留它的位置线索**
        // (所在目录会记进 scrcpy_dir),启动时能在那一带重新找回 scrcpy
        if saved.sanitize() && saved.remember_paths {
            eprintln!(
                "[settings] 已修正 settings.json 里的路径(死路径转为目录线索): {}",
                settings::path().display()
            );
        }
        let (saved_args, legacy_mouse_args_removed) =
            sanitize_saved_scrcpy_args(&saved.scrcpy_args);
        let settings_cache = SettingsCache::new(loaded_raw.clone());

        // 自动寻找 scrcpy 与 server:先用上次记住的路径/目录,再自动寻找
        let (scrcpy_path, server_path, version, found_msg) = {
            let (remembered_exe, note) = locate_remembered_scrcpy(&saved);
            let from_memory = remembered_exe.is_some();
            let exe = match remembered_exe {
                Some(p) => Some(p),
                None => adb::find_scrcpy(),
            };
            // server 找不到就留空:后续用户选定 scrcpy 位置后,由 sync_suite 按其同目录
            // 自动补齐(官方发行包三者同目录),避免预先填入的相对路径阻塞自动发现
            let server = if !saved.server_path.trim().is_empty() {
                saved.server_path.trim().to_string()
            } else {
                adb::find_server(exe.as_deref())
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            };
            match exe {
                Some(p) => {
                    let ps = p.display().to_string();
                    let v = adb::scrcpy_version_at(&ps).unwrap_or_default();
                    let msg = if !note.is_empty() {
                        format!("{note}(记忆来自 {})", settings::path().display())
                    } else if from_memory {
                        format!(
                            "已使用上次记住的 scrcpy: {ps}(来自 {})",
                            settings::path().display()
                        )
                    } else {
                        format!("已自动找到 scrcpy: {ps}")
                    };
                    (ps, server, v, msg)
                }
                None => (
                    String::new(),
                    server,
                    String::new(),
                    "未找到 scrcpy,请在左栏指定 scrcpy.exe 或它所在的目录(指定一次后会被记住)"
                        .to_string(),
                ),
            }
        };
        // 目录栏的初值:记住的目录 > 由记住的 exe 推导出的目录(都没有就留空)
        let scrcpy_dir_init = if !saved.scrcpy_dir.trim().is_empty() {
            saved.scrcpy_dir.trim().to_string()
        } else {
            PathBuf::from(scrcpy_path.trim())
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        };

        // 启动时定位 adb:上次记住的手动路径 > scrcpy 同目录(官方 Windows 发行包含同目录
        // adb.exe)> PATH;拿到后才列设备,否则 Windows 上 scrcpy 正常但设备列表却为空
        let startup_adb = {
            let manual = adb::find_adb_explicit(saved.adb_path.trim());
            match manual {
                Some(p) => Some(p),
                None => {
                    let exe = if scrcpy_path.trim().is_empty() {
                        None
                    } else {
                        Some(PathBuf::from(scrcpy_path.trim()))
                    };
                    adb::find_adb(exe.as_deref())
                }
            }
        };
        if let Some(a) = &startup_adb {
            adb::set_adb_bin(Some(a));
        }
        let adb_init = startup_adb
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let devices = adb::list_devices();

        let mut app = Self {
            shared,
            grab_flag,
            mouse_grab_flag,
            mouse_captured_prev: false,
            cursor_hide_flag,
            cursor_hidden_prev: false,
            mouse_found_flag,
            enabled_prev: false,
            space_rx: None,
            bg_tex: None,
            bg_failed: None,
            diag_preview: String::new(),
            diag_preview_open: false,
            look_saved: None,
            look_cache_warned: false,
            settings_cache,
            settings_loaded: loaded_raw.clone(),
            remember_paths: remembered.remember_paths,
            _capture: capture,
            capture_err,
            gui_rx,
            devices,
            selected: 0,
            scrcpy_args: if saved_args.trim().is_empty() {
                BASE_SCRCPY_ARGS.into()
            } else {
                saved_args.clone()
            },
            scrcpy_status_rx: None,
            scrcpy_path,
            scrcpy_dir: scrcpy_dir_init,
            server_path,
            adb_path: adb_init,
            // 置空使其在首帧自动做一次全套联动补齐
            suite_synced: (String::new(), String::new(), String::new(), String::new()),
            applied_suite: None,
            server_version: version,
            test_msg: None,
            server: None,
            connect_rx: None,
            waiting_key: None,
            picking: None,
            resizing: None,
            easing_edit: None,
            draft_active: false,
            shot: None,
            shot_lum: None,
            shot_rx: None,
            shot_zoom: 1.0,
            shot_zoom_auto: true,
            audio_rx: None,
            overlay_filter: OverlayFilter::default(),
            right_tab: RightTab::Keys,
            left_tab: LeftTab::default(),
            style: ui_style,
            h_page: HPage::default(),
            vk_filters: VkFilters::KEYS_PAGE,
            vk_fps_filters: VkFilters::FPS_PAGE,
            vk_sel: None,
            macro_recording: None,
            macro_page_steps: Vec::new(),
            macro_page_instructions: Vec::new(),
            macro_page_virtual_profile: None,
            macro_virtual_editor: None,
            macro_instruction_kind: MacroInstructionKind::Key,
            macro_page_key: None,
            macro_page_fps_only: false,
            macro_idle_ms: 800,
            macro_show_events: false,
            macro_expanded: None,
            macro_loaded_index: None,
            scroll_to_new: false,
            shutdown_done: false,
            vk_add_kind: VkAddKind::Key,
            vk_combo_pending: Vec::new(),
            vk_bottom_add_active: false,
            debug_show_fps: false,
            debug_show_resolution: false,
            debug_show_aim: false,
            log_height: 170.0,
            log_window_open: false,
            debug_overlay_open: false,
            debug_info: RemoteDebugInfo::default(),
            debug_rx: None,
            debug_last_query: Instant::now(),
            wheel_info: None,
            draft: DraftBind::default(),
            logs: VecDeque::new(),
            profile_path,
            grab_enabled: false,
            about_open: false,
            help_open: false,
            help: crate::help::HelpWindow::new(),
            license_open: false,
            args_helper: None,
            dialog: None,
            dialog_purpose: DialogPurpose::ScrcpyExe,
            loginfo_rx: None,
            pending_log: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            pending_undo: None,
            undo_frame_marked: false,
            scheme_saved: doc.active,
            scheme_dirty: false,
        };
        app.log("就绪。顺序: 连接手机 -> [连接控制] -> [启动 scrcpy] -> 按总开关键开启映射");
        app.log(found_msg);
        if legacy_mouse_args_removed {
            app.log("已清除旧配置中的 --mouse=uhid/aoa：该模式会让 scrcpy 捕获鼠标并干扰键盘，已改为默认 SDK 鼠标模式");
        }
        if remembered.remember_paths && remembered.has_any_path() {
            app.log(format!(
                "已读取记住的路径({}),改动会自动保存;不想记住可在左栏取消勾选",
                settings::path().display()
            ));
        }
        match &startup_adb {
            Some(p) => app.log(format!("adb: {}", p.display())),
            None => app
                .log("未找到 adb(设备列表将为空): 请将 adb.exe 所在目录加入 PATH,或在左栏手动指定"),
        }
        // 记住的设备:若仍在线,直接选中它,省得每次重连都要在下拉里挑
        if !remembered.selected_serial.trim().is_empty() {
            if let Some(i) = app
                .devices
                .iter()
                .position(|d| d == remembered.selected_serial.trim())
            {
                app.selected = i;
                app.log(format!(
                    "已选中上次使用的设备: {}",
                    remembered.selected_serial
                ));
            }
        }
        let _ = cc;
        app
    }

    fn log(&mut self, s: impl Into<String>) {
        self.logs.push_back(s.into());
        while self.logs.len() > 200 {
            self.logs.pop_front();
        }
    }

    fn serial(&self) -> String {
        self.devices.get(self.selected).cloned().unwrap_or_default()
    }

    /// 测试 scrcpy:能否运行 + server 是否存在,并更新只读版本显示
    fn test_scrcpy(&mut self) {
        let ver = adb::scrcpy_version_at(&self.scrcpy_path);
        let srv_ok = PathBuf::from(&self.server_path).is_file();
        match ver {
            Some(v) if srv_ok => {
                self.server_version = v.clone();
                self.test_msg = Some((true, format!("scrcpy {v} · server 就绪")));
                self.log(format!(
                    "scrcpy 测试通过: 版本 {v},server: {}",
                    self.server_path
                ));
            }
            Some(v) => {
                self.server_version = v.clone();
                self.test_msg = Some((
                    false,
                    format!("scrcpy {v} 可用,但 scrcpy-server 不存在,请指定"),
                ));
                self.log("scrcpy 测试: server 文件缺失");
            }
            None => {
                self.test_msg = Some((false, "无法运行 scrcpy,请检查程序路径".into()));
                self.log("scrcpy 测试失败: 无法执行(检查路径)");
            }
        }
    }

    fn connect_control(&mut self) {
        // 连接前先做一次联动,确保 adb 已定位(push/forward 都依赖它)
        self.resync();
        // **先彻底收掉旧的 server**,再启新的。
        //
        // 为什么顺序重要:旧 server 的 adb 子进程还占着本地 forward 端口,
        // 设备端也还占着 `scrcpy_xxxx` 这个 abstract socket 名(我们的 scid
        // 是固定值)。不先关就启新的,两边抢同一个端口与 socket 名,
        // 表现是"重连之后按键时灵时不灵、且没有任何报错"。
        // ControlServer::drop 会 kill adb 子进程并 `adb forward --remove`。
        if let Some(old) = self.server.take() {
            self.shared.lock().unwrap().control = None;
            drop(old);
            self.log("已关闭上一个 control server(重连前先腾干净端口与 socket)");
        }
        let serial = self.serial();
        if serial.is_empty() {
            self.log("错误: 未选择设备");
            return;
        }
        let server_path = self.server_path.clone();
        let version = if self.server_version.is_empty() {
            adb::scrcpy_version_at(&self.scrcpy_path).unwrap_or_else(|| "4.1".into())
        } else {
            self.server_version.clone()
        };
        let (tx, rx) = channel();
        self.connect_rx = Some(rx);
        self.log("正在启动 control-only scrcpy-server...");
        std::thread::spawn(move || {
            let r = (|| -> Result<(ControlServer, ControlClient), String> {
                let (w, h) = adb::display_size(&serial).map_err(|e| format!("{e:#}"))?;
                let server =
                    adb::start_control_server(&serial, &server_path, &version, SCID, LOCAL_PORT)
                        .map_err(|e| format!("{e:#}"))?;
                // 等待 server listen,重试连接
                let mut last_err = String::new();
                for _ in 0..20 {
                    match ControlClient::connect(LOCAL_PORT, w, h) {
                        Ok(c) => return Ok((server, c)),
                        Err(e) => {
                            last_err = format!("{e:#}");
                            std::thread::sleep(Duration::from_millis(150));
                        }
                    }
                }
                Err(format!("连接控制通道超时: {last_err}"))
            })();
            let _ = tx.send(r);
        });
    }

    fn disconnect(&mut self) {
        self.shared.lock().unwrap().control = None;
        self.server = None;
        self.log("已断开控制通道");
    }

    /// 当前是否有可取消的后台任务或交互操作。
    fn pending_task_label(&self) -> Option<&'static str> {
        if self.connect_rx.is_some() {
            Some("连接")
        } else if self.shot_rx.is_some() {
            Some("截图")
        } else if self.debug_rx.is_some() {
            Some("调试刷新")
        } else if self.space_rx.is_some() {
            Some("坐标刷新")
        } else if self.audio_rx.is_some() {
            Some("音频唤醒")
        } else if self.loginfo_rx.is_some() {
            Some("日志收集")
        } else if self.macro_recording.is_some() {
            Some("宏录制")
        } else if self.picking.is_some() {
            Some("取点")
        } else if self.waiting_key.is_some() {
            Some("按键捕获")
        } else if self.resizing.is_some() {
            Some("范围修改")
        } else if self.easing_edit.is_some() {
            Some("曲线编辑")
        } else {
            None
        }
    }

    /// 取消所有等待中的任务/交互。已经启动的外部进程不会因为取消按钮被强杀；
    /// 尚未完成、结果还没回收的任务会丢弃回执，避免 UI 永远卡在“进行中”。
    fn cancel_pending_tasks(&mut self) {
        let label = self.pending_task_label().unwrap_or("当前任务");
        self.connect_rx = None;
        self.shot_rx = None;
        self.debug_rx = None;
        self.space_rx = None;
        self.audio_rx = None;
        self.loginfo_rx = None;
        self.macro_recording = None;
        self.picking = None;
        self.waiting_key = None;
        self.resizing = None;
        self.easing_edit = None;
        self.draft_active = false;
        self.draft.key = None;
        self.log(format!("已取消{label}"));
    }
    /// 幂等退出清理：窗口关闭、eframe on_exit 和 Drop 任一路径都会调用。
    /// 先断控制通道，再释放 server / 输入捕获，最后恢复系统光标。
    fn shutdown(&mut self) {
        if self.shutdown_done {
            return;
        }
        self.shutdown_done = true;
        if let Ok(mut g) = self.shared.lock() {
            g.enabled = false;
            g.control = None;
        }
        self.server = None;
        self.connect_rx = None;
        self.scrcpy_status_rx = None;
        self.debug_rx = None;
        self.shot_rx = None;
        self.audio_rx = None;
        self.loginfo_rx = None;
        self.cursor_hide_flag.store(false, Ordering::Relaxed);
        self._capture = None;
        crate::capture::set_cursor_visible_from_ui(true);
        crate::diag_info!("lifecycle", "PadApp 退出清理完成");
        crate::diag::flush();
    }

    /// scrcpy 启动前把自动追加的参数写回可见参数框。
    /// 只有虚拟手柄模式需要禁用 scrcpy 自带输入；触摸拖动模式不再自动添加 --mouse=uhid。
    fn prepare_scrcpy_args(&mut self) -> String {
        let mode = self.shared.lock().unwrap().profile.aim.input_mode;
        let before = self.scrcpy_args.trim().to_string();
        self.scrcpy_args = prepare_scrcpy_args_with_mode(&self.scrcpy_args, mode);
        if self.scrcpy_args != before {
            crate::diag_info!(
                "scrcpy",
                "auto args: mode={:?}, before={:?}, after={:?}",
                mode,
                before,
                self.scrcpy_args
            );
        }
        self.scrcpy_args.trim().to_string()
    }
    fn take_screenshot(&mut self) {
        let serial = self.serial();
        if serial.is_empty() {
            self.log("错误: 未选择设备");
            return;
        }
        let (tx, rx) = channel();
        self.shot_rx = Some(rx);
        std::thread::spawn(move || {
            let r = adb::screencap_png(&serial)
                .map_err(|e| format!("{e:#}"))
                .and_then(|png| {
                    image::load_from_memory(&png)
                        .map(|img| {
                            let rgba = img.to_rgba8();
                            let (w, h) = rgba.dimensions();
                            egui::ColorImage::from_rgba_unmultiplied(
                                [w as usize, h as usize],
                                &rgba,
                            )
                        })
                        .map_err(|e| format!("解码截图失败: {e}"))
                });
            let _ = tx.send(r);
        });
    }

    /// 唤醒设备音频转发。
    ///
    /// 有些机器(小米/HyperOS 等)scrcpy 刚连上时音频流是"哑"的,必须在手机上按一下
    /// 音量键才出声。这里在 scrcpy 启动后延迟注入一次音量 +1/-1(净变化为 0),
    /// 复现用户手动按键触发的 audio policy 更新。
    fn wake_audio(&mut self) {
        if self
            .scrcpy_args
            .split_whitespace()
            .any(|a| a == "--no-audio")
        {
            return;
        }
        if self.audio_rx.is_some() {
            return;
        }
        let serial = self.serial();
        if serial.is_empty() {
            return;
        }
        let (tx, rx) = channel();
        self.audio_rx = Some(rx);
        self.log("正在唤醒设备音频转发...");
        std::thread::spawn(move || {
            // 分两次:音频流本身要一两秒才起来,第一次可能扑空;两次的净音量都是 0
            let mut last = Ok(());
            for wait in [2u64, 3] {
                std::thread::sleep(Duration::from_secs(wait));
                last = adb::nudge_audio(&serial);
            }
            let _ = tx.send(match last {
                Ok(()) => "已唤醒设备音频转发(音量键 +1/-1,净变化为 0)".to_string(),
                Err(e) => format!("音频唤醒失败: {e}"),
            });
        });
    }

    fn assign_key(&mut self, slot: KeySlot, code: u16) {
        self.push_undo();
        // 切换键属于"组合表结构",改完自动落盘(见 sync_scheme_state);
        // 普通键位不在此列 —— 拖动圆圈/连续改键太频繁,由 [保存配置] 决定何时写。
        let mut switch_key_touched = false;
        {
            let mut g = self.shared.lock().unwrap();
            match slot {
                KeySlot::NewBind => self.draft.key = Some(code),
                KeySlot::Bind(i) => {
                    if let Some(b) = g.profile.binds.get_mut(i) {
                        b.key = code;
                    }
                }
                KeySlot::WheelDir { wheel, dir } => {
                    if let Some(w) = g.profile.wheels.get_mut(wheel) {
                        if w.kind == WheelKind::Standard {
                            match dir {
                                0 => w.up = code,
                                1 => w.down = code,
                                2 => w.left = code,
                                _ => w.right = code,
                            }
                        } else if let Some(d) = w.directions.get_mut(dir) {
                            d.key = code;
                        }
                    }
                }
                KeySlot::WheelEnable(i) => {
                    if let Some(w) = g.profile.wheels.get_mut(i) {
                        let mode = w.temp.as_ref().map(|t| t.mode).unwrap_or(TempMode::Hold);
                        w.temp = Some(TempWheel { key: code, mode });
                    }
                }
                KeySlot::Toggle => g.profile.toggle_key = code,
                KeySlot::CursorToggle => g.profile.cursor_toggle_key = code,
                KeySlot::AimHold => g.profile.aim.hold_key = code,
                KeySlot::AimToggle => g.profile.aim.toggle_key = code,
                KeySlot::AimSuspend => g.profile.aim.suspend_key = code,
                KeySlot::SwitchKey(i) => {
                    if let Some(s) = g.switch_keys.get_mut(i) {
                        s.key = code;
                        if s.keys.len() > 1 {
                            s.keys[0] = code;
                        } else {
                            s.keys = vec![code];
                        }
                    }
                    // 同一个物理键挂两行没有意义(引擎只会认第一行),
                    // 这里顺手把其余同名行清空为"未绑定",免得看着像生效了其实没有。
                    for (j, s) in g.switch_keys.iter_mut().enumerate() {
                        if j != i && s.key == code {
                            s.key = 0;
                        }
                    }
                    switch_key_touched = true;
                }
                KeySlot::SwitchKeySecond(i) => {
                    if let Some(s) = g.switch_keys.get_mut(i) {
                        if s.keys.is_empty() {
                            s.keys.push(s.key);
                        }
                        if s.keys.len() < 2 {
                            s.keys.push(code);
                        } else {
                            s.keys[1] = code;
                        }
                    }
                }
                KeySlot::ComboKey { combo, slot } => {
                    if let Some(combo) = g.profile.combos.get_mut(combo) {
                        if slot < combo.keys.len() {
                            combo.keys[slot] = code;
                        } else {
                            combo.keys.push(code);
                        }
                    }
                }
                KeySlot::MacroTrigger => self.macro_page_key = Some(code),
                KeySlot::MacroInstructionKey(index) => {
                    if let Some(MacroInstruction::Key {
                        code: instruction_code,
                        ..
                    }) = self.macro_page_instructions.get_mut(index)
                    {
                        *instruction_code = code;
                    }
                }
                KeySlot::MacroInstructionComboKey { instruction, slot } => {
                    if let Some(MacroInstruction::Combo { keys, .. }) =
                        self.macro_page_instructions.get_mut(instruction)
                    {
                        if slot < keys.len() {
                            keys[slot] = code;
                        } else {
                            keys.push(code);
                        }
                    }
                }
            }
        }
        if switch_key_touched {
            self.scheme_dirty = true;
        }
        self.log(format!("键位已绑定: {}", key_name(code)));
    }

    /// 截图取点后写入坐标(入参为像素,配置里存相对值)
    fn assign_coord(&mut self, slot: CoordSlot, x: i32, y: i32) {
        self.push_undo();
        let m = self.mapper();
        {
            let mut g = self.shared.lock().unwrap();
            match slot {
                // 新增草稿是临时状态,直接存像素(绘制与提交时再换算)
                CoordSlot::NewBind => {
                    self.draft.x = x;
                    self.draft.y = y;
                }
                CoordSlot::Bind(i) => {
                    if let Some(b) = g.profile.binds.get_mut(i) {
                        if let Action::Tap { x: ax, y: ay, .. }
                        | Action::Hold { x: ax, y: ay, .. } = &mut b.action
                        {
                            *ax = m.rel_x(x);
                            *ay = m.rel_y(y);
                        }
                    }
                }
                CoordSlot::WheelCenter(i) => {
                    if let Some(w) = g.profile.wheels.get_mut(i) {
                        w.cx = m.rel_x(x);
                        w.cy = m.rel_y(y);
                    }
                }
                CoordSlot::SwipeStart(i) => {
                    if let Some(b) = g.profile.binds.get_mut(i) {
                        if let Action::Swipe(s) = &mut b.action {
                            s.start = (m.rel_x(x), m.rel_y(y));
                        }
                    }
                }
                CoordSlot::SwipeEnd(i) => {
                    if let Some(b) = g.profile.binds.get_mut(i) {
                        if let Action::Swipe(s) = &mut b.action {
                            s.end = (m.rel_x(x), m.rel_y(y));
                        }
                    }
                }
                CoordSlot::NewSwipeStart => self.draft.swipe_start = (x, y),
                CoordSlot::NewSwipeEnd => self.draft.swipe_end = (x, y),
                CoordSlot::CircleAngle(i) => {
                    if let Some(b) = g.profile.binds.get_mut(i) {
                        if let Action::Swipe(s) = &mut b.action {
                            // 只用角度(与单位无关),起终点换算成像素后计算
                            let sp = m.point(s.start.0, s.start.1);
                            let ep = m.point(s.end.0, s.end.1);
                            set_circle_angle(&mut s.path, sp, ep, x, y);
                        }
                    }
                }
                CoordSlot::NewCircleAngle => {
                    set_circle_angle(
                        &mut self.draft.swipe_path,
                        self.draft.swipe_start,
                        self.draft.swipe_end,
                        x,
                        y,
                    );
                }
                CoordSlot::ComboPoint(i) => {
                    if let Some(combo) = g.profile.combos.get_mut(i) {
                        if let Action::Tap { x: ax, y: ay, .. }
                        | Action::Hold { x: ax, y: ay, .. } = &mut combo.action
                        {
                            *ax = m.rel_x(x);
                            *ay = m.rel_y(y);
                        }
                    }
                }
                CoordSlot::ComboSwipeStart(i) => {
                    if let Some(combo) = g.profile.combos.get_mut(i) {
                        if let Action::Swipe(s) = &mut combo.action {
                            s.start = (m.rel_x(x), m.rel_y(y));
                        }
                    }
                }
                CoordSlot::ComboSwipeEnd(i) => {
                    if let Some(combo) = g.profile.combos.get_mut(i) {
                        if let Action::Swipe(s) = &mut combo.action {
                            s.end = (m.rel_x(x), m.rel_y(y));
                        }
                    }
                }
                CoordSlot::ComboCircleAngle(i) => {
                    if let Some(combo) = g.profile.combos.get_mut(i) {
                        if let Action::Swipe(s) = &mut combo.action {
                            let sp = m.point(s.start.0, s.start.1);
                            let ep = m.point(s.end.0, s.end.1);
                            set_circle_angle(&mut s.path, sp, ep, x, y);
                        }
                    }
                }
                CoordSlot::AimAnchor => {
                    g.profile.aim.anchor_x = m.rel_x(x);
                    g.profile.aim.anchor_y = m.rel_y(y);
                }
            }
        }
        self.log(format!("坐标已设置: ({x}, {y})"));
    }

    /// 组装完整日志文本(含环境信息)
    fn build_log_content(&self, info: &adb::DeviceInfo) -> String {
        let mut s = String::new();
        s.push_str("scrcpy-pad 运行日志\n");
        s.push_str(&format!(
            "保存时间: {}\n",
            fmt_timestamp(std::time::SystemTime::now())
        ));
        s.push_str(&format!("程序版本: {}\n", env!("CARGO_PKG_VERSION")));
        s.push_str(&format!("主机环境: {}\n", info.host_os));
        s.push_str(&format!(
            "scrcpy: {} ({})\n",
            info.scrcpy,
            if self.scrcpy_path.is_empty() {
                "PATH"
            } else {
                &self.scrcpy_path
            }
        ));
        s.push_str(&format!("server: {}\n", self.server_path));
        s.push('\n');
        s.push_str(&format!("设备: {}\n", info.serial));
        s.push_str(&format!("品牌/型号: {} {}\n", info.brand, info.model));
        s.push_str(&format!("Android: {}\n", info.android));
        s.push_str(&format!("分辨率: {}\n", info.screen));
        s.push('\n');
        s.push_str("===== 运行日志 =====\n");
        for l in self.logs.iter() {
            s.push_str(l);
            s.push('\n');
        }
        s
    }

    fn key_button(ui: &mut egui::Ui, waiting: bool, code: Option<u16>) -> egui::Response {
        let label = if waiting {
            "按任意键...".to_string()
        } else {
            code.map(key_name).unwrap_or_else(|| "未绑定".into())
        };
        ui.add(egui::Button::new(label).min_size(egui::vec2(110.0, 0.0)))
    }

    /// "浏览器标签页"式的方形标签按钮(不立体、大小不变)。
    /// 右栏四标签页与可视化风格的操作栏按钮统一用它,观感一致。
    ///
    /// 未选中态以前是完全透明、无描边，只靠文字颜色暗示可点击；现在用
    /// 强调色做低透明度底色 + 半透明描边，既与背景有区分，又不会像实心
    /// 按钮一样抢视觉。底色/边框都只改变 alpha，暗色与浅色主题共用一套做法。
    fn tab_button(ui: &mut egui::Ui, label: &str, selected: bool, accent: egui::Color32) -> bool {
        let (fill_alpha, border_alpha) = if selected { (72, 224) } else { (26, 128) };
        let fill = theme::with_alpha(accent, fill_alpha);
        let stroke = egui::Stroke::new(1.0, theme::with_alpha(accent, border_alpha));
        ui.add(
            egui::Button::new(egui::RichText::new(label).strong())
                .fill(fill)
                .stroke(stroke)
                .corner_radius(3.0)
                .min_size(egui::vec2(72.0, 24.0)),
        )
        .clicked()
    }

    // ===================== 撤销 / 重做 =====================

    /// 把当前配置压入撤销栈(所有键位修改入口调用),并清空重做栈
    fn push_undo(&mut self) {
        let profile = self.shared.lock().unwrap().profile.clone();
        self.push_undo_snapshot(profile);
    }

    /// 用给定快照记录撤销点。
    /// 供已经持有配置锁的调用点使用(再加锁会自锁),快照必须是"修改之前"的状态。
    fn push_undo_snapshot(&mut self, profile: Profile) {
        // 栈满时丢弃最旧的一步。用 remove(0) 是 O(n),但 n ≤ 50 且只在满栈时发生,
        // 比起改用 VecDeque(会牵动 redo 逻辑)不值得。
        if self.undo_stack.len() >= UNDO_DEPTH {
            self.undo_stack.remove(0);
        }
        self.undo_stack.push(profile);
        self.redo_stack.clear();
        self.undo_frame_marked = true;
    }

    fn undo(&mut self) {
        if let Some(prev) = self.undo_stack.pop() {
            let current = self.shared.lock().unwrap().profile.clone();
            self.redo_stack.push(current);
            self.shared.lock().unwrap().profile = prev;
            self.log("已撤销");
        }
    }

    fn redo(&mut self) {
        if let Some(next) = self.redo_stack.pop() {
            let current = self.shared.lock().unwrap().profile.clone();
            self.undo_stack.push(current);
            self.shared.lock().unwrap().profile = next;
            self.log("已重做");
        }
    }

    /// 把 `Shared` 里的实时状态收拢成一份可落盘的配置文档。
    ///
    /// 关键一步:`profile`(引擎眼里"此刻生效的那套")要先写回 `schemes[active_scheme]`
    /// —— 界面上的改动都直接改 `profile`,组合表本身是"存档"。
    fn config_doc(&self) -> ConfigFile {
        let mut g = self.shared.lock().unwrap();
        g.stash_active();
        ConfigFile {
            format_version: g.profile.format_version,
            active: g.active_scheme,
            switch_keys: g.switch_keys.clone(),
            fast_switch_enabled: g.fast_switch_enabled,
            schemes: g.schemes.clone(),
        }
    }

    /// 保存当前键位配置到默认位置
    fn save_profile(&mut self) {
        self.stamp_profile_meta();
        let text = render_config(&self.config_doc());
        match text {
            Ok(text) => {
                if let Some(parent) = self.profile_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match std::fs::write(&self.profile_path, text) {
                    Ok(_) => {
                        self.scheme_saved = self.active_scheme();
                        self.scheme_dirty = false;
                        self.log(format!("已保存到 {}", self.profile_path.display()))
                    }
                    Err(e) => self.log(format!("保存失败: {e}")),
                }
            }
            Err(e) => self.log(format!("序列化失败: {e}")),
        }
    }

    /// 引擎此刻生效的组合下标(界面用它标记当前项)
    fn active_scheme(&self) -> usize {
        self.shared.lock().unwrap().active_scheme
    }

    /// 把一份文档整体装进 `Shared`:组合表、切换键表、生效下标,
    /// 以及引擎真正读的那份 `profile`。
    fn install_config(&self, doc: &ConfigFile) {
        let mut g = self.shared.lock().unwrap();
        g.schemes = doc.schemes.clone();
        g.switch_keys = doc.switch_keys.clone();
        g.fast_switch_enabled = doc.fast_switch_enabled;
        g.active_scheme = doc.active.min(doc.schemes.len().saturating_sub(1));
        g.profile = doc.active_profile().cloned().unwrap_or_default();
    }

    /// 帧末把生效中的 `profile` 收回组合表;若引擎(切换键)刚换了组合,
    /// 就把新的 `active` 写回文件 —— 相当于"记住上次用的是哪一套"。
    ///
    /// 另外两类改动也走这里落盘:界面上的**组合表结构**变更(新建/删除/改名组合、
    /// 增删切换键位、改切换目标)由 `scheme_dirty` 标记;键位主体的细节(拖动圆圈、
    /// 增删键位)仍由 [保存配置] / Ctrl+S 决定何时写 —— 否则一次拖动就是几百次写盘。
    ///
    /// 只写文件、不记日志:开打中按一下切换键不该在日志区刷屏。
    fn sync_scheme_state(&mut self) {
        let active = {
            let mut g = self.shared.lock().unwrap();
            g.stash_active();
            g.active_scheme
        };
        if active == self.scheme_saved && !self.scheme_dirty {
            return;
        }
        self.scheme_saved = active;
        self.scheme_dirty = false;
        if let Ok(text) = render_config(&self.config_doc()) {
            if let Some(parent) = self.profile_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&self.profile_path, text);
        }
    }

    /// 手动切换生效的组合(左侧面板选中某套)。切换要先抬起旧组合的触点,
    /// 这由引擎的 `sync_structures` 兜底(结构指纹变了就抬干净再重建),
    /// 所以这里只换数据、不动引擎。
    fn select_scheme(&mut self, idx: usize) {
        if !self.shared.lock().unwrap().select_scheme(idx) {
            return;
        }
        self.undo_stack.clear();
        self.redo_stack.clear();
        let name = self.shared.lock().unwrap().profile.name.clone();
        self.log(format!("已切换到按键组合「{name}」"));
    }

    /// 新建一套组合(复制当前这套)并切过去
    fn add_scheme(&mut self) {
        let name = {
            let mut g = self.shared.lock().unwrap();
            g.stash_active();
            let n = g.schemes.len() + 1;
            let mut copy = g.profile.clone();
            copy.name = format!("按键组合{n}");
            g.schemes.push(copy);
            // 新组合立刻生效:用户点"新建"通常就是想马上配它
            g.active_scheme = g.schemes.len() - 1;
            g.profile = g.schemes[g.active_scheme].clone();
            g.profile.name.clone()
        };
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.scheme_dirty = true;
        self.log(format!(
            "已新建按键组合「{name}」(复制自当前这套)并切换过去,可在下方改名"
        ));
    }

    fn delete_scheme(&mut self, idx: usize) {
        // 先在锁内算好结论、拿到要显示的文案,出了作用域再 self.log:
        // 持锁期间 self 被借住,`&mut self` 的方法调不了。
        let (msg, removed) = {
            let mut g = self.shared.lock().unwrap();
            if g.schemes.len() <= 1 || idx >= g.schemes.len() {
                // 至少留一套:删空会让引擎没有配置可跑
                ("至少要保留一套按键组合".to_string(), false)
            } else {
                g.stash_active();
                let removed_name = g.schemes[idx].name.clone();
                g.schemes.remove(idx);
                // 切换键的目标下标要跟着位移:指向被删那套的直接作废,
                // 排在后面的整体前移一格,否则会悄悄指到别的组合上。
                g.switch_keys.retain(|s| s.target != idx);
                for s in g.switch_keys.iter_mut() {
                    if s.target > idx {
                        s.target -= 1;
                    }
                }
                if g.active_scheme == idx {
                    g.active_scheme = 0;
                    g.profile = g.schemes[0].clone();
                } else if g.active_scheme > idx {
                    g.active_scheme -= 1;
                }
                (format!("已删除按键组合「{removed_name}」"), true)
            }
        };
        self.undo_stack.clear();
        self.redo_stack.clear();
        if removed {
            self.scheme_dirty = true;
        }
        self.log(msg);
    }

    /// 从当前指向的配置文件重新加载(重定向后也从新路径加载)
    fn reload_profile_from_current(&mut self) {
        match read_profile_at(&self.profile_path) {
            Ok(doc) => {
                self.push_undo();
                self.install_config(&doc);
                self.scheme_saved = doc.active;
                self.scheme_dirty = false;
                self.log(format!(
                    "配置已重新加载(可撤销): {}",
                    self.profile_path.display()
                ));
                // 已知屏幕尺寸时,顺手把旧格式配置升级为相对坐标(与[选用配置]行为一致)
                if let Some((w, h)) = self.screen_size() {
                    self.sync_display_space(w, h);
                }
            }
            Err(e) => self.log(format!("重新加载失败: {e}")),
        }
    }

    /// 切回程序默认的配置文件(`config_dir/profile.yaml`,即首次运行时程序自己
    /// 创建的那份)并加载它的内容。文件被删掉时会按出厂默认重新写一份,所以这个
    /// 按钮总能回到"最初那份配置"。
    fn load_default_profile(&mut self) {
        let path = profile_path();
        let doc = if path.exists() {
            match read_profile_at(&path) {
                Ok(p) => p,
                // 文件在但读不出来(损坏/手改坏了)时不覆盖它,只报告
                Err(e) => {
                    self.log(format!("默认配置读取失败: {e}"));
                    return;
                }
            }
        } else {
            match write_default_profile(&path) {
                Ok(_) => ConfigFile::default(),
                Err(e) => {
                    self.log(format!("默认配置不可用: {e}"));
                    return;
                }
            }
        };
        self.profile_path = path.clone();
        self.apply_profile_switch(doc);
        // 先取数、释放锁,再 log:避免 format! 参数里两次 lock 死锁
        let (nb, nw) = {
            let g = self.shared.lock().unwrap();
            (g.profile.binds.len(), g.profile.wheels.len())
        };
        self.log(format!(
            "已选用默认配置: {} ({} 按键 / {} 轮盘)",
            path.display(),
            nb,
            nw
        ));
    }

    /// 切换当前配置文件后整体替换配置内容;
    /// 撤销/重做栈指向旧文件数据,与当前上下文无关,一并清空避免误操作
    fn apply_profile_switch(&mut self, doc: ConfigFile) {
        self.scheme_saved = doc.active;
        self.scheme_dirty = false;
        self.install_config(&doc);
        self.undo_stack.clear();
        self.redo_stack.clear();
        // 已知屏幕尺寸时,顺手把旧格式配置升级为相对坐标
        if let Some((w, h)) = self.screen_size() {
            self.sync_display_space(w, h);
        }
    }

    /// 进入取点。取点/改范围这类交互同一时刻只保留一个,以最后一次操作为准:
    /// 正在改响应范围时点[取点],就直接转去取点,不再两头挂着。
    fn begin_pick(&mut self, slot: CoordSlot) {
        self.resizing = None;
        self.picking = Some(slot);
    }

    /// 进入"修改响应范围"(同样按最新操作优先,终止正在进行的取点)
    fn begin_resize(&mut self, i: usize) {
        self.push_undo();
        self.picking = None;
        self.resizing = Some(ResizeTarget::Bind(i));
        self.log("响应范围修改中: 用 Ctrl++ / Ctrl+- 或拖动圆圈调整");
    }

    /// 进入轮盘的"改响应范围"(半径)。与键位同一套交互:改范围期间
    /// `Ctrl++ / Ctrl+-` 调圆圈、也可直接在截图上拖动。
    fn begin_resize_wheel(&mut self, i: usize) {
        self.push_undo();
        self.picking = None;
        self.resizing = Some(ResizeTarget::Wheel(i));
        self.log("轮盘半径修改中: 用 Ctrl++ / Ctrl+- 或拖动圆圈调整");
    }

    /// 取消新增草稿:清除取点等待、等待按键与草稿圆圈/轨迹
    fn cancel_draft(&mut self) {
        self.picking = None;
        if self.waiting_key == Some(KeySlot::NewBind) {
            self.waiting_key = None;
        }
        self.draft.key = None;
        self.macro_recording = None;
        self.vk_combo_pending.clear();
        self.draft_active = false;
    }

    /// 检测【当前】配置文件是否符合格式要求(不弹文件选择框)
    fn check_current_profile(&mut self) {
        match read_profile_at(&self.profile_path) {
            Ok(doc) => {
                let ap = doc.active_profile();
                let (nb, nw) = ap
                    .map(|p| (p.binds.len(), p.wheels.len()))
                    .unwrap_or((0, 0));
                self.log(format!(
                    "检测通过: {} 是合法配置 ({} 套组合 / 当前这套 {} 按键 / {} 轮盘)",
                    self.profile_path.display(),
                    doc.schemes.len(),
                    nb,
                    nw
                ))
            }
            Err(e) => self.log(format!(
                "检测不通过: {} —— {e}",
                self.profile_path.display()
            )),
        }
    }

    /// 分辨率预设:先把参数整体重置为初始“无”状态,再叠加对应 --max-size
    fn set_res_preset(&mut self, max_size: u32) {
        self.scrcpy_args = format!("{BASE_SCRCPY_ARGS} --max-size={max_size}");
        self.log(format!("启动参数已设为分辨率预设(最长边上限 {max_size})"));
        // scrcpy 的 --max-size 只是“上限”:它只会缩小,永远不会放大。
        // 选了 4k 却拿不到 4k,通常就是设备本身没有这么大的画面可采。
        if let Some((w, h)) = self.screen_size() {
            let native = w.max(h);
            if native < max_size {
                self.log(format!(
                    "提示: 设备当前最长边只有 {native}px,而 scrcpy 不会放大,实际输出仍是 {native}px 档;\
                     想要更高分辨率需设备本身能输出(如 `adb shell wm size 3840x2160`,或开发者选项里的分辨率设定)"
                ));
            }
        }
    }

    /// 顶栏“启动预设”下拉的执行动作
    fn apply_start_preset(&mut self, p: StartPreset) {
        match p {
            StartPreset::None => {
                self.scrcpy_args = BASE_SCRCPY_ARGS.to_string();
                self.log(format!("启动参数已重置: {BASE_SCRCPY_ARGS}"));
            }
            StartPreset::Uhd2k => self.set_res_preset(2560),
            StartPreset::Uhd4k => self.set_res_preset(3840),
            StartPreset::Fhd1080 => self.set_res_preset(1080),
            StartPreset::Hd720 => self.set_res_preset(720),
            StartPreset::NoAudio => {
                if self
                    .scrcpy_args
                    .split_whitespace()
                    .any(|a| a == "--no-audio")
                {
                    self.log("参数已含 --no-audio,无需重复");
                } else {
                    let base = self.scrcpy_args.trim();
                    self.scrcpy_args = format!("{base} --no-audio")
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ");
                    self.log("已追加 --no-audio(不使用音频输出)");
                }
            }
            StartPreset::WithAudio => {
                let had = self
                    .scrcpy_args
                    .split_whitespace()
                    .any(|a| a == "--no-audio");
                self.scrcpy_args = self
                    .scrcpy_args
                    .split_whitespace()
                    .filter(|a| *a != "--no-audio")
                    .collect::<Vec<_>>()
                    .join(" ");
                if had {
                    self.log("已移除 --no-audio(scrcpy 默认转发音频输出)");
                } else {
                    self.log("本就未含 --no-audio:scrcpy 默认开启音频输出,无需额外参数");
                }
            }
        }
    }

    /// 用当前生效的 adb 重新拉取设备列表
    fn refresh_devices(&mut self) {
        self.devices = adb::list_devices();
        if self.selected >= self.devices.len() {
            self.selected = 0;
        }
        self.log(format!("设备已刷新,共 {} 台", self.devices.len()));
    }

    // ===================== scrcpy / server / adb 联动 =====================

    /// 解析当前实际使用的 adb:手动指定的 adb 路径 > scrcpy 同目录 > PATH
    fn effective_adb(&self) -> Option<PathBuf> {
        let manual = self.adb_path.trim();
        if !manual.is_empty() {
            let p = PathBuf::from(manual);
            if p.is_file() {
                return Some(p);
            }
        }
        let exe = if self.scrcpy_path.trim().is_empty() {
            None
        } else {
            Some(PathBuf::from(self.scrcpy_path.trim()))
        };
        adb::find_adb(exe.as_deref())
    }

    /// 手动改动路径的按钮(浏览/自动寻找/输入失焦)调用:强制下一帧做一次完整联动
    fn resync(&mut self) {
        self.suite_synced = (String::new(), String::new(), String::new(), String::new());
        self.sync_suite();
    }

    /// 联动补齐:scrcpy.exe / scrcpy-server / adb.exe 三者任填一个,其余为空时
    /// 自动从同目录(官方 Windows 发行包三者在同一目录)推导补齐;
    /// 另外"只指定了 scrcpy 目录"也走这里解析出可执行文件;
    /// 生效的 adb 变化时自动刷新设备列表。每帧调用,仅在四元组文本变化时执行。
    fn sync_suite(&mut self) {
        let trio = (
            self.scrcpy_path.clone(),
            self.server_path.clone(),
            self.adb_path.clone(),
            self.scrcpy_dir.clone(),
        );
        if trio == self.suite_synced {
            return;
        }
        self.suite_synced = trio;

        // 0) 只给了"scrcpy 目录":先把它解析成可执行文件(用户最自然的用法)
        if self.scrcpy_path.trim().is_empty() && !self.scrcpy_dir.trim().is_empty() {
            let d = PathBuf::from(self.scrcpy_dir.trim());
            if d.is_dir() {
                if let Some(exe) = adb::find_scrcpy_under(&d, 3) {
                    self.scrcpy_path = exe.display().to_string();
                    self.log(format!("已按 scrcpy 目录补齐: {}", self.scrcpy_path));
                }
            }
        }

        // 1) 由 scrcpy.exe 推导同目录/相邻的 scrcpy-server 与 adb
        let scrcpy_txt = self.scrcpy_path.trim().to_string();
        if !scrcpy_txt.is_empty() {
            let exe = PathBuf::from(&scrcpy_txt);
            if self.server_path.trim().is_empty() {
                if let Some(s) = adb::find_server(Some(&exe)) {
                    self.server_path = s.display().to_string();
                    self.log(format!("server 已自动补齐: {}", self.server_path));
                }
            }
            if self.adb_path.trim().is_empty() {
                if let Some(a) = adb::find_adb(Some(&exe)) {
                    self.adb_path = a.display().to_string();
                    self.log(format!("adb 已自动补齐: {}", self.adb_path));
                }
            }
        }
        // 2) 由 scrcpy-server 所在目录推导 scrcpy.exe 与 adb
        let server_txt = self.server_path.trim().to_string();
        if !server_txt.is_empty() {
            let sp = PathBuf::from(&server_txt);
            if let Some(dir) = sp.parent() {
                if self.scrcpy_path.trim().is_empty() {
                    let exe = dir.join(adb::scrcpy_exe_name());
                    if exe.is_file() {
                        self.scrcpy_path = exe.display().to_string();
                        self.log(format!("scrcpy 已自动补齐: {}", self.scrcpy_path));
                    }
                }
                if self.adb_path.trim().is_empty() {
                    if let Some(a) = adb::find_adb_in_dir(dir) {
                        self.adb_path = a.display().to_string();
                        self.log(format!("adb 已自动补齐: {}", self.adb_path));
                    }
                }
            }
        }
        // 3) 由 adb 所在目录推导 scrcpy.exe 与 scrcpy-server
        let adb_txt = self.adb_path.trim().to_string();
        if !adb_txt.is_empty() {
            let ap = PathBuf::from(&adb_txt);
            if let Some(dir) = ap.parent() {
                if self.scrcpy_path.trim().is_empty() {
                    let exe = dir.join(adb::scrcpy_exe_name());
                    if exe.is_file() {
                        self.scrcpy_path = exe.display().to_string();
                        self.log(format!("scrcpy 已自动补齐: {}", self.scrcpy_path));
                    }
                }
                if self.server_path.trim().is_empty() {
                    let srv = dir.join("scrcpy-server");
                    if srv.is_file() {
                        self.server_path = srv.display().to_string();
                        self.log(format!("server 已自动补齐: {}", self.server_path));
                    }
                }
            }
        }
        // 4) 生效 adb 变化 -> 应用并刷新设备
        let effective = self.effective_adb();
        if effective.as_ref().map(|p| p.display().to_string()) != adb::adb_bin_now() {
            adb::set_adb_bin(effective.as_deref());
            match &effective {
                Some(p) => self.log(format!("adb: {}", p.display())),
                None => self
                    .log("未找到 adb(设备列表将为空): 请将 adb.exe 所在目录加入 PATH,或手动指定"),
            }
            self.refresh_devices();
        }

        // 5) 用户手工改过路径(且与上次应用的组合不同)-> 重新应用并刷新设备。
        //    只靠 `suite_synced` 判断是不够的:文本改回原值时它不会变化,
        //    但用户明确期望"改完路径立刻生效"。
        let applied = (
            self.scrcpy_path.clone(),
            self.server_path.clone(),
            self.adb_path.clone(),
        );
        if self.applied_suite.as_ref() != Some(&applied) {
            self.applied_suite = Some(applied);
            self.refresh_devices();
        }
    }

    // ===================== 程序级设置(记住路径) =====================

    /// 把当前界面上的路径/参数打包成一份设置
    fn settings_snapshot(&self) -> Settings {
        let mut s = Settings {
            remember_paths: self.remember_paths,
            scrcpy_path: self.scrcpy_path.trim().to_string(),
            scrcpy_dir: self.scrcpy_dir.trim().to_string(),
            server_path: self.server_path.trim().to_string(),
            adb_path: self.adb_path.trim().to_string(),
            scrcpy_args: self.scrcpy_args.trim().to_string(),
            selected_serial: self.serial(),
            // 日志级别由界面上的下拉框负责写入,这里原样带回上次读到的值,
            // 免得"改一次别的设置"就把用户选的级别冲掉
            log_level: self.settings_loaded.log_level.clone(),
            saved_at: 0,
        };
        if !self.remember_paths {
            // 关掉[记住路径]的语义是"下次启动不再使用",**不是**"把文件清空":
            // 路径文本原样保留,免得开关来回拨一次就把辛苦找好的 scrcpy 位置弄丢。
            s.scrcpy_path = self.settings_loaded.scrcpy_path.clone();
            s.scrcpy_dir = self.settings_loaded.scrcpy_dir.clone();
            s.server_path = self.settings_loaded.server_path.clone();
            s.adb_path = self.settings_loaded.adb_path.clone();
        }
        s
    }

    /// 登记"路径需要记住"(帧末统一写盘,内容未变则跳过)。
    ///
    /// 注意:**即使关掉了[记住路径]也要登记**。开关状态本身就是设置的一部分,
    /// 早退不写会导致"我明明关掉了,下次启动它又自己打开了,还把旧路径读了回来"。
    fn remember_now(&mut self) {
        // 时间戳由 settings::save_if_dirty 在真正落盘时打,这里传 0 即可
        self.settings_cache.mark_dirty(self.settings_snapshot());
    }

    /// 立刻把设置写入磁盘并给出明确日志。
    ///
    /// 帧末也会存一次,但"用户刚设置好 scrcpy 目录"这种关键时刻必须当场确认:
    /// 用户反馈过"设置好了、重启却依旧没被记住",而日志里看不出到底写没写。
    fn save_settings_now(&mut self) {
        self.remember_now();
        if !self.remember_paths {
            // 开关本身照写(否则下次启动又变回"记住"),但要说清路径没有被存
            let flag = self.settings_cache.save_if_dirty();
            if let Some(Err(e)) = flag {
                self.log(format!("设置保存失败({e})"));
            }
            self.log(format!(
                "当前未勾选[记住路径]: 路径不会写入 {}(勾选后会自动补存)",
                settings::path().display()
            ));
            return;
        }
        match self.settings_cache.save_if_dirty() {
            Some(Ok(p)) => self.log(format!("设置已保存: {}", p.display())),
            Some(Err(e)) => self.log(format!("设置保存失败({e}),本次改动只在本次运行内有效")),
            None => self.log(format!(
                "设置内容未变,无需写盘: {}",
                settings::path().display()
            )),
        }
    }

    /// 帧末落盘:写在配置目录的 settings.json 里,与主题(look.json)同一目录。
    /// 成功时**不**打日志(路径栏边打边存,每敲一个字符都会写一次,刷屏没意义);
    /// 失败必须说,否则用户以为记住了、其实没有。
    fn persist_settings(&mut self) {
        if let Some(Err(e)) = self.settings_cache.save_if_dirty() {
            self.log(format!("设置保存失败({e}),本次改动只在本次运行内有效"));
        }
    }

    /// 按"scrcpy 目录"补齐三件套(用户最自然的用法:直接指给它发行包那个文件夹)
    fn apply_scrcpy_dir(&mut self) {
        let dir = self.scrcpy_dir.trim().to_string();
        if dir.is_empty() {
            self.log("请先填写或选择 scrcpy 所在目录");
            return;
        }
        let d = PathBuf::from(&dir);
        if !d.is_dir() {
            self.log(format!("不是有效目录: {dir}"));
            return;
        }
        self.scrcpy_dir = dir.clone();
        match adb::find_scrcpy_under(&d, 3) {
            Some(exe) => {
                self.scrcpy_path = exe.display().to_string();
                if let Some(s) = adb::find_server(Some(&exe)) {
                    self.server_path = s.display().to_string();
                }
                if let Some(a) = adb::find_adb(Some(&exe)) {
                    self.adb_path = a.display().to_string();
                }
                self.log(format!("已按目录补齐 scrcpy: {}", self.scrcpy_path));
            }
            None => self.log(format!(
                "该目录里没有找到 {}(可以直接指定 scrcpy 可执行文件本身): {dir}",
                adb::scrcpy_exe_name()
            )),
        }
        self.resync();
        self.test_scrcpy();
        self.save_settings_now();
    }

    /// 在系统文件管理器里打开配置目录(找不到文件管理器时退回日志提示)
    fn open_config_dir(&mut self) {
        let dir = config_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.log(format!("无法创建配置目录 {}({e})", dir.display()));
            return;
        }
        #[cfg(target_os = "windows")]
        let cmd = "explorer";
        #[cfg(target_os = "macos")]
        let cmd = "open";
        #[cfg(all(unix, not(target_os = "macos")))]
        let cmd = "xdg-open";
        match std::process::Command::new(cmd).arg(&dir).spawn() {
            Ok(_) => self.log(format!("已在文件管理器中打开: {}", dir.display())),
            Err(_) => self.log(format!("配置目录(请手动打开): {}", dir.display())),
        }
    }

    // ===================== 诊断面板 =====================

    /// 左侧「诊断」面板:逐条自检 + 一键把日志交给作者。
    ///
    /// 设计沿用「鼠标瞄准(FPS)」那一套已经验证好用的表达方式:
    /// **✓/✗ 逐项 + 一句人话结论 + 可点的修复动作**。
    /// 为什么值得单独做一块:外接键盘失灵、FPS 没反应、偶发断触这几类问题
    /// 全都只在别人的机器上出现,靠用户口述几乎无法定位 ——
    /// 必须让**程序自己说出**"现在是哪一环不通",并让用户能一键把现场证据拿出来。
    fn ui_diagnostics(&mut self, ui: &mut egui::Ui) {
        let th = self.theme();
        ui.label("底层运行状况。出问题时这里直接说原因,并可把日志交给作者。");

        let (connected, live, capture_err) = {
            let g = self.shared.lock().unwrap();
            (
                g.control
                    .as_ref()
                    .map(|c| c.is_connected())
                    .unwrap_or(false),
                g.live,
                self.capture_err.clone(),
            )
        };
        let mouse_found = self.mouse_found_flag.load(Ordering::Relaxed);
        let level = crate::diag::level();
        let log_path = crate::diag::path();

        // ---- 逐项自检 ----
        ui.separator();
        ui.label("状态自检:");
        let mut blocker: Option<&str> = None;
        for (ok, text) in [
            (
                capture_err.is_none(),
                "输入捕获已启动(读得到 /dev/input,或 Windows 钩子已装上)",
            ),
            (mouse_found, "检测到鼠标类设备(有相对位移轴)"),
            (connected, "控制通道已连接"),
            (live.refused == 0, "没有因为触点池满而被拒的按下"),
        ] {
            if !ok && blocker.is_none() {
                blocker = Some(text);
            }
            ui.colored_label(
                if ok { th.ok } else { th.warn },
                format!("{} {}", if ok { "✓" } else { "✗" }, text),
            );
        }
        if let Some(e) = &capture_err {
            ui.colored_label(th.danger, e.clone());
        }
        ui.label(format!(
            "引擎触点占用 {}/{} · 累计被放弃 {} 次{}",
            live.pointers,
            crate::engine::DEVICE_MAX_POINTERS,
            live.refused,
            if live.refused == 0 {
                String::new()
            } else {
                format!(
                    "(最近一次是 {})",
                    if live.last_refused == 0 {
                        "瞄准".to_string()
                    } else {
                        key_name(live.last_refused)
                    }
                )
            }
        ));
        if let Some(b) = blocker {
            ui.colored_label(th.warn, format!("→ 现在不完整,因为: {b}"));
        }

        // ---- 日志级别 ----
        ui.separator();
        ui.horizontal(|ui| {
            ui.label("诊断日志级别:");
            let mut cur = level.name().to_string();
            egui::ComboBox::from_id_salt("diag_level")
                .selected_text(cur.clone())
                .width(96.0)
                .show_ui(ui, |ui| {
                    for name in ["error", "warn", "info", "debug", "trace"] {
                        ui.selectable_value(&mut cur, name.to_string(), name);
                    }
                });
            if cur != level.name() {
                if let Some(l) = crate::diag::Level::parse(&cur) {
                    crate::diag::set_level(l, "界面设置");
                    // 同时写进 settings.json,否则重启就回到默认级别 ——
                    // 而"下次启动还能看到细节"正是排查偶发问题最需要的
                    self.settings_loaded.log_level = cur.clone();
                    self.save_settings_now();
                    self.log(format!(
                        "诊断日志级别已设为 {cur}(已写入 settings.json 的 log_level;环境变量 {} 会覆盖它)",
                        crate::diag::ENV_LEVEL
                    ));
                }
            }
        });
        ui.small(format!("当前来源: {}", crate::diag::level_source()));
        ui.small("更详细的日志:把 settings.json 的 log_level 改成 debug 或 trace。");

        // ---- 日志操作 ----
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            if ui
                .small_button("打开日志目录")
                .on_hover_text(log_path.display().to_string())
                .clicked()
            {
                self.open_config_dir();
            }
            if ui
                .small_button("复制日志路径")
                .on_hover_text("把这行路径发给作者,或先自己打开看看")
                .clicked()
            {
                ui.ctx().copy_text(log_path.display().to_string());
                self.log(format!("已复制日志路径: {}", log_path.display()));
            }
            if ui
                .small_button("刷新预览")
                .on_hover_text("重新读取日志末尾(不会改动日志)")
                .clicked()
            {
                self.diag_preview = crate::diag::tail(8 * 1024);
            }
            if ui
                .small_button("导出诊断报告")
                .on_hover_text(
                    "把环境快照 + 当前配置摘要 + 日志全文合成一个 txt,存到日志同目录,\n\
                     直接把这个文件发给作者即可,不必再描述现象",
                )
                .clicked()
            {
                self.export_diagnostic_report();
            }
        });
        ui.small(format!("日志文件: {}", log_path.display()));
        ui.small("每次启动整体重写;崩溃现场会保留到下次启动为止。");

        // ---- 日志尾部预览 ----
        let mut open = self.diag_preview_open;
        ui.checkbox(&mut open, "显示日志末尾");
        self.diag_preview_open = open;
        if self.diag_preview_open {
            if self.diag_preview.is_empty() {
                self.diag_preview = crate::diag::tail(8 * 1024);
            }
            egui::ScrollArea::vertical()
                .max_height(180.0)
                .id_salt("diag_tail")
                .show(ui, |ui| {
                    // 必须用 monospace 且禁止换行:日志是按列对齐的,折行后没法看
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(self.diag_preview.as_str())
                                .monospace()
                                .small(),
                        )
                        .wrap(),
                    );
                });
        }
    }

    /// 把"当前自己填的按键组合 + 全部自检结论 + 日志尾部"合成一份报告,
    /// 写到日志同目录下,方便用户直接发文件而不必描述现象。
    fn export_diagnostic_report(&mut self) {
        let mut out = String::new();
        out.push_str(&format!(
            "scrcpy-pad 诊断报告 v{}\n\n",
            env!("CARGO_PKG_VERSION")
        ));
        if let Some(s) = crate::diag::snapshot() {
            out.push_str(&s);
            out.push_str("\n\n");
        }
        out.push_str("===== 当前配置摘要 =====\n");
        {
            let g = self.shared.lock().unwrap();
            out.push_str(&format!(
                "生效组合: {}(第 {} 套 / 共 {} 套)\n",
                g.profile.name,
                g.active_scheme + 1,
                g.schemes.len()
            ));
            out.push_str(&format!(
                "按键 {} 个,轮盘 {} 个,切换键 {} 个\n",
                g.profile.binds.len(),
                g.profile.wheels.len(),
                g.switch_keys.len()
            ));
            out.push_str(&format!(
                "映射: {};控制通道: {}\n",
                if g.enabled { "已开启" } else { "已关闭" },
                match g.control.as_ref() {
                    Some(c) if c.is_connected() => format!("已连接 {}x{}", c.screen_w, c.screen_h),
                    Some(_) => "已断开".to_string(),
                    None => "未连接".to_string(),
                }
            ));
        }
        out.push_str(&format!(
            "触点占用 {}/{} · 累计被放弃 {} 次\n",
            {
                let g = self.shared.lock().unwrap();
                g.live.pointers
            },
            crate::engine::DEVICE_MAX_POINTERS,
            {
                let g = self.shared.lock().unwrap();
                g.live.refused
            }
        ));
        out.push_str("\n===== 诊断日志全文 =====\n");
        out.push_str(&crate::diag::tail(usize::MAX));

        let path = crate::diag::path()
            .with_file_name(format!("diagnostics-report-{}.txt", timestamp_compact()));
        match std::fs::write(&path, out) {
            Ok(_) => {
                crate::diag::flush();
                self.log(format!("诊断报告已写出: {}", path.display()));
            }
            Err(e) => self.log(format!("诊断报告写出失败: {e}")),
        }
    }
}

impl eframe::App for PadApp {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.shutdown();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        self.poll_debug_rx();
        self.maybe_refresh_debug_info();

        let mut scrcpy_messages = Vec::new();
        let mut scrcpy_rx_done = false;
        if let Some(rx) = self.scrcpy_status_rx.as_ref() {
            loop {
                match rx.try_recv() {
                    Ok(message) => scrcpy_messages.push(message),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        scrcpy_rx_done = true;
                        break;
                    }
                }
            }
        }
        for message in scrcpy_messages {
            self.log(message);
        }
        if scrcpy_rx_done {
            self.scrcpy_status_rx = None;
        }

        // ---- 外观:配色/密度/背景图(每帧应用,改动立即生效) ----
        // 先把界面风格盖回共享配置:切换按键组合会用 YAML 里的 look 整体替换
        // profile(老 YAML 没有 style 字段),不盖回来就会"一切变回默认"。
        self.stamp_style();
        let look = self.look();
        ctx.all_styles_mut(|style| theme::apply_style(style, &look, look.has_bg()));
        self.paint_background(ctx, &look);
        self.persist_look(&look);

        // 每帧联动:路径文本变化时自动补齐 scrcpy/server/adb 并刷新设备
        self.sync_suite();

        // ---- 全局快捷键: 撤销/重做/保存/另存为/刷新设备 ----
        // 键位捕获或取点进行中、正在输入文本时不拦截,保证 Ctrl+Z 等可作为待绑定键
        if self.waiting_key.is_none() && self.picking.is_none() && !ctx.egui_wants_keyboard_input()
        {
            let (k_undo, k_redo, k_save, k_save_as, k_refresh) = ctx.input(|i| {
                let ctrl = i.modifiers.ctrl;
                let shift = i.modifiers.shift;
                (
                    ctrl && !shift && i.key_pressed(egui::Key::Z),
                    ctrl && !shift && i.key_pressed(egui::Key::Y),
                    ctrl && !shift && i.key_pressed(egui::Key::S),
                    ctrl && shift && i.key_pressed(egui::Key::S),
                    !ctrl && !shift && i.key_pressed(egui::Key::F5),
                )
            });
            if k_undo {
                self.undo();
            } else if k_redo {
                self.redo();
            } else if k_save {
                self.save_profile();
            } else if k_save_as {
                self.dialog = Some(crate::filedialog::save_file("scrcpy-pad-profile.yaml"));
                self.dialog_purpose = DialogPurpose::SaveProfileAs;
            } else if k_refresh {
                self.resync();
                self.refresh_devices();
            }
        }

        // ---- 键位绑定(本窗口键盘回退)----
        // 等待绑定时焦点必在本窗口,egui 必然收到按键事件;
        // 由此不依赖全局捕获(Windows 钩子受 scrcpy 抢焦点影响,导致
        // 必须把 scrcpy 窗口置前才能绑定)。与 gui_rx 双路竞争,
        // waiting_key 只消费一次,天然去重。
        if self.waiting_key.is_some() {
            let key = ctx.input(|i| {
                i.events.iter().find_map(|e| match e {
                    egui::Event::Key {
                        key,
                        physical_key,
                        pressed: true,
                        repeat: false,
                        ..
                    } => {
                        // 全局捕获按物理键位(evdev 码)上报,优先 physical_key 保持一致
                        let k = (*physical_key).unwrap_or(*key);
                        egui_key_code(k)
                    }
                    _ => None,
                })
            });
            if let Some(code) = key {
                if let Some(slot) = self.waiting_key.take() {
                    self.assign_key(slot, code);
                }
            }
        }

        // ---- 响应范围缩放(Ctrl++ / Ctrl+-) ----
        // 修改响应范围期间,临时关掉 egui 内置的"Ctrl+/- 缩放整个界面":
        // 那个快捷键会先被 egui 吃掉,导致按下去界面变大、圆圈却纹丝不动。
        // 只有不在修改范围时才把它还给 egui。
        ctx.options_mut(|o| o.zoom_with_keyboard = self.resizing.is_none());
        if let Some(target) = self.resizing {
            let (zoom_in, zoom_out) = ctx.input(|input| {
                let ctrl = input.modifiers.ctrl;
                (
                    ctrl && (input.key_pressed(egui::Key::Plus)
                        || input.key_pressed(egui::Key::Equals)),
                    ctrl && input.key_pressed(egui::Key::Minus),
                )
            });
            if zoom_in || zoom_out {
                let factor = if zoom_in {
                    crate::keymap::RADIUS_ZOOM_FACTOR
                } else {
                    1.0 / crate::keymap::RADIUS_ZOOM_FACTOR
                };
                let mut g = self.shared.lock().unwrap();
                match target {
                    ResizeTarget::Bind(i) => {
                        if let Some(b) = g.profile.binds.get_mut(i) {
                            match &mut b.action {
                                Action::Tap { radius, .. } | Action::Hold { radius, .. } => {
                                    // 乘性缩放(每步按固定比例),符合自然的缩放手感;
                                    // 除浮点精度外不设上下限,仅防止缩到 0
                                    *radius = crate::keymap::zoom_radius(*radius, factor);
                                }
                                _ => {}
                            }
                        }
                    }
                    // 轮盘半径同样是乘性缩放(与键位圈手感一致),
                    // 只保证不缩到 0;轮盘半径不等于键位圈那个"默认半径",
                    // 所以不走键位圈的"接近默认值就精确复位"。
                    ResizeTarget::Wheel(i) => {
                        if let Some(w) = g.profile.wheels.get_mut(i) {
                            w.radius = (w.radius * factor).max(0.01);
                        }
                    }
                }
            }
        }

        // ---- 异步任务回收 ----
        if let Some(rx) = &self.connect_rx {
            if let Ok(r) = rx.try_recv() {
                self.connect_rx = None;
                match r {
                    Ok((server, client)) => {
                        self.log(format!(
                            "控制通道已连接,触摸坐标空间 {}x{}",
                            client.screen_w, client.screen_h
                        ));
                        let (w, h) = (client.screen_w, client.screen_h);
                        self.server = Some(server);
                        self.shared.lock().unwrap().control = Some(client);
                        // 连接后立刻校正坐标空间并升级旧配置(保证注入前已完成)
                        self.sync_display_space(w, h);
                    }
                    Err(e) => self.log(format!("连接失败: {e}")),
                }
            }
        }
        if let Some(rx) = &self.audio_rx {
            if let Ok(msg) = rx.try_recv() {
                self.audio_rx = None;
                self.log(msg);
            }
        }
        if let Some(rx) = &self.shot_rx {
            if let Ok(r) = rx.try_recv() {
                self.shot_rx = None;
                match r {
                    Ok(img) => {
                        let (w, h) = (img.width() as u32, img.height() as u32);
                        // 先建亮度网格再交给纹理:浮层要靠它做"自动对比"
                        self.shot_lum = LumaGrid::new(&img);
                        let tex = ctx.load_texture("screenshot", img, Default::default());
                        self.shot = Some((tex, w, h));
                        self.shot_zoom_auto = true;
                        self.shot_zoom = 1.0;
                        self.wheel_info = None; // 换了截图,之前点开的摇杆信息卡作废
                        self.log(format!("截图成功 {w}x{h},点击图像可取点"));
                        self.sync_display_space(w, h);
                    }
                    Err(e) => self.log(format!("截图失败: {e}")),
                }
            }
        }

        // ---- 文件对话框回收 ----
        if let Some(d) = &self.dialog {
            if let Some(res) = d.try_result() {
                self.dialog = None;
                let purpose = self.dialog_purpose;
                match (purpose, res) {
                    (_, None) => self.log("已取消"),
                    (DialogPurpose::ScrcpyExe, Some(p)) => {
                        self.scrcpy_path = p.display().to_string();
                        // 选中的是**目录**时(有些系统对话框允许选中文件夹)按目录处理
                        if PathBuf::from(&self.scrcpy_path).is_dir() {
                            self.scrcpy_dir = self.scrcpy_path.clone();
                            self.scrcpy_path.clear();
                            self.apply_scrcpy_dir();
                        } else {
                            self.log(format!("已选择 scrcpy: {}", self.scrcpy_path));
                            self.resync();
                            self.test_scrcpy();
                            // 选完立刻落盘并写日志:用户最在意的就是"这次到底记住了没有"
                            self.save_settings_now();
                        }
                    }
                    (DialogPurpose::ScrcpyDir, Some(p)) => {
                        self.scrcpy_dir = p.display().to_string();
                        self.log(format!("已选择 scrcpy 目录: {}", self.scrcpy_dir));
                        self.apply_scrcpy_dir();
                    }
                    (DialogPurpose::ServerJar, Some(p)) => {
                        self.server_path = p.display().to_string();
                        self.log(format!("已选择 server: {}", self.server_path));
                        self.resync();
                        self.test_scrcpy();
                        self.save_settings_now();
                    }
                    (DialogPurpose::AdbExe, Some(p)) => {
                        self.adb_path = p.display().to_string();
                        self.log(format!("已选择 adb: {}", self.adb_path));
                        self.resync();
                        self.save_settings_now();
                    }
                    (DialogPurpose::SaveLog, Some(p)) => {
                        let content = self.pending_log.take().unwrap_or_default();
                        match std::fs::write(&p, content) {
                            Ok(_) => self.log(format!("日志已保存到 {}", p.display())),
                            Err(e) => self.log(format!("日志保存失败: {e}")),
                        }
                    }
                    (DialogPurpose::SaveProfileAs, Some(p)) => {
                        self.stamp_profile_meta();
                        let text = render_config(&self.config_doc());
                        match text {
                            Ok(text) => match std::fs::write(&p, text) {
                                Ok(_) => self
                                    .log(format!("配置已另存到 {}(可直接分享该文件)", p.display())),
                                Err(e) => self.log(format!("另存失败: {e}")),
                            },
                            Err(e) => self.log(format!("序列化失败: {e}")),
                        }
                    }
                    (DialogPurpose::ChooseProfile, Some(p)) => {
                        match read_profile_at(&p) {
                            Ok(doc) => {
                                self.profile_path = p.clone();
                                self.apply_profile_switch(doc);
                                // 先取数、释放锁,再 log:避免 format! 参数里两次 lock 死锁
                                let (nb, nw) = {
                                    let g = self.shared.lock().unwrap();
                                    (g.profile.binds.len(), g.profile.wheels.len())
                                };
                                self.log(format!(
                                    "已选用配置: {} ({} 按键 / {} 轮盘)",
                                    p.display(),
                                    nb,
                                    nw
                                ));
                            }
                            Err(e) => self.log(format!("选用失败: {e}")),
                        }
                    }
                    (DialogPurpose::NewProfile, Some(p)) => match write_default_profile(&p) {
                        Ok(_) => {
                            self.profile_path = p.clone();
                            self.apply_profile_switch(ConfigFile::default());
                            self.log(format!("已新建空配置并切换: {}", p.display()));
                        }
                        Err(e) => self.log(format!("新建失败: {e}")),
                    },
                    (DialogPurpose::PickBackground, Some(p)) => {
                        let path = p.display().to_string();
                        let before = self.shared.lock().unwrap().profile.clone();
                        // 顶栏+左栏+中央区几乎铺满窗口,背景图只能透过面板显出来。
                        // 若"面板不透明度 × 压暗"已经把图压到基本看不见(用户会以为
                        // 选图没生效),就自动调到能看见的档位并写进日志说明。
                        let mut adjusted: Vec<String> = Vec::new();
                        {
                            let mut g = self.shared.lock().unwrap();
                            let look = &mut g.profile.look;
                            look.bg_path = path.clone();
                            let visible = f32::from(255 - look.panel_alpha) / 255.0
                                * f32::from(255 - look.bg_dim)
                                / 255.0;
                            if visible < 0.20 {
                                if look.panel_alpha > 150 {
                                    adjusted
                                        .push(format!("面板不透明度 {} → 150", look.panel_alpha));
                                    look.panel_alpha = 150;
                                }
                                if look.bg_dim > 120 {
                                    adjusted.push(format!("背景图压暗 {} → 120", look.bg_dim));
                                    look.bg_dim = 120;
                                }
                            }
                        }
                        self.push_undo_snapshot(before);
                        self.bg_tex = None; // 丢弃旧图,下一帧按新路径加载
                        self.bg_failed = None; // 允许对同一路径重新尝试
                        if adjusted.is_empty() {
                            self.log(format!("背景图已设为: {path}"));
                        } else {
                            self.log(format!(
                                "背景图已设为: {path}(自动调整 {};可在[外观]调整)",
                                adjusted.join("、")
                            ));
                        }
                    }
                }
            }
        }

        // ---- 日志环境信息收集完成 -> 打开另存对话框 ----
        if let Some(rx) = &self.loginfo_rx {
            if let Ok(info) = rx.try_recv() {
                self.loginfo_rx = None;
                let content = self.build_log_content(&info);
                self.pending_log = Some(content);
                let name = format!("scrcpy-pad-log-{}.txt", timestamp_compact());
                self.dialog = Some(crate::filedialog::save_file(&name));
                self.dialog_purpose = DialogPurpose::SaveLog;
                self.log("请选择日志保存位置...");
            }
        }

        // ---- 按键事件(绑定捕获 / 宏录制用;键盘与鼠标按键共用码空间) ----
        let mut gui_events = Vec::new();
        while let Ok(ev) = self.gui_rx.try_recv() {
            gui_events.push(ev);
        }
        for ev in gui_events {
            if let Some(rec) = self.macro_recording.as_mut() {
                if let CaptureEvent::Button { code, pressed } = ev {
                    Self::record_macro_button(rec, code, pressed, Instant::now());
                }
                continue;
            }
            if let (Some(slot), Some(code)) = (self.waiting_key, ev.pressed_code()) {
                self.waiting_key = None;
                self.assign_key(slot, code);
            }
        }
        if self
            .macro_recording
            .as_ref()
            .is_some_and(|rec| rec.last_event.elapsed().as_millis() >= rec.idle_ms as u128)
        {
            self.finish_macro_recording();
        }

        // ---- 引擎侧的解释性消息(映射开关、触点池满、配置重建等)写进日志 ----
        // 总开关键是在引擎线程里处理的,以前不留任何痕迹 —— 于是"映射到底开没开、
        // 刚才是谁把它关了"完全看不出来,用户只能反复按 F8 试。
        {
            let (msgs, recheck) = {
                let mut g = self.shared.lock().unwrap();
                (
                    std::mem::take(&mut g.notices),
                    std::mem::take(&mut g.space_recheck),
                )
            };
            for m in msgs {
                self.log(m);
            }
            // 引擎发现"空闲后又开始按键":重新确认一次触摸坐标空间。
            // 这期间手机可能转过屏,而坐标空间不更新的话注入会落到错的地方
            // (表现同样是"按键不反应"),以前只能靠关一次开一次映射来碰运气。
            if recheck {
                self.space_rx = None;
                self.refresh_display_space();
            }
        }

        // ---- 同步映射开关到键盘 grab(鼠标捕获由引擎维护) ----
        let (enabled, connected) = {
            let g = self.shared.lock().unwrap();
            (
                g.enabled,
                g.control
                    .as_ref()
                    .map(|c| c.is_connected())
                    .unwrap_or(false),
            )
        };
        self.grab_flag
            .store(enabled && self.grab_enabled, Ordering::Relaxed);
        // 由关到开(准备开打)时重新确认一次当前屏幕方向:
        // 手机随时可能横竖屏切换,坐标空间必须跟着当前方向走
        if enabled && !self.enabled_prev && connected {
            self.refresh_display_space();
        }
        self.enabled_prev = enabled;
        // 后台取回的显示器尺寸 -> 校正坐标空间(只改空间,不改任何已取好的坐标)
        if let Some(rx) = &self.space_rx {
            if let Ok((w, h)) = rx.try_recv() {
                self.space_rx = None;
                self.sync_display_space(w, h);
            }
        }
        // 鼠标是否已被捕获(引擎写入),仅用于界面显示;状态变化时记一条日志
        let mouse_captured = self.mouse_grab_flag.load(Ordering::Relaxed);
        let cursor_hidden = mouse_captured || self.cursor_hide_flag.load(Ordering::Relaxed);
        if mouse_captured != self.mouse_captured_prev {
            self.mouse_captured_prev = mouse_captured;
            // ShowCursor 必须在持有可见窗口的 GUI 线程调用；后台捕获线程只负责
            // 回中与 SetCursor(NULL)。这里做状态切换，帧末每帧再补一次隐藏。
            crate::capture::set_cursor_visible_from_ui(!mouse_captured);
            self.log(if mouse_captured {
                "鼠标已捕获: 视角由鼠标控制(按 Ctrl+Alt 可交还给系统)"
            } else {
                "鼠标已交还给系统(按 Ctrl+Alt 可收回)"
            });
        }
        if cursor_hidden != self.cursor_hidden_prev {
            self.cursor_hidden_prev = cursor_hidden;
            crate::capture::set_cursor_visible_from_ui(!cursor_hidden);
            self.log(if cursor_hidden {
                "系统鼠标已隐藏"
            } else {
                "系统鼠标已恢复显示"
            });
        }

        // 控制通道意外断开检测
        if self.server.is_some() && !connected && self.connect_rx.is_none() {
            // 先把仍然"按着"的触点统计出来:断开后这些 UP 发不出去,设备端会留下
            // 未抬起的触点(只能靠重连时重建)。引擎那边会在下一轮发现通道不可用
            // 并清空本地状态,这里只负责让用户知道发生了什么。
            let pending = {
                let g = self.shared.lock().unwrap();
                g.live.pointers
            };
            self.server = None;
            self.shared.lock().unwrap().control = None;
            if pending > 0 {
                self.log(format!(
                    "控制通道已断开(当时有 {pending} 个触点未抬起;重连后会自动重建,不必手动收拾)"
                ));
                crate::diag_warn!("app", "控制通道断开时仍有 {pending} 个触点未抬起");
            } else {
                self.log("控制通道已断开");
            }
        }

        // ================= 布局分派 =================
        // 鸿蒙风格是一套**重新设计**的界面(照 harmonyos-pc 的 HDC UI):
        // 顶栏 + 左侧导航 + 中央卡片 + 右侧屏幕预览 + 底部状态栏。
        // 默认/可视化风格保持原来的"顶栏 + 左栏 + 中央标签页"布局。
        if self.ui_style() == theme::UiStyle::Harmony {
            self.layout_harmony(ui, connected, enabled, mouse_captured);
        } else {
            // ================= 顶栏 =================
            egui::Panel::top("top").show(ui, |ui| {
                // 仅顶栏生效的细滚动条:非浮动、不随悬停加粗,避免盖住内容
                let top_scroll_style = {
                    let mut thin = ui.style().as_ref().clone();
                    thin.spacing.scroll.floating = false;
                    thin.spacing.scroll.bar_width = 4.0;
                    thin.spacing.scroll.handle_min_length = 10.0;
                    thin
                };
                ui.set_style(top_scroll_style);
                let th = self.theme();
                egui::ScrollArea::horizontal().show(ui, |ui| {
                    ui.horizontal(|ui| {
                        // 撤销/重做(与 Ctrl+Z / Ctrl+Y 等价)
                        if ui.button("⟲ 撤销").clicked() {
                            self.undo();
                        }
                        if ui.button("⟳ 重做").clicked() {
                            self.redo();
                        }
                        ui.separator();

                        ui.label("设备:");
                        let cur = self.serial();
                        egui::ComboBox::from_id_salt("dev")
                            .selected_text(if cur.is_empty() { "无设备" } else { &cur })
                            .show_ui(ui, |ui| {
                                for (i, d) in self.devices.iter().enumerate() {
                                    ui.selectable_value(&mut self.selected, i, d);
                                }
                            });
                        if ui.button("刷新").clicked() {
                            self.act_refresh_devices();
                        }

                        ui.separator();
                        self.ui_scrcpy_launch_row(ui);

                        ui.separator();
                        if connected {
                            ui.colored_label(th.ok, "● 控制已连接");
                            if ui.button("断开").clicked() {
                                self.disconnect();
                            }
                        } else if self.connect_rx.is_some() {
                            ui.label("连接中...");
                        } else if ui.button("连接控制").clicked() {
                            self.connect_control();
                        }

                        ui.separator();
                        let tk = self.shared.lock().unwrap().profile.toggle_key;
                        let tkn = key_name(tk).replace("KEY_", "");
                        let txt = if enabled {
                            format!("映射: 开 ({tkn})")
                        } else {
                            format!("映射: 关 ({tkn})")
                        };
                        let color = if enabled { th.ok } else { th.muted };
                        if ui
                            .add(
                                egui::Button::new(txt)
                                    .fill(color.gamma_multiply(0.3))
                                    .stroke(egui::Stroke::new(1.0, theme::with_alpha(color, 180))),
                            )
                            .clicked()
                        {
                            // 关闭映射必须走引擎的收尾(抬起所有按住的触点),否则手机上会"卡键"
                            self.act_toggle_mapping();
                        }

                        ui.separator();
                        if let Some(label) = self.pending_task_label() {
                            if ui
                                .add(
                                    egui::Button::new(format!("取消{label}"))
                                        .fill(th.danger.gamma_multiply(0.35))
                                        .stroke(egui::Stroke::new(
                                            1.0,
                                            theme::with_alpha(th.danger, 190),
                                        )),
                                )
                                .clicked()
                            {
                                self.cancel_pending_tasks();
                            }
                        }
                        if ui.button("关于").clicked() {
                            self.about_open = true;
                        }
                    });
                });
            });

            // ================= 左栏 =================
            egui::Panel::left("left")
                .min_size(260.0)
                // 显式给默认宽度并封顶:否则面板会按内容(长路径/多按钮行)自行撑宽,
                // 把中央的虚拟键盘/键位列表挤到需要横向滚动(视觉验证时发现)。
                .default_size(340.0)
                .max_size(430.0)
                .show(ui, |ui| {
                    // 日志是真正的可拖动底部面板：和截图取点框同一套 resizable panel。
                    let log_default = self
                        .log_height
                        .clamp(80.0, (ui.available_height() - 160.0).max(80.0));
                    egui::Panel::bottom("left_log_panel")
                        .resizable(true)
                        .default_size(log_default)
                        .min_size(80.0)
                        .max_size((ui.available_height() - 120.0).max(80.0))
                        .show(ui, |ui| {
                            self.ui_log_card(ui, ui.available_height());
                        });

                    // 左栏上部:配置区。自身可滚动(内容再多也只占这块,不会把日志顶出屏幕)
                    let cfg_max_h = ui.available_height().max(120.0);
                    egui::ScrollArea::vertical()
                        .id_salt("left_cfg")
                        .max_height(cfg_max_h)
                        .show(ui, |ui| {
                            if ui
                                .button("使用说明")
                                .on_hover_text("打开使用说明(独立窗口:左侧章节索引,右侧图文与示例)")
                                .clicked()
                            {
                                self.help_open = true;
                            }
                            ui.separator();
                            if let Some(err) = &self.capture_err {
                                ui.colored_label(self.theme().danger, "输入捕获不可用:");
                                ui.label(err);
                                ui.separator();
                            }

                            self.ui_profile_config(ui);

                            ui.separator();
                            // ---- 中段:外观 / 总开关 / 键位组合 / 引擎状态 / 诊断 ----
                            // 可视化风格:外观、键位组合、诊断变成三张"浏览器标签页"夹在
                            // 配置与 scrcpy 管理之间(默认选中键位组合);总开关等即时控件仍在标签页之外。
                            // 默认/鸿蒙风格:维持折叠头(现状不变)。
                            let visual = self.ui_style() == theme::UiStyle::Visual;
                            if visual {
                                let th = self.theme();
                                ui.horizontal(|ui| {
                                    for tab in [LeftTab::Schemes, LeftTab::Look, LeftTab::Diag] {
                                        if Self::tab_button(
                                            ui,
                                            tab.label(),
                                            self.left_tab == tab,
                                            th.ok,
                                        ) {
                                            self.left_tab = tab;
                                        }
                                    }
                                });
                                match self.left_tab {
                                    LeftTab::Schemes => self.ui_schemes(ui),
                                    LeftTab::Look => self.ui_look(ui),
                                    LeftTab::Diag => self.ui_diagnostics(ui),
                                }
                            } else {
                                egui::CollapsingHeader::new("外观（开发中）")
                                    .default_open(false)
                                    .show(ui, |ui| {
                                        self.ui_look(ui);
                                    });
                            }

                            self.ui_toggle_key_row(ui);

                            if !visual {
                                ui.separator();
                                egui::CollapsingHeader::new("按键组合 / 切换键位")
                                    .default_open(false)
                                    .show(ui, |ui| {
                                        self.ui_schemes(ui);
                                    });
                            }

                            // 引擎运行状态:把"按了没反应"的原因直接摆出来
                            self.ui_engine_status(ui);

                            if !visual {
                                ui.separator();
                                egui::CollapsingHeader::new("诊断")
                                    .default_open(false)
                                    .show(ui, |ui| {
                                        self.ui_diagnostics(ui);
                                    });
                            }

                            ui.separator();
                            self.ui_scrcpy_manage(ui);
                        }); // ← 配置区滚动到这里结束
                });

            // ================= 中央区 =================
            egui::CentralPanel::default().show(ui, |ui| {
                // Screenshot picker is pinned to the bottom of the right-hand body.
                egui::Panel::bottom("screenshot_picker")
                    .resizable(true)
                    .default_size(330.0)
                    .min_size(170.0)
                    .show(ui, |ui| {
                        egui::ScrollArea::both()
                            .id_salt("screenshot_panel_scroll")
                            .show(ui, |ui| {
                                self.ui_picker(ui);
                            });
                    });

                egui::CentralPanel::default().show(ui, |ui| {
                    // 可视化风格:三张标签页(键位 / FPS设置 / 其他功能),摇杆并入键位页;
                    // 其余风格:维持四张(键位/摇杆/FPS/其他)。
                    let visual = self.ui_style() == theme::UiStyle::Visual;
                    ui.horizontal(|ui| {
                        let th = self.theme();
                        let tabs: &[RightTab] = if visual {
                            &[
                                RightTab::Keys,
                                RightTab::Macro,
                                RightTab::Fps,
                                RightTab::Other,
                            ]
                        } else {
                            &[
                                RightTab::Keys,
                                RightTab::Macro,
                                RightTab::Wheels,
                                RightTab::Fps,
                                RightTab::Other,
                            ]
                        };
                        for tab in tabs {
                            if Self::tab_button(ui, tab.label(), self.right_tab == *tab, th.ok) {
                                self.right_tab = *tab;
                            }
                        }
                    });
                    ui.separator();
                    let content_height = ui.available_height().max(160.0);
                    egui::ScrollArea::both()
                        .max_height(content_height)
                        .id_salt("right_tab_content")
                        .show(ui, |ui| {
                            if visual {
                                match self.right_tab {
                                    // 键位页:虚拟键盘+鼠标取键 + 操作栏 + 全部既有列表(多加,不是选择)
                                    RightTab::Keys => self.ui_visual_keys(ui),
                                    RightTab::Macro => self.ui_macro_page(ui),
                                    // FPS 页:虚拟键盘只能设置 FPS 相关键 + 既有 FPS 面板
                                    RightTab::Fps => self.ui_visual_fps(ui, mouse_captured),
                                    RightTab::Other => self.ui_other(ui),
                                    // 摇杆已并入键位页,此页在可视化风格下不可达
                                    RightTab::Wheels => self.ui_wheels(ui),
                                }
                            } else {
                                match self.right_tab {
                                    RightTab::Keys => self.ui_binds(ui),
                                    RightTab::Macro => self.ui_macro_page(ui),
                                    RightTab::Wheels => self.ui_wheels(ui),
                                    RightTab::Fps => self.ui_aim(ui, mouse_captured),
                                    RightTab::Other => self.ui_other(ui),
                                }
                            }
                        });
                });
            });
        } // ← 布局分派:非鸿蒙分支到这里结束

        // ================= 关于窗口 =================
        let mut open_license = false;
        if self.about_open {
            egui::Window::new("关于")
                .open(&mut self.about_open)
                .show(ctx, |ui| {
                    ui.heading("scrcpy-pad");
                    ui.label(format!("版本 v{}", env!("CARGO_PKG_VERSION")));
                    ui.label(format!("作者: {AUTHOR}"));
                    ui.hyperlink(REPO_URL);
                    ui.separator();
                    ui.label("基于 scrcpy 控制协议的键鼠映射游戏控制台");
                    ui.label("本程序以 MIT 许可证发布并遵循该协议:可自由使用、修改与再分发,");
                    ui.label("但须保留版权声明与许可声明。");
                    ui.label("MIT License © 2026 Azrl");
                    ui.separator();
                    if ui.button("许可证").clicked() {
                        open_license = true;
                    }
                });
        }
        if open_license {
            self.license_open = true;
        }

        // ================= 许可证窗口 =================
        if self.license_open {
            egui::Window::new("MIT 许可证")
                .open(&mut self.license_open)
                .default_width(560.0)
                .show(ctx, |ui| {
                    ui.label("以下为本程序使用的 MIT 许可证全文:");
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .max_height(420.0)
                        .show(ui, |ui| {
                            ui.monospace(LICENSE_TEXT);
                        });
                });
        }

        // ================= 使用说明窗口 =================
        {
            let th = self.theme();
            self.help.show(ctx, &mut self.help_open, &th);
        }

        // ================= 调试信息悬浮窗 =================
        self.ui_debug_overlay(ctx);
        self.ui_log_window(ctx);
        self.ui_macro_virtual_window(ctx);

        // ================= 参数助手窗口 =================
        self.ui_args_helper(ctx);

        // ================= 滑动曲线参数编辑窗口 =================
        self.ui_easing_editor(ctx);

        // ---- 本帧的撤销点统一入栈 ----
        // UI 里有些控件直接在持锁状态下改配置,无法即时加锁压栈,故先存快照、帧末统一记录;
        // 同一帧的多个改动合并成一个撤销步。若本帧已有显式撤销点则丢弃,避免重复。
        let pending = self.pending_undo.take();
        if let Some(before) = pending {
            // 注意顺序与合并规则:
            //  - 帧中没有显式撤销点:这份快照代表整帧改动的"改动前"状态,直接入栈;
            //  - 帧中已有显式撤销点(某个按钮已压过栈):那份快照仍代表"更早"的状态,
            //    必须保留,否则连拖多次后撤销只能回到拖动中途。
            self.push_undo_snapshot(before);
        }
        self.undo_frame_marked = false;

        // ---- 组合表同步 + "上次用哪套"落盘 ----
        // 界面上的改动都写在 `profile` 上,组合表是存档,帧末统一收回;
        // 引擎用切换键换组合时界面看不见,只能靠比对下标发现。
        self.sync_scheme_state();

        // ---- 本帧的程序级设置统一落盘(路径改动 / 启动参数 / 选中设备) ----
        self.remember_now();
        self.persist_settings();

        if mouse_captured || self.cursor_hide_flag.load(Ordering::Relaxed) {
            crate::capture::hide_cursor_shape_from_ui();
        }
        ctx.request_repaint_after(Duration::from_millis(120));
    }
}

impl Drop for PadApp {
    fn drop(&mut self) {
        self.cursor_hide_flag.store(false, Ordering::Relaxed);
        crate::capture::set_cursor_visible_from_ui(true);
    }
}

impl PadApp {
    /// 左侧"按键组合"面板:选择/改名/增删组合,以及切换键位的增删改。
    ///
    /// 全部改动只写 `Shared`(组合表 + 切换键表),真正的"抬起旧触点再换车"
    /// 由引擎按结构指纹兜底(与"开打中改配置"同一条路),这里不碰引擎状态。
    fn ui_schemes(&mut self, ui: &mut egui::Ui) {
        ui.heading("按键组合");
        ui.small(
            "每套组合是一份独立的键位配置;切换键按下即整套换车。\n\
             默认配置文件 profile.yaml 里装的就是这些组合。",
        );

        let (names, active, n) = {
            let g = self.shared.lock().unwrap();
            (
                g.schemes.iter().map(|p| p.name.clone()).collect::<Vec<_>>(),
                g.active_scheme,
                g.schemes.len(),
            )
        };

        let mut pick: Option<usize> = None;
        let mut del: Option<usize> = None;
        for i in 0..n {
            ui.horizontal(|ui| {
                // radio 只用来看"哪套在生效",点它才切(再点当前这套不做事)
                if ui.radio(active == i, "").clicked() && i != active {
                    pick = Some(i);
                }
                let mut name = names[i].clone();
                if ui
                    .add(egui::TextEdit::singleline(&mut name).desired_width(112.0))
                    .changed()
                {
                    self.rename_scheme(i, name);
                }
                if ui
                    .add_enabled(n > 1, egui::Button::new("删除").small())
                    .on_hover_text("至少保留一套组合")
                    .clicked()
                {
                    del = Some(i);
                }
            });
        }
        ui.horizontal(|ui| {
            if ui
                .button("新建组合")
                .on_hover_text("复制当前这套并切过去")
                .clicked()
            {
                self.add_scheme();
            }
        });

        ui.separator();
        let mut fast = self.shared.lock().unwrap().fast_switch_enabled;
        if ui
            .checkbox(&mut fast, "启用快速切换")
            .on_hover_text("只有勾选后，切换键才会真正切换组合；不勾选也可在[其他功能]里预先设置")
            .changed()
        {
            self.shared.lock().unwrap().fast_switch_enabled = fast;
            self.scheme_dirty = true;
            self.log(if fast {
                "已启用快速切换"
            } else {
                "已停用快速切换（键位设置保留）"
            });
        }
        ui.small("切换键的具体按键/组合键和目标组合，在[其他功能 → 切换键位]里设置。");

        if let Some(i) = pick {
            self.select_scheme(i);
        }
        if let Some(i) = del {
            self.delete_scheme(i);
        }
    }

    /// 组合改名(生效中的那套连同 `profile.name` 一起改,两者必须一致)
    fn rename_scheme(&mut self, i: usize, name: String) {
        {
            let mut g = self.shared.lock().unwrap();
            let Some(slot) = g.schemes.get_mut(i) else {
                return;
            };
            slot.name = name;
            if g.active_scheme == i {
                g.profile.name = g.schemes[i].name.clone();
            }
        }
        // 名字是组合表的一部分,改完该自动落盘(见 sync_scheme_state)
        self.scheme_dirty = true;
    }

    fn ui_binds(&mut self, ui: &mut egui::Ui) {
        self.ui_binds_list(ui);
        // ---- 新增绑定 + 组合键 ----
        self.ui_binds_new(ui);
        self.apply_pending_scroll(ui);
    }

    fn apply_pending_scroll(&mut self, ui: &mut egui::Ui) {
        if self.scroll_to_new && self.ui_style() != theme::UiStyle::Harmony {
            ui.scroll_to_cursor(Some(egui::Align::BOTTOM));
            self.scroll_to_new = false;
        }
    }

    /// 键位绑定列表(可视化风格的键位页也用它;那里新增草稿走 vk 操作栏,不渲染新增节)。
    fn ui_binds_list(&mut self, ui: &mut egui::Ui) {
        ui.heading("按键映射");
        ui.label("勾选每行的 [仅 FPS]：该物理键只在 FPS 模式生效并覆盖同键普通映射。")
            .on_hover_text("FPS 模式运行时优先使用“仅 FPS”键位；退出或按住才退出后恢复普通映射。");
        let mut to_delete: Option<usize> = None;
        let bind_count = self.shared.lock().unwrap().profile.binds.len();

        for i in 0..bind_count {
            let is_macro = {
                let g = self.shared.lock().unwrap();
                matches!(
                    g.profile.binds.get(i).map(|b| &b.action),
                    Some(Action::Macro(_))
                )
            };
            if is_macro {
                continue;
            }
            if let Some(d) = ui.horizontal(|ui| self.ui_bind_row(ui, i)).inner {
                to_delete = Some(d);
            }
        }
        if let Some(i) = to_delete {
            self.delete_bind(i);
        }
    }

    /// 渲染第 i 条键位绑定(单行,完整编辑器)。返回 Some(i) 表示该条要删除。
    /// ui_binds 的列表与可视化风格的操作栏共用这一份,保证两处行为完全一致。
    fn ui_bind_row(&mut self, ui: &mut egui::Ui, i: usize) -> Option<usize> {
        let mut to_delete: Option<usize> = None;
        ui.horizontal(|ui| {
            let (key, kind, is_swipe, is_point, fps_only) = {
                let g = self.shared.lock().unwrap();
                let b = &g.profile.binds[i];
                (
                    b.key,
                    b.action.kind_name(),
                    matches!(b.action, Action::Swipe(_)),
                    matches!(b.action, Action::Tap { .. } | Action::Hold { .. }),
                    b.fps_only,
                )
            };
            ui.label(format!("[{kind}]"));
            let waiting = self.waiting_key == Some(KeySlot::Bind(i));
            if Self::key_button(ui, waiting, Some(key)).clicked() {
                // 开始重新捕获按键:按最新操作优先,退出正在进行的改范围/取点
                self.resizing = None;
                self.picking = None;
                self.waiting_key = Some(KeySlot::Bind(i));
            }

            let mut fps_only_edit = fps_only;
            if ui
                .checkbox(&mut fps_only_edit, "仅 FPS")
                .on_hover_text("开启后该键只在 FPS 模式生效,退出 FPS 自动抬起")
                .changed()
            {
                let before = {
                    let mut g = self.shared.lock().unwrap();
                    let before = g.profile.clone();
                    if let Some(b) = g.profile.binds.get_mut(i) {
                        b.fps_only = fps_only_edit;
                    }
                    before
                };
                self.push_undo_snapshot(before);
            }

            if is_swipe {
                // 滑动:控件已展示起终点/时长/曲线/轨迹,不再重复 desc
                let mut pick = self.picking;
                let mut easing_edit = self.easing_edit;
                let mut swipe_edit = false;
                {
                    let mut g = self.shared.lock().unwrap();
                    // 撤销快照只取这一个键位:每次拖动/聚焦都会压一份,
                    // 而整份 Profile 的深拷贝与"键位总数"成正比,这里没必要。
                    let before = g.profile.binds.get(i).cloned();
                    if let Some(b) = g.profile.binds.get_mut(i) {
                        if let Action::Swipe(s) = &mut b.action {
                            swipe_edit = swipe_controls(
                                ui,
                                s,
                                &mut pick,
                                &mut easing_edit,
                                CoordSlot::SwipeStart(i),
                                CoordSlot::SwipeEnd(i),
                                CoordSlot::CircleAngle(i),
                                EasingEditTarget::Bind(i),
                            );
                        }
                    }
                    if swipe_edit {
                        // 拖拽/聚焦开始那一帧:快照即"修改之前"的状态
                        if let Some(before) = before {
                            let mut snapshot = g.profile.clone();
                            snapshot.binds[i] = before;
                            self.pending_undo = Some(snapshot);
                        }
                    }
                }
                if let Some(slot) = pick {
                    self.begin_pick(slot);
                } else {
                    self.picking = None;
                }
                self.easing_edit = easing_edit;
            } else if is_point {
                // 点按/长按:坐标、时长、响应范围、取点、长短按切换
                let (mut do_resize, mut do_convert, mut do_pick) = (false, false, false);
                let mut point_edit = false;
                // 界面按像素显示与编辑(便于对着截图核对),配置里始终存相对值
                let m = self.mapper();
                {
                    let mut g = self.shared.lock().unwrap();
                    // 同上:撤销快照只需这一个键位
                    let before = g.profile.binds.get(i).cloned();
                    if let Some(b) = g.profile.binds.get_mut(i) {
                        match &mut b.action {
                            Action::Tap {
                                x,
                                y,
                                duration_ms,
                                radius,
                            } => {
                                ui.label("x:");
                                let mut px = m.x(*x);
                                let r = ui.add(egui::DragValue::new(&mut px).range(COORD_RANGE));
                                if r.changed() {
                                    *x = m.rel_x(px);
                                }
                                point_edit |= r.drag_started() || r.gained_focus();
                                ui.label("y:");
                                let mut py = m.y(*y);
                                let r = ui.add(egui::DragValue::new(&mut py).range(COORD_RANGE));
                                if r.changed() {
                                    *y = m.rel_y(py);
                                }
                                point_edit |= r.drag_started() || r.gained_focus();
                                ui.label("时长ms:");
                                let r = ui
                                    .add(egui::DragValue::new(duration_ms).range(0..=5000))
                                    .on_hover_text("0=按下不松手,直到再按一次");
                                point_edit |= r.drag_started() || r.gained_focus();
                                ui.label("范围:");
                                let mut pr = m.len(*radius);
                                let r =
                                    ui.add(egui::DragValue::new(&mut pr).range(0.01..=100000.0));
                                if r.changed() {
                                    *radius = m.rel_len(pr);
                                }
                                point_edit |= r.drag_started() || r.gained_focus();
                            }
                            Action::Hold { x, y, radius } => {
                                ui.label("x:");
                                let mut px = m.x(*x);
                                let r = ui.add(egui::DragValue::new(&mut px).range(COORD_RANGE));
                                if r.changed() {
                                    *x = m.rel_x(px);
                                }
                                point_edit |= r.drag_started() || r.gained_focus();
                                ui.label("y:");
                                let mut py = m.y(*y);
                                let r = ui.add(egui::DragValue::new(&mut py).range(COORD_RANGE));
                                if r.changed() {
                                    *y = m.rel_y(py);
                                }
                                point_edit |= r.drag_started() || r.gained_focus();
                                ui.label("范围:");
                                let mut pr = m.len(*radius);
                                let r =
                                    ui.add(egui::DragValue::new(&mut pr).range(0.01..=100000.0));
                                if r.changed() {
                                    *radius = m.rel_len(pr);
                                }
                                point_edit |= r.drag_started() || r.gained_focus();
                            }
                            _ => {}
                        }
                    }
                    if point_edit {
                        // 拖拽/聚焦开始那一帧:快照即"修改之前"的状态
                        if let Some(before) = before {
                            let mut snapshot = g.profile.clone();
                            snapshot.binds[i] = before;
                            self.pending_undo = Some(snapshot);
                        }
                    }
                }
                // 取点
                let waiting_p = self.picking == Some(CoordSlot::Bind(i));
                if ui
                    .button(if waiting_p {
                        "点击截图..."
                    } else {
                        "取点"
                    })
                    .clicked()
                {
                    do_pick = true;
                }
                // 长短按一键切换(坐标/范围保留)
                let is_tap = {
                    let g = self.shared.lock().unwrap();
                    matches!(
                        g.profile.binds.get(i).map(|b| &b.action),
                        Some(Action::Tap { .. })
                    )
                };
                if ui
                    .button(if is_tap { "转长按" } else { "转点按" })
                    .clicked()
                {
                    do_convert = true;
                }
                if ui.button("修改响应范围").clicked() {
                    do_resize = true;
                }
                if do_pick {
                    self.begin_pick(CoordSlot::Bind(i));
                }
                if do_convert {
                    self.push_undo();
                    let mut g = self.shared.lock().unwrap();
                    if let Some(b) = g.profile.binds.get_mut(i) {
                        b.action = match &b.action {
                            Action::Tap { x, y, radius, .. } => Action::Hold {
                                x: *x,
                                y: *y,
                                radius: *radius,
                            },
                            Action::Hold { x, y, radius } => Action::Tap {
                                x: *x,
                                y: *y,
                                duration_ms: crate::keymap::DEFAULT_TAP_DURATION_MS,
                                radius: *radius,
                            },
                            _ => unreachable!(),
                        };
                    }
                }
                if do_resize {
                    self.begin_resize(i);
                }
            } else {
                // 系统键
                let desc = {
                    let g = self.shared.lock().unwrap();
                    g.profile.binds[i].action.describe()
                };
                ui.label(desc);
                let mut g = self.shared.lock().unwrap();
                if let Some(b) = g.profile.binds.get_mut(i) {
                    if let Action::AndroidKey { keycode } = &mut b.action {
                        ui.label("keycode:");
                        ui.add(egui::DragValue::new(keycode).range(0..=999));
                    }
                }
            }

            if ui.button("删除").clicked() {
                to_delete = Some(i);
            }
        });
        to_delete
    }

    /// 删除第 i 条键位绑定(撤销 + 移除 + 交互态复位)。
    fn delete_bind(&mut self, i: usize) {
        self.push_undo();
        self.shared.lock().unwrap().profile.binds.remove(i);
        // 交互态若指向刚删掉的条目就一并收尾,免得残留在失效索引上
        if self.resizing == Some(ResizeTarget::Bind(i)) {
            self.resizing = None;
        }
        self.log("已删除绑定");
    }

    /// 新增绑定(草稿编辑器)+ 组合键列表。ui_binds 与可视化操作栏共用。
    fn ui_binds_new(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.label("新增:");
        ui.horizontal(|ui| {
            let waiting = self.waiting_key == Some(KeySlot::NewBind);
            let had_draft = self.draft.key.is_some()
                || self.draft_active
                || self.picking == Some(CoordSlot::NewBind)
                || self.waiting_key == Some(KeySlot::NewBind);
            if Self::key_button(ui, waiting, self.draft.key).clicked() {
                // Re-capturing an existing draft must preserve its point and radius.
                self.resizing = None;
                self.picking = None;
                self.waiting_key = Some(KeySlot::NewBind);
                self.draft_active = true;
                // 默认风格的新增节建的是普通键位(仅 FPS 草稿只在可视化 FPS 页产生)
                self.draft.fps_only = false;
                if !had_draft {
                    self.reset_draft_to_screen();
                }
            }
            let kind_before = self.draft.kind;
            egui::ComboBox::from_id_salt("newkind")
                .selected_text(KIND_NAMES[self.draft.kind])
                .show_ui(ui, |ui| {
                    for (i, n) in KIND_NAMES.iter().enumerate() {
                        ui.selectable_value(&mut self.draft.kind, i, *n);
                    }
                });
            if self.draft.kind != kind_before {
                // 切换新增类型即视为开始配置,显示对应预览
                self.draft_active = true;
            }
            match self.draft.kind {
                0 | 1 => {
                    ui.label("x:");
                    ui.add(egui::DragValue::new(&mut self.draft.x).range(COORD_RANGE));
                    ui.label("y:");
                    ui.add(egui::DragValue::new(&mut self.draft.y).range(COORD_RANGE));
                    if self.draft.kind == 0 {
                        ui.label("时长ms:");
                        ui.add(
                            egui::DragValue::new(&mut self.draft.tap_duration_ms).range(0..=5000),
                        )
                        .on_hover_text("0=按下不松手,直到再按一次");
                    }
                    ui.label("范围:");
                    ui.add(egui::DragValue::new(&mut self.draft.radius).range(0.01..=100000.0));
                    let waiting_p = self.picking == Some(CoordSlot::NewBind);
                    if ui
                        .button(if waiting_p {
                            "点击截图..."
                        } else {
                            "取点"
                        })
                        .clicked()
                    {
                        self.begin_pick(CoordSlot::NewBind);
                        self.draft_active = true;
                    }
                }
                2 => {
                    let mut pick = self.picking;
                    let pick_before = pick;
                    let mut easing_edit = self.easing_edit;
                    let mut tmp = Swipe {
                        // 草稿是像素坐标,滑动控件按像素编辑
                        start: (
                            self.draft.swipe_start.0 as f32,
                            self.draft.swipe_start.1 as f32,
                        ),
                        end: (self.draft.swipe_end.0 as f32, self.draft.swipe_end.1 as f32),
                        duration_ms: self.draft.swipe_duration_ms,
                        easing: self.draft.swipe_easing,
                        path: self.draft.swipe_path,
                    };
                    swipe_controls(
                        ui,
                        &mut tmp,
                        &mut pick,
                        &mut easing_edit,
                        CoordSlot::NewSwipeStart,
                        CoordSlot::NewSwipeEnd,
                        CoordSlot::NewCircleAngle,
                        EasingEditTarget::New,
                    );
                    self.draft.swipe_start = (tmp.start.0 as i32, tmp.start.1 as i32);
                    self.draft.swipe_end = (tmp.end.0 as i32, tmp.end.1 as i32);
                    self.draft.swipe_duration_ms = tmp.duration_ms;
                    self.draft.swipe_easing = tmp.easing;
                    self.draft.swipe_path = tmp.path;
                    if let Some(slot) = pick {
                        self.begin_pick(slot);
                    } else {
                        self.picking = None;
                    }
                    self.easing_edit = easing_edit;
                    if pick != pick_before {
                        // 点取起点/终点/出发点即视为开始配置,显示轨迹预览
                        self.draft_active = true;
                    }
                }
                3 => {
                    ui.label("keycode(返回=4 主页=3):");
                    ui.add(egui::DragValue::new(&mut self.draft.keycode).range(0..=999));
                }
                _ => {
                    ui.horizontal(|ui| {
                        ui.label("keycode(返回=4 主页=3):");
                        ui.add(egui::DragValue::new(&mut self.draft.keycode).range(0..=999));
                    });
                }
            }
            if ui.button("新增").clicked() {
                self.commit_draft();
            }
        });

        self.ui_combos(ui);
    }

    /// 把当前草稿提交为一条键位绑定。键未捕获时只记日志,返回 false。
    /// 默认风格的[添加]按钮与可视化操作栏的[确定添加]共用,行为完全一致。
    fn commit_draft(&mut self) -> bool {
        let Some(key) = self.draft.key else {
            self.log("请先捕获按键");
            return false;
        };
        self.push_undo();
        // 草稿是像素坐标,入配置前换算成相对值
        let m = self.mapper();
        let (dx, dy) = (self.draft.x, self.draft.y);
        let action = match self.draft.kind {
            0 => Action::Tap {
                x: m.rel_x(dx),
                y: m.rel_y(dy),
                duration_ms: self.draft.tap_duration_ms,
                radius: m.rel_len(self.draft.radius),
            },
            1 => Action::Hold {
                x: m.rel_x(dx),
                y: m.rel_y(dy),
                radius: m.rel_len(self.draft.radius),
            },
            2 => Action::Swipe(Swipe {
                start: (
                    m.rel_x(self.draft.swipe_start.0),
                    m.rel_y(self.draft.swipe_start.1),
                ),
                end: (
                    m.rel_x(self.draft.swipe_end.0),
                    m.rel_y(self.draft.swipe_end.1),
                ),
                duration_ms: self.draft.swipe_duration_ms,
                easing: self.draft.swipe_easing,
                path: self.draft.swipe_path,
            }),
            3 => Action::AndroidKey {
                keycode: self.draft.keycode,
            },
            _ => Action::AndroidKey {
                keycode: self.draft.keycode,
            },
        };
        self.shared.lock().unwrap().profile.binds.push(KeyBind {
            key,
            action,
            // FPS 页建的草稿只入"仅 FPS"键位;键位页建的为普通键位
            fps_only: self.draft.fps_only,
        });
        self.draft.key = None;
        // 添加完成即彻底收尾:草稿预览、取点、改范围等交互全部结束,
        // 不再"刚添加完又停在取点状态"。之后想改,再点该条的[取点]即可。
        self.draft_active = false;
        self.picking = None;
        self.resizing = None;
        self.log("已新增按键映射");
        self.scroll_to_new = true;
        true
    }

    // ==================== 可视化风格:虚拟键盘交互 ====================
    //
    // 交互模型(用户原话的落实):
    //   * 点空闲键 -> 新建草稿并立刻进入截图取点(操作栏出现,可[取消取点]);
    //   * 点已设置的键 -> 选中它,操作栏显示与右栏列表**同一套**编辑器
    //     (ui_bind_row / ui_vk_special —— 功能是"多加"进去的,不是替换);
    //   * 取点中,虚拟键盘上对应键呼吸闪烁(正在取点中时相应位置会亮起);
    //   * 键位页与 FPS 页的**显示能力完全一样**(同一份 vk_lights / 同一组勾选),
    //     区别只有两点:FPS 页默认只显示"仅 FPS"键位(普通映射/组合键/摇杆要手动勾),
    //     以及 FPS 页只允许编辑 FPS 相关的键。

    /// 键位页:显示开关 + 虚拟键盘/鼠标 + 操作栏 + 全部既有列表(列表全保留)。
    fn ui_visual_keys(&mut self, ui: &mut egui::Ui) {
        ui.heading("键位(可视化)");
        ui.horizontal(|ui| self.vk_filters.ui(ui, "vk_filters_keys"))
            .response
            .on_hover_text("控制虚拟键盘上哪几类已设置的键亮起(不影响下方列表与实际映射)");
        self.ui_vk_area(ui, false);
        self.ui_vk_panel(ui, false);
        if self.vk_add_kind == VkAddKind::Key {
            self.ui_binds_list(ui);
            ui.separator();
            if !self.vk_bottom_add_active {
                if Self::tab_button(ui, "＋ 新增按键映射", false, self.theme().ok) {
                    self.cancel_draft();
                    self.reset_draft_to_screen();
                    self.draft.key = None;
                    self.draft.fps_only = false;
                    self.draft_active = true;
                    self.vk_sel = None;
                    self.waiting_key = Some(KeySlot::NewBind);
                    self.vk_bottom_add_active = true;
                    self.scroll_to_new = true;
                }
            } else {
                self.ui_vk_draft(ui);
            }
        }
        self.apply_pending_scroll(ui);
    }

    /// FPS 页:虚拟键盘(与键位页同一套显示能力,默认只亮 FPS 键位)
    /// + 既有 FPS 面板(全保留)。功能上只能设置 FPS 相关键。
    fn ui_visual_fps(&mut self, ui: &mut egui::Ui, captured: bool) {
        ui.horizontal(|ui| self.vk_fps_filters.ui(ui, "vk_filters_fps"))
            .response
            .on_hover_text(
                "与[键位]页一样:想在这里看到普通映射/组合键/摇杆,勾上即可。\n\
                 默认只显示 FPS 独有的键位。",
            );
        self.ui_vk_area(ui, true);
        self.ui_vk_panel(ui, true);
        self.apply_pending_scroll(ui);
        let _ = captured; // FPS 面板现在由“新增项目 → 瞄准锚点”在操作栏中展开
    }

    /// 虚拟键盘 + 虚拟鼠标 + 下方操作栏。fps_tab=true 时空闲键建"仅 FPS"草稿,
    /// 非 FPS 目标不可选(FPS 页功能上只能设置 FPS)。
    fn ui_vk_area(&mut self, ui: &mut egui::Ui, fps_tab: bool) {
        // 键亮表:键码 -> (语义色, 悬停说明)。一次锁全建好,绘制期间不再碰配置锁。
        let filters = if fps_tab {
            self.vk_fps_filters
        } else {
            self.vk_filters
        };
        let lights = self.vk_lights(filters);
        // 取点中会呼吸闪烁的键(正在取点中时虚拟键盘相应位置亮起)
        let pulse_code = self.vk_pulse_code();
        // 操作栏正在编辑的键(加亮 + 白描边)
        let sel_code = self.vk_sel_code();
        let th = self.theme();
        let ok = th.ok;
        let combo_waiting =
            self.vk_add_kind == VkAddKind::Combo && !self.vk_combo_pending.is_empty();
        let combo_first = self.vk_combo_pending.first().copied();
        let pulse = if pulse_code.is_some() && !combo_waiting {
            Some(ui.time() * 4.0)
        } else {
            None
        };
        let pulse_time = ui.time() * 4.0;
        // 绘制参数:已设置的键 = 语义色键帽(亮起);空闲键 = 老式机械键帽的空壳
        // (半透明但看得见,见 KeyLook::idle)。选中键再加亮;取点中的键由 show_* 呼吸闪烁。
        let look_of = move |code: u16| {
            let mut look = if let Some((c, t)) = lights.get(&code) {
                keyboard::KeyLook {
                    fill: theme::with_alpha(*c, 176),
                    side: theme::with_alpha(darken(*c, 60), 224),
                    stroke: egui::Color32::from_rgba_unmultiplied(255, 255, 255, 190),
                    text: egui::Color32::WHITE,
                    tip: Some(t.clone()),
                }
            } else {
                keyboard::KeyLook::idle()
            };
            if combo_waiting && Some(code) == combo_first {
                look.fill = theme::with_alpha(ok, 220);
                look.side = theme::with_alpha(darken(ok, 70), 235);
                look.stroke = egui::Color32::WHITE;
                look.tip = Some("组合键第一个键已选".into());
            } else if combo_waiting {
                let k = (pulse_time.sin() * 0.5 + 0.5) as f32;
                look.fill = theme::with_alpha(th.accent, (70.0 + 120.0 * k) as u8);
                look.side = theme::with_alpha(darken(th.accent, 60), (150.0 + 70.0 * k) as u8);
                look.stroke = egui::Color32::WHITE;
                look.tip = Some("点击这里选组合键下一个键".into());
            } else if Some(code) == sel_code {
                look.fill = theme::with_alpha(ok, 220);
                look.side = theme::with_alpha(darken(ok, 70), 235);
                look.stroke = egui::Color32::WHITE;
            }
            look
        };
        let (key_click, mouse_click) = ui
            .horizontal(|ui| {
                // 右边 96px 留给虚拟鼠标(86 宽 + 10 间距)
                let kc = keyboard::show_keyboard(ui, "vk_kb", &look_of, pulse, 96.0);
                ui.add_space(10.0);
                let mc = keyboard::show_mouse(ui, "vk_mouse", &look_of, pulse);
                (kc, mc)
            })
            .inner;
        if let Some(code) = key_click.or(mouse_click) {
            self.vk_on_key_clicked(code, fps_tab);
        }
    }

    /// 预扫描配置建"键亮表":键码 -> (语义色, 悬停说明)。
    /// 颜色语义:键位=绿 / 仅FPS=橙 / 摇杆方向=青·品红 / 临时启用=品红 /
    /// 组合键与切换键=蓝 / 瞄准三键与总开关=红。
    ///
    /// `filters` 决定"哪几类亮起" —— **键位页与 FPS 页走同一套逻辑**,
    /// 只是两页的默认勾选不同(FPS 页默认只看"仅 FPS"键位)。
    /// 瞄准三键与总开关键属于 FPS 语义,只要 `filters.fps` 为真就亮。
    fn vk_lights(
        &self,
        filters: VkFilters,
    ) -> std::collections::HashMap<u16, (egui::Color32, String)> {
        fn put(
            m: &mut std::collections::HashMap<u16, (egui::Color32, String)>,
            code: u16,
            c: egui::Color32,
            tip: String,
        ) {
            if code != 0 {
                m.entry(code).or_insert((c, tip));
            }
        }
        let mut m = std::collections::HashMap::new();
        let th = self.theme();
        let g = self.shared.lock().unwrap();
        let p = &g.profile;
        // 键位(普通 / 宏 / 仅 FPS 各自受一个勾选控制)
        for b in p.binds.iter() {
            let (on, c, tag) = if matches!(b.action, Action::Macro(_)) {
                (
                    filters.macros || (b.fps_only && filters.fps),
                    th.key_macro,
                    "宏",
                )
            } else if b.fps_only {
                (filters.fps, th.warn, "仅FPS")
            } else {
                (filters.binds, th.ok, "键位")
            };
            if on {
                put(&mut m, b.key, c, format!("[{tag}] {}", b.action.describe()));
            }
        }
        for (wi, w) in p.wheels.iter().enumerate() {
            let (on, c, tag) = if w.temp.is_some() {
                (filters.wheels_temp, th.wheel_temp, "临时")
            } else {
                (filters.wheels_perm, th.wheel_perm, "永久")
            };
            if !on {
                continue;
            }
            for (_, code) in w.active_dirs() {
                put(&mut m, code, c, format!("摇杆#{}({tag}) 方向键", wi + 1));
            }
            if let Some(t) = w.temp.as_ref() {
                put(
                    &mut m,
                    t.key,
                    th.wheel_enable,
                    format!("摇杆#{} {tag}启用键", wi + 1),
                );
            }
        }
        if filters.combos {
            for (ci, c) in p.combos.iter().enumerate() {
                for (si, &k) in c.keys.iter().enumerate() {
                    put(
                        &mut m,
                        k,
                        th.accent,
                        format!("组合键#{} 第{}键", ci + 1, si + 1),
                    );
                }
            }
            for (si, s) in g.switch_keys.iter().enumerate() {
                put(
                    &mut m,
                    s.key,
                    th.accent,
                    format!("切换键位#{}(按下切到它指向的组合)", si + 1),
                );
            }
        }
        // 瞄准三键:属于 FPS 语义,受"显示FPS"勾选控制(FPS 页默认为真)
        if filters.fps {
            put(
                &mut m,
                p.aim.hold_key,
                th.danger,
                "FPS 瞄准门控键(按住开镜)".into(),
            );
            put(
                &mut m,
                p.aim.toggle_key,
                th.danger,
                "FPS 模式独立开关".into(),
            );
            put(
                &mut m,
                p.aim.suspend_key,
                th.danger,
                "按住暂时退出 FPS 并显示鼠标".into(),
            );
        }
        put(
            &mut m,
            p.cursor_toggle_key,
            th.accent,
            "全局鼠标消隐切换键".into(),
        );
        // 总开关键两页都亮(全局键)
        put(&mut m, p.toggle_key, th.danger, "映射总开关键".into());
        m
    }

    /// 取点中应当呼吸闪烁的键码(取点目标本身在哪条键上就闪哪条)
    fn vk_pulse_code(&self) -> Option<u16> {
        let g = self.shared.lock().unwrap();
        match self.picking? {
            CoordSlot::NewBind => self.draft.key,
            CoordSlot::Bind(i)
            | CoordSlot::SwipeStart(i)
            | CoordSlot::SwipeEnd(i)
            | CoordSlot::CircleAngle(i) => g.profile.binds.get(i).map(|b| b.key),
            CoordSlot::WheelCenter(i) => g
                .profile
                .wheels
                .get(i)
                .and_then(|w| w.temp.as_ref().map(|t| t.key)),
            _ => None,
        }
    }

    /// 操作栏正在编辑的目标对应的键码(用于加亮显示)
    fn vk_sel_code(&self) -> Option<u16> {
        let g = self.shared.lock().unwrap();
        match self.vk_sel? {
            VkSel::New => self.draft.key,
            VkSel::Bind(i) => g.profile.binds.get(i).map(|b| b.key),
            VkSel::Macro(i) => g.profile.binds.get(i).map(|b| b.key),
            VkSel::Toggle => Some(g.profile.toggle_key),
            VkSel::CursorToggle => Some(g.profile.cursor_toggle_key),
            VkSel::AimHold => Some(g.profile.aim.hold_key),
            VkSel::AimToggle => Some(g.profile.aim.toggle_key),
            VkSel::AimSuspend => Some(g.profile.aim.suspend_key),
            VkSel::WheelDir { wheel, dir } => g
                .profile
                .wheels
                .get(wheel)
                .and_then(|w| w.active_dirs().get(dir).map(|(_, key)| *key)),
            VkSel::WheelEnable(i) => g
                .profile
                .wheels
                .get(i)
                .and_then(|w| w.temp.as_ref().map(|t| t.key)),
            VkSel::SwitchKey(i) => g.switch_keys.get(i).map(|s| s.key),
            VkSel::SwitchKeySecond(i) => g.switch_keys.get(i).and_then(|s| s.keys.get(1)).copied(),
            VkSel::ComboKey { combo, slot } => g
                .profile
                .combos
                .get(combo)
                .and_then(|c| c.keys.get(slot))
                .copied(),
        }
    }

    /// 键码 -> 它当前被用在哪里(优先级:键位 > 特殊键 > 摇杆 > 组合键/切换键)
    fn vk_find_target(&self, code: u16) -> Option<VkSel> {
        if code == 0 {
            return None;
        }
        let g = self.shared.lock().unwrap();
        let p = &g.profile;
        if let Some(i) = p.binds.iter().position(|b| b.key == code) {
            return Some(if matches!(p.binds[i].action, Action::Macro(_)) {
                VkSel::Macro(i)
            } else {
                VkSel::Bind(i)
            });
        }
        if p.toggle_key == code {
            return Some(VkSel::Toggle);
        }
        if p.cursor_toggle_key == code {
            return Some(VkSel::CursorToggle);
        }
        if p.aim.hold_key == code {
            return Some(VkSel::AimHold);
        }
        if p.aim.toggle_key == code {
            return Some(VkSel::AimToggle);
        }
        if p.aim.suspend_key == code {
            return Some(VkSel::AimSuspend);
        }
        for (wi, w) in p.wheels.iter().enumerate() {
            for (dir, (_, kc)) in w.active_dirs().iter().enumerate() {
                if *kc == code {
                    return Some(VkSel::WheelDir { wheel: wi, dir });
                }
            }
            if w.temp.as_ref().map(|t| t.key) == Some(code) {
                return Some(VkSel::WheelEnable(wi));
            }
        }
        if let Some(i) = g.switch_keys.iter().position(|s| s.key == code) {
            return Some(VkSel::SwitchKey(i));
        }
        if let Some(i) = g
            .switch_keys
            .iter()
            .position(|s| s.keys.get(1).copied() == Some(code))
        {
            return Some(VkSel::SwitchKeySecond(i));
        }
        for (ci, c) in p.combos.iter().enumerate() {
            if let Some(si) = c.keys.iter().position(|&k| k == code) {
                return Some(VkSel::ComboKey {
                    combo: ci,
                    slot: si,
                });
            }
        }
        None
    }

    /// 该目标是否属于 FPS 范畴(FPS 页只允许编辑这些)
    fn vk_sel_is_fps(&self, sel: &VkSel) -> bool {
        let g = self.shared.lock().unwrap();
        match sel {
            VkSel::Bind(i) => g.profile.binds.get(*i).map(|b| b.fps_only).unwrap_or(false),
            VkSel::Macro(i) => g.profile.binds.get(*i).map(|b| b.fps_only).unwrap_or(false),
            VkSel::AimHold | VkSel::AimToggle | VkSel::AimSuspend => true,
            _ => false,
        }
    }

    /// 虚拟键盘/鼠标上某键被点击:
    ///   已设置 -> 选中它,操作栏显示对应编辑器;
    ///   空闲   -> 新建草稿并立刻进入截图取点(可随时[取消取点])。
    fn vk_on_key_clicked(&mut self, code: u16, fps_tab: bool) {
        match self.vk_add_kind {
            VkAddKind::Key => {}
            VkAddKind::Combo => {
                self.vk_combo_key_clicked(code, fps_tab);
                return;
            }
            VkAddKind::Wheel => {
                self.log("轮盘请在“新增项目”旁点[＋ 新增轮盘]，再在下方设置");
                return;
            }
            VkAddKind::Aim => {
                self.log("准星/瞄准锚点请在下方点[取锚点]并点击截图");
                return;
            }
        }
        let name = key_name(code);
        if let Some(sel) = self.vk_find_target(code) {
            // 虚拟键盘只允许直接编辑临时轮盘的方向键。永久轮盘和任何轮盘的
            // 启用键必须在“摇杆映射”页设置，避免误改后出现方向键/启用键冲突。
            match sel {
                VkSel::WheelEnable(_) => {
                    self.log("轮盘启用键不能在虚拟键盘上直接设置，请到[摇杆映射]页修改");
                    return;
                }
                VkSel::WheelDir { wheel, .. } => {
                    let is_temp = {
                        let g = self.shared.lock().unwrap();
                        g.profile
                            .wheels
                            .get(wheel)
                            .map(|w| w.temp.is_some())
                            .unwrap_or(false)
                    };
                    if !is_temp {
                        self.log(
                            "实体轮盘方向键请在[摇杆映射]页设置；虚拟键盘只设置临时轮盘方向键",
                        );
                        return;
                    }
                }
                _ => {}
            }
            if fps_tab && !self.vk_sel_is_fps(&sel) {
                self.log(format!(
                    "{name} 已被普通映射/摇杆/组合键占用: FPS 页只能设置 FPS 相关键,请到[键位]页修改它"
                ));
                return;
            }
            // 选中已设置的键:退出进行中的取点/等待,操作栏显示它的编辑器
            self.picking = None;
            self.waiting_key = None;
            self.resizing = None;
            self.vk_sel = Some(sel);
            return;
        }
        // 空闲键:新建草稿 -> 立刻进入取点
        self.cancel_draft();
        self.reset_draft_to_screen();
        self.draft.key = Some(code);
        self.draft.fps_only = fps_tab;
        self.draft_active = true;
        self.vk_sel = Some(VkSel::New);
        self.begin_pick(CoordSlot::NewBind);
        self.log(format!(
            "已选中 {name}: 请在截图上点击取点(或点[取消取点]放弃)"
        ));
    }

    /// 可视化“组合键”新增：第一次点选前缀，第二次点选成员。
    /// 第二次后立即生成默认点按动作，下方组合键编辑器继续负责动作与取点。
    fn vk_combo_key_clicked(&mut self, code: u16, fps_tab: bool) {
        let name = key_name(code);
        if self.vk_combo_pending.is_empty() {
            self.vk_combo_pending.push(code);
            self.log(format!("组合键第一个键已选: {name}，请再点第二个键"));
            return;
        }
        if self.vk_combo_pending[0] == code {
            self.log("组合键的前两个键不能相同");
            return;
        }
        let first = self.vk_combo_pending[0];
        self.push_undo();
        let m = self.mapper();
        let (x, y) = (m.rel_x(540), m.rel_y(960));
        let index = {
            let mut g = self.shared.lock().unwrap();
            g.profile.combos_enabled = true;
            g.profile.combos.push(KeyCombo {
                keys: vec![first, code],
                action: Action::Tap {
                    x,
                    y,
                    duration_ms: crate::keymap::DEFAULT_TAP_DURATION_MS,
                    radius: crate::keymap::DEFAULT_RADIUS,
                },
                fps_only: fps_tab,
            });
            g.profile.combos.len() - 1
        };
        self.vk_combo_pending.clear();
        self.waiting_key = None;
        self.picking = None;
        self.vk_sel = None;
        self.log(format!(
            "已新增组合键 #{}: {} + {}；可在下方继续调整动作/取点",
            index + 1,
            key_name(first),
            name
        ));
    }

    /// 可视化风格键盘下方的“新增项目”栏。
    fn ui_vk_add_bar(&mut self, ui: &mut egui::Ui, fps_tab: bool) {
        let th = self.theme();
        if fps_tab {
            self.ui_fps_add_bar(ui);
            return;
        }
        ui.horizontal(|ui| {
            ui.label("新增项目：");
            let old = self.vk_add_kind;
            egui::ComboBox::from_id_salt("vk_add_kind")
                .selected_text(self.vk_add_kind.label())
                .show_ui(ui, |ui| {
                    for kind in [
                        VkAddKind::Key,
                        VkAddKind::Combo,
                        VkAddKind::Wheel,
                        VkAddKind::Aim,
                    ] {
                        ui.selectable_value(&mut self.vk_add_kind, kind, kind.label());
                    }
                });
            if self.vk_add_kind != old {
                self.vk_combo_pending.clear();
                self.vk_sel = None;
                self.waiting_key = None;
                self.picking = None;
            }
            match self.vk_add_kind {
                VkAddKind::Key => {
                    ui.label("点击键盘/鼠标上的空闲键，或点击已设置键修改");
                }
                VkAddKind::Combo => {
                    if Self::tab_button(ui, "＋ 新增组合键", false, th.ok) {
                        self.push_undo();
                        let m = self.mapper();
                        let (x, y) = (m.rel_x(540), m.rel_y(960));
                        let mut g = self.shared.lock().unwrap();
                        g.profile.combos_enabled = true;
                        g.profile.combos.push(KeyCombo {
                            keys: vec![0, 0],
                            action: Action::Tap {
                                x,
                                y,
                                duration_ms: crate::keymap::DEFAULT_TAP_DURATION_MS,
                                radius: crate::keymap::DEFAULT_RADIUS,
                            },
                            fps_only: fps_tab,
                        });
                        self.vk_combo_pending.clear();
                        self.scroll_to_new = true;
                    }
                    if self.vk_combo_pending.is_empty() {
                        ui.label("或点下方键盘上的第 1 个键（会闪烁指引）");
                    } else {
                        ui.label(format!(
                            "前缀 {} 已选，请点第 2 个键",
                            key_name(self.vk_combo_pending[0])
                        ));
                        if Self::tab_button(ui, "清空重选", false, th.danger) {
                            self.vk_combo_pending.clear();
                        }
                    }
                }
                VkAddKind::Wheel => {
                    if Self::tab_button(ui, "＋ 新增轮盘", false, th.ok) {
                        self.add_wheel_default();
                    }
                }
                VkAddKind::Aim => {
                    if Self::tab_button(
                        ui,
                        "取锚点",
                        self.picking == Some(CoordSlot::AimAnchor),
                        th.ok,
                    ) {
                        self.begin_pick(CoordSlot::AimAnchor);
                    }
                    ui.label("锚点是在手机屏幕上放置虚拟手指的起点，不是游戏准星。");
                }
            }
        });
    }

    /// FPS 页专用添加栏：只允许 FPS 独有键位和瞄准锚点，不暴露普通键位/组合键/轮盘。
    fn ui_fps_add_bar(&mut self, ui: &mut egui::Ui) {
        if !matches!(self.vk_add_kind, VkAddKind::Key | VkAddKind::Aim) {
            self.vk_add_kind = VkAddKind::Key;
            self.vk_sel = None;
            self.waiting_key = None;
        }
        let th = self.theme();
        ui.horizontal(|ui| {
            ui.label("新增 FPS 项目：");
            let old = self.vk_add_kind;
            egui::ComboBox::from_id_salt("vk_add_kind_fps")
                .selected_text(match self.vk_add_kind {
                    VkAddKind::Aim => "瞄准锚点",
                    _ => "FPS 专用键位",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.vk_add_kind, VkAddKind::Key, "FPS 专用键位");
                    ui.selectable_value(&mut self.vk_add_kind, VkAddKind::Aim, "瞄准锚点");
                });
            if self.vk_add_kind != old {
                self.vk_combo_pending.clear();
                self.vk_sel = None;
                self.waiting_key = None;
                self.picking = None;
            }
            match self.vk_add_kind {
                VkAddKind::Key => {
                    if Self::tab_button(ui, "＋ 新增 FPS 键位", false, th.ok) {
                        self.cancel_draft();
                        self.reset_draft_to_screen();
                        self.draft.key = None;
                        self.draft.fps_only = true;
                        self.draft_active = true;
                        self.vk_sel = None;
                        self.waiting_key = Some(KeySlot::NewBind);
                    }
                    ui.label("也可直接点击键盘/鼠标上的空闲键创建仅 FPS 键位");
                }
                VkAddKind::Aim => {
                    if Self::tab_button(
                        ui,
                        "取锚点",
                        self.picking == Some(CoordSlot::AimAnchor),
                        th.ok,
                    ) {
                        self.begin_pick(CoordSlot::AimAnchor);
                    }
                    ui.label("锚点是在手机屏幕上放置虚拟手指的起点，不是游戏准星。");
                }
                VkAddKind::Combo | VkAddKind::Wheel => {}
            }
        });
    }

    /// 键盘下方的操作栏:当前选中目标的编辑器。
    /// 按钮一律"浏览器标签页"式:方形、不立体、大小不变(见 tab_button)。
    fn ui_vk_panel(&mut self, ui: &mut egui::Ui, fps_tab: bool) {
        ui.separator();
        self.ui_vk_add_bar(ui, fps_tab);
        ui.separator();
        // 取点中:提示 + 取消按钮(用户要求:先点键再取点的流程必须能取消)
        if self.picking == Some(CoordSlot::NewBind) && self.draft.key.is_some() {
            let name = self
                .draft
                .key
                .map(key_name)
                .unwrap_or_else(|| "未定".into());
            ui.horizontal(|ui| {
                ui.colored_label(
                    self.theme().warn,
                    format!("取点中: {name} —— 点击截图选点,或"),
                );
                if Self::tab_button(ui, "取消取点", false, self.theme().danger) {
                    self.cancel_draft();
                    self.vk_sel = None;
                    self.log("已取消取点");
                }
            });
        }
        let sel = match self.vk_sel {
            Some(s) => s,
            None => {
                match self.vk_add_kind {
                    VkAddKind::Key => {
                        ui.label("点击键盘/鼠标上的键开始设置:空闲键进入取点,已设置的键可修改");
                    }
                    VkAddKind::Combo => {
                        self.ui_combos(ui);
                    }
                    VkAddKind::Wheel => {
                        self.ui_wheels(ui);
                    }
                    VkAddKind::Aim => {
                        let captured = self.mouse_grab_flag.load(Ordering::Relaxed);
                        self.ui_aim(ui, captured);
                    }
                }
                return;
            }
        };
        match sel {
            // 新增草稿:类型/坐标/取点/确定/取消(与默认风格的新增节同一套参数)
            VkSel::New => self.ui_vk_draft(ui),
            // 既有键位:与列表**同一套**完整编辑行(撤销/删除/仅FPS/取点/改范围全保留)
            VkSel::Bind(i) => {
                ui.horizontal(|ui| {
                    ui.strong("修改键位");
                    if Self::tab_button(ui, "取消编辑", false, self.theme().danger) {
                        self.waiting_key = None;
                        self.picking = None;
                        self.resizing = None;
                        self.vk_sel = None;
                        self.log("已取消键位编辑");
                    }
                });
                if self.vk_sel.is_some() {
                    if let Some(d) = ui.horizontal(|ui| self.ui_bind_row(ui, i)).inner {
                        self.delete_bind(d);
                        self.vk_sel = None;
                    }
                }
            }
            VkSel::Macro(i) => {
                ui.horizontal(|ui| {
                    ui.strong("修改宏");
                    ui.colored_label(self.theme().key_macro, "宏");
                    if Self::tab_button(ui, "取消编辑", false, self.theme().danger) {
                        self.vk_sel = None;
                        self.log("已取消宏编辑");
                    }
                });
                if self.vk_sel.is_some() {
                    let mut action = {
                        let g = self.shared.lock().unwrap();
                        g.profile.binds.get(i).and_then(|b| match &b.action {
                            Action::Macro(m) => Some(m.clone()),
                            _ => None,
                        })
                    };
                    if let Some(action) = action.as_mut() {
                        let mut changed = false;
                        if !action.steps.is_empty() {
                            Self::ui_macro_recorded_steps(ui, &action.steps);
                        }
                        if let Some(vp) = &action.virtual_profile {
                            Self::ui_macro_virtual_profile_info(ui, vp);
                        }
                        if !action.instructions.is_empty() || action.virtual_profile.is_some() {
                            changed |= self.ui_macro_instruction_list(
                                ui,
                                &mut action.instructions,
                                &format!("vk_macro_{i}"),
                            );
                        } else {
                            ui.small("该宏只有录制步骤；展开后可查看，编辑请点[载入编辑]。");
                        }
                        if ui.button("保存宏修改").clicked() {
                            let mut g = self.shared.lock().unwrap();
                            if let Some(b) = g.profile.binds.get_mut(i) {
                                b.action = Action::Macro(action.clone());
                            }
                            changed = true;
                        }
                        if changed {
                            self.log("宏已更新");
                        }
                    }
                }
            }
            // 特殊键:改绑 + 取消选择
            VkSel::Toggle => self.ui_vk_special(ui, KeySlot::Toggle, "总开关键"),
            VkSel::CursorToggle => self.ui_vk_special(ui, KeySlot::CursorToggle, "鼠标消隐切换键"),
            VkSel::AimHold => self.ui_vk_special(ui, KeySlot::AimHold, "FPS 瞄准门控键"),
            VkSel::AimToggle => self.ui_vk_special(ui, KeySlot::AimToggle, "FPS 独立开关"),
            VkSel::AimSuspend => self.ui_vk_special(ui, KeySlot::AimSuspend, "FPS 暂时退出键"),
            VkSel::WheelDir { wheel, dir } => {
                self.ui_vk_special(ui, KeySlot::WheelDir { wheel, dir }, "摇杆方向键")
            }
            VkSel::WheelEnable(i) => {
                self.ui_vk_special(ui, KeySlot::WheelEnable(i), "摇杆临时启用键")
            }
            VkSel::SwitchKey(i) => self.ui_vk_special(ui, KeySlot::SwitchKey(i), "组合切换键"),
            VkSel::SwitchKeySecond(i) => {
                self.ui_vk_special(ui, KeySlot::SwitchKeySecond(i), "组合切换键第二位")
            }
            VkSel::ComboKey { combo, slot } => {
                self.ui_vk_special(ui, KeySlot::ComboKey { combo, slot }, "组合键成员")
            }
        }
    }

    /// 独立的宏页面：录制、虚拟键盘取键、宏列表和触发键绑定都在这里完成。
    fn ui_macro_page(&mut self, ui: &mut egui::Ui) {
        let th = self.theme();
        let visual = self.ui_style() == theme::UiStyle::Visual;
        ui.heading("宏（开发中）");
        ui.label("录制真实按键时序，或用设置宏编排按键、组合键、轮盘、FPS、点击、滑动与间隔，再绑定到一个触发键。");
        ui.separator();

        ui.horizontal(|ui| {
            ui.strong("录制");
            ui.label("空闲停止:");
            ui.add(
                egui::DragValue::new(&mut self.macro_idle_ms)
                    .range(100..=2000)
                    .suffix(" ms"),
            );
            ui.checkbox(&mut self.macro_show_events, "显示录制事件")
                .on_hover_text("默认只显示结果摘要；录制长按时不会把自动重复显示成连续点击");
            if self.macro_recording.is_some() {
                if Self::tab_button(ui, "停止录制", false, th.danger) {
                    self.finish_macro_recording();
                }
            } else if Self::tab_button(ui, "开始录制", false, th.ok) {
                self.waiting_key = None;
                self.picking = None;
                self.macro_page_steps.clear();
                self.macro_recording = Some(MacroRecording {
                    steps: Vec::new(),
                    held: HashSet::new(),
                    last_event: Instant::now(),
                    last_step_at: Instant::now(),
                    idle_ms: self.macro_idle_ms,
                });
                self.log("宏录制开始：操作按键或点击下方虚拟键盘；空闲会自动停止");
            }
            if Self::tab_button(ui, "清空", false, th.accent) {
                self.reset_macro_draft();
            }
            if visual && Self::tab_button(ui, "扩展宏...", false, th.accent) {
                self.open_macro_virtual_editor();
            }
        });

        ui.horizontal(|ui| {
            ui.label("触发键:");
            let waiting = self.waiting_key == Some(KeySlot::MacroTrigger);
            if Self::key_button(ui, waiting, self.macro_page_key).clicked() {
                self.waiting_key = Some(KeySlot::MacroTrigger);
                self.log("请按任意键作为宏触发键");
            }
            ui.checkbox(&mut self.macro_page_fps_only, "仅 FPS");
            let ready = self.macro_page_key.is_some()
                && (!self.macro_page_steps.is_empty() || !self.macro_page_instructions.is_empty());
            ui.add_enabled_ui(ready, |ui| {
                if Self::tab_button(ui, "新建宏", false, th.ok) {
                    self.macro_add_from_page();
                }
            });
            if Self::tab_button(ui, "取消编辑", false, th.danger) {
                self.reset_macro_draft();
                self.log("已取消宏编辑");
            }
        });

        let mut open_extended_editor = false;
        let virtual_profile_snapshot = self.macro_page_virtual_profile.clone();
        if let Some(vp) = virtual_profile_snapshot.as_ref() {
            let bind_count = vp.binds.len();
            let wheel_count = vp.wheels.len();
            ui.horizontal(|ui| {
                ui.colored_label(
                    th.accent,
                    format!("扩展宏虚拟键位: {bind_count} 个按键 / {wheel_count} 个轮盘"),
                );
                if Self::tab_button(ui, "清除扩展", false, th.danger) {
                    self.macro_page_virtual_profile = None;
                }
                if visual && Self::tab_button(ui, "编辑扩展宏...", false, th.accent) {
                    open_extended_editor = true;
                }
            });
            ui.collapsing("查看扩展宏临时键位表", |ui| {
                Self::ui_macro_virtual_profile_info(ui, vp);
            });
        }
        if open_extended_editor {
            self.open_macro_virtual_editor();
        }

        if self.macro_recording.is_some() {
            let n = self
                .macro_recording
                .as_ref()
                .map(|r| r.steps.len())
                .unwrap_or(0);
            ui.colored_label(
                th.warn,
                format!("录制中… 已记录 {n} 条结果（长按重复已合并）"),
            );
        } else if self.macro_page_steps.is_empty() {
            ui.small("尚未录制。先点[开始录制]，再操作按键或点击下方虚拟键盘；也可以直接设置宏。");
        } else {
            ui.colored_label(th.ok, macro_summary(&self.macro_page_steps));
            if self.macro_show_events {
                let start = self.macro_page_steps.len().saturating_sub(16);
                for (i, step) in self.macro_page_steps[start..].iter().enumerate() {
                    ui.small(format!(
                        "{}. {} {} (+{}ms)",
                        start + i + 1,
                        key_name(step.code),
                        if step.pressed { "按下" } else { "抬起" },
                        step.delay_ms
                    ));
                }
            }
        }

        ui.separator();
        ui.horizontal(|ui| {
            ui.strong("设置宏");
            ui.small("把按键、组合键、轮盘、FPS、点击、滑动、间隔和嵌套宏编排成一次操作");
            if visual && Self::tab_button(ui, "扩展宏...", false, th.accent) {
                self.open_macro_virtual_editor();
            }
        });
        let mut instructions = std::mem::take(&mut self.macro_page_instructions);
        self.ui_macro_instruction_list(ui, &mut instructions, "macro_page");
        self.macro_page_instructions = instructions;

        if visual {
            ui.separator();
            ui.label("虚拟键盘：录制时点击会写入宏；非录制时点击用于选择触发键。");
            let mut in_macro: HashSet<u16> = self.macro_page_steps.iter().map(|s| s.code).collect();
            for instruction in &self.macro_page_instructions {
                match instruction {
                    MacroInstruction::Key { code, .. } if *code != 0 => {
                        in_macro.insert(*code);
                    }
                    MacroInstruction::Combo { keys, .. } => {
                        in_macro.extend(keys.iter().copied().filter(|key| *key != 0));
                    }
                    _ => {}
                }
            }
            let selected = self.macro_page_key;
            let look_of = move |code: u16| {
                if Some(code) == selected {
                    keyboard::KeyLook {
                        fill: theme::with_alpha(th.ok, 210),
                        side: theme::with_alpha(darken(th.ok, 60), 230),
                        stroke: egui::Color32::WHITE,
                        text: egui::Color32::WHITE,
                        tip: Some("宏触发键".into()),
                    }
                } else if in_macro.contains(&code) {
                    keyboard::KeyLook {
                        fill: theme::with_alpha(th.key_macro, 190),
                        side: theme::with_alpha(darken(th.key_macro, 60), 220),
                        stroke: egui::Color32::WHITE,
                        text: egui::Color32::WHITE,
                        tip: Some("已录制".into()),
                    }
                } else {
                    keyboard::KeyLook::idle()
                }
            };
            let (key_click, mouse_click) = ui
                .horizontal(|ui| {
                    let kc = keyboard::show_keyboard(ui, "macro_kb", &look_of, None, 96.0);
                    ui.add_space(10.0);
                    let mc = keyboard::show_mouse(ui, "macro_mouse", &look_of, None);
                    (kc, mc)
                })
                .inner;
            if let Some(code) = key_click.or(mouse_click) {
                self.macro_virtual_key(code);
            }
        }

        ui.separator();
        ui.heading("已录宏");
        let rows: Vec<(usize, u16, usize, usize, bool)> = {
            let g = self.shared.lock().unwrap();
            g.profile
                .binds
                .iter()
                .enumerate()
                .filter_map(|(i, b)| match &b.action {
                    Action::Macro(m) => {
                        Some((i, b.key, m.steps.len(), m.instructions.len(), b.fps_only))
                    }
                    _ => None,
                })
                .collect()
        };
        let mut delete = None;
        let mut load = None;
        for (i, key, steps, instructions, fps) in rows {
            let expanded = self.macro_expanded == Some(i);
            ui.horizontal(|ui| {
                ui.colored_label(th.key_macro, format!("宏#{}", i + 1));
                let waiting = self.waiting_key == Some(KeySlot::Bind(i));
                if Self::key_button(ui, waiting, Some(key)).clicked() {
                    self.waiting_key = Some(KeySlot::Bind(i));
                }
                let count_text = match (steps, instructions) {
                    (0, n) => format!("设置 {n} 项"),
                    (n, 0) => format!("录制 {n} 项"),
                    (s, i) => format!("录制 {s} 项 / 设置 {i} 项"),
                };
                ui.label(count_text);
                if fps {
                    ui.colored_label(th.key_fps, "仅 FPS");
                }
                if ui.button(if expanded { "收起" } else { "展开" }).clicked() {
                    self.macro_expanded = if expanded { None } else { Some(i) };
                }
                let loaded = self.macro_loaded_index == Some(i);
                if ui
                    .button(if loaded {
                        "取消编辑"
                    } else {
                        "载入编辑"
                    })
                    .clicked()
                {
                    if loaded {
                        self.reset_macro_draft();
                        self.log("已取消宏编辑");
                    } else {
                        load = Some(i);
                    }
                }
                if ui.button("删除").clicked() {
                    delete = Some(i);
                }
            });
            if expanded {
                let mut action = {
                    let g = self.shared.lock().unwrap();
                    g.profile.binds.get(i).and_then(|b| match &b.action {
                        Action::Macro(m) => Some(m.clone()),
                        _ => None,
                    })
                };
                if let Some(action) = action.as_mut() {
                    let before = action.clone();
                    if !action.steps.is_empty() {
                        Self::ui_macro_recorded_steps(ui, &action.steps);
                    }
                    if let Some(vp) = &action.virtual_profile {
                        Self::ui_macro_virtual_profile_info(ui, vp);
                    }
                    if !action.instructions.is_empty() || action.virtual_profile.is_some() {
                        ui.small("设置动作:");
                        self.ui_macro_instruction_list(
                            ui,
                            &mut action.instructions,
                            &format!("macro_expand_{i}"),
                        );
                    } else {
                        ui.small("该宏只有录制步骤，录制步骤只读。");
                    }
                    if action != &before {
                        {
                            let mut g = self.shared.lock().unwrap();
                            if let Some(b) = g.profile.binds.get_mut(i) {
                                b.action = Action::Macro(action.clone());
                            }
                        }
                        self.log(format!("宏#{} 已更新", i + 1));
                    }
                }
            }
        }
        if let Some(i) = load {
            let loaded = {
                let g = self.shared.lock().unwrap();
                g.profile.binds.get(i).and_then(|b| match &b.action {
                    Action::Macro(m) => Some((b.key, m.clone(), b.fps_only)),
                    _ => None,
                })
            };
            if let Some((key, action, fps)) = loaded {
                self.macro_page_key = Some(key);
                self.macro_page_steps = action.steps;
                self.macro_page_instructions = action.instructions;
                self.macro_page_virtual_profile = action.virtual_profile.as_deref().cloned();
                self.macro_page_fps_only = fps;
                self.macro_loaded_index = Some(i);
                self.waiting_key = None;
                self.log("已载入宏步骤到编辑区；修改后点[新建宏]会保存为新宏");
            }
        }
        if let Some(i) = delete {
            self.push_undo();
            self.shared.lock().unwrap().profile.binds.remove(i);
            if self.macro_loaded_index == Some(i) {
                self.reset_macro_draft();
            }
            self.log("已删除宏");
        }
    }

    fn reset_macro_draft(&mut self) {
        self.macro_recording = None;
        self.macro_page_steps.clear();
        self.macro_page_instructions.clear();
        self.macro_page_virtual_profile = None;
        self.macro_page_key = None;
        self.macro_page_fps_only = false;
        self.macro_loaded_index = None;
        self.picking = None;
        self.waiting_key = None;
    }

    fn open_macro_virtual_editor(&mut self) {
        let (source_scheme, profile) = {
            let g = self.shared.lock().unwrap();
            let source = g.active_scheme.min(g.schemes.len().saturating_sub(1));
            let profile = self
                .macro_page_virtual_profile
                .clone()
                .or_else(|| g.schemes.get(source).cloned())
                .unwrap_or_else(|| g.profile.clone());
            (source, profile)
        };
        let mut profile = profile;
        sanitize_virtual_profile(&mut profile);
        self.macro_virtual_editor = Some(MacroVirtualEditor {
            profile,
            selected_key: None,
            source_scheme,
        });
    }

    fn ui_macro_virtual_window(&mut self, ctx: &egui::Context) {
        let Some(mut editor) = self.macro_virtual_editor.take() else {
            return;
        };
        let schemes: Vec<String> = {
            let g = self.shared.lock().unwrap();
            g.schemes.iter().map(|p| p.name.clone()).collect()
        };
        let mut open = true;
        let mut save = false;
        let mut cancel = false;
        egui::Window::new("扩展宏：虚拟键位")
            .open(&mut open)
            .default_size([760.0, 600.0])
            .min_size([520.0, 360.0])
            .resizable(true)
            .show(ctx, |ui| {
                ui.label("先在这里模拟一套虚拟键位；宏执行时按这套位置和动作解析。关闭窗口或点取消会完全丢弃本次设置。");
                ui.horizontal(|ui| {
                    ui.label("继承已有键位组合:");
                    let mut source = editor.source_scheme;
                    egui::ComboBox::from_id_salt("macro_virtual_source")
                        .selected_text(schemes.get(source).cloned().unwrap_or_else(|| "当前配置".into()))
                        .show_ui(ui, |ui| {
                            for (i, name) in schemes.iter().enumerate() {
                                ui.selectable_value(&mut source, i, name);
                            }
                        });
                    if source != editor.source_scheme {
                        let mut inherited = self
                            .shared
                            .lock()
                            .unwrap()
                            .schemes
                            .get(source)
                            .cloned()
                            .unwrap_or_default();
                        sanitize_virtual_profile(&mut inherited);
                        editor.profile = inherited;
                        editor.source_scheme = source;
                        editor.selected_key = None;
                    }
                });
                ui.separator();
                let lights = virtual_keyboard_lights(&editor.profile);
                let selected = editor.selected_key;
                let look_of = move |code: u16| {
                    if Some(code) == selected {
                        keyboard::KeyLook {
                            fill: theme::with_alpha(egui::Color32::LIGHT_BLUE, 210),
                            side: theme::with_alpha(egui::Color32::BLUE, 220),
                            stroke: egui::Color32::WHITE,
                            text: egui::Color32::WHITE,
                            tip: Some("当前虚拟键位".into()),
                        }
                    } else if let Some((c, tip)) = lights.get(&code) {
                        keyboard::KeyLook {
                            fill: theme::with_alpha(*c, 185),
                            side: theme::with_alpha(darken(*c, 60), 220),
                            stroke: egui::Color32::WHITE,
                            text: egui::Color32::WHITE,
                            tip: Some(tip.clone()),
                        }
                    } else {
                        keyboard::KeyLook::idle()
                    }
                };
                let clicked = ui
                    .horizontal(|ui| {
                        let kc = keyboard::show_keyboard(ui, "macro_virtual_kb", &look_of, None, 96.0);
                        ui.add_space(10.0);
                        let mc = keyboard::show_mouse(ui, "macro_virtual_mouse", &look_of, None);
                        kc.or(mc)
                    })
                    .inner;
                if let Some(code) = clicked {
                    if editor.profile.binds.iter().any(|b| b.key == code) {
                        editor.selected_key = Some(code);
                    } else {
                        editor.profile.binds.push(KeyBind {
                            key: code,
                            action: Action::Tap {
                                x: 0.5,
                                y: 0.5,
                                duration_ms: crate::keymap::DEFAULT_TAP_DURATION_MS,
                                radius: crate::keymap::DEFAULT_RADIUS,
                            },
                            fps_only: false,
                        });
                        editor.selected_key = Some(code);
                    }
                }
                ui.separator();
                if let Some(key) = editor.selected_key {
                    ui.horizontal(|ui| {
                        ui.strong(format!("虚拟键位: {}", key_name(key)));
                        if ui.small_button("删除键位").clicked() {
                            editor.profile.binds.retain(|b| b.key != key);
                            editor.selected_key = None;
                        }
                    });
                    if let Some(index) = editor.profile.binds.iter().position(|b| b.key == key) {
                        Self::ui_macro_virtual_bind(ui, &mut editor.profile.binds[index]);
                    }
                } else {
                    ui.small("点击虚拟键盘上的键，可新增或编辑虚拟键位。");
                }
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("保存到宏草稿").clicked() {
                        save = true;
                    }
                    if ui.button("取消").clicked() {
                        cancel = true;
                    }
                });
            });
        if save {
            self.macro_page_virtual_profile = Some(editor.profile.clone());
            self.log("扩展宏虚拟键位已保存到当前宏草稿");
        } else if !cancel && open {
            self.macro_virtual_editor = Some(editor);
        } else {
            self.log("已取消扩展宏设置，未保留任何修改");
        }
    }

    fn ui_macro_virtual_bind(ui: &mut egui::Ui, bind: &mut KeyBind) {
        let kind = match bind.action {
            Action::Tap { .. } => 0,
            Action::Hold { .. } => 1,
            Action::Swipe(_) => 2,
            Action::AndroidKey { .. } => 3,
            Action::Macro(_) => 0,
        };
        let mut kind = kind;
        ui.horizontal(|ui| {
            ui.label("动作:");
            egui::ComboBox::from_id_salt(("macro_virtual_action", bind.key))
                .selected_text(match kind {
                    0 => "点按",
                    1 => "长按",
                    2 => "滑动",
                    _ => "系统键",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut kind, 0, "点按");
                    ui.selectable_value(&mut kind, 1, "长按");
                    ui.selectable_value(&mut kind, 2, "滑动");
                    ui.selectable_value(&mut kind, 3, "系统键");
                });
        });
        let old_kind = match bind.action {
            Action::Tap { .. } => 0,
            Action::Hold { .. } => 1,
            Action::Swipe(_) => 2,
            Action::AndroidKey { .. } => 3,
            Action::Macro(_) => 0,
        };
        if kind != old_kind {
            bind.action = match kind {
                1 => Action::Hold {
                    x: 0.5,
                    y: 0.5,
                    radius: crate::keymap::DEFAULT_RADIUS,
                },
                2 => Action::Swipe(Swipe {
                    start: (0.5, 0.7),
                    end: (0.5, 0.3),
                    duration_ms: 300,
                    easing: Easing::Linear,
                    path: SwipePath::Line,
                }),
                3 => Action::AndroidKey { keycode: 4 },
                _ => Action::Tap {
                    x: 0.5,
                    y: 0.5,
                    duration_ms: crate::keymap::DEFAULT_TAP_DURATION_MS,
                    radius: crate::keymap::DEFAULT_RADIUS,
                },
            };
        }
        match &mut bind.action {
            Action::Tap {
                x,
                y,
                duration_ms,
                radius,
            } => {
                ui.horizontal(|ui| {
                    ui.label("X/Y");
                    ui.add(egui::DragValue::new(x).range(0.0..=1.0).speed(0.005));
                    ui.add(egui::DragValue::new(y).range(0.0..=1.0).speed(0.005));
                    ui.label("时长");
                    ui.add(
                        egui::DragValue::new(duration_ms)
                            .range(0..=600_000)
                            .suffix(" ms"),
                    );
                    ui.label("范围");
                    ui.add(egui::DragValue::new(radius).range(0.001..=0.5).speed(0.001));
                });
            }
            Action::Hold { x, y, radius } => {
                ui.horizontal(|ui| {
                    ui.label("X/Y");
                    ui.add(egui::DragValue::new(x).range(0.0..=1.0).speed(0.005));
                    ui.add(egui::DragValue::new(y).range(0.0..=1.0).speed(0.005));
                    ui.label("范围");
                    ui.add(egui::DragValue::new(radius).range(0.001..=0.5).speed(0.001));
                });
            }
            Action::Swipe(s) => {
                ui.horizontal(|ui| {
                    ui.label("起点 X/Y");
                    ui.add(
                        egui::DragValue::new(&mut s.start.0)
                            .range(0.0..=1.0)
                            .speed(0.005),
                    );
                    ui.add(
                        egui::DragValue::new(&mut s.start.1)
                            .range(0.0..=1.0)
                            .speed(0.005),
                    );
                    ui.label("终点 X/Y");
                    ui.add(
                        egui::DragValue::new(&mut s.end.0)
                            .range(0.0..=1.0)
                            .speed(0.005),
                    );
                    ui.add(
                        egui::DragValue::new(&mut s.end.1)
                            .range(0.0..=1.0)
                            .speed(0.005),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("时长");
                    ui.add(
                        egui::DragValue::new(&mut s.duration_ms)
                            .range(10..=600_000)
                            .suffix(" ms"),
                    );
                });
            }
            Action::AndroidKey { keycode } => {
                ui.horizontal(|ui| {
                    ui.label("系统 keycode");
                    ui.add(egui::DragValue::new(keycode).range(0..=999));
                });
            }
            Action::Macro(_) => {}
        }
        ui.checkbox(&mut bind.fps_only, "仅 FPS");
    }

    fn ui_macro_recorded_steps(ui: &mut egui::Ui, steps: &[MacroStep]) {
        ui.small("录制步骤（只读）:");
        egui::ScrollArea::vertical()
            .id_salt("macro_recorded_steps")
            .max_height(160.0)
            .show(ui, |ui| {
                for (i, step) in steps.iter().enumerate() {
                    ui.monospace(format!(
                        "{:>3}. {} {}  +{}ms",
                        i + 1,
                        key_name(step.code),
                        if step.pressed { "按下" } else { "抬起" },
                        step.delay_ms
                    ));
                }
            });
    }

    fn ui_macro_virtual_profile_info(ui: &mut egui::Ui, profile: &Profile) {
        ui.small(format!(
            "扩展宏临时键位表: {} 个按键 / {} 个轮盘",
            profile.binds.len(),
            profile.wheels.len()
        ));
        for bind in profile.binds.iter().take(64) {
            ui.monospace(format!(
                "虚拟 {} → {}",
                key_name(bind.key),
                bind.action.describe()
            ));
        }
        if profile.binds.len() > 64 {
            ui.small(format!("... 还有 {} 个虚拟键位", profile.binds.len() - 64));
        }
        for (i, wheel) in profile.wheels.iter().enumerate() {
            let dirs = wheel
                .active_dirs()
                .iter()
                .map(|(_, key)| key_name(*key))
                .collect::<Vec<_>>()
                .join("/");
            ui.monospace(format!("虚拟轮盘#{} → {}", i + 1, dirs));
        }
    }

    /// 设置宏动作列表：下拉选择 + 添加，删除和精确编辑。
    fn ui_macro_instruction_list(
        &mut self,
        ui: &mut egui::Ui,
        instructions: &mut Vec<MacroInstruction>,
        id: &str,
    ) -> bool {
        let before = instructions.clone();
        let mut remove = None;
        for (i, instruction) in instructions.iter_mut().enumerate() {
            ui.push_id(format!("{id}_{i}"), |ui| {
                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        ui.strong(format!("{}. {}", i + 1, instruction.label()));
                        if ui.small_button("删除").clicked() {
                            remove = Some(i);
                        }
                    });
                    self.ui_macro_instruction(ui, instruction, i, id);
                });
            });
        }
        if let Some(i) = remove {
            instructions.remove(i);
        }
        ui.horizontal(|ui| {
            ui.label("新增项目：");
            egui::ComboBox::from_id_salt(format!("{id}_add_kind"))
                .selected_text(self.macro_instruction_kind.label())
                .show_ui(ui, |ui| {
                    for kind in MacroInstructionKind::ALL {
                        ui.selectable_value(&mut self.macro_instruction_kind, kind, kind.label());
                    }
                });
            if ui.button("新增操作").clicked() {
                instructions.push(self.macro_instruction_kind.make());
            }
        });
        before != *instructions
    }

    /// 单个设置宏动作的参数编辑器。
    fn ui_macro_instruction(
        &mut self,
        ui: &mut egui::Ui,
        instruction: &mut MacroInstruction,
        index: usize,
        id: &str,
    ) {
        let mut pending_key = None;
        match instruction {
            MacroInstruction::Delay { ms } => {
                ui.horizontal(|ui| {
                    ui.label("等待");
                    ui.add(egui::DragValue::new(ms).range(0..=600_000).suffix(" ms"));
                });
            }
            MacroInstruction::Key {
                code,
                mode,
                duration_ms,
                delay_ms,
            } => {
                ui.horizontal(|ui| {
                    ui.label("按键");
                    let waiting = self.waiting_key == Some(KeySlot::MacroInstructionKey(index));
                    if Self::key_button(ui, waiting, Some(*code)).clicked() {
                        pending_key = Some(KeySlot::MacroInstructionKey(index));
                    }
                    ui.selectable_value(mode, MacroKeyMode::Tap, "点按");
                    ui.selectable_value(mode, MacroKeyMode::Hold, "长按");
                });
                ui.horizontal(|ui| {
                    ui.label("持续");
                    ui.add(
                        egui::DragValue::new(duration_ms)
                            .range(5..=600_000)
                            .suffix(" ms"),
                    );
                    ui.label("间隔");
                    ui.add(
                        egui::DragValue::new(delay_ms)
                            .range(0..=600_000)
                            .suffix(" ms"),
                    );
                });
            }
            MacroInstruction::Combo {
                keys,
                duration_ms,
                delay_ms,
            } => {
                ui.horizontal_wrapped(|ui| {
                    ui.label("组合键");
                    for (slot, key) in keys.iter_mut().enumerate() {
                        let waiting = self.waiting_key
                            == Some(KeySlot::MacroInstructionComboKey {
                                instruction: index,
                                slot,
                            });
                        if Self::key_button(ui, waiting, Some(*key)).clicked() {
                            pending_key = Some(KeySlot::MacroInstructionComboKey {
                                instruction: index,
                                slot,
                            });
                        }
                    }
                    if keys.len() < 4 && ui.small_button("＋").clicked() {
                        keys.push(0);
                    }
                    if keys.len() > 2 && ui.small_button("－").clicked() {
                        keys.pop();
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("持续");
                    ui.add(
                        egui::DragValue::new(duration_ms)
                            .range(5..=600_000)
                            .suffix(" ms"),
                    );
                    ui.label("间隔");
                    ui.add(
                        egui::DragValue::new(delay_ms)
                            .range(0..=600_000)
                            .suffix(" ms"),
                    );
                });
            }
            MacroInstruction::Wheel {
                wheel,
                part,
                duration_ms,
                delay_ms,
            } => {
                let wheels = {
                    let g = self.shared.lock().unwrap();
                    g.profile
                        .wheels
                        .iter()
                        .enumerate()
                        .map(|(i, w)| (i, format!("轮盘#{} ({:?})", i + 1, w.kind)))
                        .collect::<Vec<_>>()
                };
                ui.horizontal_wrapped(|ui| {
                    ui.label("轮盘");
                    egui::ComboBox::from_id_salt(format!("{id}_wheel_{index}"))
                        .selected_text(format!("轮盘#{}", *wheel + 1))
                        .show_ui(ui, |ui| {
                            for (wi, label) in &wheels {
                                ui.selectable_value(wheel, *wi, label);
                            }
                        });
                    ui.label("方向");
                    let selected = match part {
                        MacroWheelPart::Up => "上",
                        MacroWheelPart::Down => "下",
                        MacroWheelPart::Left => "左",
                        MacroWheelPart::Right => "右",
                        MacroWheelPart::Custom(i) => {
                            ui.label(format!("自定义#{i}"));
                            "自定义"
                        }
                    };
                    egui::ComboBox::from_id_salt(format!("{id}_part_{index}"))
                        .selected_text(selected)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(part, MacroWheelPart::Up, "上");
                            ui.selectable_value(part, MacroWheelPart::Down, "下");
                            ui.selectable_value(part, MacroWheelPart::Left, "左");
                            ui.selectable_value(part, MacroWheelPart::Right, "右");
                            for c in 0..8usize {
                                ui.selectable_value(
                                    part,
                                    MacroWheelPart::Custom(c),
                                    format!("自定义#{c}"),
                                );
                            }
                        });
                });
                ui.horizontal(|ui| {
                    ui.label("持续");
                    ui.add(
                        egui::DragValue::new(duration_ms)
                            .range(5..=600_000)
                            .suffix(" ms"),
                    );
                    ui.label("间隔");
                    ui.add(
                        egui::DragValue::new(delay_ms)
                            .range(0..=600_000)
                            .suffix(" ms"),
                    );
                });
            }
            MacroInstruction::Fps { on, delay_ms } => {
                ui.horizontal(|ui| {
                    ui.checkbox(on, "开启 FPS 模式");
                    ui.label("间隔");
                    ui.add(
                        egui::DragValue::new(delay_ms)
                            .range(0..=600_000)
                            .suffix(" ms"),
                    );
                });
            }
            MacroInstruction::Click {
                x,
                y,
                duration_ms,
                delay_ms,
            } => {
                ui.horizontal(|ui| {
                    ui.label("位置 X/Y");
                    ui.add(egui::DragValue::new(x).range(0.0..=1.0).speed(0.005));
                    ui.add(egui::DragValue::new(y).range(0.0..=1.0).speed(0.005));
                });
                ui.horizontal(|ui| {
                    ui.label("持续");
                    ui.add(
                        egui::DragValue::new(duration_ms)
                            .range(5..=600_000)
                            .suffix(" ms"),
                    );
                    ui.label("间隔");
                    ui.add(
                        egui::DragValue::new(delay_ms)
                            .range(0..=600_000)
                            .suffix(" ms"),
                    );
                });
            }
            MacroInstruction::Swipe {
                start_x,
                start_y,
                end_x,
                end_y,
                duration_ms,
                delay_ms,
            } => {
                ui.horizontal(|ui| {
                    ui.label("起点 X/Y");
                    ui.add(egui::DragValue::new(start_x).range(0.0..=1.0).speed(0.005));
                    ui.add(egui::DragValue::new(start_y).range(0.0..=1.0).speed(0.005));
                    ui.label("终点 X/Y");
                    ui.add(egui::DragValue::new(end_x).range(0.0..=1.0).speed(0.005));
                    ui.add(egui::DragValue::new(end_y).range(0.0..=1.0).speed(0.005));
                });
                ui.horizontal(|ui| {
                    ui.label("滑动时长");
                    ui.add(
                        egui::DragValue::new(duration_ms)
                            .range(10..=600_000)
                            .suffix(" ms"),
                    );
                    ui.label("间隔");
                    ui.add(
                        egui::DragValue::new(delay_ms)
                            .range(0..=600_000)
                            .suffix(" ms"),
                    );
                });
            }
            MacroInstruction::Macro { action, delay_ms } => {
                let macros = {
                    let g = self.shared.lock().unwrap();
                    g.profile
                        .binds
                        .iter()
                        .filter_map(|b| match &b.action {
                            Action::Macro(m) => Some((b.key, m.clone())),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                };
                ui.horizontal_wrapped(|ui| {
                    ui.label("嵌套宏");
                    egui::ComboBox::from_id_salt(format!("{id}_nested_{index}"))
                        .selected_text(format!(
                            "{}步/{}项",
                            action.steps.len(),
                            action.instructions.len()
                        ))
                        .show_ui(ui, |ui| {
                            for (key, candidate) in &macros {
                                if ui
                                    .selectable_label(
                                        candidate.steps.len() == action.steps.len()
                                            && candidate.instructions.len()
                                                == action.instructions.len(),
                                        format!(
                                            "{} ({}步/{}项)",
                                            key_name(*key),
                                            candidate.steps.len(),
                                            candidate.instructions.len()
                                        ),
                                    )
                                    .clicked()
                                {
                                    *action = Box::new(candidate.clone());
                                }
                            }
                        });
                    if ui.small_button("清空").clicked() {
                        *action = Box::new(MacroAction::default());
                    }
                    ui.label("间隔");
                    ui.add(
                        egui::DragValue::new(delay_ms)
                            .range(0..=600_000)
                            .suffix(" ms"),
                    );
                });
            }
        }
        if let Some(slot) = pending_key {
            self.waiting_key = Some(slot);
            self.log("请按键盘/鼠标键完成宏动作设置");
        }
    }

    fn macro_virtual_key(&mut self, code: u16) {
        if let Some(KeySlot::MacroInstructionKey(index)) = self.waiting_key {
            self.assign_key(KeySlot::MacroInstructionKey(index), code);
            return;
        }
        if let Some(KeySlot::MacroInstructionComboKey { instruction, slot }) = self.waiting_key {
            self.assign_key(
                KeySlot::MacroInstructionComboKey { instruction, slot },
                code,
            );
            return;
        }
        if let Some(rec) = self.macro_recording.as_mut() {
            let now = Instant::now();
            let delay = if rec.steps.is_empty() {
                0
            } else {
                now.saturating_duration_since(rec.last_step_at)
                    .as_millis()
                    .min(2000) as u32
            };
            rec.steps.push(MacroStep {
                code,
                pressed: true,
                delay_ms: delay,
            });
            rec.steps.push(MacroStep {
                code,
                pressed: false,
                delay_ms: 40,
            });
            rec.last_event = now;
            rec.last_step_at = now + Duration::from_millis(40);
            rec.held.remove(&code);
        } else {
            self.macro_page_key = Some(code);
        }
    }

    /// 把捕获层上报的按键事件写入录制宏。重复 KEY_DOWN（键盘自动重复）
    /// 只更新空闲计时，不再追加一次按下；抬起时才用真实持续时间写一条记录。
    fn record_macro_button(rec: &mut MacroRecording, code: u16, pressed: bool, now: Instant) {
        rec.last_event = now;
        if pressed && rec.held.contains(&code) {
            return;
        }
        if !pressed && !rec.held.remove(&code) {
            return;
        }
        if pressed {
            rec.held.insert(code);
        }
        let delay = if rec.steps.is_empty() {
            0
        } else {
            now.duration_since(rec.last_step_at).as_millis().min(2000) as u32
        };
        rec.steps.push(MacroStep {
            code,
            pressed,
            delay_ms: delay,
        });
        rec.last_step_at = now;
    }

    fn macro_add_from_page(&mut self) {
        let Some(key) = self.macro_page_key else {
            self.log("请先选择宏触发键");
            return;
        };
        if self.macro_page_steps.is_empty() && self.macro_page_instructions.is_empty() {
            self.log("宏没有录制步骤或设置动作");
            return;
        }
        self.push_undo();
        self.shared.lock().unwrap().profile.binds.push(KeyBind {
            key,
            action: Action::Macro(MacroAction {
                steps: self.macro_page_steps.clone(),
                instructions: self.macro_page_instructions.clone(),
                virtual_profile: self.macro_page_virtual_profile.clone().map(Box::new),
            }),
            fps_only: self.macro_page_fps_only,
        });
        self.reset_macro_draft();
        self.log("已新建宏");
    }
    fn finish_macro_recording(&mut self) {
        if let Some(rec) = self.macro_recording.take() {
            self.macro_page_steps = rec.steps;
            self.log(format!(
                "宏录制结束：{} 个事件",
                self.macro_page_steps.len()
            ));
        }
    }
    /// 新增草稿编辑器(点击空闲键后出现在操作栏)。
    /// 与默认风格的新增节同一套参数控件;类型选择用方形标签按钮。
    fn ui_vk_draft(&mut self, ui: &mut egui::Ui) {
        let th = self.theme();
        ui.horizontal(|ui| {
            ui.strong("新增键位:");
            ui.monospace(
                self.draft
                    .key
                    .map(key_name)
                    .unwrap_or_else(|| "未定".into()),
            );
            if Self::tab_button(
                ui,
                "改键",
                self.waiting_key == Some(KeySlot::NewBind),
                th.accent,
            ) {
                self.picking = None;
                self.resizing = None;
                self.waiting_key = Some(KeySlot::NewBind);
            }
            ui.checkbox(&mut self.draft.fps_only, "仅 FPS")
                .on_hover_text("开启后该键只在 FPS 模式生效(FPS 页点空闲键会自动勾上)");
            for (ki, n) in KIND_NAMES.iter().enumerate() {
                if Self::tab_button(ui, n, self.draft.kind == ki, th.accent) {
                    self.draft.kind = ki;
                    self.draft_active = true;
                }
            }
        });
        match self.draft.kind {
            0 | 1 => {
                ui.horizontal(|ui| {
                    ui.label("x:");
                    ui.add(egui::DragValue::new(&mut self.draft.x).range(COORD_RANGE));
                    ui.label("y:");
                    ui.add(egui::DragValue::new(&mut self.draft.y).range(COORD_RANGE));
                    if self.draft.kind == 0 {
                        ui.label("时长ms:");
                        ui.add(
                            egui::DragValue::new(&mut self.draft.tap_duration_ms).range(0..=5000),
                        )
                        .on_hover_text("0=按下不松手,直到再按一次");
                    }
                    ui.label("范围:");
                    ui.add(egui::DragValue::new(&mut self.draft.radius).range(0.01..=100000.0));
                    let waiting_p = self.picking == Some(CoordSlot::NewBind);
                    if Self::tab_button(
                        ui,
                        if waiting_p { "取点中..." } else { "取点" },
                        waiting_p,
                        th.ok,
                    ) {
                        self.begin_pick(CoordSlot::NewBind);
                        self.draft_active = true;
                    }
                });
            }
            2 => {
                // 滑动:与既有新增节同一套 swipe_controls(草稿用 New* 槽位;
                // 可视化风格下新增节不渲染,不会出现重复控件 id)
                let mut pick = self.picking;
                let mut easing_edit = self.easing_edit;
                let mut tmp = Swipe {
                    start: (
                        self.draft.swipe_start.0 as f32,
                        self.draft.swipe_start.1 as f32,
                    ),
                    end: (self.draft.swipe_end.0 as f32, self.draft.swipe_end.1 as f32),
                    duration_ms: self.draft.swipe_duration_ms,
                    easing: self.draft.swipe_easing,
                    path: self.draft.swipe_path,
                };
                swipe_controls(
                    ui,
                    &mut tmp,
                    &mut pick,
                    &mut easing_edit,
                    CoordSlot::NewSwipeStart,
                    CoordSlot::NewSwipeEnd,
                    CoordSlot::NewCircleAngle,
                    EasingEditTarget::New,
                );
                self.draft.swipe_start = (tmp.start.0 as i32, tmp.start.1 as i32);
                self.draft.swipe_end = (tmp.end.0 as i32, tmp.end.1 as i32);
                self.draft.swipe_duration_ms = tmp.duration_ms;
                self.draft.swipe_easing = tmp.easing;
                self.draft.swipe_path = tmp.path;
                if let Some(slot) = pick {
                    self.begin_pick(slot);
                } else {
                    self.picking = None;
                }
                self.easing_edit = easing_edit;
            }
            3 => {
                ui.horizontal(|ui| {
                    ui.label("keycode(返回=4 主页=3):");
                    ui.add(egui::DragValue::new(&mut self.draft.keycode).range(0..=999));
                });
            }
            _ => {
                ui.horizontal(|ui| {
                    ui.label("keycode(返回=4 主页=3):");
                    ui.add(egui::DragValue::new(&mut self.draft.keycode).range(0..=999));
                });
            }
        }
        ui.horizontal(|ui| {
            if Self::tab_button(ui, "确认新增", false, th.ok) {
                if self.commit_draft() {
                    self.vk_sel = None;
                    self.vk_bottom_add_active = false;
                }
            }
            if Self::tab_button(ui, "取消", false, th.danger) {
                self.cancel_draft();
                self.vk_sel = None;
                self.vk_bottom_add_active = false;
                self.log("已取消新增");
            }
        });
    }

    /// 特殊目标(总开关/FPS 三键/摇杆方向/临时启用/切换键/组合键成员)的编辑行。
    /// 键细节(坐标/半径等)仍在下方对应列表里改,这里负责"哪个键"。
    fn ui_vk_special(&mut self, ui: &mut egui::Ui, slot: KeySlot, title: &str) {
        ui.horizontal(|ui| {
            ui.strong(title);
            let waiting = self.waiting_key == Some(slot);
            let code = self.vk_slot_code(&slot);
            if Self::key_button(ui, waiting, Some(code)).clicked() {
                self.picking = None;
                self.resizing = None;
                self.waiting_key = Some(slot);
                self.log("按任意键完成改绑(或点[取消选择]退出)");
            }
            if Self::tab_button(ui, "取消选择", false, self.theme().accent) {
                self.vk_sel = None;
                self.waiting_key = None;
            }
        });
        if self.waiting_key == Some(slot) {
            ui.small("等待按键中... 按任意键完成改绑");
        }
    }

    /// 读取一个 KeySlot 当前持有的键码(0 = 未绑定)
    fn vk_slot_code(&self, slot: &KeySlot) -> u16 {
        let g = self.shared.lock().unwrap();
        match slot {
            KeySlot::Toggle => g.profile.toggle_key,
            KeySlot::CursorToggle => g.profile.cursor_toggle_key,
            KeySlot::AimHold => g.profile.aim.hold_key,
            KeySlot::AimToggle => g.profile.aim.toggle_key,
            KeySlot::AimSuspend => g.profile.aim.suspend_key,
            KeySlot::WheelDir { wheel, dir } => g
                .profile
                .wheels
                .get(*wheel)
                .and_then(|w| w.active_dirs().get(*dir).map(|(_, key)| *key))
                .unwrap_or(0),
            KeySlot::WheelEnable(i) => g
                .profile
                .wheels
                .get(*i)
                .and_then(|w| w.temp.as_ref().map(|t| t.key))
                .unwrap_or(0),
            KeySlot::SwitchKey(i) => g.switch_keys.get(*i).map(|s| s.key).unwrap_or(0),
            KeySlot::SwitchKeySecond(i) => g
                .switch_keys
                .get(*i)
                .and_then(|s| s.keys.get(1))
                .copied()
                .unwrap_or(0),
            KeySlot::ComboKey { combo, slot } => g
                .profile
                .combos
                .get(*combo)
                .and_then(|c| c.keys.get(*slot))
                .copied()
                .unwrap_or(0),
            KeySlot::MacroTrigger => self.macro_page_key.unwrap_or(0),
            KeySlot::MacroInstructionKey(index) => self
                .macro_page_instructions
                .get(*index)
                .and_then(|instruction| match instruction {
                    MacroInstruction::Key { code, .. } => Some(*code),
                    _ => None,
                })
                .unwrap_or(0),
            KeySlot::MacroInstructionComboKey { instruction, slot } => self
                .macro_page_instructions
                .get(*instruction)
                .and_then(|item| match item {
                    MacroInstruction::Combo { keys, .. } => keys.get(*slot).copied(),
                    _ => None,
                })
                .unwrap_or(0),
            KeySlot::NewBind | KeySlot::Bind(_) => 0,
        }
    }

    /// Optional simultaneous chords.  The first key is the chord leader: it is
    /// held briefly so a following key can complete the chord without firing
    /// the leader's single-key action first.
    fn ui_combos(&mut self, ui: &mut egui::Ui) {
        let (mut enabled, combos, mapper) = {
            let g = self.shared.lock().unwrap();
            let screen = g
                .control
                .as_ref()
                .map(|c| (c.screen_w, c.screen_h))
                .or(g.profile.screen)
                .unwrap_or((1080, 2400));
            (
                g.profile.combos_enabled,
                g.profile.combos.clone(),
                g.profile.mapper(screen),
            )
        };
        ui.separator();
        ui.heading("组合键（可选）");
        if ui
            .checkbox(&mut enabled, "启用组合键")
            .on_hover_text("关闭时组合键设置保留但整体变灰失效；第一个键作为组合前缀。")
            .changed()
        {
            self.push_undo();
            self.shared.lock().unwrap().profile.combos_enabled = enabled;
        }
        ui.label("按键顺序：第一个键是前缀，后续键要与前缀同时按住。Ctrl+R 应先按 Ctrl 再按 R。")
            .on_hover_text(
                "有单键映射时使用 45ms 快速判定；没有单键映射时可等待 120ms，降低误触。",
            );

        let mut delete_combo = None;
        let mut delete_slot = None;
        let mut add_slot = None;
        let mut snapshots: Vec<(usize, KeyCombo)> = Vec::new();
        let mut combo_pick = self.picking;
        let mut combo_easing = self.easing_edit;
        for (index, original) in combos.into_iter().enumerate() {
            let mut combo = original;
            ui.add_enabled_ui(enabled, |ui| {
                egui::Frame::group(ui.style())
                    .inner_margin(egui::Margin::same(8))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(format!("组合 {}", index + 1));
                            if ui.small_button("删除组合").clicked() {
                                delete_combo = Some(index);
                            }
                            if ui.small_button("＋ 新增成员").clicked() {
                                add_slot = Some(index);
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label("按键:");
                            for (slot, key) in combo.keys.iter().copied().enumerate() {
                                let waiting = self.waiting_key
                                    == Some(KeySlot::ComboKey { combo: index, slot });
                                let shown = (key != 0).then_some(key);
                                if Self::key_button(ui, waiting, shown).clicked() {
                                    self.waiting_key =
                                        Some(KeySlot::ComboKey { combo: index, slot });
                                }
                                if combo.keys.len() > 2 && ui.small_button("×").clicked() {
                                    delete_slot = Some((index, slot));
                                }
                                ui.label(if slot == 0 { "前缀" } else { "成员" });
                            }
                        });
                        let action_changed = combo_action_editor(
                            ui,
                            &mut combo.action,
                            &mapper,
                            index,
                            &mut combo_pick,
                            &mut combo_easing,
                        );
                        let fps_changed = ui.checkbox(&mut combo.fps_only, "仅视角模式").changed();
                        if action_changed || fps_changed {
                            snapshots.push((index, combo.clone()));
                        }
                    });
            });
        }
        self.picking = combo_pick;
        self.easing_edit = combo_easing;
        if let Some(index) = delete_combo {
            self.push_undo();
            let mut g = self.shared.lock().unwrap();
            if index < g.profile.combos.len() {
                g.profile.combos.remove(index);
            }
            self.waiting_key = None;
        }
        if let Some((combo_index, slot)) = delete_slot {
            self.push_undo();
            let mut g = self.shared.lock().unwrap();
            if let Some(combo) = g.profile.combos.get_mut(combo_index)
                && slot < combo.keys.len()
            {
                combo.keys.remove(slot);
            }
            self.waiting_key = None;
        }
        if let Some(index) = add_slot {
            self.push_undo();
            let mut g = self.shared.lock().unwrap();
            if let Some(combo) = g.profile.combos.get_mut(index) {
                combo.keys.push(0);
            }
        }
        for (index, combo) in snapshots {
            self.push_undo();
            if let Some(target) = self.shared.lock().unwrap().profile.combos.get_mut(index) {
                *target = combo;
            }
        }
        if ui.button("＋ 新增组合键").clicked() {
            self.push_undo();
            let m = self.mapper();
            let (x, y) = (m.rel_x(540), m.rel_y(960));
            self.shared.lock().unwrap().profile.combos.push(KeyCombo {
                keys: vec![0, 0],
                action: Action::Tap {
                    x,
                    y,
                    duration_ms: crate::keymap::DEFAULT_TAP_DURATION_MS,
                    radius: crate::keymap::DEFAULT_RADIUS,
                },
                fps_only: false,
            });
            self.scroll_to_new = true;
        }
        self.apply_pending_scroll(ui);
    }

    fn ui_wheels(&mut self, ui: &mut egui::Ui) {
        ui.heading("轮盘(虚拟摇杆)");
        ui.label("设置[启用键]后变为临时轮盘:仅在启用期间生效,期间方向键的其它绑定让位");
        let mut to_delete: Option<usize> = None;
        let wheel_count = self.shared.lock().unwrap().profile.wheels.len();

        for i in 0..wheel_count {
            ui.horizontal(|ui| {
                let temp_info = {
                    let g = self.shared.lock().unwrap();
                    let w = &g.profile.wheels[i];
                    (
                        w.temp.as_ref().map(|t| (t.key, t.mode)),
                        w.mode,
                        w.kind,
                        format!(
                            "轮盘{}{}",
                            i + 1,
                            if w.temp.is_some() { "(临时)" } else { "" },
                        ),
                    )
                };
                let (temp, old_mode, old_kind, title) = temp_info;
                ui.label(title);

                let mut kind = old_kind;
                egui::ComboBox::from_id_salt(("wheel_kind", i))
                    .selected_text(kind.label())
                    .show_ui(ui, |ui| {
                        for k in [WheelKind::Standard, WheelKind::Custom, WheelKind::Execute] {
                            ui.selectable_value(&mut kind, k, k.label());
                        }
                    });
                if kind != old_kind {
                    self.push_undo();
                    {
                        let mut g = self.shared.lock().unwrap();
                        if let Some(w) = g.profile.wheels.get_mut(i) {
                            w.kind = kind;
                            w.ensure_custom_directions();
                        }
                    }
                    self.log(format!("轮盘类型已切换为: {}", kind.label()));
                }

                if kind == WheelKind::Standard {
                    let mut mode = old_mode;
                    egui::ComboBox::from_id_salt(("wheel_mode", i))
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
                    if mode != old_mode {
                        self.push_undo();
                        {
                            let mut g = self.shared.lock().unwrap();
                            if let Some(w) = g.profile.wheels.get_mut(i) {
                                w.mode = mode;
                            }
                        }
                        self.log(format!("轮盘模式已切换为: {}", mode.label()));
                    }
                } else {
                    ui.label("双键方向取平均");
                }

                // 启用键(设置后变为临时轮盘)
                ui.label("启用键:");
                let ek = temp.map(|(k, _)| k);
                let waiting_e = self.waiting_key == Some(KeySlot::WheelEnable(i));
                if Self::key_button(ui, waiting_e, ek).clicked() {
                    self.waiting_key = Some(KeySlot::WheelEnable(i));
                }
                if let Some((_, mode)) = temp {
                    if ui
                        .button(match mode {
                            TempMode::Hold => "模式:长按启用",
                            TempMode::Toggle => "模式:再按切换",
                        })
                        .clicked()
                    {
                        self.push_undo();
                        let mut g = self.shared.lock().unwrap();
                        if let Some(t) = g.profile.wheels[i].temp.as_mut() {
                            t.mode = match t.mode {
                                TempMode::Hold => TempMode::Toggle,
                                TempMode::Toggle => TempMode::Hold,
                            };
                        }
                    }
                    if ui.button("设为永久").clicked() {
                        self.push_undo();
                        {
                            let mut g = self.shared.lock().unwrap();
                            g.profile.wheels[i].temp = None;
                        }
                        self.log("已设为永久轮盘");
                    }
                } else {
                    ui.label("(永久)");
                }
            });
            let (dirs, kind) = {
                let g = self.shared.lock().unwrap();
                let w = &g.profile.wheels[i];
                (w.active_dirs(), w.kind)
            };
            if kind != WheelKind::Standard {
                ui.horizontal(|ui| {
                    ui.label("方向数量:");
                    let mut count = dirs.len().clamp(2, 8);
                    let r = ui.add(egui::DragValue::new(&mut count).range(2..=8));
                    if r.drag_started() || r.gained_focus() {
                        self.push_undo();
                    }
                    if r.changed() {
                        let mut g = self.shared.lock().unwrap();
                        g.profile.wheels[i].set_direction_count(count);
                    }
                    ui.label("最多 8 个；同时只接受最早按下的 2 个方向并取平均");
                });
            }
            for (d, (angle, code)) in dirs.into_iter().enumerate() {
                ui.horizontal(|ui| {
                    let label = match angle.round() as i32 {
                        -90 => "上".to_string(),
                        0 => "右".to_string(),
                        90 => "下".to_string(),
                        a if a.abs() == 180 => "左".to_string(),
                        a => format!("{a}°"),
                    };
                    ui.label(format!("{}{}", label, d + 1));
                    if kind != WheelKind::Standard {
                        ui.label("角度:");
                        let mut a = angle;
                        let r = ui.add(
                            egui::DragValue::new(&mut a)
                                .range(-360.0..=360.0)
                                .suffix("°"),
                        );
                        if r.drag_started() || r.gained_focus() {
                            self.push_undo();
                        }
                        if r.changed() {
                            let mut g = self.shared.lock().unwrap();
                            if let Some(dir) = g.profile.wheels[i].directions.get_mut(d) {
                                dir.angle_deg = a;
                            }
                        }
                    }
                    ui.label("按键:");
                    let waiting = self.waiting_key == Some(KeySlot::WheelDir { wheel: i, dir: d });
                    if Self::key_button(ui, waiting, Some(code)).clicked() {
                        self.waiting_key = Some(KeySlot::WheelDir { wheel: i, dir: d });
                    }
                });
            }
            // 半径 / 影响范围:两行放不下(双向滚动区里横排太长),故半径独占一行、
            // 影响范围另起一行,并给出"实际推出的像素距离"便于和游戏里的判定圈对照。
            ui.horizontal(|ui| {
                let waiting_p = self.picking == Some(CoordSlot::WheelCenter(i));
                let m = self.mapper();
                {
                    let mut g = self.shared.lock().unwrap();
                    // 撤销快照只取这一个轮盘(理由同按键区:避免每帧深拷贝整份配置)
                    let before = g.profile.wheels.get(i).cloned();
                    let mut wheel_edit = false;
                    let w = &mut g.profile.wheels[i];
                    ui.label("圆心x:");
                    let mut px = m.x(w.cx);
                    let r = ui.add(egui::DragValue::new(&mut px).range(COORD_RANGE));
                    if r.changed() {
                        w.cx = m.rel_x(px);
                    }
                    wheel_edit |= r.drag_started() || r.gained_focus();
                    ui.label("y:");
                    let mut py = m.y(w.cy);
                    let r = ui.add(egui::DragValue::new(&mut py).range(COORD_RANGE));
                    if r.changed() {
                        w.cy = m.rel_y(py);
                    }
                    wheel_edit |= r.drag_started() || r.gained_focus();
                    ui.label("半径:");
                    let mut pr = m.len(w.radius);
                    let r = ui.add(egui::DragValue::new(&mut pr).range(10..=1000));
                    if r.changed() {
                        w.radius = m.rel_len(pr);
                    }
                    wheel_edit |= r.drag_started() || r.gained_focus();
                    if w.kind == WheelKind::Execute {
                        ui.label("中心范围:");
                        let mut cr = m.len(w.center_radius);
                        let r = ui.add(egui::DragValue::new(&mut cr).range(10..=500).suffix("px"));
                        if r.changed() {
                            w.center_radius = m.rel_len(cr);
                        }
                        wheel_edit |= r.drag_started() || r.gained_focus();
                        ui.label("滑动:");
                        let r = ui.add(
                            egui::DragValue::new(&mut w.execute_duration_ms)
                                .range(30..=500)
                                .suffix("ms"),
                        );
                        wheel_edit |= r.drag_started() || r.gained_focus();
                    }
                    // 影响范围:触点实际推出的距离 = 半径 × 系数。
                    // 半径仍是截图上那个圆环(视觉/响应圈),系数单独决定手指推多远,
                    // 于是可以把"看得见的圈"和"游戏里真实摇杆的判定圈"解耦。
                    ui.label("影响范围:");
                    let mut scope = w.scope();
                    let r = ui
                        .add(
                            egui::DragValue::new(&mut scope)
                                .speed(0.02)
                                .range(crate::keymap::SCOPE_MIN..=crate::keymap::SCOPE_MAX)
                                .suffix("×半径"),
                        )
                        .on_hover_text(
                            "触点实际推出的距离 = 半径 × 该系数。\n\
                             1.0 = 与半径一致(老配置默认);\n\
                             调大 = 手指推得更远,摇杆更灵敏;调小 = 更精细。\n\
                             不等于 1.0 时,截图上会多画一圈橙色虚线表示实际推出距离。",
                        );
                    if r.changed() {
                        w.scope = crate::keymap::clamp_scope(scope);
                    }
                    wheel_edit |= r.drag_started() || r.gained_focus();
                    let push_px = w.push_px(&m);
                    ui.small(format!("= {push_px:.0}px"));
                    if wheel_edit {
                        if let Some(before) = before {
                            let mut snapshot = g.profile.clone();
                            snapshot.wheels[i] = before;
                            self.pending_undo = Some(snapshot);
                        }
                    }
                }
                if ui
                    .button("影响范围复位")
                    .on_hover_text("恢复为 1.0(推出距离等于半径)")
                    .clicked()
                {
                    self.push_undo();
                    self.shared.lock().unwrap().profile.wheels[i].scope =
                        crate::keymap::DEFAULT_WHEEL_SCOPE;
                }
                if ui
                    .button(if waiting_p {
                        "点击截图..."
                    } else {
                        "取圆心"
                    })
                    .clicked()
                {
                    self.begin_pick(CoordSlot::WheelCenter(i));
                }
                // 与按键一致的"改响应范围":进入后 Ctrl++/- 调半径,或直接在截图上拖动
                let resizing_w = self.resizing == Some(ResizeTarget::Wheel(i));
                if ui
                    .button(if resizing_w {
                        "完成"
                    } else {
                        "改响应范围"
                    })
                    .on_hover_text("进入后用 Ctrl++ / Ctrl+- 缩放半径,或直接在截图上拖动圆圈")
                    .clicked()
                {
                    if resizing_w {
                        self.resizing = None;
                    } else {
                        self.begin_resize_wheel(i);
                    }
                }
                if ui.button("删除").clicked() {
                    to_delete = Some(i);
                }
            });
        }
        if let Some(i) = to_delete {
            self.push_undo();
            self.shared.lock().unwrap().profile.wheels.remove(i);
            if self.resizing == Some(ResizeTarget::Wheel(i)) {
                self.resizing = None;
            }
            self.log("已删除轮盘");
        }
        if ui.button("新增轮盘").clicked() {
            self.push_undo();
            // 半径固定 150px、落点避开已有摇杆(见 keymap::Wheel::new_default /
            // next_wheel_spot):新建出来的摇杆不该比用户辛苦调小的那个大一圈,
            // 也不该叠在别的摇杆上让人分不清
            let m = self.mapper();
            let (cx, cy) = {
                let g = self.shared.lock().unwrap();
                crate::keymap::next_wheel_spot(&g.profile.wheels)
            };
            let wheel = Wheel::new_default(&m, cx, cy);
            let r_px = m.len(wheel.radius);
            let n = {
                let mut g = self.shared.lock().unwrap();
                g.profile.wheels.push(wheel);
                g.profile.wheels.len()
            };
            self.log(format!(
                "已新增轮盘 {n}(半径 {r_px:.0}px,圆心 {:.0}%,{:.0}%)",
                cx * 100.0,
                cy * 100.0
            ));
            self.scroll_to_new = true;
        }
        self.apply_pending_scroll(ui);
    }

    /// 新建一个默认轮盘，并返回它在配置中的下标。
    fn add_wheel_default(&mut self) -> usize {
        self.push_undo();
        self.scroll_to_new = true;
        let m = self.mapper();
        let (cx, cy) = {
            let g = self.shared.lock().unwrap();
            crate::keymap::next_wheel_spot(&g.profile.wheels)
        };
        let wheel = Wheel::new_default(&m, cx, cy);
        let r_px = m.len(wheel.radius);
        let n = {
            let mut g = self.shared.lock().unwrap();
            g.profile.wheels.push(wheel);
            g.profile.wheels.len()
        };
        self.log(format!(
            "已新增轮盘 {n}(半径 {r_px:.0}px,圆心 {:.0}%,{:.0}%)",
            cx * 100.0,
            cy * 100.0
        ));
        n - 1
    }
    /// 在手机截图上叠加显示所有键位/轮盘的位置示意
    fn draw_overlay(&self, ui: &egui::Ui, rect: egui::Rect, scale: f32) {
        use egui::{Align2, FontId, Stroke, vec2};
        use theme::size;
        let painter = ui.painter();
        let to_screen = |x: i32, y: i32| rect.min + vec2(x as f32 * scale, y as f32 * scale);
        let short_name = |code: u16| key_name(code).replace("KEY_", "");

        // 坐标换算器必须**先**构造好(它内部会加配置锁);
        // 若先拿到配置锁再调 self.mapper() 会自锁,界面会直接卡死。
        let m = self.mapper();
        let g = self.shared.lock().unwrap();
        let th = g.profile.look.theme();
        // 键位浮层的显示亮度档位(0=默认;负=加深,正=变浅)。
        // 手动微调档位;每个标注的实际档位还要叠上"按截图采样的自动对比"
        // (见 theme 的浮层可读性说明:描边 + 衬底 + 自动对比三招)
        let tone_manual = g.profile.look.overlay_tone;
        let auto = g.profile.look.overlay_auto;
        let tone_at = |sx: i32, sy: i32| match (&self.shot_lum, auto) {
            (Some(grid), true) => theme::auto_tone(grid.around(sx, sy), tone_manual),
            _ => theme::clamp_tone(tone_manual),
        };

        // 键位(含草稿标记)仅在"全部/仅键位"时显示
        if self.overlay_filter.keys {
            let fps_overlay_active = g.aim_live.mode_active && !g.aim_live.suspended;
            for (i, b) in g.profile.binds.iter().enumerate() {
                if matches!(b.action, Action::Macro(_)) {
                    continue;
                }
                // 仅 FPS 键位始终显示，方便用户在截图取点阶段看见。
                // 独立紫色与普通点按/长按区分；FPS 运行中提亮。
                // 表示它此刻正在抢占同键普通映射。
                let fps_only_dim = if b.fps_only && !fps_overlay_active {
                    0.68
                } else {
                    1.0
                };
                match &b.action {
                    Action::Tap { x, y, radius, .. } | Action::Hold { x, y, radius } => {
                        let (px, py) = (m.x(*x), m.y(*y));
                        let p = to_screen(px, py);
                        let r = m.len(*radius) * scale;
                        let tone = tone_at(px, py);
                        // 修改响应范围中的键位显示黄色;否则点按绿、长按橙
                        let (ring, fill) = if self.resizing == Some(ResizeTarget::Bind(i)) {
                            (th.key_resize, th.key_resize_fill)
                        } else if b.fps_only {
                            (th.key_fps, th.key_fps_fill)
                        } else if matches!(b.action, Action::Tap { .. }) {
                            (th.key_tap, th.key_tap_fill)
                        } else {
                            (th.key_hold, th.key_hold_fill)
                        };
                        let (ring, fill) = (
                            ring.gamma_multiply(fps_only_dim),
                            fill.gamma_multiply(fps_only_dim),
                        );
                        let (ring, fill) = theme::tone_ring_fill(ring, fill, tone);
                        painter.circle_filled(p, r, fill);
                        // 外套(halo):先画一圈反相粗线,再画本色圈 ——
                        // 于是无论在亮块还是暗块上,圈都有一圈边界可辨
                        painter.circle_stroke(
                            p,
                            r,
                            Stroke::new(size::KEY_STROKE + 2.5, theme::casing(ring)),
                        );
                        painter.circle_stroke(p, r, Stroke::new(size::KEY_STROKE, ring));
                        theme::paint_label(
                            painter,
                            p,
                            Align2::CENTER_CENTER,
                            &short_name(b.key),
                            FontId::proportional(size::KEY_FONT),
                            theme::tone_text(tone),
                        );
                    }
                    Action::Swipe(s) => {
                        let sp = m.point(s.start.0, s.start.1);
                        let ep = m.point(s.end.0, s.end.1);
                        draw_swipe_track(
                            painter,
                            &th,
                            s.path,
                            sp,
                            ep,
                            &to_screen,
                            scale,
                            &short_name(b.key),
                            tone_at(sp.0, sp.1),
                        );
                    }
                    Action::AndroidKey { .. } => {}
                    Action::Macro(_) => {}
                }
            }
            // 草稿(新增绑定)高亮:0 不显示,1 显示点/圈,2 显示滑动轨迹。
            // 仅在新增草稿进行中或等待设置按键时显示,取消后即消失。
            let draft_kind = if self.draft_active || self.waiting_key == Some(KeySlot::NewBind) {
                if self.draft.kind == 2 { 2 } else { 1 }
            } else {
                0
            };
            match draft_kind {
                2 => {
                    // 草稿坐标本身就是像素,直接绘制
                    draw_swipe_track(
                        painter,
                        &th,
                        self.draft.swipe_path,
                        self.draft.swipe_start,
                        self.draft.swipe_end,
                        &to_screen,
                        scale,
                        "新增",
                        tone_at(self.draft.swipe_start.0, self.draft.swipe_start.1),
                    );
                }
                1 => {
                    let dp = to_screen(self.draft.x, self.draft.y);
                    let r = self.draft.radius * scale;
                    let tone = tone_at(self.draft.x, self.draft.y);
                    let ink = theme::tone_color(th.draft, tone);
                    painter.circle_stroke(
                        dp,
                        r,
                        Stroke::new(size::KEY_STROKE + 2.5, theme::casing(ink)),
                    );
                    painter.circle_stroke(dp, r, Stroke::new(size::KEY_STROKE, ink));
                    theme::paint_label(
                        painter,
                        dp + vec2(0.0, r + 10.0),
                        Align2::CENTER_CENTER,
                        "新增",
                        FontId::proportional(size::SMALL_FONT),
                        ink,
                    );
                }
                _ => {}
            }
        }

        // 组合键没有单独的“触发坐标”，在动作落点绘制蓝色组合标记；滑动沿用轨迹。
        if self.overlay_filter.combos {
            for (ci, combo) in g.profile.combos.iter().enumerate() {
                let keys = combo
                    .keys
                    .iter()
                    .filter(|k| **k != 0)
                    .map(|k| short_name(*k))
                    .collect::<Vec<_>>()
                    .join("+");
                match &combo.action {
                    Action::Tap { x, y, radius, .. } | Action::Hold { x, y, radius } => {
                        let (px, py) = (m.x(*x), m.y(*y));
                        let p = to_screen(px, py);
                        let r = m.len(*radius) * scale;
                        let tone = tone_at(px, py);
                        let ink = theme::tone_color(th.accent, tone);
                        painter.circle_filled(p, r, theme::with_alpha(ink, 45));
                        painter.circle_stroke(
                            p,
                            r,
                            Stroke::new(size::KEY_STROKE + 2.0, theme::casing(ink)),
                        );
                        painter.circle_stroke(p, r, Stroke::new(size::KEY_STROKE, ink));
                        theme::paint_label(
                            painter,
                            p,
                            Align2::CENTER_CENTER,
                            &format!("组合{}", ci + 1),
                            FontId::proportional(size::SMALL_FONT),
                            theme::tone_text(tone),
                        );
                        if !keys.is_empty() {
                            theme::paint_label(
                                painter,
                                p + vec2(0.0, r + 10.0),
                                Align2::CENTER_CENTER,
                                &keys,
                                FontId::proportional(size::SMALL_FONT),
                                ink,
                            );
                        }
                    }
                    Action::Swipe(s) => {
                        let sp = m.point(s.start.0, s.start.1);
                        let ep = m.point(s.end.0, s.end.1);
                        draw_swipe_track(
                            painter,
                            &th,
                            s.path,
                            sp,
                            ep,
                            &to_screen,
                            scale,
                            &format!("组合{} {}", ci + 1, keys),
                            tone_at(sp.0, sp.1),
                        );
                    }
                    Action::AndroidKey { .. } | Action::Macro(_) => {}
                }
            }
        }

        // 宏没有固定落点，用截图左下角的标签列出触发键，确保浮层和虚拟键盘
        // 都能直接看到宏；原始动作不会挤占截图。
        if self.overlay_filter.macros {
            let macros: Vec<(u16, u32)> = g
                .profile
                .binds
                .iter()
                .filter_map(|b| match &b.action {
                    Action::Macro(m) => Some((
                        b.key,
                        m.steps
                            .iter()
                            .map(|s| s.delay_ms)
                            .sum::<u32>()
                            .saturating_add(
                                m.instructions
                                    .iter()
                                    .map(|i| match i {
                                        MacroInstruction::Delay { ms } => *ms,
                                        MacroInstruction::Key { duration_ms, .. }
                                        | MacroInstruction::Combo { duration_ms, .. }
                                        | MacroInstruction::Wheel { duration_ms, .. }
                                        | MacroInstruction::Click { duration_ms, .. }
                                        | MacroInstruction::Swipe { duration_ms, .. } => {
                                            *duration_ms
                                        }
                                        MacroInstruction::Fps { .. }
                                        | MacroInstruction::Macro { .. } => 0,
                                    })
                                    .sum::<u32>(),
                            ),
                    )),
                    _ => None,
                })
                .collect();
            for (n, (key, ms)) in macros.iter().take(8).enumerate() {
                let pos = rect.min + vec2(12.0, 20.0 + n as f32 * 18.0);
                painter.rect_filled(
                    egui::Rect::from_min_size(
                        pos - vec2(6.0, 9.0),
                        vec2(rect.width().min(260.0) - 12.0, 18.0),
                    ),
                    3.0,
                    th.key_macro_fill,
                );
                theme::paint_label(
                    painter,
                    pos,
                    Align2::LEFT_CENTER,
                    &format!("宏 {} / {}ms", short_name(*key), ms),
                    FontId::proportional(size::SMALL_FONT),
                    th.key_macro,
                );
            }
        }

        // FPS 瞄准锚点:与键位一样画在截图上,标出鼠标拖动时的落点起点
        if self.overlay_filter.aim && g.profile.aim.anchor_set() {
            let aim = &g.profile.aim;
            let (ax, ay) = (m.x(aim.anchor_x), m.y(aim.anchor_y));
            let p = to_screen(ax, ay);
            let tone = tone_at(ax, ay);
            let c = theme::tone_color(th.aim, tone);
            let thin = Stroke::new(1.0, c);
            // 阈值归中时,先把触发归中的偏移范围画成虚线圆,便于对照调参
            if aim.recenter == RecenterMode::Threshold {
                let r = aim.recenter_threshold.max(1) as f32 * scale;
                let n = 72;
                let dash = theme::with_alpha(c, 130);
                let mut i = 0;
                while i < n {
                    let a0 = i as f32 / n as f32 * std::f32::consts::TAU;
                    let a1 = (i + 1) as f32 / n as f32 * std::f32::consts::TAU;
                    painter.line_segment(
                        [
                            p + vec2(a0.cos() * r, a0.sin() * r),
                            p + vec2(a1.cos() * r, a1.sin() * r),
                        ],
                        Stroke::new(1.0, dash),
                    );
                    i += 3; // 隔两段画一段 => 虚线
                }
            }
            // 落点范围示意:半透明填充 + 圆圈 + 十字(圈同样带反相外套)
            let ring = size::AIM_RING;
            let arm = size::AIM_ARM;
            painter.circle_filled(p, ring, theme::with_alpha(c, 56));
            painter.circle_stroke(
                p,
                ring,
                Stroke::new(size::KEY_STROKE + 2.5, theme::casing(c)),
            );
            painter.circle_stroke(p, ring, Stroke::new(size::KEY_STROKE, c));
            painter.line_segment([p - vec2(arm, 0.0), p + vec2(arm, 0.0)], thin);
            painter.line_segment([p - vec2(0.0, arm), p + vec2(0.0, arm)], thin);
            // 标签与键位一致:显示名称,门控键存在时一并显示
            let label = if aim.hold_key == 0 {
                "瞄准锚点".to_string()
            } else {
                format!("瞄准锚点 [{}]", key_name(aim.hold_key))
            };
            theme::paint_label(
                painter,
                p + vec2(0.0, -(arm + 6.0)),
                Align2::CENTER_CENTER,
                &label,
                FontId::proportional(size::LABEL_FONT),
                theme::tone_text(tone),
            );
        }

        // 轮盘按过滤条件显示;临时轮盘用虚线圆环区分
        if self.overlay_filter.wheels_perm || self.overlay_filter.wheels_temp {
            for (wi, w) in g.profile.wheels.iter().enumerate() {
                let show = if w.temp.is_none() {
                    self.overlay_filter.wheels_perm
                } else {
                    self.overlay_filter.wheels_temp
                };
                if !show {
                    continue;
                }
                let c = to_screen(m.x(w.cx), m.y(w.cy));
                let r = m.len(w.radius) * scale;
                let tone = tone_at(m.x(w.cx), m.y(w.cy));
                let selected = self.wheel_info == Some(wi);
                // 正在"改响应范围"(改半径)的这个轮盘:圆环改用黄色,与键位一致
                let resizing_this = self.resizing == Some(ResizeTarget::Wheel(wi));
                // 影响范围:触点实际推出的距离,默认与半径一致(scope=1.0)时两者重合,
                // 此时不再多画一圈,避免与半径圆环糊在一起。
                let push_r = w.push_px(&m) * scale;
                let scope = w.scope();
                // 画布上的标注**只留"摇杆N / 临时摇杆N"**。
                // 方向键、启用键、影响范围这些细节改由"点一下摇杆"弹出信息卡显示
                // (用户反馈:那一长串字压在游戏画面上,既挡视野又看不清)。
                let quick_label = format!(
                    "{}{}",
                    if w.temp.is_some() {
                        "临时摇杆"
                    } else {
                        "摇杆"
                    },
                    wi + 1
                );
                if let Some(t) = &w.temp {
                    // 临时轮盘:虚线圆环(摇杆的视觉结构不变,只按亮度档位调明暗)
                    let base = if resizing_this {
                        th.key_resize
                    } else {
                        th.wheel_temp
                    };
                    let color = theme::tone_color(base, tone);
                    let n = 48;
                    let pts: Vec<egui::Pos2> = (0..=n)
                        .map(|i| {
                            let a = i as f32 * std::f32::consts::TAU / n as f32;
                            c + vec2(a.cos() * r, a.sin() * r)
                        })
                        .collect();
                    // 外套:先用反相粗虚线垫一层,再画本色虚线
                    for shape in egui::Shape::dashed_line(
                        &pts,
                        Stroke::new(5.0, theme::casing(color)),
                        6.0,
                        5.0,
                    ) {
                        painter.add(shape);
                    }
                    for shape in egui::Shape::dashed_line(&pts, Stroke::new(2.0, color), 6.0, 5.0) {
                        painter.add(shape);
                    }
                    if selected {
                        painter.circle_stroke(c, r + 6.0, Stroke::new(1.5, theme::tone_text(tone)));
                    }
                    painter.circle_filled(c, 5.0, color);
                    // 圆心那个小圈是"摇杆"观感的关键:它让圆心看起来是个可推的摇杆头,
                    // 所以无论亮度档位怎么调都保留(只跟着一起调明暗)
                    let knob = theme::tone_color(egui::Color32::WHITE, tone);
                    painter.circle_stroke(
                        c,
                        size::WHEEL_RING,
                        Stroke::new(3.5, theme::casing(knob)),
                    );
                    painter.circle_stroke(c, size::WHEEL_RING, Stroke::new(1.0, knob));
                    theme::paint_label(
                        painter,
                        c - vec2(0.0, r + 14.0),
                        Align2::CENTER_CENTER,
                        &quick_label,
                        FontId::proportional(size::LABEL_FONT),
                        theme::tone_text(tone),
                    );
                    let _ = t;
                } else {
                    // 永久轮盘:实线圆环
                    let base = if resizing_this {
                        th.key_resize
                    } else {
                        th.wheel_perm
                    };
                    let color = theme::tone_color(base, tone);
                    painter.circle_stroke(c, r, Stroke::new(6.0, theme::casing(color)));
                    painter.circle_stroke(c, r, Stroke::new(size::KEY_STROKE, color));
                    if selected {
                        painter.circle_stroke(c, r + 6.0, Stroke::new(1.5, theme::tone_text(tone)));
                    }
                    painter.circle_filled(c, 5.0, color);
                    let knob = theme::tone_color(egui::Color32::WHITE, tone);
                    painter.circle_stroke(
                        c,
                        size::WHEEL_RING,
                        Stroke::new(3.5, theme::casing(knob)),
                    );
                    painter.circle_stroke(c, size::WHEEL_RING, Stroke::new(1.0, knob));
                    theme::paint_label(
                        painter,
                        c - vec2(0.0, r + 14.0),
                        Align2::CENTER_CENTER,
                        &quick_label,
                        FontId::proportional(size::LABEL_FONT),
                        theme::tone_text(tone),
                    );
                }
                // 影响范围外环(橙色虚线):只在与半径明显不同时绘制,
                // 它表示"方向键按下后手指实际被推到多远",用来对照游戏里真实摇杆的判定圈。
                if (push_r - r).abs() > 1.0 {
                    let n = 48;
                    let pts: Vec<egui::Pos2> = (0..=n)
                        .map(|i| {
                            let a = i as f32 * std::f32::consts::TAU / n as f32;
                            c + vec2(a.cos() * push_r, a.sin() * push_r)
                        })
                        .collect();
                    let ring_ink = theme::tone_color(th.key_hold, tone);
                    for shape in egui::Shape::dashed_line(
                        &pts,
                        Stroke::new(3.5, theme::casing(ring_ink)),
                        7.0,
                        6.0,
                    ) {
                        painter.add(shape);
                    }
                    for shape in
                        egui::Shape::dashed_line(&pts, Stroke::new(1.5, ring_ink), 7.0, 6.0)
                    {
                        painter.add(shape);
                    }
                    theme::paint_label(
                        painter,
                        c + vec2(0.0, push_r + 12.0),
                        Align2::CENTER_CENTER,
                        &format!("影响范围 {push_r:.0}px(×{scope:.2})"),
                        FontId::proportional(size::SMALL_FONT),
                        theme::tone_text(tone),
                    );
                }
                // 点开的摇杆:在圈旁显示一张信息卡(四个方向键 + 启用键),不点不显示
                if selected {
                    draw_wheel_card(painter, &th, rect, w, wi, c, r, tone);
                }
            }
        }
    }

    /// 参数助手浮窗:每次打开重建默认参数;关闭即丢弃窗口内临时修改
    fn ui_args_helper(&mut self, ctx: &egui::Context) {
        if self.args_helper.is_none() {
            return;
        }
        let mut open = true;
        let mut helper = self.args_helper.take().unwrap();
        egui::Window::new("scrcpy 常用参数助手")
            .open(&mut open)
            .default_width(500.0)
            .show(ctx, |ui| {
                self.arg_help_panel(ui, &mut helper);
            });
        if open {
            // 窗口仍开着:放回状态,保留当前选中与临时修改
            self.args_helper = Some(helper);
        }
        // 关闭则丢弃(None)
    }

    /// 滑动曲线参数编辑浮窗(含速率函数预览图)
    fn ui_easing_editor(&mut self, ctx: &egui::Context) {
        let Some(target) = self.easing_edit else {
            return;
        };
        // 读取当前曲线
        let mut easing = match target {
            EasingEditTarget::Bind(i) => {
                let g = self.shared.lock().unwrap();
                match g.profile.binds.get(i).map(|b| &b.action) {
                    Some(Action::Swipe(s)) => s.easing,
                    _ => {
                        self.easing_edit = None;
                        return;
                    }
                }
            }
            EasingEditTarget::Combo(i) => {
                let g = self.shared.lock().unwrap();
                match g.profile.combos.get(i).map(|c| &c.action) {
                    Some(Action::Swipe(s)) => s.easing,
                    _ => {
                        self.easing_edit = None;
                        return;
                    }
                }
            }
            EasingEditTarget::New => self.draft.swipe_easing,
        };

        let mut open = true;
        // changed:数值确实变了,需要写回;undo_point:编辑刚开始,需要记一个撤销点
        let mut changed = false;
        let mut undo_point = false;
        egui::Window::new("滑动曲线设置")
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label("速率曲线预览(横轴时间,纵轴进度):");
                draw_easing_preview(ui, easing);
                ui.separator();
                match &mut easing {
                    Easing::Linear => {
                        ui.label("匀速曲线无参数");
                    }
                    Easing::EaseIn { power }
                    | Easing::EaseOut { power }
                    | Easing::Smooth { power } => {
                        ui.label("幂次:");
                        let r = ui.add(egui::DragValue::new(power).range(0.1..=10.0));
                        changed |= r.changed();
                        undo_point |= r.drag_started() || r.gained_focus();
                    }
                    Easing::Bezier { x1, y1, x2, y2 } => {
                        ui.label("x1:");
                        let r = ui.add(egui::DragValue::new(x1).range(0.0..=1.0));
                        changed |= r.changed();
                        undo_point |= r.drag_started() || r.gained_focus();
                        ui.label("y1:");
                        let r = ui.add(egui::DragValue::new(y1).range(-2.0..=2.0));
                        changed |= r.changed();
                        undo_point |= r.drag_started() || r.gained_focus();
                        ui.label("x2:");
                        let r = ui.add(egui::DragValue::new(x2).range(0.0..=1.0));
                        changed |= r.changed();
                        undo_point |= r.drag_started() || r.gained_focus();
                        ui.label("y2:");
                        let r = ui.add(egui::DragValue::new(y2).range(-2.0..=2.0));
                        changed |= r.changed();
                        undo_point |= r.drag_started() || r.gained_focus();
                    }
                }
            });

        if !open {
            self.easing_edit = None;
            return;
        }
        if changed {
            let mut g = self.shared.lock().unwrap();
            // 编辑刚开始那一帧:写回之前的配置就是撤销点
            let before = if undo_point {
                Some(g.profile.clone())
            } else {
                None
            };
            match target {
                EasingEditTarget::Bind(i) => {
                    if let Some(b) = g.profile.binds.get_mut(i) {
                        if let Action::Swipe(s) = &mut b.action {
                            s.easing = easing;
                        }
                    }
                }
                EasingEditTarget::Combo(i) => {
                    if let Some(combo) = g.profile.combos.get_mut(i) {
                        if let Action::Swipe(s) = &mut combo.action {
                            s.easing = easing;
                        }
                    }
                }
                EasingEditTarget::New => self.draft.swipe_easing = easing,
            }
            if let Some(b) = before {
                self.pending_undo = Some(b);
            }
        }
    }

    fn arg_help_panel(&mut self, ui: &mut egui::Ui, h: &mut ArgHelp) {
        ui.label(
            "单击选中参数;双击某条进入临时编辑(可改数值等)。\n临时修改在窗口关闭前一直保留,关闭后不保存;焦点移到别处不会丢失。",
        );
        let scrcpy_gh = "https://github.com/Genymobile/scrcpy";
        let count = h.entries.len();
        egui::ScrollArea::vertical()
            .max_height(230.0)
            .show(ui, |ui| {
                for i in 0..count {
                    ui.horizontal(|ui| {
                        let name = h.entries[i].name;
                        let sel_resp = ui.selectable_label(h.selected == i, name);
                        if sel_resp.double_clicked() {
                            h.selected = i;
                            h.editing = Some(i);
                        } else if sel_resp.clicked() {
                            h.selected = i;
                        }
                        if h.editing == Some(i) {
                            // 临时编辑态:直接改本行参数文本;点 ✓ 或双击别的参数结束编辑
                            ui.add(
                                egui::TextEdit::singleline(&mut h.entries[i].flag)
                                    .desired_width(240.0),
                            );
                            if ui.small_button("✓").clicked() {
                                h.editing = None;
                            }
                        } else {
                            let r = ui.add(
                                egui::Label::new(
                                    egui::RichText::new(h.entries[i].flag.as_str()).monospace(),
                                )
                                .sense(egui::Sense::click()),
                            );
                            if r.double_clicked() {
                                h.selected = i;
                                h.editing = Some(i);
                            } else if r.clicked() {
                                h.selected = i;
                            }
                        }
                    });
                }
            });
        ui.separator();
        if count > 0 {
            let sel = h.selected.min(count - 1);
            let e = &h.entries[sel];
            ui.horizontal(|ui| {
                ui.strong(e.name);
                ui.monospace(&e.flag);
            });
            ui.label(e.desc);
            ui.add_space(4.0);
            ui.label("使用示例:");
            ui.monospace(e.usage);
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("加入 scrcpy 参数").clicked() {
                    let flag = h.entries[sel].flag.trim().to_string();
                    if flag.is_empty() {
                        self.log("该参数文本为空,无法加入");
                    } else {
                        let base = self.scrcpy_args.trim();
                        self.scrcpy_args = if base.is_empty() {
                            flag.clone()
                        } else {
                            format!("{base} {flag}")
                        };
                        self.log(format!("已加入参数: {flag}"));
                    }
                }
                if ui.button("撤去该参数").clicked() {
                    let flag = h.entries[sel].flag.trim();
                    if !flag.is_empty() {
                        self.scrcpy_args = self
                            .scrcpy_args
                            .split_whitespace()
                            .filter(|a| *a != flag)
                            .collect::<Vec<_>>()
                            .join(" ");
                        self.log(format!("已从参数框移除: {flag}"));
                    }
                }
            });
            ui.add_space(4.0);
            ui.hyperlink_to("scrcpy 官方文档/源码 (GitHub)", scrcpy_gh);
        }
    }

    /// **界面/画布**用的坐标空间:优先截图尺寸,其次控制通道尺寸。
    ///
    /// 为什么优先截图:键位浮层是画在截图上的,用截图自己的尺寸换算,
    /// "画的圈"与"看到的图"才永远对齐。以前优先取控制通道尺寸,而那个尺寸会在
    /// 连接控制、手机转屏、后台刷新时改变 —— 于是同一个摇杆会**毫无征兆地突然变大
    /// 或移位**(用户反馈"创建新摇杆的时候,旧的摇杆会瞬间变大")。
    /// 注入使用的坐标空间由引擎按控制通道尺寸单独计算,不受这里影响。
    fn screen_size(&self) -> Option<(u32, u32)> {
        if let Some((_, w, h)) = self.shot.as_ref() {
            if *w > 0 && *h > 0 {
                return Some((*w, *h));
            }
        }
        let g = self.shared.lock().unwrap();
        if let Some(c) = g.control.as_ref() {
            if c.screen_w > 0 && c.screen_h > 0 {
                return Some((c.screen_w, c.screen_h));
            }
        }
        None
    }

    /// **注入**用的坐标空间(引擎实际使用的那个):控制通道的尺寸。
    /// 仅用于界面提示 —— 截图与它不一致时,取点会不准。
    fn inject_space(&self) -> Option<(u32, u32)> {
        let g = self.shared.lock().unwrap();
        g.control
            .as_ref()
            .map(|c| (c.screen_w, c.screen_h))
            .filter(|(w, h)| *w > 0 && *h > 0)
    }

    /// 坐标换算器:配置坐标(相对值) <-> 当前屏幕像素。
    /// 屏幕尺寸未知时按 1080x2400 估算,只影响界面显示,不影响注入。
    ///
    /// 内部会加配置锁,调用方**不要**在已持有配置锁时调用它(会自锁)。
    fn mapper(&self) -> Mapper {
        let unit = self.shared.lock().unwrap().profile.coord_unit();
        Mapper::new(unit, self.screen_size().unwrap_or((1080, 2400)))
    }

    /// 当前主题的语义色(界面里一律用它,不写死颜色)
    fn theme(&self) -> Theme {
        self.shared.lock().unwrap().profile.look.theme()
    }

    /// 当前界面风格(默认/鸿蒙/可视化)。布局分支一律经它判断。
    /// 读的是 `self.style`(界面级),不是 `profile.look.style`:后者会被
    /// "切换按键组合"整体替换掉(见 `stamp_style`)。
    fn ui_style(&self) -> theme::UiStyle {
        self.style
    }

    /// 把界面风格回写进共享配置(每帧一次,值不变时零开销)。
    ///
    /// 为什么需要它:切换按键组合 / 按切换键换组合时,`profile` 会被 YAML 里的那套
    /// 整体替换,而 YAML 的 `look` 没有 `style` 字段(反序列化缺省 = 默认风格)。
    /// 如果不守住,用户会看到"切一下组合,整个 UI 变回默认"——这正是被反馈的 bug。
    /// 风格是界面级设置,只能由界面侧持有并在换组合后重新盖上。
    fn stamp_style(&mut self) {
        let style = self.style;
        let mut g = self.shared.lock().unwrap();
        if g.profile.look.style != style {
            g.profile.look.style = style;
        }
    }

    /// 请求切换界面风格:写进配置并立即落盘(look.json),然后关闭窗口。
    /// main.rs 的重启循环随后用新风格重开 UI(见文件头 STYLE_RESTART 的说明)。
    fn request_style_restart(&mut self, ctx: &egui::Context, new_style: theme::UiStyle) {
        self.style = new_style;
        {
            let mut g = self.shared.lock().unwrap();
            g.profile.look.style = new_style;
            let look = g.profile.look.clone();
            drop(g);
            self.persist_look(&look);
        }
        self.log(format!(
            "界面风格切换为「{}」: 正在关闭并重新打开窗口...",
            new_style.label()
        ));
        STYLE_RESTART.store(true, Ordering::SeqCst);
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// 当前外观设置
    fn look(&self) -> theme::Look {
        self.shared.lock().unwrap().profile.look.clone()
    }

    /// 新草稿:按当前屏幕尺寸初始化预览位置与半径(草稿坐标是像素)
    fn reset_draft_to_screen(&mut self) {
        let (w, h) = self.screen_size().unwrap_or((1080, 2400));
        let r_px = self.mapper().len(crate::keymap::DEFAULT_RADIUS);
        self.draft.x = (w / 2) as i32;
        self.draft.y = (h / 2) as i32;
        self.draft.radius = r_px;
        self.draft.swipe_start = ((w / 2) as i32, (h as f32 * 0.75) as i32);
        self.draft.swipe_end = ((w / 2) as i32, (h as f32 * 0.25) as i32);
    }

    /// 保存前补全配置元信息:记录这份布局是按哪个分辨率设计的(仅供显示参考)
    fn stamp_profile_meta(&mut self) {
        let Some(space) = self.screen_size() else {
            return;
        };
        let mut g = self.shared.lock().unwrap();
        if g.profile.format_version >= crate::keymap::PROFILE_VERSION {
            g.profile.screen = Some(space);
        }
    }

    /// 截图尺寸就是"当前屏幕方向"下的真实触摸坐标空间,用它校正控制通道。
    ///
    /// 只校正坐标空间,**绝不改动已取好的锚点**:手机临时切到别的方向
    /// (例如横屏配好后弹了一下竖屏、再切回来)必须原样可用。
    /// 锚点暂时超出当前空间时,由注入前的钳制兜底,并在界面上给出提示即可。
    ///
    /// 顺带把旧格式(v1,像素坐标)升级为相对坐标:**只改单位,不改任何坐标的实际含义**。
    fn sync_display_space(&mut self, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        let mut changed = false;
        let upgraded;
        {
            let mut g = self.shared.lock().unwrap();
            if let Some(c) = g.control.as_mut() {
                if (c.screen_w, c.screen_h) != (w, h) {
                    // 走 set_screen:它会同时更新写线程用的原子量。
                    // 直接写字段的话,协议里声明的尺寸会永远停在连接时的那个值。
                    c.set_screen(w, h);
                    changed = true;
                }
            }
            upgraded = crate::keymap::upgrade_profile(&mut g.profile, (w, h));
        }
        if changed {
            self.log(format!("触摸坐标空间已更新为 {w}x{h}"));
        }
        if upgraded {
            self.log(format!(
                "配置已升级为相对坐标(按 {w}x{h} 换算),换分辨率/换手机后键位不再错位"
            ));
        }
    }

    /// 配置区(保存/另存为/选用/新建/默认/检测/保存日志)。
    /// 默认与可视化风格放在左栏;鸿蒙风格放在"按键映射"页的卡片里 —— 两条布局共用这一份。
    fn ui_profile_config(&mut self, ui: &mut egui::Ui) {
        ui.heading("配置");
        ui.horizontal(|ui| {
            if ui.button("保存配置").clicked() {
                self.save_profile();
            }
            if ui.button("另存为...").clicked() {
                self.dialog = Some(crate::filedialog::save_file("scrcpy-pad-profile.yaml"));
                self.dialog_purpose = DialogPurpose::SaveProfileAs;
            }
            if ui.button("重新加载").clicked() {
                self.reload_profile_from_current();
            }
        });
        // 键位文件重定向:指定任意目录/文件名为当前键位(可无文件则新建)。
        // 用 horizontal_wrapped:左栏可以被拖窄,按钮多了以后换行总比被裁掉好
        ui.horizontal_wrapped(|ui| {
            if ui.button("选用配置...").clicked() {
                self.dialog = Some(crate::filedialog::pick_file());
                self.dialog_purpose = DialogPurpose::ChooseProfile;
            }
            if ui.button("新建配置...").clicked() {
                self.dialog = Some(crate::filedialog::save_file("scrcpy-pad-profile.yaml"));
                self.dialog_purpose = DialogPurpose::NewProfile;
            }
            if ui
                .button("默认配置")
                .on_hover_text("切回程序默认的配置文件(首次运行时自动创建的那份),并加载它的内容")
                .clicked()
            {
                self.load_default_profile();
            }
            if ui.button("检测当前 yaml").clicked() {
                self.check_current_profile();
            }
        });
        // 长路径必须**换行**,否则会把整个左栏撑宽,挤掉中央的键位/键盘区
        ui.add(
            egui::Label::new(
                egui::RichText::new(format!(
                    "当前配置文件: {}{}",
                    self.profile_path.display(),
                    if self.profile_path == profile_path() {
                        " (默认)"
                    } else {
                        ""
                    }
                ))
                .small(),
            )
            .wrap(),
        );
        if ui.button("保存日志...").clicked() {
            let serial = self.serial();
            if serial.is_empty() {
                self.log("错误: 未选择设备(日志需包含设备信息)");
            } else {
                let ver = if self.server_version.is_empty() {
                    adb::scrcpy_version_at(&self.scrcpy_path).unwrap_or_default()
                } else {
                    self.server_version.clone()
                };
                let (tx, rx) = channel();
                self.loginfo_rx = Some(rx);
                self.log("正在收集设备信息...");
                std::thread::spawn(move || {
                    let info = adb::device_info(&serial, &ver);
                    let _ = tx.send(info);
                });
            }
        }
    }

    /// scrcpy 三件套的定位与管理(目录/自动寻找/测试/记住路径/打开配置目录)。
    /// 默认与可视化风格放在左栏;鸿蒙风格放在"连接与设备"页的卡片里。
    fn ui_scrcpy_manage(&mut self, ui: &mut egui::Ui) {
        ui.heading("scrcpy 管理");
        ui.small(
            "官方 Windows 包里 scrcpy.exe、scrcpy-server、adb.exe 三者同目录。\n\
             可以直接指定 scrcpy 所在**目录**(最省事),也可以只填其中一个文件,\n\
             其余留空会自动补齐。",
        );
        // scrcpy 目录:用户最自然的用法就是把发行包那个文件夹指给它
        ui.horizontal(|ui| {
            ui.label("scrcpy 目录");
            ui.add(
                egui::TextEdit::singleline(&mut self.scrcpy_dir)
                    .desired_width(220.0)
                    .hint_text("例如 D:\\scrcpy-win64-v3.3"),
            );
            if ui.small_button("浏览目录").clicked() {
                self.dialog = Some(crate::filedialog::pick_folder());
                self.dialog_purpose = DialogPurpose::ScrcpyDir;
            }
            if ui
                .small_button("应用")
                .on_hover_text("按这个目录补齐 scrcpy / server / adb")
                .clicked()
            {
                self.apply_scrcpy_dir();
            }
        });
        ui.horizontal(|ui| {
            if ui.button("自动寻找全部").clicked() {
                match adb::find_scrcpy() {
                    Some(p) => {
                        self.scrcpy_path = p.display().to_string();
                        if let Some(d) = PathBuf::from(&self.scrcpy_path).parent() {
                            self.scrcpy_dir = d.display().to_string();
                        }
                        self.log(format!("已找到 scrcpy: {}", self.scrcpy_path));
                        self.resync();
                        self.test_scrcpy();
                        self.save_settings_now();
                    }
                    None => self.log("未找到 scrcpy,请手动指定它所在目录或 scrcpy.exe"),
                }
            }
            if ui.button("测试并刷新").clicked() {
                self.resync();
                self.test_scrcpy();
                // 验证 adb 是否真的可运行(打印版本行),便于诊断
                if let Some(exe) = self.effective_adb() {
                    let exe_s = exe.display().to_string();
                    match adb::adb_version_at(&exe_s) {
                        Some(v) => self.log(format!("adb 版本: {v}")),
                        None => self.log(format!("adb 可执行失败,无法读取版本: {exe_s}")),
                    }
                } else {
                    self.log("未找到 adb(可点击上方 [浏览]/[自动] 手动指定)");
                }
                self.refresh_devices();
            }
        });
        // 记住路径:与主题(look.json)一样存在配置目录里,重启后自动沿用
        ui.horizontal(|ui| {
            if ui
                .checkbox(&mut self.remember_paths, "记住路径(下次启动直接用)")
                .on_hover_text(
                    "把 scrcpy / scrcpy-server / adb 路径、scrcpy 目录与启动参数存到\n\
                     配置目录的 settings.json,即使 scrcpy 目录不在程序同级,\n\
                     重启后也不必重新寻找。关掉它只是『下次启动不再使用』,\n\
                     文件里已记住的路径会保留。",
                )
                .changed()
            {
                if self.remember_paths {
                    self.log("已开启记住路径: 本次路径将写入 settings.json");
                } else {
                    self.log("已关闭记住路径: 下次启动不再使用其中的路径(文件里已记住的内容保留)");
                }
                // 开关状态本身立刻落盘,免得下次启动又变回上一次的样子
                self.save_settings_now();
            }
            if ui
                .small_button("打开配置目录")
                .on_hover_text("profile.yaml / look.json / settings.json 所在目录")
                .clicked()
            {
                self.open_config_dir();
            }
        });
        if let Some((ok, msg)) = &self.test_msg {
            let th = self.theme();
            ui.colored_label(if *ok { th.ok } else { th.danger }, msg);
        }

        ui.horizontal(|ui| {
            ui.label("scrcpy.exe ");
            ui.text_edit_singleline(&mut self.scrcpy_path);
            if ui.small_button("浏览").clicked() {
                self.dialog = Some(crate::filedialog::pick_file());
                self.dialog_purpose = DialogPurpose::ScrcpyExe;
            }
        });
        ui.horizontal(|ui| {
            ui.label("scrcpy-server");
            ui.text_edit_singleline(&mut self.server_path);
            if ui.small_button("浏览").clicked() {
                self.dialog = Some(crate::filedialog::pick_file());
                self.dialog_purpose = DialogPurpose::ServerJar;
            }
        });
        ui.horizontal(|ui| {
            ui.label("adb.exe    ");
            ui.text_edit_singleline(&mut self.adb_path);
            if ui.small_button("浏览").clicked() {
                self.dialog = Some(crate::filedialog::pick_file());
                self.dialog_purpose = DialogPurpose::AdbExe;
            }
            if ui.small_button("自动").clicked() {
                match self.effective_adb() {
                    Some(p) => {
                        self.adb_path = p.display().to_string();
                        self.log(format!("adb: {}", self.adb_path));
                        self.resync();
                    }
                    None => self.log("未找到 adb,请手动指定路径或加入 PATH"),
                }
            }
        });
        let adb_eff = self
            .effective_adb()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "未找到(将回退 PATH 的 adb)".to_string());
        let ver_txt = if self.server_version.is_empty() {
            "未检测".to_string()
        } else {
            self.server_version.clone()
        };
        ui.small(format!("当前 adb: {adb_eff}"));
        ui.small(format!("server 版本: {ver_txt}"));
    }

    /// 总开关键 + [映射时屏蔽原键] 勾选(默认/可视化风格在左栏中段;鸿蒙在"连接与设备"页)
    fn ui_toggle_key_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("总开关键:");
            let tk = { self.shared.lock().unwrap().profile.toggle_key };
            let waiting = self.waiting_key == Some(KeySlot::Toggle);
            if Self::key_button(ui, waiting, Some(tk)).clicked() {
                self.waiting_key = Some(KeySlot::Toggle);
            }
        });
        ui.checkbox(
            &mut self.grab_enabled,
            "映射时屏蔽原键(grab)\n注意:开启后映射期间键盘只对本程序生效",
        );
    }

    /// 引擎运行状态一行(触点占用 / 因池满被放弃)。
    /// 默认/可视化风格在左栏中段;鸿蒙在"连接与设备"页与底部状态栏。
    fn ui_engine_status(&mut self, ui: &mut egui::Ui) {
        let g = self.shared.lock().unwrap();
        let st = g.live;
        let th = g.profile.look.theme();
        let max = crate::engine::DEVICE_MAX_POINTERS;
        let (color, tail) = if st.refused > 0 {
            (
                th.warn,
                format!(
                    "(最近被放弃的是 {})",
                    if st.last_refused == 0 {
                        "瞄准".to_string()
                    } else {
                        key_name(st.last_refused)
                    }
                ),
            )
        } else {
            (th.muted, String::new())
        };
        ui.colored_label(
            color,
            format!(
                "引擎: 触点 {}/{} · 因触点池满被放弃 {} 次{tail}",
                st.pointers, max, st.refused
            ),
        )
        .on_hover_text(
            "设备端同时最多认 10 个触点(普通键位 + 摇杆 + 瞄准共用)。\n\
             这里显示此刻占用了几个。\n\
             若『被放弃』不为 0,说明某一刻同时按住的键比设备能接的还多 ——\n\
             那一次按下会被设备直接丢掉(表现为『按了没反应』),日志里也会记。",
        );
    }

    /// 运行日志列表(默认/可视化风格在左栏底部;鸿蒙在"诊断"页)
    fn ui_log_list(&self, ui: &mut egui::Ui, max_h: f32) {
        egui::ScrollArea::vertical()
            .id_salt("log_list")
            .stick_to_bottom(true)
            .max_height(max_h)
            .show(ui, |ui| {
                for l in self.logs.iter().rev().take(500) {
                    ui.monospace(l);
                }
            });
    }

    fn ui_log_card(&mut self, ui: &mut egui::Ui, max_h: f32) {
        ui.horizontal(|ui| {
            ui.heading("日志");
            if ui
                .button(if self.log_window_open {
                    "关闭弹窗"
                } else {
                    "弹窗查看"
                })
                .clicked()
            {
                self.log_window_open = !self.log_window_open;
            }
        });

        self.ui_log_list(ui, max_h.max(80.0));
    }

    fn ui_log_window(&mut self, ctx: &egui::Context) {
        if !self.log_window_open {
            return;
        }
        let mut open = self.log_window_open;
        egui::Window::new("scrcpy-pad 日志")
            .open(&mut open)
            .default_size([760.0, 460.0])
            .min_size([420.0, 240.0])
            .resizable(true)
            .show(ctx, |ui| self.ui_log_list(ui, 420.0));
        self.log_window_open = open;
    }

    // ==================== 顶栏动作(两个布局共用) ====================
    //
    // 默认/可视化风格的顶栏与鸿蒙风格的顶栏都调用这几个方法,
    // 保证"按一下做同一件事"只有一份实现(与 ui_bind_row 的纪律一致)。

    /// [刷新设备]:重新定位三件套并刷新设备列表
    fn act_refresh_devices(&mut self) {
        self.resync();
        self.refresh_devices();
    }

    /// [启动 scrcpy]:准备参数、启动、把关键输出接进日志
    fn act_launch_scrcpy(&mut self) {
        // 启动前联动一次:确保 server/adb 路径已就绪(如已手动粘贴 scrcpy 路径)
        self.resync();
        let launch_args = self.prepare_scrcpy_args();
        self.log(format!("scrcpy 启动参数: {launch_args}"));
        match adb::launch_scrcpy(&self.scrcpy_path, &self.serial(), &launch_args) {
            Ok(rx) => {
                self.scrcpy_status_rx = Some(rx);
                self.log("scrcpy 启动请求已发送；stdout/stderr/退出码会显示在日志区，完整内容见 diagnostics.log");
                // 部分机型音频转发起步慢,自动补一次音量键唤醒
                self.wake_audio();
            }
            Err(e) => self.log(format!("启动失败: {e:#}")),
        }
    }

    /// 顶栏的"映射: 开/关"按钮。关闭映射必须走引擎的收尾(抬起所有按住的触点),
    /// 否则手机上会"卡键" —— 这里只置请求位,由引擎在下一轮(≤4ms)执行。
    fn act_toggle_mapping(&mut self) {
        let now = {
            let mut g = self.shared.lock().unwrap();
            g.enabled = !g.enabled;
            if !g.enabled {
                g.toolbar_release = true;
            }
            g.enabled
        };
        if now {
            self.log("映射已开启");
        } else {
            self.log("映射已关闭: 正在抬起全部触点");
        }
    }

    /// scrcpy 启动参数行(参数框 + 助手 + 启动预设 + [启动 scrcpy])。
    /// 默认/可视化风格在顶栏;鸿蒙风格在"连接与设备"页的卡片里 —— 共用这一份。
    fn ui_scrcpy_launch_row(&mut self, ui: &mut egui::Ui) {
        ui.label("scrcpy参数:");
        if ui
            .small_button("...")
            .on_hover_text("打开常用参数助手")
            .clicked()
        {
            self.args_helper = Some(ArgHelp::defaults());
        }
        ui.add(egui::TextEdit::singleline(&mut self.scrcpy_args).desired_width(200.0));
        // 启动预设:分辨率预设先重置为默认再叠加;音频类直接在现有参数上增删
        let preset_resp = egui::ComboBox::from_id_salt("startpreset")
            .selected_text("启动预设")
            .show_ui(ui, |ui| {
                for p in [
                    StartPreset::None,
                    StartPreset::Uhd2k,
                    StartPreset::Uhd4k,
                    StartPreset::Fhd1080,
                    StartPreset::Hd720,
                    StartPreset::NoAudio,
                    StartPreset::WithAudio,
                ] {
                    if ui.selectable_label(false, p.label()).clicked() {
                        self.apply_start_preset(p);
                    }
                }
            });
        preset_resp.response.on_hover_text(
            "分辨率预设只是给 scrcpy 传 --max-size(最长边上限)。\n\
             scrcpy 只缩小、不放大:设备本身能输出多高,画面上限就是多高。\n\
             例如手机屏幕只有 1080p,选 4k 也拿不到 4k。",
        );
        if ui.button("启动 scrcpy").clicked() {
            self.act_launch_scrcpy();
        }
    }

    // ==================== 鸿蒙风格:照 harmonyos-pc(HDC UI)重新设计的界面 ====================
    //
    // 这不是"默认界面换个底色":整张界面的**骨架重新排过** ——
    //   ┌ 顶栏(白底 + 细线): ● scrcpy-pad [设备▼] [状态 pill] … [使用说明][刷新设备][连接/断开][保存配置] ┐
    //   ├ 左侧导航(图标+文字,选中淡蓝底) ┬ 中央:卡片页(浅灰底) ┬ 右侧:屏幕预览(截图取点) ┤
    //   └ 底部状态栏:最新一条日志 ………… [引擎: 触点 n/10]                                  ┘
    // 每个页面的内容都调用与默认风格**同一份**实现(ui_profile_config / ui_scrcpy_manage /
    // ui_binds / ui_wheels / ui_aim / ui_look / ui_diagnostics …),所以不存在"鸿蒙少了某个功能"。

    fn layout_harmony(
        &mut self,
        ui: &mut egui::Ui,
        connected: bool,
        enabled: bool,
        mouse_captured: bool,
    ) {
        let line = egui::Stroke::new(1.0, theme::harmony::LINE);
        let panel = |ui: &egui::Ui, inner: egui::Margin| {
            egui::Frame::default()
                .fill(ui.visuals().window_fill)
                .inner_margin(inner)
                .stroke(line)
        };
        // ---- 顶栏 ----
        egui::Panel::top("h_top")
            .frame(panel(ui, egui::Margin::symmetric(16, 10)))
            .show(ui, |ui| self.h_top_bar(ui, connected, enabled));
        // ---- 底部状态栏(最新一条日志 + 引擎状态) ----
        egui::Panel::bottom("h_status")
            .frame(panel(ui, egui::Margin::symmetric(14, 7)))
            .show(ui, |ui| self.h_status_bar(ui));
        // ---- 左侧导航 ----
        egui::Panel::left("h_nav")
            .resizable(false)
            .default_size(196.0)
            .frame(panel(ui, egui::Margin::symmetric(12, 16)))
            .show(ui, |ui| self.h_nav(ui));
        // ---- 右侧:屏幕预览(= 截图取点,原样复用) ----
        egui::Panel::right("h_preview")
            .resizable(true)
            .default_size(430.0)
            .min_size(300.0)
            .frame(panel(ui, egui::Margin::same(16)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("屏幕预览");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some((_, w, h)) = &self.shot {
                            ui.small(format!("{w} × {h}"));
                        }
                    });
                });
                egui::ScrollArea::both()
                    .id_salt("h_preview_scroll")
                    .show(ui, |ui| self.ui_picker_body(ui));
            });
        // ---- 中央:当前导航页(卡片) ----
        egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .fill(ui.visuals().panel_fill)
                    .inner_margin(egui::Margin::same(16)),
            )
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("h_page_scroll")
                    .show(ui, |ui| self.h_page_body(ui, mouse_captured));
            });
    }

    /// 鸿蒙风格顶栏:标题 + 设备 + 状态 pill + 主操作按钮
    fn h_top_bar(&mut self, ui: &mut egui::Ui, connected: bool, enabled: bool) {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("●")
                    .size(18.0)
                    .color(theme::harmony::PRIMARY),
            );
            ui.heading(
                egui::RichText::new("scrcpy-pad")
                    .strong()
                    .color(theme::harmony::TEXT),
            );
            ui.label(egui::RichText::new("触控映射").color(theme::harmony::MUTED));
            ui.separator();
            let cur = self.serial();
            egui::ComboBox::from_id_salt("h_dev")
                .selected_text(if cur.is_empty() {
                    "无设备".to_string()
                } else {
                    cur
                })
                .show_ui(ui, |ui| {
                    for (i, d) in self.devices.iter().enumerate() {
                        ui.selectable_value(&mut self.selected, i, d);
                    }
                });
            Self::h_pill(
                ui,
                if connected {
                    "控制已连接"
                } else {
                    "未连接"
                },
                if connected {
                    theme::harmony::OK
                } else {
                    theme::harmony::DANGER
                },
            );
            let tk = self.shared.lock().unwrap().profile.toggle_key;
            let tkn = key_name(tk).replace("KEY_", "");
            Self::h_pill(
                ui,
                &format!("映射 {} ({tkn})", if enabled { "开" } else { "关" }),
                if enabled {
                    theme::harmony::OK
                } else {
                    theme::harmony::MUTED
                },
            );
            // 右侧:主操作(照 HDC UI —— 右侧放实心蓝按钮)
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if Self::h_primary_button(ui, "保存配置").clicked() {
                    self.save_profile();
                }
                if let Some(label) = self.pending_task_label() {
                    if ui
                        .add(
                            egui::Button::new(format!("取消{label}"))
                                .fill(theme::harmony::DANGER.gamma_multiply(0.25))
                                .stroke(egui::Stroke::new(
                                    1.0,
                                    theme::with_alpha(theme::harmony::DANGER, 190),
                                )),
                        )
                        .clicked()
                    {
                        self.cancel_pending_tasks();
                    }
                }
                if connected {
                    if ui.button("断开").clicked() {
                        self.disconnect();
                    }
                } else if self.connect_rx.is_some() {
                    ui.label("连接中...");
                } else if ui.button("连接控制").clicked() {
                    self.connect_control();
                }
                if ui.button("刷新设备").clicked() {
                    self.act_refresh_devices();
                }
                if ui.button("使用说明").clicked() {
                    self.help_open = true;
                }
            });
        });
    }

    /// 鸿蒙风格左侧导航:图标 + 文字;选中项淡蓝底 + 蓝字(照 HDC UI)
    fn h_nav(&mut self, ui: &mut egui::Ui) {
        for page in HPage::ALL {
            let (icon, title) = page.icon_title();
            let selected = self.h_page == page;
            let (rect, resp) = ui
                .allocate_exact_size(egui::vec2(ui.available_width(), 36.0), egui::Sense::click());
            let painter = ui.painter();
            if selected || resp.hovered() {
                let fill = if selected {
                    theme::with_alpha(theme::harmony::PRIMARY, 30)
                } else {
                    theme::with_alpha(theme::harmony::PRIMARY, 14)
                };
                painter.rect_filled(rect, egui::CornerRadius::same(8), fill);
            }
            let fg = if selected {
                theme::harmony::PRIMARY
            } else {
                theme::harmony::TEXT
            };
            painter.text(
                rect.left_center() + egui::vec2(10.0, 0.0),
                egui::Align2::LEFT_CENTER,
                format!("{icon}  {title}"),
                egui::FontId::proportional(15.0),
                fg,
            );
            if resp.clicked() {
                self.h_page = page;
            }
        }
        // 底部:次要操作(撤销/重做/关于)—— 顶栏只放主操作,避免溢出窗口宽度
        ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
            ui.horizontal(|ui| {
                if ui.small_button("关于").clicked() {
                    self.about_open = true;
                }
                if ui.small_button("重做").clicked() {
                    self.redo();
                }
                if ui.small_button("撤销").clicked() {
                    self.undo();
                }
            });
        });
    }

    /// 鸿蒙风格底部状态栏:最新一条日志(左) + 引擎状态(右)
    fn h_status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let last = self.logs.back().cloned().unwrap_or_default();
            ui.label(
                egui::RichText::new(last)
                    .size(12.0)
                    .color(theme::harmony::MUTED),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.ui_engine_status(ui);
            });
        });
    }

    /// 鸿蒙风格中央页:按左侧导航分发到各卡片
    fn h_page_body(&mut self, ui: &mut egui::Ui, mouse_captured: bool) {
        match self.h_page {
            HPage::Connect => {
                let (connected, _) = {
                    let g = self.shared.lock().unwrap();
                    (
                        g.control
                            .as_ref()
                            .map(|c| c.is_connected())
                            .unwrap_or(false),
                        g.enabled,
                    )
                };
                let th = self.theme();
                Self::harmony_card(ui, "设备连接", |ui| {
                    ui.horizontal(|ui| {
                        if ui.button("刷新设备").clicked() {
                            self.act_refresh_devices();
                        }
                        let cur = self.serial();
                        egui::ComboBox::from_id_salt("h_page_dev")
                            .selected_text(if cur.is_empty() {
                                "无设备".to_string()
                            } else {
                                cur
                            })
                            .show_ui(ui, |ui| {
                                for (i, d) in self.devices.iter().enumerate() {
                                    ui.selectable_value(&mut self.selected, i, d);
                                }
                            });
                        if connected {
                            Self::h_pill(ui, "控制已连接", theme::harmony::OK);
                            if ui.button("断开").clicked() {
                                self.disconnect();
                            }
                        } else if self.connect_rx.is_some() {
                            ui.label("连接中...");
                        } else if Self::h_primary_button(ui, "连接控制").clicked() {
                            self.connect_control();
                        }
                    });
                    if self.devices.is_empty() {
                        ui.small(
                            egui::RichText::new("未发现设备:请连接手机并打开 USB 调试")
                                .color(theme::harmony::MUTED),
                        );
                    }
                });
                Self::harmony_card(ui, "scrcpy 画面", |ui| {
                    ui.horizontal(|ui| self.ui_scrcpy_launch_row(ui));
                    ui.small(
                        egui::RichText::new(
                            "画面由 scrcpy 本体窗口显示,与本程序互不干扰;启动后点右侧[截取手机屏幕]取画面。",
                        )
                        .color(theme::harmony::MUTED),
                    );
                });
                Self::harmony_card(ui, "映射运行时", |ui| {
                    self.ui_toggle_key_row(ui);
                    ui.horizontal(|ui| {
                        let (enabled, tk) = {
                            let g = self.shared.lock().unwrap();
                            (g.enabled, g.profile.toggle_key)
                        };
                        let tkn = key_name(tk).replace("KEY_", "");
                        let txt = if enabled {
                            format!("映射已开启 ({tkn})")
                        } else {
                            format!("开启并开始映射 ({tkn})")
                        };
                        let btn = egui::Button::new(egui::RichText::new(txt).color(if enabled {
                            theme::harmony::TEXT
                        } else {
                            egui::Color32::WHITE
                        }))
                        .fill(if enabled {
                            theme::harmony::BTN
                        } else {
                            theme::harmony::PRIMARY
                        })
                        .stroke(egui::Stroke::NONE);
                        if ui.add(btn).clicked() {
                            self.act_toggle_mapping();
                        }
                        ui.small(
                            egui::RichText::new("默认 F8 开关;FPS 模式有独立开关")
                                .color(theme::harmony::MUTED),
                        );
                    });
                    let _ = th;
                    self.ui_engine_status(ui);
                });
                Self::harmony_card(ui, "scrcpy 管理", |ui| self.ui_scrcpy_manage(ui));
            }
            HPage::Macro => Self::harmony_card(ui, "宏", |ui| self.ui_macro_page(ui)),
            HPage::Keys => {
                Self::harmony_card(ui, "配置", |ui| self.ui_profile_config(ui));
                Self::harmony_card(ui, "按键映射", |ui| self.ui_binds(ui));
                Self::harmony_card(ui, "按键组合 / 切换键位", |ui| self.ui_schemes(ui));
            }
            HPage::Wheels => Self::harmony_card(ui, "虚拟轮盘", |ui| self.ui_wheels(ui)),
            HPage::Fps => {
                Self::harmony_card(ui, "鼠标瞄准", |ui| self.ui_aim(ui, mouse_captured))
            }
            HPage::Look => Self::harmony_card(ui, "外观", |ui| self.ui_look(ui)),
            HPage::Diag => {
                Self::harmony_card(ui, "诊断", |ui| self.ui_diagnostics(ui));
                let h = ui.available_height().max(180.0);
                Self::harmony_card(ui, "运行日志", |ui| self.ui_log_card(ui, h));
            }
        }
    }

    /// 鸿蒙风格的卡片:白底 + 细描边 + 14px 圆角 + 标题(照 HDC UI 的 card())
    fn harmony_card(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
        egui::Frame::default()
            .fill(ui.visuals().window_fill)
            .stroke(egui::Stroke::new(1.0, theme::harmony::LINE))
            .corner_radius(egui::CornerRadius::same(theme::harmony::CARD_ROUND as u8))
            .inner_margin(egui::Margin::same(16))
            .outer_margin(egui::Margin::symmetric(0, 7))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    egui::RichText::new(title)
                        .size(17.0)
                        .strong()
                        .color(theme::harmony::TEXT),
                );
                ui.add_space(8.0);
                add(ui);
            });
    }

    /// 鸿蒙风格的状态 pill(淡色底 + 同色文字,照 HDC UI)
    fn h_pill(ui: &mut egui::Ui, text: &str, color: egui::Color32) {
        egui::Frame::default()
            .fill(theme::with_alpha(color, 34))
            .corner_radius(egui::CornerRadius::same(20))
            .inner_margin(egui::Margin::symmetric(10, 4))
            .show(ui, |ui| {
                ui.label(egui::RichText::new(text).size(12.0).color(color));
            });
    }

    /// 鸿蒙风格的实心主按钮(蓝底白字)
    fn h_primary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
        ui.add(
            egui::Button::new(egui::RichText::new(text).color(egui::Color32::WHITE))
                .fill(theme::harmony::PRIMARY)
                .stroke(egui::Stroke::NONE),
        )
    }

    /// 外观设置面板:风格 / 配色 / 控件密度 / 背景图(改动可撤销,随配置保存)
    fn ui_look(&mut self, ui: &mut egui::Ui) {
        let mut pick_bg = false;
        let mut edit = false;
        let mut style_restart: Option<theme::UiStyle> = None;
        let before = self.shared.lock().unwrap().profile.clone();
        {
            let mut g = self.shared.lock().unwrap();
            let look = &mut g.profile.look;

            // ---- 界面风格(默认/鸿蒙/可视化) ----
            // 风格牵动整体布局,选择后需要关闭再重开窗口(见 request_style_restart)。
            // 不能像配色那样热应用,所以这里不进撤销栈(edit 不置位),直接走重启。
            // 注意:选中的值先落在一个临时变量里,由 request_style_restart 统一改
            // PadApp.style(界面级持有),避免在这里直接改 profile.look.style ——
            // 那个副本会被"切换按键组合"覆盖(见 stamp_style)。
            ui.horizontal(|ui| {
                ui.label("界面风格:");
                let mut want = self.style;
                egui::ComboBox::from_id_salt("look_style")
                    .selected_text(want.label())
                    .show_ui(ui, |ui| {
                        for s in theme::UiStyle::ALL {
                            ui.selectable_value(&mut want, s, s.label());
                        }
                    });
                if want != self.style {
                    style_restart = Some(want);
                }
                ui.small("切换风格需重开窗口").on_hover_text(
                    "风格决定整体布局(按键布局、信息显示都会变),无法热应用。\n\
                                    选择后会自动关闭再重新打开窗口,并记住这次的选择。",
                );
            });
            ui.separator();

            ui.horizontal(|ui| {
                ui.label("配色:");
                egui::ComboBox::from_id_salt("look_preset")
                    .selected_text(look.preset.label())
                    .show_ui(ui, |ui| {
                        for p in [
                            Preset::Dark,
                            Preset::Light,
                            Preset::Nord,
                            Preset::Catppuccin,
                        ] {
                            if ui
                                .selectable_value(&mut look.preset, p, p.label())
                                .changed()
                            {
                                edit = true;
                            }
                        }
                    });
                ui.label("密度:");
                egui::ComboBox::from_id_salt("look_density")
                    .selected_text(look.density.label())
                    .show_ui(ui, |ui| {
                        for d in [Density::Compact, Density::Standard, Density::Loose] {
                            if ui
                                .selectable_value(&mut look.density, d, d.label())
                                .changed()
                            {
                                edit = true;
                            }
                        }
                    });
            });

            ui.horizontal(|ui| {
                ui.label("背景图:");
                if look.has_bg() {
                    let name = std::path::Path::new(&look.bg_path)
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| look.bg_path.clone());
                    ui.label(name).on_hover_text(&look.bg_path);
                    if ui.small_button("更换").clicked() {
                        pick_bg = true;
                    }
                    if ui.small_button("清除").clicked() {
                        look.clear_bg();
                        edit = true;
                    }
                } else {
                    ui.label("未设置");
                    if ui.small_button("选择图片...").clicked() {
                        pick_bg = true;
                    }
                }
            });

            if look.has_bg() {
                ui.horizontal(|ui| {
                    ui.label("填充:");
                    egui::ComboBox::from_id_salt("look_bgfit")
                        .selected_text(look.bg_fit.label())
                        .show_ui(ui, |ui| {
                            for f in [BgFit::Cover, BgFit::Contain, BgFit::Tile] {
                                if ui
                                    .selectable_value(&mut look.bg_fit, f, f.label())
                                    .changed()
                                {
                                    edit = true;
                                }
                            }
                        });
                });
                // 连续滑块:开始拖动/聚焦那一帧记一个撤销点,避免每帧塞一条
                let r = ui
                    .add(egui::Slider::new(&mut look.bg_dim, 0..=255).text("背景图压暗"))
                    .on_hover_text("数值越大背景图越暗、文字越清楚;0=完全不压暗");
                if r.drag_started() || r.gained_focus() {
                    edit = true;
                }
                let r = ui
                    .add(egui::Slider::new(&mut look.panel_alpha, 30..=255).text("面板不透明度"))
                    .on_hover_text(
                        "面板的不透明程度:数值越小背景图越明显、文字越难看清;\n255=完全不透明(此时看不到背景图)",
                    );
                if r.drag_started() || r.gained_focus() {
                    edit = true;
                }
            }
            // 键位浮层的可读性:自动对比 + 手动微调。
            // 光靠手动调亮度解决不了"画面上既有亮块又有暗块"的问题 ——
            // 现在默认按截图**逐块采样**,自动决定每一处该用深色还是浅色。
            let r = ui
                .checkbox(&mut look.overlay_auto, "键位自动对比(按截图明暗自动选深浅)")
                .on_hover_text(
                    "采样每个键位/摇杆底下的画面明暗,自动决定那一处用深色还是浅色,\n\
                     并给圈描一圈反相外套、给文字垫一层半透明衬底 ——\n\
                     这是制图学 halo 与字幕衬底那套成熟做法,亮块暗块上都能看清。\n\
                     关掉后改用下面的固定档位(与旧版一致)。",
                );
            if r.changed() {
                edit = true;
            }
            let r = ui
                .add(
                    egui::Slider::new(&mut look.overlay_tone, theme::TONE_MIN..=theme::TONE_MAX)
                        .text("键位显示亮度(微调)"),
                )
                .on_hover_text(
                    "在自动对比的基础上做微调:\n\
                     往左=整体加深(适合明亮的游戏画面),往右=整体变浅(适合昏暗画面),0=不偏。\n\
                     只改明暗、不改色相:点按绿/长按橙/永久摇杆青/临时摇杆品红照样分得清,\n\
                     摇杆的圆环 + 圆心小圈 + 方向标注也原样保留。\n\
                     关掉上面的[自动对比]后,这个滑块就是唯一依据(0 = 与旧版一致)。",
                );
            if r.drag_started() || r.gained_focus() {
                edit = true;
            }
            ui.small("外观随配置保存;[选用配置]会一并切换外观");
        }
        if pick_bg {
            self.dialog = Some(crate::filedialog::pick_file());
            self.dialog_purpose = DialogPurpose::PickBackground;
        }
        if edit {
            self.push_undo_snapshot(before);
        }
        if let Some(s) = style_restart {
            let ctx = ui.ctx().clone();
            self.request_style_restart(&ctx, s);
        }
    }

    /// 按填充方式把背景图画到窗口背景层(带可读性遮罩)
    fn paint_background(&mut self, ctx: &egui::Context, look: &theme::Look) {
        if !look.has_bg() {
            if self.bg_tex.is_some() {
                self.bg_tex = None;
            }
            return;
        }
        // 路径变化时才重新解码,避免每帧读盘
        let need_load = self
            .bg_tex
            .as_ref()
            .map(|(p, _)| p != &look.bg_path)
            .unwrap_or(true);
        // 同一路径此前已加载失败:不再重试(换图后会重置),否则会每帧刷屏
        if need_load && self.bg_failed.as_deref() == Some(look.bg_path.as_str()) {
            return;
        }
        if need_load {
            self.bg_tex = None;
            let mut err = None;
            match std::fs::read(&look.bg_path) {
                Ok(bytes) => match image::load_from_memory(&bytes) {
                    Ok(img) => {
                        let rgba = img.to_rgba8();
                        let (w, h) = rgba.dimensions();
                        let color = egui::ColorImage::from_rgba_unmultiplied(
                            [w as usize, h as usize],
                            &rgba.into_raw(),
                        );
                        let tex = ctx.load_texture("bg", color, egui::TextureOptions::LINEAR);
                        self.bg_tex = Some((look.bg_path.clone(), tex));
                        self.bg_failed = None;
                    }
                    Err(e) => {
                        err = Some(format!("背景图解码失败({e}):仅支持 png / jpg"));
                    }
                },
                Err(e) => err = Some(format!("背景图读取失败: {e}")),
            }
            if let Some(e) = err {
                self.bg_failed = Some(look.bg_path.clone());
                self.log(e);
            }
        }
        let Some((_, tex)) = &self.bg_tex else { return };
        let screen = ctx.content_rect();
        let painter = ctx.layer_painter(egui::LayerId::background());
        let ts = tex.size_vec2();
        match look.bg_fit {
            BgFit::Tile => {
                for y in 0..((screen.height() / ts.y).ceil() as i32 + 1) {
                    for x in 0..((screen.width() / ts.x).ceil() as i32 + 1) {
                        let min = screen.min + egui::vec2(x as f32 * ts.x, y as f32 * ts.y);
                        painter.image(
                            tex.id(),
                            egui::Rect::from_min_size(min, ts),
                            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                            egui::Color32::WHITE,
                        );
                    }
                }
            }
            BgFit::Cover | BgFit::Contain => {
                let scale = if look.bg_fit == BgFit::Cover {
                    (screen.width() / ts.x).max(screen.height() / ts.y)
                } else {
                    (screen.width() / ts.x).min(screen.height() / ts.y)
                };
                let size = ts * scale;
                // 居中:超出部分按 UV 裁切
                let min = screen.center() - size / 2.0;
                let uv = if look.bg_fit == BgFit::Cover {
                    let u0 = (-min.x / size.x).clamp(0.0, 1.0);
                    let v0 = (-min.y / size.y).clamp(0.0, 1.0);
                    let u1 = ((screen.max.x - min.x) / size.x).clamp(0.0, 1.0);
                    let v1 = ((screen.max.y - min.y) / size.y).clamp(0.0, 1.0);
                    egui::Rect::from_min_max(egui::pos2(u0, v0), egui::pos2(u1, v1))
                } else {
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0))
                };
                painter.image(
                    tex.id(),
                    egui::Rect::from_min_size(min, size),
                    uv,
                    egui::Color32::WHITE,
                );
            }
        }
        // 可读性兜底:整体压暗
        if look.bg_dim > 0 {
            painter.rect_filled(screen, 0.0, egui::Color32::from_black_alpha(look.bg_dim));
        }
    }

    /// 外观改动落盘到程序自用的缓存(与键位配置同目录)。
    /// 只在内容真的变化时写,避免每帧写盘;写失败只提示一次。
    fn persist_look(&mut self, look: &theme::Look) {
        if self.look_saved.as_ref() == Some(look) {
            return;
        }
        self.look_saved = Some(look.clone());
        let path = look_cache_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let res = serde_json::to_string_pretty(look)
            .map_err(|e| e.to_string())
            .and_then(|text| std::fs::write(&path, text).map_err(|e| e.to_string()));
        if let Err(e) = res {
            if !self.look_cache_warned {
                self.look_cache_warned = true;
                self.log(format!(
                    "外观缓存写入失败({e}),本次外观改动只在本次运行内有效: {}",
                    path.display()
                ));
            }
        }
    }

    /// 后台查询"当前显示器尺寸"(手机横竖屏随时可能变化,
    /// 开打前确认一次可避免拿着旧方向的坐标空间去注入)
    fn refresh_display_space(&mut self) {
        if self.space_rx.is_some() {
            return;
        }
        let serial = self.serial();
        if serial.is_empty() {
            return;
        }
        let (tx, rx) = channel();
        self.space_rx = Some(rx);
        std::thread::spawn(move || {
            // 查询失败时回传 (0,0),由 sync_display_space 忽略,避免卡住后续刷新
            let _ = tx.send(adb::display_size(&serial).unwrap_or((0, 0)));
        });
    }

    /// 鼠标瞄准(FPS / 开放世界)。内容**直接展开**(不再套"（开发中）"折叠栏 ——
    /// 用户明确要求把它放出来,少一层点击)。
    fn ui_aim(&mut self, ui: &mut egui::Ui, captured: bool) {
        ui.heading("鼠标瞄准（FPS / 开放世界）");
        ui.small("注意：本功能处于开发阶段，实际应用效果可能与描述有出入。");
        self.ui_aim_body(ui, captured);
    }

    fn ui_aim_body(&mut self, ui: &mut egui::Ui, captured: bool) {
        let th = self.theme();
        let gamepad_mode = {
            let mode = self.shared.lock().unwrap().profile.aim.input_mode;
            matches!(
                mode,
                ViewInputMode::VirtualGamepadContinuous | ViewInputMode::VirtualGamepadSegmented
            )
        };
        if gamepad_mode {
            ui.label("把鼠标的相对位移映射成虚拟 Xbox 手柄右摇杆。游戏必须支持手柄右摇杆视角；该模式不需要锚点。");
            ui.label("用法:先开启总映射,连接控制通道,按视角模式开关键进入。");
        } else {
            ui.label("把鼠标的相对位移映射成手机上的手指拖动。FPS 模式用于开镜/射击；开放世界模式用于无需射击的无限水平转向。");
            ui.label("锚点默认不设置。锚点不是游戏准星，而是虚拟手指落下的起点；应放在游戏 UI 之外的干净区域。");
            ui.label("用法:先开启总映射，再到“瞄准锚点（开发中）”取点，按视角模式开关键进入。");
        }

        // —— 生效条件自检:直接告诉用户"现在为什么没反应" ——
        let (mapping_enabled, aim_on, anchor_ok, hold_key, connected, space, live) = {
            let g = self.shared.lock().unwrap();
            let a = &g.profile.aim;
            (
                g.enabled,
                a.enabled,
                a.anchor_set(),
                a.hold_key,
                g.control
                    .as_ref()
                    .map(|c| c.is_connected())
                    .unwrap_or(false),
                g.control.as_ref().map(|c| (c.screen_w, c.screen_h)),
                g.aim_live,
            )
        };
        // 锚点在当前坐标空间之外(例如取点后转过屏)——注入会被系统丢弃
        let anchor_outside = match space {
            Some((w, h)) => {
                let g = self.shared.lock().unwrap();
                let a = &g.profile.aim;
                let (ax, ay) = g.profile.mapper((w, h)).point(a.anchor_x, a.anchor_y);
                a.anchor_set() && (ax < 0 || ay < 0 || ax as u32 >= w || ay as u32 >= h)
            }
            None => false,
        };
        let mouse_found = self.mouse_found_flag.load(Ordering::Relaxed);
        let mut blocker: Option<String> = None;
        ui.separator();
        ui.label("生效条件自检:");
        let mut checks = vec![
            (
                mapping_enabled,
                "映射总开关已开启（视角模式只在映射开启后生效）".to_string(),
            ),
            (aim_on, "已勾选 [启用鼠标视角]".to_string()),
            (mouse_found, "检测到鼠标设备".to_string()),
            (
                live.mode_active,
                "FPS 模式已开启(可用自定义开关键)".to_string(),
            ),
            (!live.suspended, "当前没有按住临时退出键".to_string()),
            (connected, "控制通道已连接(已点[启动])".to_string()),
        ];
        if gamepad_mode {
            checks.push((live.gamepad_created, "虚拟手柄已在设备端创建".to_string()));
        } else {
            checks.push((
                anchor_ok,
                "已设置锚点(勾选启用时会自动放置,可再[取锚点]调整)".to_string(),
            ));
        }
        for (ok, text) in checks {
            if !ok && blocker.is_none() {
                blocker = Some(text.clone());
            }
            ui.colored_label(
                if ok { th.ok } else { th.warn },
                format!("{} {}", if ok { "✓" } else { "✗" }, text),
            );
        }
        if let Some((w, h)) = space {
            ui.label(format!(
                "触摸坐标空间: {w}x{h}{}",
                if w > h { "(横屏)" } else { "(竖屏)" }
            ));
        }
        if anchor_outside && !gamepad_mode {
            ui.colored_label(
                th.danger,
                "锚点超出了上面的坐标空间(手机方向变了?):注入时会被自动钳到屏幕内,\n\
                 切回原来的方向即完全恢复,也可以现在[取锚点]重取一次",
            );
        }
        if gamepad_mode {
            ui.label(format!(
                "运行状态: 位移{}次 最近({:.0},{:.0}) 虚拟手柄{} 输入报告{}条",
                live.motions,
                live.last_dx,
                live.last_dy,
                if live.gamepad_created {
                    "已创建"
                } else {
                    "未创建"
                },
                live.gamepad_reports,
            ));
        } else {
            ui.label(format!(
                "运行状态: 偏移({:.0},{:.0}) 触点{} 已注入{}条",
                live.ox,
                live.oy,
                if live.down { "按下" } else { "抬起" },
                live.sent
            ));
        }
        if let Some(b) = blocker {
            ui.colored_label(th.warn, format!("→ 现在没反应,因为: {b}"));
        } else if !live.active {
            ui.colored_label(
                th.warn,
                "→ 已就绪,但当前不满足瞄准条件(绑了[按住才瞄准]时需按住该键)",
            );
        } else if hold_key != 0 {
            ui.colored_label(
                th.warn,
                format!(
                    "→ 已就绪,但绑定了[按住才瞄准]:需按住 {} 时才会转动视角",
                    key_name(hold_key)
                ),
            );
        } else {
            ui.colored_label(th.ok, "→ 已就绪,移动鼠标即可转动视角");
        }
        ui.separator();

        let mut to_pick: Option<CoordSlot> = None;
        let mut pick_hold_key = false;
        let mut pick_toggle_key = false;
        let mut pick_suspend_key = false;
        let mut toggled = false;
        // 本帧修改前的快照:面板里任何一处改动都记一次撤销。
        // 连续拖动的数值控件只在"开始编辑"那一帧记录,避免每帧都产生一个撤销步。
        let undo_before: Option<Profile>;
        let mut undo_needed = false;
        // 锚点以像素显示(存储是相对值)
        let am = self.mapper();

        {
            let mut g = self.shared.lock().unwrap();
            undo_before = Some(g.profile.clone());
            let aim = &mut g.profile.aim;

            if ui.checkbox(&mut aim.enabled, "启用鼠标视角").changed() {
                toggled = true;
                undo_needed = true;
            }

            if ui
                .checkbox(
                    &mut aim.open_world,
                    "开放世界模式（无需射击/开镜，鼠标无限水平转向）",
                )
                .on_hover_text("使用无缝换手的触摸拖动带：越过回中半径后保留超出量继续同向转向。")
                .changed()
            {
                if aim.open_world {
                    aim.hold_key = 0;
                }
                undo_needed = true;
            }
            if aim.open_world {
                ui.horizontal(|ui| {
                    ui.label("水平回中半径");
                    let r = ui.add(
                        egui::Slider::new(&mut aim.open_world_radius, 0.05..=0.45).suffix(" ×屏宽"),
                    );
                    if r.drag_started() || r.gained_focus() {
                        undo_needed = true;
                    }
                    ui.label("平滑");
                    let s = ui.add(egui::Slider::new(&mut aim.open_world_smoothing, 0.15..=1.0));
                    if s.drag_started() || s.gained_focus() {
                        undo_needed = true;
                    }
                });
            }

            ui.horizontal(|ui| {
                ui.label("视角输入:");
                let previous = aim.input_mode;
                egui::ComboBox::from_id_salt("aim_input_mode")
                    .selected_text(aim.input_mode.label())
                    .show_ui(ui, |ui| {
                        for mode in [
                            ViewInputMode::TouchDrag,
                            ViewInputMode::VirtualGamepadContinuous,
                            ViewInputMode::VirtualGamepadSegmented,
                        ] {
                            ui.selectable_value(&mut aim.input_mode, mode, mode.label());
                        }
                    });
                if aim.input_mode != previous {
                    undo_needed = true;
                }
            });
            match aim.input_mode {
                ViewInputMode::TouchDrag | ViewInputMode::UhidMouse | ViewInputMode::AoaMouse => {}
                ViewInputMode::VirtualGamepadContinuous
                | ViewInputMode::VirtualGamepadSegmented => {
                    let segmented = aim.input_mode == ViewInputMode::VirtualGamepadSegmented;
                    ui.colored_label(
                        th.ok,
                        if segmented {
                            "虚拟手柄分段回中：右摇杆达到极限后归中并保留超出量，模仿人类一段一段滑动视角；不需要游戏支持鼠标，只需要支持手柄右摇杆。"
                        } else {
                            "虚拟手柄连续：鼠标位移驱动右摇杆，停止一小段时间后自动回中；不需要游戏支持鼠标，只需要支持手柄右摇杆。"
                        },
                    );
                }
            }

            if !gamepad_mode {
                ui.separator();
                ui.heading("瞄准锚点（开发中）");
                ui.small("锚点是鼠标拖动映射到手机屏幕时，虚拟手指的起点；不是游戏准星。请取在游戏 UI 之外的干净区域。默认不自动设置，未设置时 FPS 模式不会真正落下拖动触点。");
                ui.horizontal(|ui| {
                    ui.label("锚点:");
                    if aim.anchor_set() {
                        let (ax, ay) = am.point(aim.anchor_x, aim.anchor_y);
                        ui.label(format!("({ax}, {ay})"));
                    } else {
                        ui.colored_label(th.warn, "未设置");
                    }
                    let waiting_p = self.picking == Some(CoordSlot::AimAnchor);
                    if ui
                        .button(if waiting_p {
                            "点击截图..."
                        } else {
                            "取锚点"
                        })
                        .clicked()
                    {
                        to_pick = Some(CoordSlot::AimAnchor);
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("拖动死区:");
                    let r = ui.add(
                        egui::DragValue::new(&mut aim.drag_deadzone)
                            .range(0.0..=160.0)
                            .suffix(" px"),
                    );
                    if r.drag_started() || r.gained_focus() {
                        undo_needed = true;
                    }
                    if ui
                        .checkbox(&mut aim.boundary, "限制在屏幕边界内（开发中）")
                        .on_hover_text("关闭后，累计偏移到达回转半径会无缝抬指/重按并保留余量；适合配合指针消隐做无限转向。")
                        .changed()
                    {
                        undo_needed = true;
                    }
                });
                if !aim.boundary {
                    ui.horizontal(|ui| {
                        ui.label("回转半径:");
                        let r = ui.add(
                            egui::DragValue::new(&mut aim.recenter_threshold)
                                .range(20..=4000)
                                .suffix(" px"),
                        );
                        if r.drag_started() || r.gained_focus() {
                            undo_needed = true;
                        }
                        ui.label("（无边界模式下到达此半径就抬指重按）");
                    });
                }
            }

            ui.horizontal(|ui| {
                ui.label("灵敏度 X:");
                let rx = ui.add(
                    egui::DragValue::new(&mut aim.sensitivity_x)
                        .range(0.1..=20.0)
                        .speed(0.1),
                );
                if rx.drag_started() || rx.gained_focus() {
                    undo_needed = true;
                }
                ui.label("Y:");
                let ry = ui.add(
                    egui::DragValue::new(&mut aim.sensitivity_y)
                        .range(0.1..=20.0)
                        .speed(0.1),
                );
                if ry.drag_started() || ry.gained_focus() {
                    undo_needed = true;
                }
                if ui.checkbox(&mut aim.invert_y, "反转 Y").changed() {
                    undo_needed = true;
                }
            });

            ui.horizontal(|ui| {
                ui.label("视角移速:");
                let rs = ui
                    .add(
                        egui::Slider::new(&mut aim.move_speed, 0.2..=3.0)
                            .suffix("x")
                            .text(""),
                    )
                    .on_hover_text("同时作用于触摸拖动、开放世界和虚拟手柄右摇杆；1.0 为原速。");
                if rs.drag_started() || rs.gained_focus() {
                    undo_needed = true;
                }
            });

            ui.horizontal(|ui| {
                ui.label("归中:");
                egui::ComboBox::from_id_salt("aim_recenter")
                    .selected_text(aim.recenter.label())
                    .show_ui(ui, |ui| {
                        for m in [
                            RecenterMode::Idle,
                            RecenterMode::Threshold,
                            RecenterMode::Never,
                        ] {
                            if ui
                                .selectable_value(&mut aim.recenter, m, m.label())
                                .changed()
                            {
                                undo_needed = true;
                            }
                        }
                    });
                match aim.recenter {
                    RecenterMode::Idle => {
                        ui.label("静止");
                        let r = ui.add(
                            egui::DragValue::new(&mut aim.recenter_idle_ms)
                                .range(20..=2000)
                                .suffix(" ms"),
                        );
                        if r.drag_started() || r.gained_focus() {
                            undo_needed = true;
                        }
                    }
                    RecenterMode::Threshold => {
                        ui.label("阈值");
                        let r = ui.add(
                            egui::DragValue::new(&mut aim.recenter_threshold)
                                .range(20..=4000)
                                .suffix(" px"),
                        );
                        if r.drag_started() || r.gained_focus() {
                            undo_needed = true;
                        }
                    }
                    RecenterMode::Never => {}
                }
            });

            if !aim.open_world {
                ui.horizontal(|ui| {
                    ui.label("按住才瞄准:");
                    let hk = aim.hold_key;
                    let waiting = self.waiting_key == Some(KeySlot::AimHold);
                    let shown = if hk == 0 { None } else { Some(hk) };
                    if Self::key_button(ui, waiting, shown).clicked() {
                        pick_hold_key = true;
                    }
                    if hk != 0 && ui.small_button("清除").clicked() {
                        aim.hold_key = 0;
                        undo_needed = true;
                    }
                    if ui
                        .small_button("用右键")
                        .on_hover_text("开镜时才转动视角(按住右键瞄准)")
                        .clicked()
                    {
                        aim.hold_key = crate::keymap::BTN_RIGHT;
                        undo_needed = true;
                    }
                    ui.label("(可绑鼠标右键,开镜时才动视角)");
                });
            }

            ui.horizontal(|ui| {
                ui.label("视角模式开关键:");
                let tk = aim.toggle_key;
                let waiting = self.waiting_key == Some(KeySlot::AimToggle);
                let shown = if tk == 0 { None } else { Some(tk) };
                if Self::key_button(ui, waiting, shown).clicked() {
                    pick_toggle_key = true;
                }
                if tk != 0 && ui.small_button("清除").clicked() {
                    aim.toggle_key = 0;
                    undo_needed = true;
                }
                ui.label("(只在总映射开启后可进入/退出)");
            });
            ui.horizontal(|ui| {
                ui.label("按住才退出:");
                let sk = aim.suspend_key;
                let waiting = self.waiting_key == Some(KeySlot::AimSuspend);
                let shown = if sk == 0 { None } else { Some(sk) };
                if Self::key_button(ui, waiting, shown).clicked() {
                    pick_suspend_key = true;
                }
                if sk != 0 && ui.small_button("清除").clicked() {
                    aim.suspend_key = 0;
                    undo_needed = true;
                }
                ui.label("(按住暂时退出 FPS,恢复普通映射并显示鼠标;松开回到 FPS)");
            });

            if ui
                .checkbox(&mut aim.capture_mouse, "指针消隐(视角模式下隐藏系统光标)")
                .changed()
            {
                undo_needed = true;
            }
            if captured {
                ui.colored_label(th.ok, "当前: 鼠标已捕获,视角跟随鼠标");
            } else {
                ui.label("当前: 鼠标未被捕获");
            }
            ui.small("开打后按 Ctrl+Alt 可把鼠标交还给系统,再按一次收回。");
        }

        // 记录撤销(快照取自本帧修改之前;含刚启用时的自动放置锚点)
        if undo_needed {
            if let Some(before) = undo_before {
                self.push_undo_snapshot(before);
            }
        }

        if let Some(slot) = to_pick {
            self.begin_pick(slot);
        }
        if pick_hold_key {
            self.waiting_key = Some(KeySlot::AimHold);
        }
        if pick_toggle_key {
            self.waiting_key = Some(KeySlot::AimToggle);
        }
        if pick_suspend_key {
            self.waiting_key = Some(KeySlot::AimSuspend);
        }
        if toggled {
            // 用户明确要求锚点默认不设置。这里只提示，不再替用户放置。
            let needs_anchor = { self.shared.lock().unwrap().profile.aim.enabled };
            if needs_anchor {
                self.log("鼠标视角已启用；锚点默认未设置，请在“瞄准锚点（开发中）”中取点");
            }
        }
    }

    fn ui_switch_keys(&mut self, ui: &mut egui::Ui) {
        let (names, active, n) = {
            let g = self.shared.lock().unwrap();
            (
                g.schemes.iter().map(|p| p.name.clone()).collect::<Vec<_>>(),
                g.active_scheme,
                g.schemes.len(),
            )
        };
        let rows = self.shared.lock().unwrap().switch_keys.clone();
        ui.heading("切换键位");
        ui.small("支持单键或最多两个键的组合；按键顺序无关。可在[按键组合]里启用快速切换。");
        let mut delete = None;
        for (i, original) in rows.iter().enumerate() {
            let keys = original.effective_keys();
            let mut changed = false;
            ui.horizontal(|ui| {
                ui.label(format!("切换{}:", i + 1));
                let waiting = self.waiting_key == Some(KeySlot::SwitchKey(i));
                let shown = keys.first().copied().filter(|k| *k != 0);
                if Self::key_button(ui, waiting, shown).clicked() {
                    self.waiting_key = Some(KeySlot::SwitchKey(i));
                }
                if keys.len() >= 2 {
                    let waiting2 = self.waiting_key == Some(KeySlot::SwitchKeySecond(i));
                    if Self::key_button(ui, waiting2, keys.get(1).copied()).clicked() {
                        self.waiting_key = Some(KeySlot::SwitchKeySecond(i));
                    }
                    if ui.button("删除组合键").clicked() {
                        if let Some(s) = self.shared.lock().unwrap().switch_keys.get_mut(i) {
                            s.keys.truncate(1);
                        }
                        changed = true;
                    }
                } else if ui.button("新增组合键").clicked() {
                    if let Some(s) = self.shared.lock().unwrap().switch_keys.get_mut(i) {
                        s.keys = vec![s.key, 0];
                    }
                    changed = true;
                }
                ui.label("方式:");
                let mut direction = original.direction;
                egui::ComboBox::from_id_salt(("switch_direction", i))
                    .selected_text(match direction {
                        SwitchDirection::Target => "指定组合",
                        SwitchDirection::Next => "正向循环",
                        SwitchDirection::Prev => "反向循环",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut direction, SwitchDirection::Target, "指定组合");
                        ui.selectable_value(&mut direction, SwitchDirection::Next, "正向循环");
                        ui.selectable_value(&mut direction, SwitchDirection::Prev, "反向循环");
                    });
                if direction != original.direction {
                    if let Some(s) = self.shared.lock().unwrap().switch_keys.get_mut(i) {
                        s.direction = direction;
                    }
                    changed = true;
                }
                if direction == SwitchDirection::Target {
                    let mut target = original.target;
                    let cur = names.get(target).cloned().unwrap_or_else(|| "?".into());
                    egui::ComboBox::from_id_salt(("switch_target_other", i))
                        .selected_text(cur)
                        .width(112.0)
                        .show_ui(ui, |ui| {
                            for (j, name) in names.iter().enumerate() {
                                ui.selectable_value(&mut target, j, name);
                            }
                        });
                    if target != original.target {
                        if let Some(s) = self.shared.lock().unwrap().switch_keys.get_mut(i) {
                            s.target = target;
                        }
                        changed = true;
                    }
                }
                if ui.button("删除").clicked() {
                    delete = Some(i);
                }
            });
            if changed {
                self.scheme_dirty = true;
            }
        }
        let next = if n > 1 { (active + 1) % n } else { 0 };
        if ui.button("新增切换键位").clicked() {
            self.shared.lock().unwrap().switch_keys.push(SwitchKey {
                key: 0,
                keys: Vec::new(),
                target: next,
                direction: SwitchDirection::Target,
            });
            self.scheme_dirty = true;
        }
        if let Some(i) = delete {
            self.shared.lock().unwrap().switch_keys.remove(i);
            self.scheme_dirty = true;
            if self.waiting_key == Some(KeySlot::SwitchKey(i))
                || self.waiting_key == Some(KeySlot::SwitchKeySecond(i))
            {
                self.waiting_key = None;
            }
        }
    }

    fn fps_text(info: &RemoteDebugInfo) -> String {
        info.fps
            .map(|v| format!("{v:.1} Hz"))
            .unwrap_or_else(|| "等待刷新".to_string())
    }

    fn resolution_text(info: &RemoteDebugInfo) -> String {
        info.resolution
            .map(|(w, h)| format!("{w} × {h}"))
            .unwrap_or_else(|| "等待刷新".to_string())
    }

    fn refresh_debug_info(&mut self) {
        if self.debug_rx.is_some() {
            return;
        }
        self.debug_last_query = Instant::now();
        let serial = self.serial();
        if serial.is_empty() {
            self.debug_info.error = Some("未选择设备".to_string());
            return;
        }
        let (tx, rx) = channel();
        self.debug_rx = Some(rx);
        std::thread::spawn(move || {
            let fps = adb::display_refresh_rate(&serial).ok();
            let resolution = adb::display_size(&serial).ok();
            let error = if fps.is_none() || resolution.is_none() {
                Some("部分调试信息读取失败".to_string())
            } else {
                None
            };
            let info = RemoteDebugInfo {
                fps,
                resolution,
                updated_at: Some(std::time::SystemTime::now()),
                error,
            };
            let _ = tx.send(Ok(info));
        });
    }

    fn poll_debug_rx(&mut self) {
        let mut result = None;
        let mut disconnected = false;
        if let Some(rx) = self.debug_rx.as_ref() {
            match rx.try_recv() {
                Ok(value) => result = Some(value),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => disconnected = true,
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(value) = result {
            self.debug_rx = None;
            match value {
                Ok(info) => self.debug_info = info,
                Err(error) => self.debug_info.error = Some(error),
            }
        } else if disconnected {
            self.debug_rx = None;
        }
    }

    fn maybe_refresh_debug_info(&mut self) {
        let wanted = self.debug_overlay_open || self.debug_show_fps || self.debug_show_resolution;
        if wanted
            && self.debug_rx.is_none()
            && self.debug_last_query.elapsed() >= Duration::from_secs(1)
        {
            self.refresh_debug_info();
        }
    }

    fn ui_other(&mut self, ui: &mut egui::Ui) {
        let th = self.theme();
        self.ui_switch_keys(ui);
        ui.separator();
        ui.heading("鼠标消隐");
        ui.label("设置后不管普通模式还是 FPS 模式，按一下隐藏系统鼠标，再按一下显示。");
        let mut cursor_key = 0u16;
        ui.horizontal(|ui| {
            ui.label("切换键:");
            cursor_key = self.shared.lock().unwrap().profile.cursor_toggle_key;
            let waiting = self.waiting_key == Some(KeySlot::CursorToggle);
            if Self::key_button(ui, waiting, (cursor_key != 0).then_some(cursor_key)).clicked() {
                self.waiting_key = Some(KeySlot::CursorToggle);
                self.log("请按一个键作为鼠标消隐切换键");
            }
            if cursor_key != 0 && ui.small_button("清除").clicked() {
                self.push_undo();
                self.shared.lock().unwrap().profile.cursor_toggle_key = 0;
                self.waiting_key = None;
            }
            let hidden = self.cursor_hide_flag.load(Ordering::Relaxed);
            if ui
                .button(if hidden {
                    "立即显示鼠标"
                } else {
                    "立即隐藏鼠标"
                })
                .clicked()
            {
                self.cursor_hide_flag.store(!hidden, Ordering::Relaxed);
            }
        });
        let hidden = self.cursor_hide_flag.load(Ordering::Relaxed);
        ui.colored_label(
            if hidden { th.warn } else { th.muted },
            if hidden {
                "当前: 系统鼠标已消隐"
            } else {
                "当前: 系统鼠标可见"
            },
        );
        ui.separator();

        ui.heading("调试信息");
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.debug_show_fps, "显示当前手机帧率/刷新率");
            ui.checkbox(&mut self.debug_show_resolution, "显示当前手机分辨率");
            ui.checkbox(&mut self.debug_show_aim, "显示鼠标视角位移统计");
            if ui.button("刷新").clicked() {
                self.refresh_debug_info();
            }
            let overlay_label = if self.debug_overlay_open {
                "关闭悬浮"
            } else {
                "悬浮显示"
            };
            if ui.button(overlay_label).clicked() {
                if !self.debug_overlay_open
                    && !self.debug_show_fps
                    && !self.debug_show_resolution
                    && !self.debug_show_aim
                {
                    self.debug_show_fps = true;
                    self.debug_show_resolution = true;
                }
                self.debug_overlay_open = !self.debug_overlay_open;
            }
        });
        let info = self.debug_info.clone();
        if self.debug_show_fps {
            ui.label(format!("手机帧率/刷新率: {}", Self::fps_text(&info)));
        }
        if self.debug_show_resolution {
            ui.label(format!("手机分辨率: {}", Self::resolution_text(&info)));
        }
        if self.debug_show_aim {
            let live = self.shared.lock().unwrap().aim_live;
            ui.label(format!(
                "鼠标视角: 位移 {} 次，最近 ({:.1}, {:.1})",
                live.motions, live.last_dx, live.last_dy
            ));
        }
        if let Some(updated) = info.updated_at {
            ui.small(format!("最后刷新: {}", fmt_timestamp(updated)));
        }
        if self.debug_rx.is_some() {
            ui.colored_label(th.warn, "正在刷新...");
        }
        if let Some(error) = info.error {
            ui.colored_label(th.danger, error);
        }
    }

    fn ui_debug_overlay(&mut self, ctx: &egui::Context) {
        if !self.debug_overlay_open {
            return;
        }
        let info = self.debug_info.clone();
        let live = self.shared.lock().unwrap().aim_live;
        let mut close = false;
        let data = DebugOverlayData {
            show_fps: self.debug_show_fps,
            show_resolution: self.debug_show_resolution,
            show_aim: self.debug_show_aim,
            fps: Self::fps_text(&info),
            resolution: Self::resolution_text(&info),
            aim_motions: live.motions,
            aim_last_dx: live.last_dx,
            aim_last_dy: live.last_dy,
            updated: info
                .updated_at
                .map(fmt_timestamp)
                .unwrap_or_else(|| "尚未刷新".to_string()),
            error: info.error,
        };
        let _ = ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("scrcpy_pad_debug_overlay"),
            egui::ViewportBuilder::default()
                .with_title("scrcpy-pad 调试信息")
                .with_always_on_top()
                .with_inner_size([320.0, 150.0])
                .with_min_inner_size([220.0, 90.0])
                .with_resizable(true),
            |ui, _class| {
                ui.heading("调试信息");
                ui.separator();
                if data.show_fps {
                    ui.label(format!("手机帧率/刷新率: {}", data.fps));
                }
                if data.show_resolution {
                    ui.label(format!("手机分辨率: {}", data.resolution));
                }
                if data.show_aim {
                    ui.label(format!(
                        "鼠标视角: 位移 {} 次，最近 ({:.1}, {:.1})",
                        data.aim_motions, data.aim_last_dx, data.aim_last_dy
                    ));
                }
                ui.small(format!("最后刷新: {}", data.updated));
                if let Some(error) = &data.error {
                    ui.colored_label(egui::Color32::LIGHT_RED, error);
                }
                if ui.button("关闭悬浮").clicked() {
                    close = true;
                }
                if ui.input(|i| i.viewport().close_requested()) {
                    close = true;
                }
            },
        );
        if close {
            self.debug_overlay_open = false;
        }
    }

    fn ui_picker(&mut self, ui: &mut egui::Ui) {
        ui.heading("截图取点");
        self.ui_picker_body(ui);
    }

    /// 截图取点的按钮行 + 预览画布(不含标题)。
    /// 默认/可视化风格在"截图取点"标题下;鸿蒙风格在右侧"屏幕预览"面板里 —— 共用这一份。
    fn ui_picker_body(&mut self, ui: &mut egui::Ui) {
        let preview_scale_base = self
            .shot
            .as_ref()
            .map(|(_, w, h)| {
                screenshot_fit_scale(
                    ui.available_width(),
                    ui.ctx().viewport_rect().height(),
                    *w,
                    *h,
                )
            })
            .unwrap_or(1.0);
        ui.horizontal(|ui| {
            let taking = self.shot_rx.is_some();
            if ui
                .button(if taking {
                    "截图中..."
                } else {
                    "截取手机屏幕"
                })
                .clicked()
                && !taking
            {
                self.take_screenshot();
            }
            if ui.button("+").on_hover_text("放大截图预览").clicked() {
                self.shot_zoom_auto = false;
                self.shot_zoom = (self.shot_zoom * 1.2).clamp(0.25, 3.0);
            }
            if ui.button("-").on_hover_text("缩小截图预览").clicked() {
                self.shot_zoom_auto = false;
                self.shot_zoom = (self.shot_zoom / 1.2).clamp(0.25, 3.0);
            }
            if ui
                .button("重置")
                .on_hover_text("恢复默认截图大小")
                .clicked()
            {
                self.shot_zoom_auto = true;
                self.shot_zoom = 1.0;
            }
            ui.label(format!(
                "{:.0}%{}",
                preview_scale_base * self.shot_zoom * 100.0,
                if self.shot_zoom_auto {
                    "（自动）"
                } else {
                    ""
                }
            ));
            // 浮层显示过滤
            ui.label("显示:");
            self.overlay_filter.ui(ui);
            if self.picking.is_some() {
                let draft_pick = self.picking.map(is_draft_slot).unwrap_or(false);
                ui.colored_label(
                    self.theme().warn,
                    if draft_pick {
                        "取点中(新增): 请点击截图上的目标位置"
                    } else {
                        "取点中: 请点击截图上的目标位置"
                    },
                );
                if ui.button("取消取点").clicked() {
                    if draft_pick {
                        // 新增草稿尚未添加,取消时连同圆圈一起清除
                        self.cancel_draft();
                    } else {
                        self.picking = None;
                    }
                }
            } else if self.waiting_key == Some(KeySlot::NewBind) || self.draft_active {
                // 新增取点后按键尚未设置,或草稿仍在进行:均可取消并消除圆圈/轨迹
                if self.waiting_key == Some(KeySlot::NewBind) {
                    ui.colored_label(self.theme().warn, "已取点: 请按下要绑定的按键");
                } else {
                    ui.label("新增未完成(尚未[添加])");
                }
                if ui.button("取消取点").clicked() {
                    self.cancel_draft();
                    self.log("已取消新增");
                }
            } else if let Some(target) = self.resizing {
                // 直接显示当前半径(像素),改没改一眼就能看出来
                let space = self.screen_size().unwrap_or((1080, 2400));
                let cur_px = {
                    let g = self.shared.lock().unwrap();
                    let m = g.profile.mapper(space);
                    match target {
                        ResizeTarget::Bind(i) => match g.profile.binds.get(i).map(|b| &b.action) {
                            Some(Action::Tap { radius, .. })
                            | Some(Action::Hold { radius, .. }) => m.len(*radius),
                            _ => 0.0,
                        },
                        ResizeTarget::Wheel(i) => g
                            .profile
                            .wheels
                            .get(i)
                            .map(|w| m.len(w.radius))
                            .unwrap_or(0.0),
                    }
                };
                let what = match target {
                    ResizeTarget::Bind(_) => "响应范围修改中",
                    ResizeTarget::Wheel(_) => "轮盘半径修改中",
                };
                let th = self.theme();
                ui.colored_label(
                    th.warn,
                    format!("{what}: Ctrl++ / Ctrl+- 缩放,或在截图上拖动(当前 {cur_px:.0}px)"),
                );
                if ui.button("完成").clicked() {
                    self.resizing = None;
                }
            } else {
                ui.label("先点某条映射的[取点],再点击截图上的位置");
            }
        });

        // 截图方向/尺寸与当前触摸坐标空间不一致时(手机转过屏、或截图是上一次
        // 方向下截的)必须说清楚:浮层按**截图**绘制(所以画得对),但注入按
        // **坐标空间**走 —— 这种状态下取点会落偏,重新截一张即可恢复。
        if let Some((_, sw, sh)) = self.shot.as_ref().map(|(t, w, h)| (t.id(), *w, *h)) {
            if let Some((iw, ih)) = self.inject_space() {
                if (iw, ih) != (sw, sh) {
                    ui.colored_label(
                        self.theme().warn,
                        format!(
                            "注意: 截图是 {sw}x{sh},当前触摸坐标空间是 {iw}x{ih}(手机转过屏?)\n\
                             浮层按截图绘制,注入却按坐标空间走 —— 建议重新[截取手机屏幕]后再取点"
                        ),
                    );
                }
            }
        }

        let shot = self.shot.as_ref().map(|(t, w, h)| (t.id(), *w, *h));
        if let Some((tex_id, w, h)) = shot {
            let avail = ui.available_width();
            // 自动适应同时参考可用宽度、窗口高度和截图长宽比；窗口改变时重算，
            // 竖屏不再只按宽度硬撑，横屏也不会顶出可视区域。
            let base_scale = screenshot_fit_scale(avail, ui.ctx().viewport_rect().height(), w, h);
            let scale = (base_scale * self.shot_zoom).clamp(0.05, 4.0);
            let size = egui::vec2(w as f32 * scale, h as f32 * scale);
            // 取点或修改响应范围时需要拖拽响应
            let sense = if self.resizing.is_some() {
                egui::Sense::drag()
            } else {
                egui::Sense::click()
            };
            let (rect, resp) = ui.allocate_exact_size(size, sense);
            ui.painter().image(
                tex_id,
                rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
            self.draw_overlay(ui, rect, scale);

            // 拖动圆圈缩放响应范围。
            // 这里直接看原始指针状态,而不是 resp.dragged():截图位于双向滚动区内,
            // 拖拽仲裁可能被滚动区吃掉,表现为"拖不动"。只要按下时落在截图上,
            // 按住期间半径就一直跟着指针走(直接点一下也能把半径设到该距离)。
            if let Some(target) = self.resizing {
                let (down, pos, origin) = ui.input(|inp| {
                    (
                        inp.pointer.primary_down(),
                        inp.pointer.interact_pos(),
                        inp.pointer.press_origin(),
                    )
                });
                // 纵向截图很窄,图片左右会留大片空白:把命中区按可用宽度横向摊开,
                // 免得必须"精确点中那张小图"才能拖动。
                let hit = rect.expand2(egui::vec2(((avail - rect.width()) / 2.0).max(0.0), 0.0));
                if down && origin.map(|o| hit.contains(o)).unwrap_or(false) {
                    if let Some(pos) = pos {
                        // 目标圆心(像素):键位取自己的坐标,轮盘取圆心
                        let (cx, cy) = {
                            let g = self.shared.lock().unwrap();
                            let m = g.profile.mapper((w, h));
                            match target {
                                ResizeTarget::Bind(i) => {
                                    match g.profile.binds.get(i).map(|b| &b.action) {
                                        Some(Action::Tap { x, y, .. })
                                        | Some(Action::Hold { x, y, .. }) => m.point(*x, *y),
                                        _ => (0, 0),
                                    }
                                }
                                ResizeTarget::Wheel(i) => match g.profile.wheels.get(i) {
                                    Some(wl) => m.point(wl.cx, wl.cy),
                                    None => (0, 0),
                                },
                            }
                        };
                        let dx = pos.x - (rect.min.x + cx as f32 * scale);
                        let dy = pos.y - (rect.min.y + cy as f32 * scale);
                        let new_r = ((dx * dx + dy * dy).sqrt() / scale).max(0.01);
                        let mut g = self.shared.lock().unwrap();
                        let m = g.profile.mapper((w, h));
                        match target {
                            ResizeTarget::Bind(i) => {
                                if let Some(b) = g.profile.binds.get_mut(i) {
                                    match &mut b.action {
                                        Action::Tap { radius, .. }
                                        | Action::Hold { radius, .. } => {
                                            // 界面按像素拖动,存储换算成相对值
                                            *radius = m.rel_len(new_r);
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            ResizeTarget::Wheel(i) => {
                                if let Some(wl) = g.profile.wheels.get_mut(i) {
                                    wl.radius = m.rel_len(new_r);
                                }
                            }
                        }
                    }
                }
            }

            if resp.clicked() {
                if let Some(pos) = resp.interact_pointer_pos() {
                    let px = ((pos.x - rect.min.x) / scale) as i32;
                    let py = ((pos.y - rect.min.y) / scale) as i32;
                    if let Some(slot) = self.picking.take() {
                        self.assign_coord(slot, px, py);
                        // 新增取点后若尚未设键位,立即进入等待按键
                        if slot == CoordSlot::NewBind && self.draft.key.is_none() {
                            self.waiting_key = Some(KeySlot::NewBind);
                            self.log("已取点,请按下要绑定的按键");
                        }
                    } else if let Some(i) = self.wheel_at(px, py) {
                        // 点到某个摇杆的响应圈:弹出/收起它的方向键信息卡
                        // (画布上只留"摇杆N",细节按需查看)
                        self.wheel_info = if self.wheel_info == Some(i) {
                            None
                        } else {
                            Some(i)
                        };
                    } else {
                        // 点到空白处:收起信息卡,并照旧报一次坐标
                        self.wheel_info = None;
                        self.log(format!("截图坐标: ({px}, {py})"));
                    }
                }
            }
        }
    }

    /// 命中测试:截图坐标 (px, py) 落在哪个摇杆的响应圈里(没有则 None)。
    ///
    /// 命中半径取"该摇杆的半径"与一个最小手感半径(24px)的较大者 ——
    /// 用户可能把摇杆调得很小,但"点一下看键位"这个动作不该要求点得那么准。
    /// 多个摇杆重叠时取**最后绘制**的那个(与看到的上下层一致)。
    fn wheel_at(&self, px: i32, py: i32) -> Option<usize> {
        let (profile, space) = {
            let g = self.shared.lock().unwrap();
            (
                g.profile.clone(),
                g.control
                    .as_ref()
                    .map(|c| (c.screen_w, c.screen_h))
                    .unwrap_or((1080, 2400)),
            )
        };
        let space = self.screen_size().unwrap_or(space);
        let m = profile.mapper(space);
        let (fx, fy) = (px as f32, py as f32);
        let mut hit = None;
        for (i, w) in profile.wheels.iter().enumerate() {
            let (cx, cy) = (m.x(w.cx) as f32, m.y(w.cy) as f32);
            let r = m.len(w.radius).max(WHEEL_CLICK_MIN_RADIUS);
            if (fx - cx).hypot(fy - cy) <= r {
                hit = Some(i);
            }
        }
        hit
    }
}

/// "点一下摇杆看键位"的最小命中半径(截图像素):摇杆调得很小时也点得中
const WHEEL_CLICK_MIN_RADIUS: f32 = 24.0;

/// 配置目录:profile.yaml / look.json / settings.json 三者同处一地,
/// 便于一起备份、清理或整体搬走。
///
/// 优先系统标准配置目录:
///   Linux   : ~/.config/scrcpy-pad
///   Windows : %APPDATA%\scrcpy-pad\config
///   macOS   : ~/Library/Application Support/dev.scrcpy-pad
/// 取不到时(极少数环境)退化为"程序旁边",即**便携模式**:
/// 配置跟着程序走,U 盘拷走也不丢。
pub fn config_dir() -> PathBuf {
    if let Some(dirs) = directories::ProjectDirs::from("dev", "", "scrcpy-pad") {
        return dirs.config_dir().to_path_buf();
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            return dir.to_path_buf();
        }
    }
    PathBuf::from(".")
}

fn profile_path() -> PathBuf {
    config_dir().join("profile.yaml")
}

/// 外观设置的"程序自用"缓存:与键位配置同目录(便于一起备份/清理),
/// 只由程序自己读写,不提供给用户选择或编辑。
fn look_cache_path() -> PathBuf {
    config_dir().join("look.json")
}

/// 读取外观缓存;不存在或内容损坏时返回 None(退回配置里的外观)
fn load_look_cache() -> Option<theme::Look> {
    let path = look_cache_path();
    let text = read_config_text(&path)?;
    match serde_json::from_str::<theme::Look>(&text) {
        Ok(l) => Some(l),
        Err(e) => {
            eprintln!("[look] {} 解析失败({e}),改用配置里的外观", path.display());
            backup_broken_config(&path);
            None
        }
    }
}

/// 读取键位配置(YAML);不存在或内容损坏时返回 None(调用方用默认值)。
///
/// 文件里是**多套按键组合**(见 [`keymap::ConfigFile`]):切换键要指向"哪一套",
/// 分开存反而要多维护一张名单,所以一份文件装全部。
fn load_profile() -> Option<ConfigFile> {
    let path = profile_path();
    let text = read_config_text(&path)?;
    match serde_norway::from_str::<ConfigFile>(&text) {
        Ok(mut doc) => {
            doc.normalize();
            Some(doc)
        }
        Err(e) => {
            eprintln!("[profile] {} 解析失败({e}),改用默认配置", path.display());
            backup_broken_config(&path);
            None
        }
    }
}

/// 读取配置文件文本,容忍 UTF-8 BOM。
///
/// 必要性:Windows 上记事本/若干编辑器保存 UTF-8 时会加上 BOM(EF BB BF),
/// 而 BOM 会让 serde_json 直接解析失败 —— 程序于是悄悄退回默认值,
/// 并在下一次落盘时**把用户原本的内容整份覆盖掉**。
/// 用户反馈的"我设置好 scrcpy 目录后,settings.json 里根本没有位置信息"
/// 正是这一类的表现;一个字节的差别不该毁掉整份配置,所以读进来先去掉它。
pub(crate) fn read_config_text(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Some(match text.strip_prefix('\u{feff}') {
        Some(rest) => rest.to_string(),
        None => text,
    })
}

/// 配置解析失败时把原文件另存一份(文件名后追加 `.broken`,如
/// `profile.yaml` -> `profile.yaml.broken`,look.json 同理)。
///
/// 为什么:解析失败后程序按默认值运行,而帧末的"内容变了就落盘"会把默认值
/// 写回同一个文件 —— 用户辛苦配的内容就此消失。先留个副本,至少还能捞回来。
pub(crate) fn backup_broken_config(path: &Path) {
    if path.is_file() {
        // 用 with_extension 会把原扩展名换掉(profile.yaml -> profile.broken),
        // 这里要的是"加一个后缀",因此直接在文件名上拼。
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".broken");
        let _ = std::fs::copy(path, path.with_file_name(name));
    }
}

/// 读取并严格校验任意路径下的键位配置(YAML);返回详细中文错误便于排查
fn read_profile_at(path: &std::path::Path) -> Result<ConfigFile, String> {
    let text = read_config_text(path).ok_or_else(|| "读取失败".to_string())?;
    let mut doc: ConfigFile =
        serde_norway::from_str(&text).map_err(|e| format!("不是合法的键位 yaml: {e}"))?;
    doc.normalize();
    Ok(doc)
}

/// 把配置渲染成"文件头说明 + 数据"的 YAML 文本(落盘的唯一出口)
fn render_config(doc: &ConfigFile) -> Result<String, String> {
    let body = serde_norway::to_string(doc).map_err(|e| format!("序列化失败: {e}"))?;
    Ok(format!("{}{body}", crate::keymap::YAML_HEADER))
}

fn sanitize_saved_scrcpy_args(raw: &str) -> (String, bool) {
    let mut out = Vec::new();
    let mut removed = false;
    let mut iter = raw.split_whitespace().peekable();
    while let Some(token) = iter.next() {
        let lower = token.to_ascii_lowercase();
        if lower == "--mouse=uhid"
            || lower == "--mouse=aoa"
            || lower == "-m=uhid"
            || lower == "-m=aoa"
        {
            removed = true;
            continue;
        }
        if (lower == "--mouse" || lower == "-m")
            && iter
                .peek()
                .is_some_and(|next| matches!(next.to_ascii_lowercase().as_str(), "uhid" | "aoa"))
        {
            let _ = iter.next();
            removed = true;
            continue;
        }
        out.push(token.to_string());
    }
    (out.join(" "), removed)
}

fn prepare_scrcpy_args_with_mode(base: &str, mode: ViewInputMode) -> String {
    let mut args = base.trim().to_string();
    let gamepad_mode = matches!(
        mode,
        ViewInputMode::VirtualGamepadContinuous | ViewInputMode::VirtualGamepadSegmented
    );
    if !gamepad_mode {
        return args;
    }
    let before = args.clone();
    let has_mouse = args
        .split_whitespace()
        .any(|arg| arg == "-M" || arg == "--mouse" || arg.starts_with("--mouse="));
    let has_keyboard = args
        .split_whitespace()
        .any(|arg| arg == "--keyboard" || arg.starts_with("--keyboard="));
    let has_gamepad = args
        .split_whitespace()
        .any(|arg| arg == "-G" || arg == "--gamepad" || arg.starts_with("--gamepad="));
    if !has_mouse {
        args.push_str(" --mouse=disabled");
    }
    if !has_keyboard {
        args.push_str(" --keyboard=disabled");
    }
    if !has_gamepad {
        args.push_str(" --gamepad=disabled");
    }
    if args != before {
        crate::diag_info!("scrcpy", "virtual-gamepad args enabled: {}", args.trim());
    }
    args.trim().to_string()
}
/// 按记住的设置定位 scrcpy 可执行文件。返回 (可执行文件, 额外说明)。
///
/// 三级兼底,专门对付"设置过了、重启还是找不到":
///   ① 记住的路径本身能解析出可执行文件(**目录也算**,里面找 scrcpy.exe);
///   ② 到记住的目录(以及上次 exe 的所在目录)里重新找一遍
///      —— 换版本时目录名会变(scrcpy-win64-v3.1 → v3.3)、解压时多套一层,
///      只要基准目录没变就还能找回来;
///   ③ 交给调用方走自动寻找。
fn locate_remembered_scrcpy(remembered: &Settings) -> (Option<PathBuf>, String) {
    if let Some(p) = adb::find_scrcpy_explicit(&remembered.scrcpy_path) {
        return (Some(p), String::new());
    }
    // 记住的目录可能是发行包目录本身,也可能是"解压到某个文件夹"的那一层 ——
    // find_scrcpy_under 两种都认,且只往名字像 scrcpy 的子目录里下探
    for dir in remembered.search_dirs() {
        let d = PathBuf::from(&dir);
        if let Some(p) = adb::find_scrcpy_under(&d, 3) {
            return (
                Some(p),
                format!("已在记住的目录里重新找到 scrcpy: {}", d.display()),
            );
        }
    }
    (None, String::new())
}

/// 向指定路径写入全新默认配置(父目录不存在则自动创建)
fn write_default_profile(path: &std::path::Path) -> Result<(), String> {
    let text = render_config(&ConfigFile::default())?;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(path, text).map_err(|e| format!("写入失败: {e}"))
}

/// epoch 秒 -> "2026-09-05 14:25:30"(本地时区,民用历算法)
fn fmt_timestamp(t: std::time::SystemTime) -> String {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02}")
}

/// epoch 秒 -> "20260905-142530"(用于文件名)
fn timestamp_compact() -> String {
    fmt_timestamp(std::time::SystemTime::now())
        .chars()
        .filter(|c| c.is_ascii_digit())
        .take(14)
        .collect()
}

/// 自 epoch 起的天数 -> (年, 月, 日)(Howard Hinnant 民用历算法)
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 由圆形轨迹的圆心/半径与取点坐标计算出发点角度,写入 path.start_angle
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
    let a = (y as f32 - cy).atan2(x as f32 - cx);
    *start_angle = a;
}

/// 由滑动轨迹类型计算圆心与半径(仅圆形有意义;非圆形返回 None)
fn circle_geometry(
    path: &SwipePath,
    start: (i32, i32),
    end: (i32, i32),
) -> Option<(f32, f32, f32)> {
    let SwipePath::Circle { as_diameter, .. } = path else {
        return None;
    };
    if *as_diameter {
        let cx = (start.0 + end.0) as f32 / 2.0;
        let cy = (start.1 + end.1) as f32 / 2.0;
        let r = ((end.0 - start.0) as f32).hypot((end.1 - start.1) as f32) / 2.0;
        Some((cx, cy, r))
    } else {
        let cx = start.0 as f32;
        let cy = start.1 as f32;
        let r = ((end.0 - start.0) as f32).hypot((end.1 - start.1) as f32);
        Some((cx, cy, r))
    }
}

impl EasingEditTarget {
    fn id(self) -> usize {
        match self {
            EasingEditTarget::Bind(i) => i,
            EasingEditTarget::Combo(i) => i + 100_000,
            EasingEditTarget::New => usize::MAX,
        }
    }
}

/// 该取点槽位是否属于"新增草稿"(取消时需一并清除草稿圆圈/轨迹)
fn is_draft_slot(slot: CoordSlot) -> bool {
    matches!(
        slot,
        CoordSlot::NewBind
            | CoordSlot::NewSwipeStart
            | CoordSlot::NewSwipeEnd
            | CoordSlot::NewCircleAngle
    )
}

fn combo_action_editor(
    ui: &mut egui::Ui,
    action: &mut Action,
    m: &Mapper,
    id: usize,
    pick: &mut Option<CoordSlot>,
    easing_edit: &mut Option<EasingEditTarget>,
) -> bool {
    let mut changed = false;
    let mut kind = match action {
        Action::Tap { .. } => 0,
        Action::Hold { .. } => 1,
        Action::Swipe(_) => 2,
        Action::AndroidKey { .. } => 3,
        Action::Macro(_) => usize::MAX,
    };
    ui.horizontal(|ui| {
        ui.label("动作:");
        if kind == usize::MAX {
            ui.label("宏（请在宏页编辑）");
        } else {
            egui::ComboBox::from_id_salt(("combo_action_kind", id))
                .selected_text(["点按", "长按", "滑动", "系统键"][kind])
                .show_ui(ui, |ui| {
                    for (index, name) in ["点按", "长按", "滑动", "系统键"].into_iter().enumerate()
                    {
                        ui.selectable_value(&mut kind, index, name);
                    }
                });
        }
    });
    let old_kind = match action {
        Action::Tap { .. } => 0,
        Action::Hold { .. } => 1,
        Action::Swipe(_) => 2,
        Action::AndroidKey { .. } => 3,
        Action::Macro(_) => usize::MAX,
    };
    if kind != usize::MAX && kind != old_kind {
        let (x, y, radius) = match action {
            Action::Tap { x, y, radius, .. } | Action::Hold { x, y, radius } => (*x, *y, *radius),
            Action::Swipe(s) => (s.start.0, s.start.1, crate::keymap::DEFAULT_RADIUS),
            Action::AndroidKey { .. } => (0.5, 0.5, crate::keymap::DEFAULT_RADIUS),
            Action::Macro(_) => (0.5, 0.5, crate::keymap::DEFAULT_RADIUS),
        };
        *action = match kind {
            0 => Action::Tap {
                x,
                y,
                duration_ms: crate::keymap::DEFAULT_TAP_DURATION_MS,
                radius,
            },
            1 => Action::Hold { x, y, radius },
            2 => Action::Swipe(Swipe {
                start: (x, y),
                end: (x, (y + 0.2).min(1.0)),
                duration_ms: 300,
                easing: Easing::Linear,
                path: SwipePath::Line,
            }),
            3 => Action::AndroidKey { keycode: 4 },
            _ => Action::AndroidKey { keycode: 4 },
        };
        changed = true;
    }

    match action {
        Action::Tap {
            x,
            y,
            duration_ms,
            radius,
        } => {
            ui.horizontal(|ui| {
                let waiting = *pick == Some(CoordSlot::ComboPoint(id));
                if ui
                    .button(if waiting { "点击取点..." } else { "取点" })
                    .clicked()
                {
                    *pick = Some(CoordSlot::ComboPoint(id));
                }
                ui.label("落点 x/y:");
                let mut px = m.x(*x);
                let mut py = m.y(*y);
                if ui
                    .add(egui::DragValue::new(&mut px).range(COORD_RANGE))
                    .changed()
                {
                    *x = m.rel_x(px);
                    changed = true;
                }
                if ui
                    .add(egui::DragValue::new(&mut py).range(COORD_RANGE))
                    .changed()
                {
                    *y = m.rel_y(py);
                    changed = true;
                }
                ui.label("时长");
                if ui
                    .add(egui::DragValue::new(duration_ms).range(0..=5000))
                    .changed()
                {
                    changed = true;
                }
                ui.label("范围");
                let mut pr = m.len(*radius);
                if ui
                    .add(egui::DragValue::new(&mut pr).range(0.01..=100000.0))
                    .changed()
                {
                    *radius = m.rel_len(pr);
                    changed = true;
                }
            });
        }
        Action::Hold { x, y, radius } => {
            ui.horizontal(|ui| {
                let waiting = *pick == Some(CoordSlot::ComboPoint(id));
                if ui
                    .button(if waiting { "点击取点..." } else { "取点" })
                    .clicked()
                {
                    *pick = Some(CoordSlot::ComboPoint(id));
                }
                ui.label("落点 x/y:");
                let mut px = m.x(*x);
                let mut py = m.y(*y);
                if ui
                    .add(egui::DragValue::new(&mut px).range(COORD_RANGE))
                    .changed()
                {
                    *x = m.rel_x(px);
                    changed = true;
                }
                if ui
                    .add(egui::DragValue::new(&mut py).range(COORD_RANGE))
                    .changed()
                {
                    *y = m.rel_y(py);
                    changed = true;
                }
                ui.label("范围");
                let mut pr = m.len(*radius);
                if ui
                    .add(egui::DragValue::new(&mut pr).range(0.01..=100000.0))
                    .changed()
                {
                    *radius = m.rel_len(pr);
                    changed = true;
                }
            });
        }
        Action::Swipe(s) => {
            changed |= swipe_controls(
                ui,
                s,
                pick,
                easing_edit,
                CoordSlot::ComboSwipeStart(id),
                CoordSlot::ComboSwipeEnd(id),
                CoordSlot::ComboCircleAngle(id),
                EasingEditTarget::Combo(id),
            );
        }
        Action::AndroidKey { keycode } => {
            ui.horizontal(|ui| {
                ui.label("系统 keycode:");
                if ui
                    .add(egui::DragValue::new(keycode).range(0..=999))
                    .changed()
                {
                    changed = true;
                }
            });
        }
        Action::Macro(_) => {
            ui.label("宏请在独立的“宏”页面编辑。");
        }
    }
    changed
}

/// 渲染滑动键的编辑控件:取起点/取终点、时长、曲线、轨迹、圆形专用按钮、曲线设置。
/// 返回"是否需要记录一个撤销点":离散选择(曲线/轨迹)在点选当帧返回 true;
/// 时长这类连续控件只在开始拖动/聚焦那一帧返回 true,避免每帧都记一个撤销步。
fn swipe_controls(
    ui: &mut egui::Ui,
    s: &mut Swipe,
    pick: &mut Option<CoordSlot>,
    easing_edit: &mut Option<EasingEditTarget>,
    slot_start: CoordSlot,
    slot_end: CoordSlot,
    slot_angle: CoordSlot,
    target: EasingEditTarget,
) -> bool {
    let mut changed = false;

    // 取起点 / 取终点
    let w_start = *pick == Some(slot_start);
    if ui
        .button(if w_start {
            "点击取起点..."
        } else {
            "取起点"
        })
        .clicked()
    {
        *pick = Some(slot_start);
    }
    let w_end = *pick == Some(slot_end);
    if ui
        .button(if w_end {
            "点击取终点..."
        } else {
            "取终点"
        })
        .clicked()
    {
        *pick = Some(slot_end);
    }

    ui.label("时长ms:");
    let r_dur = ui.add(egui::DragValue::new(&mut s.duration_ms).range(10..=5000));
    if r_dur.drag_started() || r_dur.gained_focus() {
        changed = true;
    }

    // 曲线选择
    ui.label("曲线:");
    let cur_ease = s.easing;
    egui::ComboBox::from_id_salt(("swipe_easing", target.id()))
        .selected_text(cur_ease.label())
        .show_ui(ui, |ui| {
            if ui
                .selectable_label(cur_ease == Easing::Linear, "默认(匀速)")
                .clicked()
            {
                s.easing = Easing::Linear;
                changed = true;
            }
            if ui
                .selectable_label(matches!(cur_ease, Easing::EaseIn { .. }), "加速曲线")
                .clicked()
            {
                s.easing = Easing::EaseIn { power: 2.0 };
                changed = true;
            }
            if ui
                .selectable_label(matches!(cur_ease, Easing::EaseOut { .. }), "减速曲线")
                .clicked()
            {
                s.easing = Easing::EaseOut { power: 2.0 };
                changed = true;
            }
            if ui
                .selectable_label(matches!(cur_ease, Easing::Smooth { .. }), "钟形曲线")
                .clicked()
            {
                s.easing = Easing::Smooth { power: 3.0 };
                changed = true;
            }
            if ui
                .selectable_label(matches!(cur_ease, Easing::Bezier { .. }), "贝塞尔曲线")
                .clicked()
            {
                s.easing = Easing::Bezier {
                    x1: 0.42,
                    y1: 0.0,
                    x2: 0.58,
                    y2: 1.0,
                };
                changed = true;
            }
        });

    // 轨迹选择
    ui.label("轨迹:");
    let cur_path = s.path;
    egui::ComboBox::from_id_salt(("swipe_path", target.id()))
        .selected_text(cur_path.label())
        .show_ui(ui, |ui| {
            if ui
                .selectable_label(matches!(cur_path, SwipePath::Line), "默认(条形)")
                .clicked()
            {
                s.path = SwipePath::Line;
                changed = true;
            }
            if ui
                .selectable_label(matches!(cur_path, SwipePath::Rect), "方形")
                .clicked()
            {
                s.path = SwipePath::Rect;
                changed = true;
            }
            if ui
                .selectable_label(matches!(cur_path, SwipePath::Circle { .. }), "圆形")
                .clicked()
            {
                s.path = SwipePath::Circle {
                    as_diameter: true,
                    start_angle: 0.0,
                };
                changed = true;
            }
        });

    // 圆形专用按钮
    if let SwipePath::Circle { as_diameter, .. } = &mut s.path {
        if ui
            .button(if *as_diameter {
                "视为直径"
            } else {
                "视为圆心"
            })
            .clicked()
        {
            *as_diameter = !*as_diameter;
            changed = true;
        }
        let w_angle = *pick == Some(slot_angle);
        if ui
            .button(if w_angle {
                "点击设出发点..."
            } else {
                "设置出发点"
            })
            .clicked()
        {
            *pick = Some(slot_angle);
        }
    }

    // 曲线设置(仅非匀速曲线有参数)
    if !matches!(s.easing, Easing::Linear) {
        if ui.button("设置...").clicked() {
            *easing_edit = Some(target);
        }
    }

    changed
}

/// 摇杆信息卡:画布上点开某个摇杆时,在它旁边列出四个方向对应的键位。
///
/// 为什么做成"点开才显示":那一长串方向键/启用键一直压在海报一样的游戏画面上,
/// 既挡视野又读不清(用户附了截图)。现在画布上只留"摇杆N",细节按需弹出。
/// 卡片整体垫一层衬底 + 描边字,所以压在亮块或暗块上都读得清;
/// 右边放不下就翻到左边,保证不会被画布边缘裁掉。
#[allow(clippy::too_many_arguments)]
fn draw_wheel_card(
    painter: &egui::Painter,
    th: &Theme,
    canvas: egui::Rect,
    w: &Wheel,
    wi: usize,
    center: egui::Pos2,
    radius: f32,
    tone: f32,
) {
    use egui::{Align2, FontId, Stroke};
    let ink = theme::tone_text(tone);
    let title = format!(
        "{}{}",
        if w.temp.is_some() {
            "临时摇杆"
        } else {
            "摇杆"
        },
        wi + 1
    );
    let mut lines: Vec<(String, egui::Color32)> = Vec::new();
    lines.push((format!("类型 {}", w.kind.label()), ink));
    for (i, (angle, key)) in w.active_dirs().into_iter().enumerate() {
        let name = match angle.round() as i32 {
            -90 => "上".to_string(),
            0 => "右".to_string(),
            90 => "下".to_string(),
            a if a.abs() == 180 => "左".to_string(),
            a => format!("{a}°"),
        };
        lines.push((format!("{} {} {}", name, i + 1, key_name(key)), ink));
    }
    if let Some(t) = &w.temp {
        let mode = match t.mode {
            TempMode::Hold => "按住启用",
            TempMode::Toggle => "再按切换",
        };
        lines.push((
            format!("启用 {} · {mode}", key_name(t.key)),
            theme::tone_color(th.wheel_enable, tone),
        ));
    }

    let font = FontId::proportional(theme::size::LABEL_FONT);
    // 先量一遍:卡片宽度取最长一行,行高由字体给
    let galleys: Vec<std::sync::Arc<egui::Galley>> = lines
        .iter()
        .map(|(s, c)| painter.layout_no_wrap(s.clone(), font.clone(), *c))
        .collect();
    let title_galley = painter.layout_no_wrap(title.clone(), font.clone(), ink);
    let line_h = galleys
        .iter()
        .map(|g| g.size().y)
        .fold(title_galley.size().y, f32::max)
        + 2.0;
    let text_w = galleys
        .iter()
        .map(|g| g.size().x)
        .fold(title_galley.size().x, f32::max);

    let pad = egui::vec2(8.0, 6.0);
    let size = egui::vec2(text_w, line_h * (galleys.len() + 1) as f32) + pad * 2.0;
    // 默认放右边;右边不够就放左边;再不够就贴着画布内边
    let mut min = egui::pos2(center.x + radius + 12.0, center.y - size.y * 0.5);
    if min.x + size.x > canvas.max.x - 4.0 {
        min.x = center.x - radius - 12.0 - size.x;
    }
    min.x = min.x.clamp(
        canvas.min.x + 4.0,
        (canvas.max.x - size.x - 4.0).max(canvas.min.x + 4.0),
    );
    min.y = min.y.clamp(
        canvas.min.y + 4.0,
        (canvas.max.y - size.y - 4.0).max(canvas.min.y + 4.0),
    );
    let card = egui::Rect::from_min_size(min, size);

    // 衬底 + 一圈摇杆语义色的边,和画布上的圈一眼对应
    let edge = theme::tone_color(
        if w.temp.is_some() {
            th.wheel_temp
        } else {
            th.wheel_perm
        },
        tone,
    );
    painter.rect_filled(card, 4.0, theme::plate(ink));
    painter.rect_stroke(card, 4.0, Stroke::new(1.5, edge), egui::StrokeKind::Inside);

    let mut y = card.min.y + pad.y;
    theme::paint_text(
        painter,
        egui::pos2(card.min.x + pad.x, y),
        Align2::LEFT_TOP,
        &title,
        font.clone(),
        edge,
    );
    y += line_h;
    for g in galleys {
        painter.galley(egui::pos2(card.min.x + pad.x, y), g, ink);
        y += line_h;
    }
}

/// 在截图上绘制滑动轨迹示意:边界实色、内部半透明,尽量不遮挡截图内容。
/// 入参为像素坐标(调用方负责从配置的相对坐标换算);`tone` 是键位显示亮度档位。
fn draw_swipe_track<F: Fn(i32, i32) -> egui::Pos2>(
    painter: &egui::Painter,
    th: &Theme,
    path: SwipePath,
    start: (i32, i32),
    end: (i32, i32),
    to_screen: &F,
    scale: f32,
    label: &str,
    tone: f32,
) {
    use egui::{Align2, FontId, Stroke};
    let pts: Vec<egui::Pos2> = crate::keymap::swipe_points(path, start, end, 64)
        .iter()
        .map(|&(x, y)| to_screen(x, y))
        .collect();
    if pts.len() < 2 {
        return;
    }
    let (edge_color, fill) = theme::tone_ring_fill(th.swipe, theme::with_alpha(th.swipe, 40), tone);
    let edge = Stroke::new(theme::size::KEY_STROKE, edge_color);
    // 与键位圈同一套可读性处理:先垫一圈反相"外套"再画本色,
    // 于是轨迹压在亮块/暗块上都还有一圈边界可辨(制图学 halo 的做法)。
    let coat = theme::casing(edge_color);
    match path {
        SwipePath::Line => {
            painter.add(egui::Shape::line(pts.clone(), Stroke::new(13.5, coat)));
            painter.add(egui::Shape::line(pts.clone(), Stroke::new(10.0, fill)));
            painter.add(egui::Shape::line(pts.clone(), edge));
        }
        SwipePath::Rect => {
            let a = to_screen(start.0, start.1);
            let b = to_screen(end.0, end.1);
            let rect = egui::Rect::from_two_pos(a, b);
            painter.rect_stroke(
                rect,
                0.0,
                Stroke::new(theme::size::KEY_STROKE + 2.5, coat),
                egui::StrokeKind::Outside,
            );
            painter.rect_filled(rect, 0.0, fill);
            painter.rect_stroke(rect, 0.0, edge, egui::StrokeKind::Inside);
        }
        SwipePath::Circle { .. } => {
            if let Some((cx, cy, r)) = circle_geometry(&path, start, end) {
                let c = to_screen(cx as i32, cy as i32);
                let r_screen = r * scale;
                painter.circle_filled(c, r_screen, fill);
                painter.circle_stroke(
                    c,
                    r_screen,
                    Stroke::new(theme::size::KEY_STROKE + 2.5, coat),
                );
                painter.circle_stroke(c, r_screen, edge);
            }
        }
    }
    let p0 = pts[0];
    painter.circle_stroke(p0, 15.0, Stroke::new(3.0, coat));
    painter.circle_stroke(p0, 12.0, edge);
    theme::paint_label(
        painter,
        p0,
        Align2::CENTER_CENTER,
        label,
        FontId::proportional(theme::size::SMALL_FONT),
        theme::tone_text(tone),
    );
}

/// 绘制速率函数(缓动曲线)预览图
fn draw_easing_preview(ui: &mut egui::Ui, e: Easing) {
    let size = egui::vec2(220.0, 100.0);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter();
    // 预览图固定用深底 + 主题强调色,深浅配色下都清晰
    let accent = ui.style().visuals.hyperlink_color;
    painter.rect_filled(rect, 2.0, egui::Color32::from_gray(30));
    let n = 64;
    let pts: Vec<egui::Pos2> = (0..=n)
        .map(|i| {
            let t = i as f32 / n as f32;
            let y = crate::keymap::easing_apply(e, t);
            egui::pos2(
                rect.min.x + t * rect.width(),
                rect.max.y - y * rect.height(),
            )
        })
        .collect();
    painter.add(egui::Shape::line(pts, egui::Stroke::new(2.0, accent)));
}

/// 宏录制结果的可读摘要。默认界面只显示摘要，原始事件由“显示录制事件”展开。
fn macro_summary(steps: &[MacroStep]) -> String {
    if steps.is_empty() {
        return "空宏".to_string();
    }
    let mut keys = HashSet::new();
    let mut total_ms = 0u32;
    for step in steps {
        keys.insert(step.code);
        total_ms = total_ms.saturating_add(step.delay_ms);
    }
    format!(
        "录制宏：{} 个事件 / {} 个键 / 约 {}ms；长按重复已合并",
        steps.len(),
        keys.len(),
        total_ms
    )
}

fn sanitize_virtual_profile(profile: &mut Profile) {
    profile
        .binds
        .retain(|b| !matches!(b.action, Action::Macro(_)));
    profile
        .combos
        .retain(|c| !matches!(c.action, Action::Macro(_)));
    // 虚拟层只负责键位/轮盘坐标，不继承程序级开关与视角状态。
    profile.toggle_key = 0;
    profile.cursor_toggle_key = 0;
    profile.aim = Default::default();
}

fn virtual_keyboard_lights(
    profile: &Profile,
) -> std::collections::HashMap<u16, (egui::Color32, String)> {
    let th = theme::Theme::dark();
    let mut lights = std::collections::HashMap::new();
    for bind in &profile.binds {
        if bind.key != 0 {
            lights.insert(
                bind.key,
                (
                    if bind.fps_only {
                        th.key_fps
                    } else {
                        th.key_macro
                    },
                    bind.action.describe(),
                ),
            );
        }
    }
    for wheel in &profile.wheels {
        for (angle, key) in wheel.active_dirs() {
            if key != 0 {
                lights.entry(key).or_insert((
                    if wheel.temp.is_some() {
                        th.wheel_temp
                    } else {
                        th.wheel_perm
                    },
                    format!("虚拟轮盘方向 {angle:.0}°"),
                ));
            }
        }
    }
    lights
}

/// 截图预览的自动适应倍率：同时限制可用宽度和窗口高度，避免竖屏铺满右侧、
/// 横屏顶出可视区域。手动 +/- 仍在这个倍率上乘用户倍率。
fn screenshot_fit_scale(avail_w: f32, window_h: f32, tex_w: u32, tex_h: u32) -> f32 {
    if tex_w == 0 || tex_h == 0 {
        return 1.0;
    }
    let usable_w = (avail_w * 0.94).max(120.0);
    let usable_h = (window_h * 0.68).max(180.0);
    (usable_w / tex_w as f32)
        .min(usable_h / tex_h as f32)
        .clamp(0.05, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 亮度网格:自动对比全靠它,索引越界/退化尺寸绝不能 panic
    /// (它每帧、每个标注都要被查一次,一旦 panic 就是整个界面崩掉)。
    #[test]
    fn luma_grid_samples_safely() {
        // 左半黑、右半白的小图:检查左右采样确实不同
        let (w, h) = (8usize, 4usize);
        let mut px = Vec::new();
        for _y in 0..h {
            for x in 0..w {
                px.push(if x < w / 2 {
                    egui::Color32::BLACK
                } else {
                    egui::Color32::WHITE
                });
            }
        }
        let img = egui::ColorImage::new([w, h], px);
        let grid = LumaGrid::new(&img).expect("合法尺寸应能建立网格");
        assert!(grid.around(1, 1) < 0.2, "左半应判定为暗");
        assert!(grid.around(6, 1) > 0.8, "右半应判定为亮");
        // 暗处要变浅、亮处要加深(与 theme::auto_tone 的方向一致)
        assert!(theme::auto_tone(grid.around(1, 1), 0.0) > 0.0);
        assert!(theme::auto_tone(grid.around(6, 1), 0.0) < 0.0);

        // 越界/负坐标:夹住而不是 panic
        let _ = grid.around(-100, -100);
        let _ = grid.around(9999, 9999);
        // 极端小图
        let tiny = egui::ColorImage::new([1, 1], vec![egui::Color32::WHITE]);
        let g2 = LumaGrid::new(&tiny).expect("1x1 也应可用");
        assert!(g2.around(0, 0) > 0.8);
        // 尺寸非法 -> None(调用方退回手动档位),绝不 panic
        let bad = egui::ColorImage::new([0, 0], Vec::new());
        assert!(LumaGrid::new(&bad).is_none());
    }

    /// 回归:"设置好 scrcpy 目录后,再次启动依旧找不到 scrcpy"。
    ///
    /// 这个测试完整走一遍启动时的定位逻辑:把官方发行包的布局
    /// (scrcpy.exe / scrcpy-server / adb.exe 同目录)造在临时目录里,
    /// 然后分别以"记住的是目录""记住的是 exe""exe 被换版本改名了"三种情况
    /// 检查程序是否还能把它找回来。
    #[test]
    fn remembered_scrcpy_location_survives_restart_and_move() {
        let root =
            std::env::temp_dir().join(format!("scrcpy-pad-app-locate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let pkg = root.join("scrcpy-win64-v3.3.3");
        std::fs::create_dir_all(&pkg).unwrap();
        let exe = pkg.join(adb::scrcpy_exe_name());
        std::fs::write(&exe, b"x").unwrap();
        std::fs::write(pkg.join("scrcpy-server"), b"x").unwrap();
        std::fs::write(pkg.join(adb::adb_exe_name()), b"x").unwrap();

        // ① 记住的是**目录**(用户嘴里就是"scrcpy 目录")
        let mut s = Settings {
            scrcpy_dir: pkg.display().to_string(),
            ..Settings::default()
        };
        s.sanitize();
        assert_eq!(locate_remembered_scrcpy(&s).0, Some(exe.clone()));

        // ② 记住的是可执行文件本身
        let mut s = Settings {
            scrcpy_path: exe.display().to_string(),
            ..Settings::default()
        };
        s.sanitize();
        assert_eq!(locate_remembered_scrcpy(&s).0, Some(exe.clone()));

        // ③ 升级 scrcpy:把新版本解压到旁边、删掉旧版本目录。
        //    记住的 exe 路径与它的**父目录**都没了 —— 必须靠"再上一级"这条线索
        //    把新版本找回来(用户反馈正是"重启后依旧找不到")。
        let newer = root.join("scrcpy-win64-v3.4");
        std::fs::create_dir_all(&newer).unwrap();
        let newer_exe = newer.join(adb::scrcpy_exe_name());
        std::fs::write(&newer_exe, b"x").unwrap();
        let stale_dir = root.join("scrcpy-win64-v3.3.3");
        let stale_exe = stale_dir.join(adb::scrcpy_exe_name());
        let mut s = Settings {
            scrcpy_path: stale_exe.display().to_string(),
            ..Settings::default()
        };
        std::fs::remove_dir_all(&pkg).unwrap(); // 旧版本目录被删掉了
        assert!(s.sanitize(), "死路径应被清理");
        assert_eq!(
            PathBuf::from(&s.scrcpy_dir),
            root,
            "父目录也没了时,应把线索留在再上一级"
        );
        assert_eq!(locate_remembered_scrcpy(&s).0, Some(newer_exe.clone()));

        // ④ 记住的位置彻底不存在 -> 交给自动寻找(这里只验证"不误报")
        let mut s = Settings {
            scrcpy_dir: root.join("根本没有这个目录").display().to_string(),
            ..Settings::default()
        };
        s.sanitize();
        assert_eq!(s.scrcpy_dir, "", "不存在的目录必须清掉");
        assert_eq!(locate_remembered_scrcpy(&s).0, None);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 落盘的唯一出口:必须产出"文件头说明 + 数据"的合法 YAML,
    /// 而且原样读回来要与写出去的一模一样(否则用户的键位会被悄悄改掉)。
    #[test]
    fn rendered_config_is_a_readable_yaml_with_the_header() {
        let doc = ConfigFile::default();
        let text = render_config(&doc).expect("默认配置必须能序列化");
        assert!(text.starts_with('#'), "文件头说明必须排在最前面");
        assert!(
            text.contains("switch_keys"),
            "字段说明里应提到多套组合相关的字段"
        );
        let path =
            std::env::temp_dir().join(format!("scrcpy-pad-render-{}.yaml", std::process::id()));
        std::fs::write(&path, &text).unwrap();
        let back = read_profile_at(&path).expect("刚写出的文件必须能读回来");
        assert_eq!(back, doc, "写出去再读回来必须逐字段一致");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn legacy_uhid_is_never_auto_added() {
        assert_eq!(
            prepare_scrcpy_args_with_mode("--stay-awake", ViewInputMode::UhidMouse),
            "--stay-awake"
        );
        assert_eq!(
            prepare_scrcpy_args_with_mode("--stay-awake", ViewInputMode::AoaMouse),
            "--stay-awake"
        );
    }

    #[test]
    fn virtual_gamepad_args_are_added_once() {
        assert_eq!(
            prepare_scrcpy_args_with_mode("--stay-awake", ViewInputMode::VirtualGamepadSegmented),
            "--stay-awake --mouse=disabled --keyboard=disabled --gamepad=disabled"
        );
        assert_eq!(
            prepare_scrcpy_args_with_mode(
                "--stay-awake --mouse=disabled --keyboard=disabled --gamepad=disabled",
                ViewInputMode::VirtualGamepadContinuous,
            ),
            "--stay-awake --mouse=disabled --keyboard=disabled --gamepad=disabled"
        );
    }

    #[test]
    fn legacy_saved_scrcpy_args_drop_uhid_mouse_capture() {
        let (args, removed) =
            sanitize_saved_scrcpy_args("--stay-awake --mouse=uhid --keyboard=disabled");
        assert!(removed);
        assert_eq!(args, "--stay-awake --keyboard=disabled");
        let (args, removed) = sanitize_saved_scrcpy_args("--mouse aoa --stay-awake");
        assert!(removed);
        assert_eq!(args, "--stay-awake");
    }

    /// 风格切换请求必须是"取走即复位":main.rs 在每次 run_native 返回后都查一次,
    /// 若不复位,则"换过风格的那次运行"之后,用户正常关闭窗口也会被再次重开
    /// (幽灵重启)。同时确认复位后再次请求仍然有效(可以连续换风格)。
    #[test]
    fn style_restart_flag_is_one_shot() {
        STYLE_RESTART.store(false, Ordering::SeqCst);
        assert!(!take_style_restart(), "没有请求时不应触发重启");
        STYLE_RESTART.store(true, Ordering::SeqCst);
        assert!(take_style_restart(), "有请求时必须触发重启");
        assert!(!take_style_restart(), "取走后必须复位");
        STYLE_RESTART.store(true, Ordering::SeqCst);
        assert!(take_style_restart(), "连续换风格时第二次请求仍要生效");
    }

    /// 两页默认都显示键位、组合键、永久/临时摇杆；宏/FPS 默认关闭。
    #[test]
    fn vk_filter_defaults_match_the_two_pages() {
        let k = VkFilters::KEYS_PAGE;
        assert!(k.binds && k.combos && k.wheels_perm && k.wheels_temp);
        assert!(!k.macros && !k.fps, "宏和 FPS 默认不勾选");
        let f = VkFilters::FPS_PAGE;
        assert!(f.binds && f.combos && f.wheels_perm && f.wheels_temp);
        assert!(!f.macros && !f.fps, "宏和 FPS 默认不勾选");
    }

    /// 可视化风格的三张右侧标签与三张左侧标签:名字互不重复、均不为空。
    /// "键位"必须排在可视化右侧第一张、"键位组合"排在左侧第一张且为默认页
    /// (用户明确要求:启动程序时默认是设置键位)。
    #[test]
    fn visual_tab_labels_are_distinct_and_ordered() {
        let right = [
            RightTab::Keys,
            RightTab::Macro,
            RightTab::Fps,
            RightTab::Other,
        ];
        let left = [LeftTab::Schemes, LeftTab::Look, LeftTab::Diag];
        let mut seen = std::collections::HashSet::new();
        for t in right {
            assert!(seen.insert(t.label()), "右侧标签重复: {}", t.label());
        }
        for t in left {
            assert!(seen.insert(t.label()), "左右标签互相重复: {}", t.label());
        }
        assert_eq!(RightTab::default(), RightTab::Keys, "可视化右侧默认键位页");
        assert_eq!(LeftTab::default(), LeftTab::Schemes, "左侧默认键位组合页");
    }

    #[test]
    fn macro_recording_merges_keyboard_auto_repeat() {
        let base = Instant::now();
        let mut rec = MacroRecording {
            steps: Vec::new(),
            held: HashSet::new(),
            last_event: base,
            last_step_at: base,
            idle_ms: 800,
        };
        PadApp::record_macro_button(&mut rec, 18, true, base);
        PadApp::record_macro_button(&mut rec, 18, true, base + Duration::from_millis(33));
        PadApp::record_macro_button(&mut rec, 18, true, base + Duration::from_millis(66));
        assert_eq!(rec.steps.len(), 1, "自动重复不能变成连续按下");
        PadApp::record_macro_button(&mut rec, 18, false, base + Duration::from_millis(100));
        assert_eq!(rec.steps.len(), 2, "只应记录一次按下和一次抬起");
        assert_eq!(rec.steps[1].delay_ms, 100, "抬起延迟应保留真实长按时间");
    }

    #[test]
    fn screenshot_fit_scale_respects_window_height_and_width() {
        let portrait = screenshot_fit_scale(600.0, 900.0, 1080, 2400);
        assert!(portrait < 0.4 && portrait > 0.2, "竖屏应按高度限制自动缩小");
        let landscape = screenshot_fit_scale(1200.0, 900.0, 2400, 1080);
        assert!(
            landscape < 0.5 && landscape > 0.1,
            "横屏应按宽度限制自动缩小"
        );
        assert!(screenshot_fit_scale(0.0, 0.0, 1080, 2400) < 0.1);
    }
}
