use crate::adb::{self, ControlServer};
use crate::adbcmd;
use crate::capture::{Capture, CaptureEvent};
use crate::control::ControlClient;
use crate::engine::{Shared, SharedState, lock_shared};
use crate::keyboard;
use crate::keymap::{
    Action, Aim, ConfigFile, Easing, KeyBind, KeyCombo, KeySet, MacroAction, MacroInstruction,
    MacroStep, MacroWheelPart, Mapper, Profile, RecenterMode, SWIPE_SAMPLES, Swipe, SwipePath,
    SwitchDirection, SwitchKey, TempMode, TempWheel, ViewInputMode, Wheel, WheelKind, WheelMode,
    key_name,
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
const KIND_NAMES: [&str; 4] = ["点按", "长按", "滑动", "系统键"];

/// 坐标编辑框的取值范围:**允许负值、允许超出屏幕**。
/// 键位本来就允许落在画面外(比如横屏的布局在竖屏下显示、截图尺寸与布局方向不同),
/// 这里不做越界"纠正",免得程序擅自改动用户调好的坐标。
const COORD_RANGE: std::ops::RangeInclusive<i32> = -8192..=8192;

/// 撤销栈深度(步数)。整份配置快照,50 步足以覆盖一次调参过程。
const UNDO_DEPTH: usize = 50;

/// 调试信息轮询间隔。2026-10-07(用户第 4 条)从 1s 放宽到 2s:
/// 每一轮 = 后台 adb.exe spawn(dumpsys),开着调试弹窗打游戏时它按秒
/// 打断手感。诊断价值(帧率/刷新率走势)在 2s 粒度上不受影响。
const DEBUG_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// 分辨率查询的刷新周期。手机分辨率基本不变,没必要跟着轮询走 ——
/// `dumpsys window displays` 是这组查询里最重的一条,单独降到 30s 一次:
/// 稳态只剩"每 2s 一次 SurfaceFlinger 延迟查询"。
const DEBUG_SIZE_TTL: std::time::Duration = std::time::Duration::from_secs(30);

/// 压枪"控制强度档位表"默认展开的上限档数(用户 2026-10-10 第 3 条:
/// "挡位过多时,非添加模式自动折叠")。
///
/// 档位是这个面板里唯一会"越加越长"的列表,档位一多就把下面的[摇晃]/[覆盖灵敏度]
/// 顶出屏幕。所以档数超过这个数时,档位表**第一次出现**就默认收起,只留一行标题;
/// 用户点标题可展开/收起,点[＋ 添加一档]时强制展开(正在添加,不该藏)。
/// 注意这是"默认值"而非"每当档数变多就强制收起":一旦用户自己点过标题,
/// 之后就以用户的选择为准 —— 正在调档时列表自己收起来,才是真的难用。
const RECOIL_TIER_COLLAPSE_AT: usize = 4;

// ============================ 界面风格切换的重启标志 ============================
//
// 风格(默认/可视化)牵动整体布局,不能像配色那样每帧热应用
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
    /// 压枪/后坐力补偿的触发键(V2-1;K2er 原文:一般是鼠标左键)
    RecoilTrigger,
    /// 压枪"控制强度"挡位切换键(用户 2026-10-10 第 3 条):按一下换一档(环绕),
    /// 按住它时滚轮也用来换档。与 [`Self::RecoilTrigger`] 一样是组合键槽
    /// (见 [`Self::takes_chord`])——用户要求"可容纳单次/多次按下的按键绑定控件"。
    RecoilSwitch,
    /// 切换键位(第 i 行:按下那一组键即切到它指向的那套组合)。
    ///
    /// 用户 2026-10-09(第 4 条):这里不再有"第一个键/第二个键"两个槽 ——
    /// 整行就是**一个**组合键槽(`SwitchKey.keys`),用一个按钮捕获,
    /// 与总开关键那类系统键同一套交互(见 [`Self::keys_button`])。
    SwitchKey(usize),
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
    // ---------- R4(2026-10-08)：扩展宏弹窗里的"点虚拟键盘选键" ----------
    // 这一组和上面最大的不同：它们**不写实时配置**，只写弹窗里那份
    // `MacroVirtualEditor.profile`（宏草稿）。见 `assign_virtual_key`。
    /// 弹窗点[＋ 按键]之后等一次键盘点击（新建虚拟键位，随后自动进入截图取点）
    MacroVirtualNewBind,
    /// 弹窗里虚拟组合键的第 `slot` 个物理键
    MacroVirtualComboKey {
        combo: usize,
        slot: usize,
    },
    /// 弹窗里虚拟轮盘的方向键（`dir`：标准轮盘 0上 1下 2左 3右；多向/执行轮盘 = `directions` 下标）
    MacroVirtualWheelDir {
        wheel: usize,
        dir: usize,
    },
    /// 弹窗里虚拟轮盘的启用键（设了就是临时摇杆）
    MacroVirtualWheelEnable(usize),
}

impl KeySlot {
    /// 是否属于「扩展宏弹窗」的虚拟取键（只写宏草稿，不碰实时配置）。
    ///
    /// 关窗/取消/删除目标时要靠它把这些"待完成操作"清掉 —— 否则虚拟键盘
    /// 会一直举着"按任意键..."，用户按下的键写进一个已经不存在的弹窗。
    fn is_virtual(self) -> bool {
        matches!(
            self,
            Self::MacroVirtualNewBind
                | Self::MacroVirtualComboKey { .. }
                | Self::MacroVirtualWheelDir { .. }
                | Self::MacroVirtualWheelEnable(_)
        )
    }

    /// 该槽位存的是**一个按键集合**([`KeySet`]:单个键或最多两个键的组合),
    /// 而不是单个键码 —— 用户 2026-10-09 第 4 条要求的正是这些"系统键"槽位。
    ///
    /// 走这条路的槽位统一:一个按钮捕获(见 `PadApp::keys_button`)、
    /// 显示 `Ctrl+X`、匹配时顺序无关、**不提供**任何时长/间隔参数。
    /// `binds[]`(按键设置)与 `combos[]`(组合键设置)是用户明确划在外的两块,
    /// 它们各自的编辑器按原样工作。
    ///
    /// 用户 2026-10-10 第 2 条把**轮盘方向键**与**临时轮盘启用键**也并了进来 ——
    /// 这正是上一轮文档里点名"唯一未覆盖"的两个槽位(`Wheel.up/down/left/right`、
    /// `WheelDirection.key`、`Wheel.temp.key`)。
    fn takes_chord(self) -> bool {
        matches!(
            self,
            Self::Toggle
                | Self::CursorToggle
                | Self::AimHold
                | Self::AimToggle
                | Self::AimSuspend
                | Self::RecoilTrigger
                | Self::RecoilSwitch
                | Self::SwitchKey(_)
                | Self::WheelDir { .. }
                | Self::WheelEnable(_)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum CoordSlot {
    NewBind,
    Bind(usize),
    WheelCenter(usize),
    /// 轮盘某个方向的**手动终点**(在截图上直接点一个点;可超出/不足影响范围圆)
    WheelDirEnd {
        wheel: usize,
        dir: usize,
    },
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
    /// 宏页"点击"步骤的落点(W2-8:宏也能从截图上取点,不必手输 0..1)。
    MacroClickPoint(usize),
    /// 宏页"滑动"步骤的起点/终点
    MacroSwipeStart(usize),
    MacroSwipeEnd(usize),
    /// R4(2026-10-08):扩展宏弹窗里的取点 —— 只写弹窗那份虚拟键位表(宏草稿)。
    ///
    /// 存成"哪一类 + 第几个"而不是直接存堆下标,是为了让删除列表项之后
    /// 剩下的索引仍然自洽(`assign_virtual_coord` 取不到就什么都不做)。
    MacroVirtual(MacroVirtualPick),
}

impl CoordSlot {
    /// 是否属于「扩展宏弹窗」的虚拟取点。
    ///
    /// 截图浮层靠它决定"画实时配置还是画弹窗里那套虚拟键位",关窗/取消时也靠它
    /// 把待完成的取点清干净(不留"键位垃圾",用户 2026-10-08 明确要求)。
    fn is_virtual(self) -> bool {
        matches!(self, Self::MacroVirtual(_))
    }
}

/// R4(2026-10-08):扩展宏弹窗里"在截图上取点"的目标。
///
/// 四类正好对应弹窗里能新增的四种东西(用户口径):轮盘、组合键、按键、锚点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MacroVirtualPick {
    /// 虚拟按键(点按/长按)的落点
    Bind(usize),
    /// 虚拟组合键的落点
    Combo(usize),
    /// 虚拟轮盘的圆心
    WheelCenter(usize),
    /// 虚拟锚点(= 瞄准锚点;`aim.anchor_*`)
    AimAnchor,
}

/// R4(2026-10-08):扩展宏弹窗"虚拟层清单"上的一次操作请求。
///
/// 清单里的每一行都是**纯函数**(只读写虚拟层自己的数据,不碰 `Shared`,也不碰
/// `App`),所以"要在截图上取点 / 要在虚拟键盘上取键 / 删掉这一项"这类需要
/// `&mut App` 的动作只能回抛出来,由 [`App::apply_virtual_act`] 统一落地。
///
/// 这样分的好处:①弹窗里那套控件永远改不到实时配置;②取点/取键仍然是
/// **同一个** `picking` / `waiting_key` 机制,与主界面共用一套交互(强复用);
/// ③关窗/取消时把请求列表一丢就干净了。
#[derive(Debug, Clone, Copy, PartialEq)]
enum MacroVirtualAct {
    /// 取虚拟按键的落点
    PickBind(usize),
    /// 取虚拟组合键的落点
    PickComboPoint(usize),
    /// 取虚拟轮盘的圆心
    PickWheelCenter(usize),
    /// 取虚拟锚点
    PickAimAnchor,
    /// 等一次键盘点击(组合键第几个键 / 轮盘方向键 / 轮盘启用键)
    TakeKey(KeySlot),
    /// 删掉第 i 个虚拟组合键
    RemoveCombo(usize),
    /// 删掉第 i 个虚拟轮盘
    RemoveWheel(usize),
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
    /// 组合键切换键(整行一个组合键槽)
    SwitchKey(usize),
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
    /// 真实投屏帧率(scrcpy --print-fps 实况;与面板刷新率分开显示)
    stream_fps: String,
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

impl MacroRecording {
    /// 是否到达空闲自动停止条件。
    ///
    /// 关键不变量（用户 2026-10-07 再次强调）：**录制从第一个按键开始才算**。
    /// 点[开始录制]之后到第一个按键之间的空档既不算进宏、也不跑空闲计时 ——
    /// 否则用户想两秒再动手，录制会在"一个键都没按"的状态下自己悄悄结束。
    fn should_auto_stop(&self) -> bool {
        !self.steps.is_empty() && self.last_event.elapsed().as_millis() >= self.idle_ms as u128
    }
}

/// `pending_task_label` 的全部取值(与那里的分支一一对应,`debug_assert` 盯着它们同步)。
///
/// 单列出来是为了让顶栏[取消…]按钮能按**最长**的一条预留固定宽度 —— 按钮随任务出现
/// 和消失,宽度还会随标签变化,不预留的话整排按钮会左右横跳、甚至换行顶到下面
/// (用户 2026-10-09 明确要求不要这种抖动)。
/// 注意这里**没有"按键捕获"**:顶栏那个[取消按键捕获]已按用户 2026-10-10 第 1 条
/// 移除 —— 它离捕获现场太远,点它那一刻的鼠标按下还会被当成"要绑的键"录进去
/// (见 `note_cancel_zone`)。退出捕获一律用就地的那一个[取消设置]。
const PENDING_TASK_LABELS: [&str; 9] = [
    "连接",
    "截图",
    "坐标刷新",
    "音频唤醒",
    "日志收集",
    "宏录制",
    "取点",
    "范围修改",
    "曲线编辑",
];

/// egui 临时存储里存放"就地取消/停止控件矩形表"的键(见 `App::note_cancel_zone`)。
///
/// 放 egui 存储而不是放进 `App` 字段的原因:`cancel_bind_button` 这类控件渲染函数是
/// **没有 `self` 的关联函数**(十来个调用点都在借用 `self` 的其他部分),做成字段就得
/// 把它们的签名全改成 `&mut self`,为一个登记动作不值当。
fn cancel_zones_id() -> egui::Id {
    egui::Id::new("scrcpy-pad-cancel-zones")
}

/// egui 临时存储里"「按后延迟」总开关此刻是否打开"的键(见 `App::tail_delay_widget`)。
///
/// 走临时存储而不是给控件加参数的理由同上:`tail_delay_widget` 有 **7 处**调用,
/// 其中两处(`ui_macro_virtual_bind` / `ui_macro_virtual_combo`)是**没有 `self` 的
/// 关联函数**,把开关值一层层透传下去要改一串签名。开关值由 `PadApp::ui` 每帧开头
/// 写一次(就在 `cancel_zones_id` 旁边),控件读同一帧的这份值决定"可改 / 灰显"。
fn tail_delay_on_id() -> egui::Id {
    egui::Id::new("scrcpy-pad-tail-delay-on")
}

/// 扩展宏编辑窗口的临时状态；窗口关闭/取消时直接丢弃，不触碰原宏。
///
/// `Clone` 只为一处:`ui_macro_virtual_window` 里"拼在下方的截图"取点期间,要临时
/// 把这份键位表放回 `App`(见那里的注释),画完再把 profile 合并回来。
#[derive(Clone)]
struct MacroVirtualEditor {
    profile: Profile,
    selected_key: Option<u16>,
    source_scheme: usize,
    /// 最近一次**新增**的虚拟项(用户 2026-10-10 第 4 条"每次新增点需标明")。
    /// 截图浮层据此给这个点画一圈强调 + 「新」角标,让用户一眼找到刚放下的那个。
    /// 只影响画法,不进配置、不进宏。
    newest: Option<MacroVirtualPick>,
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
    /// 「按后延迟」(ms,用户 2026-10-10 第 2 条):这条键位上一次抬起之后,
    /// 至少再等这么久才接受下一次按下。与 [`crate::keymap::KeyBind::tail_delay_ms`] 同义。
    tail_delay_ms: u32,
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
            tail_delay_ms: crate::keymap::DEFAULT_TAIL_DELAY_MS,
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

/// 「手动 adb 命令」助手窗口的临时状态(打开期间有效,关窗丢弃)。
struct AdbHelperState {
    search: String,
    /// 内置库条目下标(过滤后仍保持指向 full list,避免搜索改变选中含义)
    selected: usize,
}

pub struct PadApp {
    shared: SharedState,
    grab_flag: Arc<std::sync::atomic::AtomicBool>,
    /// 「拦截系统默认行为」掩码(见 capture::swallow_bit):每帧按 映射开关 ×
    /// 独占键盘 × 当前绑定 预先算好,钩子回调只读一次原子量。
    swallow_flag: Arc<std::sync::atomic::AtomicU16>,
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
    /// 捕获层延迟是否超标(钩子回调 p99 > 5ms;引擎状态区据此警示)
    hook_lag_flag: Arc<std::sync::atomic::AtomicBool>,
    /// 钩子心跳结论(W1-4):0=正常,1=消息泵 ≥3s 无响应,2=钩子失效过、
    /// 已自愈重装。与 hook_lag 共用引擎状态区的告警通道;Linux 恒为 0。
    hb_flag: Arc<std::sync::atomic::AtomicU8>,
    /// Windows 低级钩子安装结果(bit0=键盘,bit1=鼠标;0=未知;W0-11)。
    /// 自检面板据此红字提示"按键捕获不可用" —— 在这之前装钩子失败只有
    /// 诊断日志知道,用户看到的是"按了没反应"却毫无线索。Linux 恒为 0。
    ///
    /// Linux 侧只写不读(读它的自检在 `#[cfg(windows)]` 里),所以那边标成"平台相关、
    /// 允许没人读"。字段本身还要留着:启动时照样要喂给捕获层(`Capture` 收它)。
    #[cfg_attr(not(windows), allow(dead_code))]
    hook_ok_flag: Arc<std::sync::atomic::AtomicU8>,
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
    /// 控制通道自动重连(2026-10-06 真机反馈):EOF/写失败后注入全线失效,
    /// 旧实现在界面上干等用户手动点[连接控制] —— 战斗中等于整个手柄失灵
    /// (真机日志里断出过 4 分钟空窗)。`reconnect_armed` = 用户想要连接
    /// (连上过或手动发起过;点[断开]即取消)。
    reconnect_armed: bool,
    /// 自动重连已尝试次数(成功后清零;超过上限停止并提示,点[连接控制]重来)
    reconnect_attempts: u32,
    /// 下一次自动重连的时刻(退避排程;None = 不在重连中)
    reconnect_due: Option<Instant>,
    /// scrcpy --print-fps 最近一次输出现值 + 时刻(真实投屏帧率,界面显示用)
    stream_fps: Option<(f32, Instant)>,

    waiting_key: Option<KeySlot>,
    /// 正在等待**组合键**输入的槽位(用户 2026-10-09 第 4 条:系统键那类
    /// "原来只允许单个键"的槽位现在统一可捕获 `Ctrl+X`)。
    ///
    /// 与 [`Self::waiting_key`] 是两个互斥的捕获模式:后者按下即捕获(一次一个键),
    /// 这里要等到**所有键都松开**才落定 —— 否则按住 `Ctrl` 的那一下就先把
    /// `Ctrl` 单独记下了。捕获期间按下的键记在 `capture_down`(当前按着)与
    /// `capture_seen`(这次捕获到的全部,最多 2 个)里。
    waiting_keys: Option<KeySlot>,
    /// 组合键捕获:此刻**按着**的键(按按下顺序,最多 2 个)。
    capture_down: Vec<u16>,
    /// 组合键捕获:这一轮**捕获到**的键(按按下顺序,最多 2 个)。
    capture_seen: Vec<u16>,
    /// 按下被 [`Self::swallow_cancel_click`] 吞掉、正等着配对抬起的那几个鼠标键。
    /// 记着它们是为了把对应的**抬起**也一起吞(否则会留下"没按过就抬起"的孤儿)。
    swallowed_buttons: Vec<u16>,
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
    /// 截图小窗(2026-10-09):true = 内容搬进独立窗口,下方面板只留按钮行。
    shot_window_open: bool,
    /// 截图小窗是否置顶(勾上=固定在其他窗口上方,不勾=可以被盖住)。默认置顶。
    shot_window_pin: bool,
    /// 上一次画截图画布时算出的"自动适应"基准倍率(按**当时那个容器**的可用宽高内接)。
    /// 界面上显示的那个百分比就是它×`shot_zoom`(见 `ui_shot_header`)。
    shot_last_base: f32,
    /// 截图小窗的**绝对**基准倍率:`程序窗口内容区 ∩ 系统屏幕`下这张图最合适的大小
    /// (用户 2026-10-10 第 3 条:"放不下时按程序窗口与系统屏幕边界调整缩放")。
    ///
    /// 小窗**不能**用"按小窗自己多大内接"那套:`±` 改窗口大小、窗口大小又改倍率,
    /// 两者会互相追着放大(每按一次 + 就 ×1.44,几帧就撑爆屏幕)。所以小窗的倍率
    /// 只由屏幕/程序窗口边界决定,窗口尺寸反过来由它算出来。
    shot_popup_base: f32,
    /// 截图小窗抬头上一帧的实际高度(pt):算小窗尺寸时要把它加在图高上,
    /// 否则图会把抬头挤出去(抬头会换行,高度不是一个常数)。
    shot_window_header_h: f32,
    /// 这个小窗是**从哪开的**:true = 扩展宏弹窗开的(画虚拟键位层),false = 主界面开的。
    /// 用户 2026-10-10(第 4 条):弹窗开的截图小窗也得看得见继承来的键位与新取的点。
    shot_window_virtual: bool,
    /// 扩展宏弹窗里截图画布区的高度(拖分界条调整;用户 2026-10-09:"文字与图像分界可拖动")。
    macro_shot_height: f32,
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
    /// 可视化风格:虚拟键盘上方五个显示开关(键位页与 FPS 页各一套,
    /// 两页的**显示能力完全一样**,只是默认值不同:见 VkFilters)
    vk_filters: VkFilters,
    vk_fps_filters: VkFilters,
    /// 可视化风格:当前在键盘下方操作栏里编辑的目标(见 VkSel)
    vk_sel: Option<VkSel>,
    /// 宏录制器。
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
    /// 宏页：「按后延迟」(ms,用户 2026-10-10 第 2 条)—— 这个宏的触发键上一次
    /// 抬起之后要等多久才接下一次触发。整条宏跑完再算冷却,所以连按宏键不会
    /// 让两次执行叠在一起。
    macro_page_tail_delay_ms: u32,
    /// 宏页：空闲自动停止时间。
    macro_idle_ms: u32,
    /// 宏页：是否展开显示原始录制事件（默认只显示结果摘要）。
    macro_show_events: bool,
    /// 录制过程中点[显示步骤]后常开:不看摘要,直接看逐条步骤(用户 2026-10-07 要求
    /// "先看步骤再决定保存",不必等保存后的[展开])。
    macro_show_recording_steps: bool,
    /// 已录宏列表中展开的宏索引。
    macro_expanded: Option<usize>,
    /// 当前载入编辑区的是哪一条宏；可视化下列表按钮据此变成“取消编辑”。
    macro_loaded_index: Option<usize>,
    /// 「宏草稿库」:长期保存的命名草稿(见 [`MacroDraft`]),与编辑区那份草稿无关。
    macro_drafts: Vec<MacroDraft>,
    /// 草稿库下拉当前选中的条目。
    macro_draft_sel: Option<usize>,
    /// [另存当前草稿]展开后输入的名字。
    macro_draft_name: String,
    /// 是否展开"给草稿起个名字"那一行。
    macro_draft_save_open: bool,
    /// 草稿库的一次性提示(保存/载入/删除的结果),`(是否成功, 文案)`。
    macro_draft_msg: Option<(bool, String)>,
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
    /// 上一次**成功**取到分辨率(手机屏幕尺寸)的时刻。None = 还没取到过。
    /// 分辨率按 [`DEBUG_SIZE_TTL`] 降频,轮询轮次里没查时沿用旧值。
    debug_size_at: Option<Instant>,
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
    /// 手动 adb 命令:命令栏(token 列表;加入时查冲突、单击可移除)
    adb_bar: Vec<String>,
    /// 手动 adb 命令:待加入参数的输入框
    adb_add_input: String,
    /// 手动 adb 命令:最近一条提示(true=成功/绿色)
    adb_msg: Option<(bool, String)>,
    /// 手动 adb 命令:助手窗口状态(None=未打开)
    adb_helper: Option<AdbHelperState>,
    /// 手动 adb 命令:用户自建预设(与内置预设同列在一个下拉里)
    adb_user_presets: Vec<adbcmd::UserPreset>,
    /// 预设下拉当前选中项;下标 < 内置预设数 时指内置预设
    adb_preset_sel: Option<usize>,
    /// 「另存当前为预设」命名行是否展开
    adb_preset_save_open: bool,
    adb_preset_name: String,
    /// 正在执行的 adb 命令(None=空闲)
    adb_run: Option<adbcmd::AdbRun>,
    adb_run_started: Option<Instant>,
    /// 最近一次执行的结果(显示在输出区)
    adb_last: Option<adbcmd::AdbRunOutcome>,
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

        // 程序级设置(scrcpy 三件套路径等):与 profile.yaml / look.json 同目录的 settings.json。
        // 有了它,scrcpy 目录即使不在程序同级,重启后也仍然记得,不必每次重新寻找。
        // 必须放在读取键位配置**之前**:里面还记着"上次选用的是哪一份配置"(W0-9)。
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

        // 配置文件里装的是多套"按键组合",`active` 指向上次用的那一套;
        // 引擎始终只认 `Shared::profile`(= 生效中的那套),组合表放在它旁边。
        //
        // 用户上次可能另选/新建过配置文件(位置记在 settings.json 的 `profile_path`):
        // 优先把它读回来。旧版这个路径只活在内存里 —— "我选的配置重启后不见了",
        // 程序每次都回到默认的 profile.yaml,用户只能一遍遍重选。
        // 读不回来时退回默认配置,并把原因上屏(不静默);
        // 记住的路径随即会被 settings 快照改成"实际在用"的默认路径(下次启动不再重复抱怨)。
        let default_profile_path = profile_path();
        let mut startup_notes: Vec<String> = Vec::new();
        let (doc, profile_path) = match remembered_profile_path(&saved) {
            Some(p) if p != default_profile_path => match read_profile_at(&p) {
                Ok(d) => {
                    startup_notes.push(format!("已恢复上次选用的配置: {}", p.display()));
                    (Some(d), p)
                }
                Err(e) => {
                    startup_notes.push(format!(
                        "上次选用的配置无法读取({e}),已改回默认配置: {}",
                        default_profile_path.display()
                    ));
                    let (d, notes) = load_profile();
                    startup_notes.extend(notes);
                    (d, default_profile_path)
                }
            },
            // 首次运行,或上次用的就是默认位置
            _ => {
                let (d, notes) = load_profile();
                startup_notes.extend(notes);
                (d, default_profile_path)
            }
        };
        let mut doc = doc.unwrap_or_default();
        doc.normalize();
        let mut profile = doc.active_profile().cloned().unwrap_or_default();
        // 外观(配色/密度/背景图)另有一份"程序自用"的缓存,与键位配置同目录。
        // 有了它,即使没点过[保存配置],重启后外观也保持上次调好的样子。
        let (cached_look, look_notes) = load_look_cache();
        startup_notes.extend(look_notes);
        if let Some(cached) = cached_look {
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
            recoil_gear_req: None,
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
        let swallow_flag = capture
            .as_ref()
            .map(|c| c.swallow.clone())
            .unwrap_or_else(|| Arc::new(0.into()));
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
        let hook_lag_flag = capture
            .as_ref()
            .map(|c| c.hook_lag.clone())
            .unwrap_or_else(|| Arc::new(false.into()));
        let hb_flag = capture
            .as_ref()
            .map(|c| c.hb_state.clone())
            .unwrap_or_else(|| Arc::new(0.into()));
        let hook_ok_flag = capture
            .as_ref()
            .map(|c| c.hook_ok.clone())
            .unwrap_or_else(|| Arc::new(0.into()));

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
        // 手动 adb 命令:命令栏恢复上次内容;用户预设从独立文件读
        // (读不动按空处理并提示,不能因为一个坏文件起不来)
        let (adb_user_presets, adb_presets_err) =
            match adbcmd::load_user_presets(&adb_presets_path()) {
                Ok(list) => (list, None),
                Err(e) => (Vec::new(), Some(e)),
            };
        // 宏草稿库同理:读不动就按空处理并提示,不能因为一个坏文件起不来
        let (macro_drafts, macro_drafts_err) = match load_macro_drafts(&macro_drafts_path()) {
            Ok(list) => (list, None),
            Err(e) => (Vec::new(), Some(e)),
        };

        let mut app = Self {
            shared,
            grab_flag,
            swallow_flag,
            mouse_grab_flag,
            mouse_captured_prev: false,
            cursor_hide_flag,
            cursor_hidden_prev: false,
            mouse_found_flag,
            hook_lag_flag,
            hb_flag,
            hook_ok_flag,
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
            reconnect_armed: false,
            reconnect_attempts: 0,
            reconnect_due: None,
            stream_fps: None,
            waiting_key: None,
            waiting_keys: None,
            capture_down: Vec::new(),
            capture_seen: Vec::new(),
            swallowed_buttons: Vec::new(),
            picking: None,
            resizing: None,
            easing_edit: None,
            draft_active: false,
            shot: None,
            shot_lum: None,
            shot_rx: None,
            shot_zoom: 1.0,
            shot_zoom_auto: true,
            shot_window_open: false,
            shot_window_pin: true,
            shot_last_base: 1.0,
            shot_popup_base: 1.0,
            shot_window_header_h: 0.0,
            shot_window_virtual: false,
            macro_shot_height: MACRO_SHOT_DEFAULT_H,
            audio_rx: None,
            overlay_filter: OverlayFilter::default(),
            right_tab: RightTab::Keys,
            left_tab: LeftTab::default(),
            style: ui_style,
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
            macro_page_tail_delay_ms: crate::keymap::DEFAULT_TAIL_DELAY_MS,
            macro_idle_ms: 800,
            macro_show_events: false,
            macro_show_recording_steps: false,
            macro_expanded: None,
            macro_drafts,
            macro_draft_sel: None,
            macro_draft_name: String::new(),
            macro_draft_save_open: false,
            macro_draft_msg: None,
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
            debug_size_at: None,
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
            adb_bar: adbcmd::split_tokens(saved.adb_command.trim()),
            adb_add_input: String::new(),
            adb_msg: None,
            adb_helper: None,
            adb_user_presets,
            adb_preset_sel: None,
            adb_preset_save_open: false,
            adb_preset_name: String::new(),
            adb_run: None,
            adb_run_started: None,
            adb_last: None,
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
        if let Some(e) = adb_presets_err {
            app.log(format!("adb 预设读取失败({e}),已按空列表处理"));
        }
        if let Some(e) = macro_drafts_err {
            app.log(format!("宏草稿库读取失败({e}),已按空列表处理"));
        }
        // 配置读取阶段的提示(W0-9):读不动/解析失败必须让用户看到,
        // 且要说清"原文件留了备份" —— 否则用户只会觉得"我的配置自己没了"。
        for note in startup_notes {
            app.log(note);
        }
        // 钩子安装结果(W0-11):装失败以前只写诊断日志,用户只看到"按了没反应"。
        // bits=0 既可能是"没装上",也可能是"等结果超时",文案两者都要覆盖。
        #[cfg(windows)]
        {
            let bits = app.hook_ok_flag.load(Ordering::Relaxed);
            if !crate::capture::hook_bits_keyboard_ok(bits) {
                app.log(
                    "⚠ 键盘捕获不可用:低级键盘钩子没有装上(或状态未知),按键映射将完全没有反应。\
                     常见原因:安全软件拦截、系统资源不足。详见运行日志",
                );
            }
            if !crate::capture::hook_bits_mouse_ok(bits) {
                app.log(
                    "⚠ 鼠标捕获不可用:低级鼠标钩子没有装上(或状态未知),鼠标按键与 FPS 瞄准将没有反应。\
                     详见运行日志",
                );
            }
        }
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

    /// 自动重连的最大尝试次数。
    ///
    /// 退避节拍前快后慢(1/1/2/3/5/8/10/10s):绝大多数断开是瞬时的(设备端
    /// server 被杀、adb 抖动),一秒左右就能回来,不该让玩家干等两位数秒;
    /// 后段放慢到 10s 封顶 —— 设备真的掉线时,也不至于无限每 10 秒刷一串
    /// adb 进程。到顶后停下来给一句提示,把决定权交回用户。
    const RECONNECT_MAX_ATTEMPTS: u32 = 8;

    fn reconnect_delay(attempt: u32) -> Duration {
        let secs = match attempt {
            0 | 1 => 1,
            2 => 2,
            3 => 3,
            4 => 5,
            5 => 8,
            _ => 10,
        };
        Duration::from_secs(secs)
    }

    /// 断开/连接失败后排下一班自动重连;超过上限则停用并提示。
    fn schedule_reconnect(&mut self) {
        if !self.reconnect_armed {
            return;
        }
        if self.reconnect_attempts >= Self::RECONNECT_MAX_ATTEMPTS {
            self.reconnect_armed = false;
            self.reconnect_due = None;
            self.log(format!(
                "自动重连已尝试 {} 次仍未成功,停止重试;设备/数据线恢复后点[连接控制]再试",
                self.reconnect_attempts
            ));
            return;
        }
        self.reconnect_due = Some(Instant::now() + Self::reconnect_delay(self.reconnect_attempts));
    }

    /// 每帧调用:到点了就发起下一次自动重连。
    fn tick_auto_reconnect(&mut self) {
        if !self.reconnect_armed || self.connect_rx.is_some() || self.server.is_some() {
            return;
        }
        let Some(due) = self.reconnect_due else {
            return;
        };
        if Instant::now() < due {
            return;
        }
        self.reconnect_due = None;
        self.reconnect_attempts += 1;
        self.log(format!(
            "自动重连第 {} 次尝试(断开期间按键/摇杆全部失效,连上后按当前按键状态重建触点)",
            self.reconnect_attempts
        ));
        self.connect_control();
    }

    fn connect_control(&mut self) {
        // 发起连接 = 用户想要连接(自动重连的启用条件)。尝试计数由成功/失败
        // 路径管理:成功清零;[连接控制]按钮先清零,自动路径靠它累计退避。
        self.reconnect_armed = true;
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
            lock_shared(&self.shared).control = None;
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
        lock_shared(&self.shared).control = None;
        self.server = None;
        // 手动断开 = 明确表达"不要再连":取消自动重连排程,并丢弃可能在途的
        // 连接尝试(rx 一丢,后台线程产出的 server 对象会随之析构,它拉起的
        // adb 子进程会被带走 —— 不会留下抢端口的孤儿)。
        self.reconnect_armed = false;
        self.reconnect_attempts = 0;
        self.reconnect_due = None;
        self.connect_rx = None;
        self.log("已断开控制通道");
    }

    /// 当前是否有可取消的后台任务或交互操作。
    fn pending_task_label(&self) -> Option<&'static str> {
        let label = if self.connect_rx.is_some() {
            Some("连接")
        } else if self.shot_rx.is_some() {
            Some("截图")
        } else if self.space_rx.is_some() {
            // 注意:调试刷新(debug_rx)故意**不在**这里 —— 它每秒都会在途一次,
            // 把它算作"可取消任务"会让右上角红色按钮每秒弹出一次(用户反馈:
            // 显示调试信息时按钮疯狂闪烁)。调试刷新本来就短,不需要取消。
            Some("坐标刷新")
        } else if self.audio_rx.is_some() {
            Some("音频唤醒")
        } else if self.loginfo_rx.is_some() {
            Some("日志收集")
        } else if self.macro_recording.is_some() {
            Some("宏录制")
        } else if self.picking.is_some() {
            Some("取点")
        }
        // 按键捕获**不在这里**(用户 2026-10-10 第 1 条):武装捕获时顶栏不再出现
        // [取消按键捕获]。原因两条 ——
        //   ① 它离捕获现场太远,用户要先在面板上找到它、再把鼠标移过去;
        //   ② 更要命的是**点它的那一下本身就是一次鼠标按下**,而输入事件在同一帧
        //      的界面绘制**之前**处理,于是"取消"这个动作会先把鼠标键录成要绑的键
        //      (组合键甚至会在松开时直接落定成绑定)。用户实测:点[取消按键捕获]后
        //      鼠标消隐被绑到左键,再删掉那个键就"左键即消隐"。
        // 退出捕获一律用就地那一个[取消设置](`cancel_bind_button`),它按**下**即生效
        // 且位置就在捕获控件旁边;捕获期间落在它上面的按下还会被整体丢掉
        // (见 `note_cancel_zone` / `swallow_cancel_click`)。
        else if self.resizing.is_some() {
            Some("范围修改")
        } else if self.easing_edit.is_some() {
            Some("曲线编辑")
        } else {
            None
        };
        // 顶栏那个[取消…]按钮按 `PENDING_TASK_LABELS` 里**最长**的一条预留固定宽度
        // (见 `cancel_button_slot_width`),所以这里每出现一种新标签都要能对上号,
        // 否则预留的槽位装不下、会被截断。debug 下每帧顺手校一次,release 里没有代价。
        debug_assert!(
            label.is_none_or(|l| PENDING_TASK_LABELS.contains(&l)),
            "新的等待标签 {label:?} 没同步进 PENDING_TASK_LABELS"
        );
        label
    }

    /// 顶栏[取消…]按钮的固定槽宽:按 [`PENDING_TASK_LABELS`] 里最长的一条量。
    ///
    /// 用字体实际排版测量(不是拍一个常数),这样换字体/换字号也不会量错。
    fn cancel_button_slot_width(ui: &egui::Ui) -> f32 {
        let font = egui::TextStyle::Button.resolve(ui.style());
        let painter = ui.painter();
        let widest = PENDING_TASK_LABELS
            .iter()
            .map(|l| {
                painter
                    .layout_no_wrap(format!("取消{l}"), font.clone(), egui::Color32::WHITE)
                    .size()
                    .x
            })
            .fold(0.0_f32, f32::max);
        // 按钮内边距 + 描边,免得最宽的那条刚好被切掉
        widest + 2.0 * ui.spacing().button_padding.x + 4.0
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
        self.waiting_keys = None;
        self.capture_down.clear();
        self.capture_seen.clear();
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
        {
            // 中毒也要把锁拿回来(lock_shared):退出时必须尽力停掉映射、断开控制
            let mut g = lock_shared(&self.shared);
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
        let mode = lock_shared(&self.shared).profile.aim.input_mode;
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
        // R4(2026-10-08):扩展宏弹窗的虚拟取键写的是宏草稿那一层 —— 既不碰实时配置,
        // 也不该在撤销栈上留一步空操作,所以先于 `push_undo()` 处理。
        // `waiting_key` 在这里清掉:与主界面的两处调用点(`take()` 之后才调
        // `assign_key`)同一条规矩 —— 一次等待只接一个键。
        if slot.is_virtual() {
            self.waiting_key = None;
            if let Some(msg) = self.assign_virtual_key(slot, code) {
                self.log(msg);
            }
            return;
        }
        // 组合键槽位(用户 2026-10-09 第 4 条):单个键也走同一个落定入口 ——
        // "一个键"就是"只含一个键的集合",两种捕获模式最后落在同一处写配置。
        if slot.takes_chord() {
            self.assign_keys(slot, KeySet::single(code));
            return;
        }
        self.push_undo();
        // 切换键走 `assign_keys`(它属于"组合表结构",改完自动落盘,见
        // sync_scheme_state);这里剩下的都是普通键位/摇杆/宏草稿的单个键码槽,
        // 不在"改完即落盘"之列 —— 拖动圆圈/连续改键太频繁,由 [保存配置] 决定何时写。
        {
            let mut g = lock_shared(&self.shared);
            match slot {
                KeySlot::NewBind => self.draft.key = Some(code),
                KeySlot::Bind(i) => {
                    if let Some(b) = g.profile.binds.get_mut(i) {
                        b.key = code;
                    }
                }
                // 组合键槽位(总开关/消隐/FPS 三键/压枪触发/切换键/轮盘方向键/
                // 临时轮盘启用键)在上面的 `takes_chord()` 分支就返回了,不会走到这里。
                KeySlot::Toggle
                | KeySlot::CursorToggle
                | KeySlot::AimHold
                | KeySlot::AimToggle
                | KeySlot::AimSuspend
                | KeySlot::RecoilTrigger
                | KeySlot::RecoilSwitch
                | KeySlot::SwitchKey(_)
                | KeySlot::WheelDir { .. }
                | KeySlot::WheelEnable(_) => {}
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
                // R4:扩展宏弹窗的虚拟取键已在函数开头单独处理(写宏草稿,不碰 Shared),
                // 这里只为穷尽枚举。
                KeySlot::MacroVirtualNewBind
                | KeySlot::MacroVirtualComboKey { .. }
                | KeySlot::MacroVirtualWheelDir { .. }
                | KeySlot::MacroVirtualWheelEnable(_) => {}
            }
        }
        self.log(format!("键位已绑定: {}", key_name(code)));
    }

    /// 组合键槽位的落定入口(用户 2026-10-09 第 4 条)。
    ///
    /// 单键与组合键同一条路:单个键 = 只含一个键的集合 —— 于是"只允许单键的
    /// 键位也套用这套逻辑"(用户原话),不需要两套写法。
    /// **不给任何时长/间隔参数**:这类键就是"这几个键一起按住",
    /// 与用户同一条要求里的限制一致。
    fn assign_keys(&mut self, slot: KeySlot, keys: KeySet) {
        self.push_undo();
        // 切换键属于"组合表结构",改完自动落盘(见 sync_scheme_state)。
        let mut switch_key_touched = false;
        let mut clobbered = false;
        {
            let mut g = lock_shared(&self.shared);
            match slot {
                KeySlot::Toggle => g.profile.toggle_key = keys,
                KeySlot::CursorToggle => g.profile.cursor_toggle_key = keys,
                KeySlot::AimHold => g.profile.aim.hold_key = keys,
                KeySlot::AimToggle => g.profile.aim.toggle_key = keys,
                KeySlot::AimSuspend => g.profile.aim.suspend_key = keys,
                KeySlot::RecoilTrigger => g.profile.aim.recoil.trigger_key = keys,
                KeySlot::RecoilSwitch => g.profile.aim.recoil.switch_key = keys,
                KeySlot::WheelDir { wheel, dir } => {
                    if let Some(w) = g.profile.wheels.get_mut(wheel) {
                        if w.kind == WheelKind::Standard {
                            match dir {
                                0 => w.up = keys,
                                1 => w.down = keys,
                                2 => w.left = keys,
                                _ => w.right = keys,
                            }
                        } else if let Some(d) = w.directions.get_mut(dir) {
                            d.key = keys;
                        }
                    }
                }
                KeySlot::WheelEnable(i) => {
                    if let Some(w) = g.profile.wheels.get_mut(i) {
                        // 与单键时代同一条规则:设了启用键 = 变成临时摇杆,
                        // 模式沿用原有的(默认长按)。清成空集合 = 撤销启用键,
                        // 退回永久轮盘(「取消设置」走的就是这一路)。
                        let mode = w.temp.as_ref().map(|t| t.mode).unwrap_or(TempMode::Hold);
                        if keys.is_empty() {
                            w.temp = None;
                        } else {
                            w.temp = Some(TempWheel { key: keys, mode });
                        }
                    }
                }
                KeySlot::SwitchKey(i) => {
                    if let Some(s) = g.switch_keys.get_mut(i) {
                        // 旧字段同步写一份:老版本读的是 `key`,让它至少看到单键值。
                        s.key = keys.only().unwrap_or(0);
                        s.keys = keys;
                    }
                    // 两行切换键共用任何一个键都会让归属变得说不清(引擎只认先命中的
                    // 那行),这里顺手把其余有交叠的行清成"未绑定",免得看着像生效了
                    // 其实没有。清空本身不清别人(未绑定不参与匹配)。
                    for (j, s) in g.switch_keys.iter_mut().enumerate() {
                        if j == i || keys.is_empty() {
                            continue;
                        }
                        let other = s.effective_keys();
                        if other.iter().any(|k| keys.contains(k)) {
                            s.key = 0;
                            s.keys = KeySet::new();
                            clobbered = true;
                        }
                    }
                    switch_key_touched = true;
                }
                _ => {}
            }
        }
        if switch_key_touched {
            self.scheme_dirty = true;
        }
        if clobbered {
            self.log("同名的切换键行已清空(两行共用一个键会说不清谁生效)");
        }
        let label = if keys.is_empty() {
            "未绑定".to_string()
        } else {
            keys.label()
        };
        self.log(format!("键位已绑定: {label}"));
    }

    /// 进入**组合键**捕获(见 [`Self::keys_button`])。
    ///
    /// 与单键捕获互斥:两处都武装的话,同一次按键会被两条路各处理一遍。
    /// 进入时清空上一轮的按键记录 —— 否则上一次没按完的半个组合会粘进来。
    fn begin_keys_capture(&mut self, slot: KeySlot) {
        self.picking = None;
        self.resizing = None;
        self.waiting_key = None;
        self.capture_down.clear();
        self.capture_seen.clear();
        self.waiting_keys = Some(slot);
        self.log("请按下组合键:按住 Ctrl 再按另一个键(只按一个键就是单键);全部松开即生效");
    }

    /// 进入单键捕获(按下的那一个键立即落定)。
    fn begin_key_capture(&mut self, slot: KeySlot) {
        self.picking = None;
        self.resizing = None;
        self.waiting_keys = None;
        self.capture_down.clear();
        self.capture_seen.clear();
        self.waiting_key = Some(slot);
        self.log("按任意键完成改绑(或点[取消选择]退出)");
    }

    /// 退出所有按键捕获(单键与组合键一起清,不留下"举着等按键"的半状态)。
    fn cancel_key_capture(&mut self) {
        self.waiting_key = None;
        self.waiting_keys = None;
        self.capture_down.clear();
        self.capture_seen.clear();
    }

    /// 截图取点后写入坐标(入参为像素,配置里存相对值)
    fn assign_coord(&mut self, slot: CoordSlot, x: i32, y: i32) {
        // R4(2026-10-08):扩展宏弹窗的取点只写弹窗自己那份虚拟键位表(宏草稿),
        // 与实时配置无关,也不该在撤销栈上留一步空操作 —— 先于 `push_undo()` 返回。
        if let CoordSlot::MacroVirtual(pick) = slot {
            self.assign_virtual_coord(pick, x, y);
            return;
        }
        self.push_undo();
        let m = self.mapper();
        // 宏页的落点写在页面草稿状态里(不碰 Shared):单独先处理,
        // 免得与下面 `lock_shared(&self.shared)` 的借用范围纠缠。
        match slot {
            CoordSlot::MacroClickPoint(i) => {
                if let Some(MacroInstruction::Click { x: ax, y: ay, .. }) =
                    self.macro_page_instructions.get_mut(i)
                {
                    *ax = m.rel_x(x);
                    *ay = m.rel_y(y);
                }
                self.log(format!("坐标已设置: ({x}, {y})"));
                return;
            }
            CoordSlot::MacroSwipeStart(i) => {
                if let Some(MacroInstruction::Swipe {
                    start_x: ax,
                    start_y: ay,
                    ..
                }) = self.macro_page_instructions.get_mut(i)
                {
                    *ax = m.rel_x(x);
                    *ay = m.rel_y(y);
                }
                self.log(format!("坐标已设置: ({x}, {y})"));
                return;
            }
            CoordSlot::MacroSwipeEnd(i) => {
                if let Some(MacroInstruction::Swipe {
                    end_x: ax,
                    end_y: ay,
                    ..
                }) = self.macro_page_instructions.get_mut(i)
                {
                    *ax = m.rel_x(x);
                    *ay = m.rel_y(y);
                }
                self.log(format!("坐标已设置: ({x}, {y})"));
                return;
            }
            _ => {}
        }
        {
            let mut g = lock_shared(&self.shared);
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
                CoordSlot::WheelDirEnd { wheel, dir } => {
                    // 存**相对坐标**(与圆心同一坐标系),与屏幕尺寸/方向无关。
                    // 注意这里只写 manual:角度与影响范围都保持原样 ——
                    // 影响范围从此只是基准,改它不会挪动这个手改点。
                    if let Some(d) = g
                        .profile
                        .wheels
                        .get_mut(wheel)
                        .and_then(|w| w.directions.get_mut(dir))
                    {
                        d.manual = Some((m.rel_x(x), m.rel_y(y)));
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
                // 宏页落点已在函数开头单独处理(写在页面草稿里,不碰 Shared),
                // 这里只为穷尽枚举。
                CoordSlot::MacroClickPoint(_)
                | CoordSlot::MacroSwipeStart(_)
                | CoordSlot::MacroSwipeEnd(_) => {}
                // R4:扩展宏弹窗的取点同样已在函数开头单独处理(写宏草稿,不碰 Shared)。
                CoordSlot::MacroVirtual(_) => {}
            }
        }
        self.log(format!("坐标已设置: ({x}, {y})"));
    }

    /// R4(2026-10-08):把一次截图取点写进**扩展宏弹窗**的虚拟键位表。
    ///
    /// 与 [`Self::assign_coord`] 的实时配置分支同一个换算口径(都用 `self.mapper()`,
    /// 即按当前配置的坐标系与屏幕尺寸),区别只有一个:目标是弹窗里那份
    /// `MacroVirtualEditor.profile`(宏草稿)。所以这里**不推送撤销**——
    /// 宏草稿的撤销语义就是弹窗自己的[取消](丢弃整份副本),往实时撤销栈里塞一步
    /// "什么都没改"的记录只会污染主界面的撤销历史。
    ///
    /// 弹窗已经关了(理论上到不了:关窗时会清掉虚拟取点,这里只是兜底)则什么都不做。
    fn assign_virtual_coord(&mut self, pick: MacroVirtualPick, x: i32, y: i32) {
        let m = self.mapper();
        let (rx, ry) = (m.rel_x(x), m.rel_y(y));
        {
            let Some(editor) = self.macro_virtual_editor.as_mut() else {
                return;
            };
            let profile = &mut editor.profile;
            match pick {
                MacroVirtualPick::Bind(i) => {
                    if let Some(b) = profile.binds.get_mut(i) {
                        if let Action::Tap { x: ax, y: ay, .. }
                        | Action::Hold { x: ax, y: ay, .. } = &mut b.action
                        {
                            *ax = rx;
                            *ay = ry;
                        }
                    }
                }
                MacroVirtualPick::Combo(i) => {
                    if let Some(c) = profile.combos.get_mut(i) {
                        if let Action::Tap { x: ax, y: ay, .. }
                        | Action::Hold { x: ax, y: ay, .. } = &mut c.action
                        {
                            *ax = rx;
                            *ay = ry;
                        }
                    }
                }
                MacroVirtualPick::WheelCenter(i) => {
                    if let Some(w) = profile.wheels.get_mut(i) {
                        w.cx = rx;
                        w.cy = ry;
                    }
                }
                MacroVirtualPick::AimAnchor => {
                    profile.aim.anchor_x = rx;
                    profile.aim.anchor_y = ry;
                }
            }
            // 刚落下的这一个就是"最新点":截图浮层给它画一圈强调 + 「新」角标
            // (用户 2026-10-10 第 4 条"每次新增点需标明")。
            editor.newest = Some(pick);
        }
        self.log(format!("扩展宏虚拟键位坐标已设置: ({x}, {y})"));
    }

    /// R4(2026-10-08):扩展宏弹窗的"点虚拟键盘选键"。
    ///
    /// `slot` 必须是 [`KeySlot::is_virtual`] 的取值(调用方 [`Self::assign_key`] 已判)。
    /// 返回 `Some(消息)` 表示这次按键已经写进宏草稿(或有意被吞掉),调用方不要再走
    /// 实时配置的分支;返回 `None` 表示弹窗已经不在了(兜底,写进哪里都不合适)。
    fn assign_virtual_key(&mut self, slot: KeySlot, code: u16) -> Option<String> {
        // 一次等待只接一个键 —— 与 [`App::assign_key`] 开头的同一条规矩。
        // 弹窗里的虚拟键盘是**直接**调本函数的(不经 `assign_key`),所以这里必须
        // 自己清:否则"等待按键"的横幅会一直挂着,而且此后**任何**一次物理按键
        // 都会顺着 `assign_key` 再往宏草稿里悄悄新建/改写一个虚拟键位。
        self.waiting_key = None;
        // 「＋ 按键」的第一步:新建一个虚拟键位(占位在屏幕中央,紧接着进入截图取点)。
        // 与弹窗里直接点虚拟键盘的区别就在这:那条路是"先建再自己取点",
        // 这条是主界面同款的"点键 → 取点"连贯流程。
        if slot == KeySlot::MacroVirtualNewBind {
            let idx = {
                let editor = self.macro_virtual_editor.as_mut()?;
                match editor.profile.binds.iter().position(|b| b.key == code) {
                    Some(i) => i,
                    None => {
                        editor.profile.binds.push(KeyBind {
                            key: code,
                            action: Action::Tap {
                                x: 0.5,
                                y: 0.5,
                                duration_ms: crate::keymap::DEFAULT_TAP_DURATION_MS,
                                radius: crate::keymap::DEFAULT_RADIUS,
                            },
                            fps_only: false,
                            tail_delay_ms: crate::keymap::DEFAULT_TAIL_DELAY_MS,
                        });
                        editor.profile.binds.len() - 1
                    }
                }
            };
            if let Some(editor) = self.macro_virtual_editor.as_mut() {
                editor.selected_key = Some(code);
                // 记下"刚新增的是这一个"(截图浮层给它画「新」角标)
                editor.newest = Some(MacroVirtualPick::Bind(idx));
            }
            // 立刻进入取点(与主界面"取点后请按下按键"互为镜像的半步)。
            // `begin_pick` 自己会做坐标空间对齐检查并在失败时说明原因。
            if self.begin_pick(CoordSlot::MacroVirtual(MacroVirtualPick::Bind(idx))) {
                return Some(format!(
                    "虚拟键位已加入: {} —— 请点击截图选点",
                    key_name(code)
                ));
            }
            return Some(format!("虚拟键位已加入: {} (暂未进入取点)", key_name(code)));
        }
        let editor = self.macro_virtual_editor.as_mut()?;
        match slot {
            KeySlot::MacroVirtualComboKey { combo, slot } => {
                if let Some(c) = editor.profile.combos.get_mut(combo) {
                    if slot < c.keys.len() {
                        c.keys[slot] = code;
                    } else {
                        c.keys.push(code);
                    }
                }
            }
            KeySlot::MacroVirtualWheelDir { wheel, dir } => {
                if let Some(w) = editor.profile.wheels.get_mut(wheel) {
                    // 弹窗的取键是"点虚拟键盘上的一个键",天然只能给单键;
                    // 存进去的是单元素集合 —— 与主界面的组合键共用同一份存储。
                    let keys = KeySet::single(code);
                    if w.kind == WheelKind::Standard {
                        match dir {
                            0 => w.up = keys,
                            1 => w.down = keys,
                            2 => w.left = keys,
                            _ => w.right = keys,
                        }
                    } else if let Some(d) = w.directions.get_mut(dir) {
                        d.key = keys;
                    }
                }
            }
            KeySlot::MacroVirtualWheelEnable(i) => {
                if let Some(w) = editor.profile.wheels.get_mut(i) {
                    // 与主界面同一条规则:设启用键 = 变成临时摇杆,模式沿用原有(默认长按)。
                    let mode = w.temp.as_ref().map(|t| t.mode).unwrap_or(TempMode::Hold);
                    w.temp = Some(TempWheel {
                        key: KeySet::single(code),
                        mode,
                    });
                }
            }
            _ => {}
        }
        Some(format!("扩展宏虚拟键位已绑定: {}", key_name(code)))
    }

    /// R4(2026-10-08):落地一条 [`MacroVirtualAct`](弹窗清单里点出来的操作)。
    ///
    /// 只在弹窗**还开着**的那一帧调用(见 `ui_macro_virtual_window` 的收尾):
    /// 关窗/取消时这些请求已经没有意义,直接丢掉 —— 这也是"不留键位垃圾"的一半。
    fn apply_virtual_act(&mut self, act: MacroVirtualAct) {
        match act {
            MacroVirtualAct::PickBind(i) => {
                self.wheel_info = None; // 收起旧的摇杆信息卡,免得和虚拟层混在一起
                self.waiting_key = None;
                self.begin_pick(CoordSlot::MacroVirtual(MacroVirtualPick::Bind(i)));
            }
            MacroVirtualAct::PickComboPoint(i) => {
                self.wheel_info = None;
                self.waiting_key = None;
                self.begin_pick(CoordSlot::MacroVirtual(MacroVirtualPick::Combo(i)));
            }
            MacroVirtualAct::PickWheelCenter(i) => {
                self.wheel_info = None;
                self.waiting_key = None;
                self.begin_pick(CoordSlot::MacroVirtual(MacroVirtualPick::WheelCenter(i)));
            }
            MacroVirtualAct::PickAimAnchor => {
                self.wheel_info = None;
                self.waiting_key = None;
                self.begin_pick(CoordSlot::MacroVirtual(MacroVirtualPick::AimAnchor));
            }
            MacroVirtualAct::TakeKey(slot) => {
                self.picking = None;
                self.resizing = None;
                self.waiting_key = Some(slot);
            }
            MacroVirtualAct::RemoveCombo(i) => {
                if let Some(editor) = self.macro_virtual_editor.as_mut()
                    && i < editor.profile.combos.len()
                {
                    editor.profile.combos.remove(i);
                    // 删一项之后下标全体前移,旧的「新」角标会指错人
                    editor.newest = None;
                }
                self.clear_virtual_pick();
                self.log("已删除虚拟组合键");
            }
            MacroVirtualAct::RemoveWheel(i) => {
                if let Some(editor) = self.macro_virtual_editor.as_mut()
                    && i < editor.profile.wheels.len()
                {
                    editor.profile.wheels.remove(i);
                    editor.newest = None;
                }
                self.wheel_info = None;
                self.clear_virtual_pick();
                self.log("已删除虚拟轮盘");
            }
        }
    }

    /// 清掉「扩展宏弹窗」的一次待完成操作(虚拟取点 / 虚拟取键)。
    ///
    /// 三个时必须走这里:关窗(含保存与取消)、[取消取点](或"取消选键")、
    /// 删掉正被指向的那一项。清干净之后截图浮层下一帧就回到实时配置 ——
    /// 这正是用户要求的"设置完毕或取消设置后,截图处标识回到操作之前的样子"。
    /// 实时配置的 `picking`/`waiting_key` 一律不动(那是主界面的活儿)。
    fn clear_virtual_pick(&mut self) {
        if self.picking.map(CoordSlot::is_virtual).unwrap_or(false) {
            self.picking = None;
        }
        if self.waiting_key.map(KeySlot::is_virtual).unwrap_or(false) {
            self.waiting_key = None;
        }
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

    /// 组合键按钮(用户 2026-10-09 第 4 条):**一个**按钮捕获并显示一组键。
    ///
    /// 显示形如 `Ctrl+X`;未绑定显示"未绑定";等待输入显示"按下组合键..."。
    /// 捕获规则:按下的键全部记下(最多两个),**全部松开**时落定 ——
    /// 所以"按住 Ctrl 再按 X"与"按住 X 再按 Ctrl"都能得到同一个组合。
    fn keys_button(ui: &mut egui::Ui, waiting: bool, keys: &KeySet) -> egui::Response {
        let label = if waiting {
            "按下按键...".to_string()
        } else if keys.is_empty() {
            "未绑定".to_string()
        } else {
            keys.label()
        };
        ui.add(egui::Button::new(label).min_size(egui::vec2(110.0, 0.0)))
            .on_hover_text(
                "点一下开始捕获:按住 Ctrl 再按另一个键就是一个组合键(最多两个键);\
                 按顺序无关 —— Ctrl+X 与 X+Ctrl 都生效。\n\
                 只按一个键就是单键绑定。滚轮不能绑在这里。",
            )
    }

    /// 把一个「就地取消 / 停止」控件的位置登记下来,供**下一帧**处理输入时使用。
    ///
    /// 为什么需要(用户 2026-10-10 第 1 条):捕获 / 录制期间,用户点"取消"的那一下
    /// 本身也是一次鼠标按键事件,而 `gui_rx` 里的输入是在同一帧界面绘制**之前**
    /// 处理的 —— 等界面画到取消按钮、`clicked()` 亮起来时,这一下可能已经
    ///   ① 记进了 `capture_down`/`capture_seen`(组合键还会在松开时直接落定成绑定);
    ///   ② 被宏录制记成一步。
    /// 所以把上一帧取消控件的位置存进 egui 临时存储,处理输入时看到落在这些矩形里的
    /// **按下就整体丢掉**,连记都不记(见 [`App::swallow_cancel_click`])。
    ///
    /// 每帧开头会清空重填(`ui()` 里),于是输入阶段读到的一定是**上一帧**的位置
    /// —— 也正是"用户当下看到的那一版界面"。
    fn note_cancel_zone(ui: &egui::Ui, r: &egui::Response) {
        ui.memory_mut(|m| {
            let mut zones = m
                .data
                .get_temp::<Vec<egui::Rect>>(cancel_zones_id())
                .unwrap_or_default();
            zones.push(r.rect);
            m.data.insert_temp(cancel_zones_id(), zones);
        });
    }

    /// 这一条输入事件是不是"按在就地取消/停止控件上"的那一下,该整体丢掉。
    ///
    /// 按下丢掉之后,它的**抬起**也要一起丢(`swallowed_buttons`)— 半吞会留下
    /// "没按过就抬起"的孤儿事件:组合键捕获会因为"全部松开"而落定一个空组合,
    /// 宏录制会多记一步无效动作。
    ///
    /// 只看鼠标键([`crate::keymap::is_mouse_button`]):能被"按在按钮上"的只有鼠标,
    /// 键盘键永远不该因此被吞。
    fn swallow_cancel_click(&mut self, ctx: &egui::Context, code: u16, pressed: bool) -> bool {
        if !crate::keymap::is_mouse_button(code) {
            return false;
        }
        if !pressed {
            // 抬起:只有当初按下被吞过的那一下才跟着丢。
            return if let Some(i) = self.swallowed_buttons.iter().position(|k| *k == code) {
                self.swallowed_buttons.remove(i);
                true
            } else {
                false
            };
        }
        let Some(pos) = ctx.input(|i| i.pointer.interact_pos()) else {
            return false;
        };
        let hit = ctx.memory(|m| {
            m.data
                .get_temp::<Vec<egui::Rect>>(cancel_zones_id())
                .unwrap_or_default()
                .iter()
                .any(|r| r.contains(pos))
        });
        if hit {
            self.swallowed_buttons.push(code);
        }
        hit
    }

    /// 「取消设置」按钮(用户 2026-10-10 第 3 条)。
    ///
    /// 风味要求"有始必有终":点了绑定按钮就开始等键,必须有同样显眼的出口。
    /// 原来的 [清除] 只在**已绑定**时出现 —— 于是"点错了想退出捕获"这一路没有
    /// 按钮可点,只能去点别处。现在这一个按钮在两种状态下都在,一次点击把两件事
    /// 都收干净:等待捕获中 = 取消这次捕获(**不动已有的绑定**);已有绑定 =
    /// 清掉这个绑定。调用方据此决定"只取消"还是"取消 + 清空"。
    ///
    /// 返回是否被点击。悬停提示按当前状态给,避免同一个按钮说两件事。
    fn cancel_bind_button(ui: &mut egui::Ui, waiting: bool) -> bool {
        let r = ui.small_button("取消设置").on_hover_text(if waiting {
            "取消这次按键捕获;已有的绑定保持不变"
        } else {
            "清掉这个绑定(恢复“未绑定”)"
        });
        Self::note_cancel_zone(ui, &r);
        // **按下**即生效,不等松开(用户 2026-10-10 第 1 条)。等松开的话,这一下
        // 点击的"抬起"边沿会先把组合键落定成绑定,取消就晚了半拍 —— 正是用户
        // 遇到的那个"取消了却把鼠标左键绑上去了"。
        r.is_pointer_button_down_on() || r.clicked()
    }

    /// "浏览器标签页"式的方形标签按钮(不立体、大小不变)。
    /// 右栏四标签页与可视化风格的操作栏按钮统一用它,观感一致。
    ///
    /// 未选中态以前是完全透明、无描边，只靠文字颜色暗示可点击；现在用
    /// 强调色做低透明度底色 + 半透明描边，既与背景有区分，又不会像实心
    /// 按钮一样抢视觉。底色/边框都只改变 alpha，暗色与浅色主题共用一套做法。
    fn tab_button(ui: &mut egui::Ui, label: &str, selected: bool, accent: egui::Color32) -> bool {
        Self::tab_button_resp(ui, label, selected, accent).clicked()
    }

    /// 方形标签按钮的 [`egui::Response`](不判点击)。
    ///
    /// 只在需要按钮**矩形**的场合用:那就是"就地停止 / 取消"类按钮 —— 它们的位置
    /// 要登记进取消区,好让按下它的那一下不被录进捕获/录制(`note_cancel_zone`)。
    fn tab_button_resp(
        ui: &mut egui::Ui,
        label: &str,
        selected: bool,
        accent: egui::Color32,
    ) -> egui::Response {
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
    }

    // ===================== 撤销 / 重做 =====================

    /// 把当前配置压入撤销栈(所有键位修改入口调用),并清空重做栈
    fn push_undo(&mut self) {
        let profile = lock_shared(&self.shared).profile.clone();
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
            let current = lock_shared(&self.shared).profile.clone();
            self.redo_stack.push(current);
            lock_shared(&self.shared).profile = prev;
            self.log("已撤销");
        }
    }

    fn redo(&mut self) {
        if let Some(next) = self.redo_stack.pop() {
            let current = lock_shared(&self.shared).profile.clone();
            self.undo_stack.push(current);
            lock_shared(&self.shared).profile = next;
            self.log("已重做");
        }
    }

    /// 把 `Shared` 里的实时状态收拢成一份可落盘的配置文档。
    ///
    /// 关键一步:`profile`(引擎眼里"此刻生效的那套")要先写回 `schemes[active_scheme]`
    /// —— 界面上的改动都直接改 `profile`,组合表本身是"存档"。
    fn config_doc(&self) -> ConfigFile {
        let mut g = lock_shared(&self.shared);
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
            Ok(text) => match write_atomic(&self.profile_path, &text) {
                Ok(_) => {
                    self.scheme_saved = self.active_scheme();
                    self.scheme_dirty = false;
                    self.log(format!("已保存到 {}", self.profile_path.display()))
                }
                Err(e) => self.log(format!("保存失败: {e}")),
            },
            Err(e) => self.log(format!("序列化失败: {e}")),
        }
    }

    /// 引擎此刻生效的组合下标(界面用它标记当前项)
    fn active_scheme(&self) -> usize {
        lock_shared(&self.shared).active_scheme
    }

    /// 把一份文档整体装进 `Shared`:组合表、切换键表、生效下标,
    /// 以及引擎真正读的那份 `profile`。
    fn install_config(&self, doc: &ConfigFile) {
        let mut g = lock_shared(&self.shared);
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
    /// 成功时不记日志:开打中按一下切换键不该在日志区刷屏。
    /// **失败必须记**(W0-9):以前这里是 `let _ =`,磁盘满/文件只读时
    /// "组合换了、文件没换"悄无声息,用户下次启动才发现配置对不上。
    fn sync_scheme_state(&mut self) {
        let active = {
            let mut g = lock_shared(&self.shared);
            g.stash_active();
            g.active_scheme
        };
        if active == self.scheme_saved && !self.scheme_dirty {
            return;
        }
        self.scheme_saved = active;
        self.scheme_dirty = false;
        match render_config(&self.config_doc()) {
            Ok(text) => {
                if let Err(e) = write_atomic(&self.profile_path, &text) {
                    self.log(format!(
                        "自动保存失败({e}),本次切换只在本次运行内有效: {}",
                        self.profile_path.display()
                    ));
                }
            }
            Err(e) => self.log(format!("序列化失败: {e}")),
        }
    }

    /// 手动切换生效的组合(左侧面板选中某套)。切换要先抬起旧组合的触点,
    /// 这由引擎的 `sync_structures` 兜底(结构指纹变了就抬干净再重建),
    /// 所以这里只换数据、不动引擎。
    fn select_scheme(&mut self, idx: usize) {
        if !lock_shared(&self.shared).select_scheme(idx) {
            return;
        }
        self.undo_stack.clear();
        self.redo_stack.clear();
        let name = lock_shared(&self.shared).profile.name.clone();
        self.log(format!("已切换到按键组合「{name}」"));
    }

    /// 新建一套组合(复制当前这套)并切过去
    fn add_scheme(&mut self) {
        let name = {
            let mut g = lock_shared(&self.shared);
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
            let mut g = lock_shared(&self.shared);
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
                // 已知屏幕尺寸时,顺手校正控制通道坐标空间(与[选用配置]行为一致)
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
        // 换回默认位置也是一次"配置选择",要活过重启(W0-9)
        self.remember_now();
        self.apply_profile_switch(doc);
        // 先取数、释放锁,再 log:避免 format! 参数里两次 lock 死锁
        let (nb, nw) = {
            let g = lock_shared(&self.shared);
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
        // 已知屏幕尺寸时,顺手校正控制通道坐标空间
        if let Some((w, h)) = self.screen_size() {
            self.sync_display_space(w, h);
        }
    }

    /// 进入取点。取点/改范围这类交互同一时刻只保留一个,以最后一次操作为准:
    /// 正在改响应范围时点[取点],就直接转去取点,不再两头挂着。
    ///
    /// W2-2:截图与注入空间纵横比不一致时**拒绝进入取点**(所有[取点]按钮的
    /// 统一入口就在这),提示去截图取点面板按一键对齐处理。返回是否武装成功 ——
    /// 调用方若已为取点先建了半成品(如虚拟键盘的"新建草稿"),失败时要自行撤销。
    fn begin_pick(&mut self, slot: CoordSlot) -> bool {
        self.resizing = None;
        if let Some(m) = self.space_guard() {
            self.log(format!(
                "已阻止取点:{}(取点会落偏)。请到「截图取点」面板按提示对齐坐标空间或重新截图",
                m.describe()
            ));
            return false;
        }
        self.picking = Some(slot);
        true
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
            // 命令栏按 token 空格连接落盘(引号片段保持原样,读回时同样规则拆开)
            adb_command: self.adb_bar.join(" "),
            selected_serial: self.serial(),
            // 日志级别由界面上的下拉框负责写入,这里原样带回上次读到的值,
            // 免得"改一次别的设置"就把用户选的级别冲掉
            log_level: self.settings_loaded.log_level.clone(),
            // 当前在用的配置文件位置(W0-9):它让"另选/新建的配置"活过重启。
            // **不受 [记住路径] 开关管辖** —— 那条开关的语义是"scrcpy 装在哪"
            // (本机环境信息);而"我在编辑哪一份配置"换了却不记得,重启后
            // 打开的是另一份文件、用户会以为改动丢了,属于正确性问题。
            profile_path: Some(self.profile_path.display().to_string()),
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

        let (connected, live, capture_err, qstats) = {
            let g = lock_shared(&self.shared);
            (
                g.control
                    .as_ref()
                    .map(|c| c.is_connected())
                    .unwrap_or(false),
                g.live,
                self.capture_err.clone(),
                // W1-3:控制队列深度快照(只读三个原子量,不碰写线程的锁)
                g.control.as_ref().map(|c| c.queue_stats()),
            )
        };
        let mouse_found = self.mouse_found_flag.load(Ordering::Relaxed);
        // 钩子安装结果(W0-11):bit0=键盘钩子,bit1=鼠标钩子,0=未知。
        //
        // 只有 Windows 装低级钩子,下面那两条自检也在 `#[cfg(windows)]` 里,所以这个
        // 取值在 Linux 上是纯多余的(WSL 的 `cargo check` 会报 unused variable)。
        // **不能注释掉** —— Windows 构建要用。按平台收进 Windows 分支即可。
        #[cfg(windows)]
        let hook_ok = self.hook_ok_flag.load(Ordering::Relaxed);
        let level = crate::diag::level();
        let log_path = crate::diag::path();

        // ---- 逐项自检 ----
        ui.separator();
        ui.label("状态自检:");
        let mut blocker: Option<&str> = None;
        // 下面 `#[cfg(windows)]` 那一块会 push 两条钩子自检;Linux 上不 push,于是这个
        // `mut` 在 Linux 构建里没有用处(WSL 的 `cargo check` 会报 unused_mut)。
        // 同样按平台标注,而不是把 push 注释掉 —— 那样 Windows 会少两条自检。
        #[cfg_attr(not(windows), allow(unused_mut))]
        let mut checks: Vec<(bool, &str)> = vec![
            (
                capture_err.is_none(),
                "输入捕获线程已启动(读得到 /dev/input,或钩子线程已起来)",
            ),
            (mouse_found, "检测到鼠标类设备(有相对位移轴)"),
            (connected, "控制通道已连接"),
            (live.refused == 0, "没有因为触点池满而被拒的按下"),
        ];
        // 这两项只在 Windows 有意义(装的是低级钩子;Linux 走 evdev,失败会走上面的
        // capture_err 分支)。放在这里是为了让"按了没反应"的用户第一眼就看到原因。
        #[cfg(windows)]
        {
            checks.push((
                crate::capture::hook_bits_keyboard_ok(hook_ok),
                "键盘低级钩子已装上(键盘映射可用)",
            ));
            checks.push((
                crate::capture::hook_bits_mouse_ok(hook_ok),
                "鼠标低级钩子已装上(鼠标按键 / FPS 瞄准可用;安装时结果)",
            ));
        }
        for (ok, text) in checks {
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
        if let Some(q) = qstats {
            // 积压/丢弃都说明设备端或转发通道跟不上:平时它一直是 0,
            // 出现数字就该怀疑 adb/设备端卡顿(W1-3)
            let extra = if q.dropped > 0 {
                format!(" · 丢旧保新 {} 条", q.dropped)
            } else {
                String::new()
            };
            ui.label(format!(
                "控制队列 深度 {} / 峰值 {}{}",
                q.depth, q.peak, extra
            ));
        }
        if let Some(b) = blocker {
            ui.colored_label(th.warn, format!("→ 现在不完整,因为: {b}"));
        }
        // 钩子装不上 = 映射整体不可用:用"红字"而不是普通 ✗(橙字)说清楚。
        // 用户的第一反馈就是"按了没反应",这里必须给出原因与下一步。
        #[cfg(windows)]
        if capture_err.is_none()
            && !(crate::capture::hook_bits_keyboard_ok(hook_ok)
                && crate::capture::hook_bits_mouse_ok(hook_ok))
        {
            ui.colored_label(
                th.danger,
                "⚠ 输入捕获不完整:有低级钩子没有装上,相关按键不会有任何反应。",
            )
            .on_hover_text(
                "键盘/鼠标低级钩子可能被安全软件拦截,或系统资源不足。\n\
                 运行日志里记着每个钩子安装失败的具体错误码。\n\
                 常见处理:把本程序加入安全软件白名单后重启。",
            );
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
            let g = lock_shared(&self.shared);
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
                let g = lock_shared(&self.shared);
                g.live.pointers
            },
            crate::engine::DEVICE_MAX_POINTERS,
            {
                let g = lock_shared(&self.shared);
                g.live.refused
            }
        ));
        out.push_str("\n===== 诊断日志全文 =====\n");
        out.push_str(&crate::diag::tail(usize::MAX));

        let path = crate::diag::path()
            .with_file_name(format!("diagnostics-report-{}.txt", timestamp_compact()));
        match write_atomic(&path, &out) {
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
        // 清空并重填"就地取消/停止控件"的位置表(见 [`Self::note_cancel_zone`])。
        //
        // 必须在**绘制之前**清:下面处理 `gui_rx` 的输入事件时读到的,就是**上一帧**
        // 画出来的那一版位置 —— 也正是用户当下看到、并且手正按着的那一版。
        ctx.memory_mut(|m| {
            m.data
                .insert_temp(cancel_zones_id(), Vec::<egui::Rect>::new());
            // 「按后延迟」总开关本帧的取值:`tail_delay_widget` 据此决定逐条数字框是
            // 可改还是灰显(见 `tail_delay_on_id`)。同一帧写一次、读一次。
            m.data.insert_temp(
                tail_delay_on_id(),
                lock_shared(&self.shared).profile.tail_delay_enabled,
            );
        });
        self.poll_debug_rx();
        self.poll_adb_run();
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
            // scrcpy --print-fps 每秒一行:解析成实况帧率,不写进界面日志
            // (一秒一行纯属刷屏);其余行照旧。诊断文件里这行也只记 debug 级。
            if let Some(fps) = adb::parse_scrcpy_fps(&message) {
                self.stream_fps = Some((fps, Instant::now()));
                continue;
            }
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
                let mut g = lock_shared(&self.shared);
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
            let mut outcome: Option<Result<(ControlServer, ControlClient), String>> = None;
            let mut thread_gone = false;
            match rx.try_recv() {
                Ok(r) => outcome = Some(r),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => thread_gone = true,
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
            if let Some(r) = outcome {
                self.connect_rx = None;
                match r {
                    Ok((server, client)) => {
                        self.log(format!(
                            "控制通道已连接,触摸坐标空间 {}x{}",
                            client.screen_w, client.screen_h
                        ));
                        let (w, h) = (client.screen_w, client.screen_h);
                        self.server = Some(server);
                        lock_shared(&self.shared).control = Some(client);
                        // 连接后立刻校正坐标空间并升级旧配置(保证注入前已完成)
                        self.sync_display_space(w, h);
                        self.reconnect_attempts = 0;
                        self.reconnect_due = None;
                    }
                    Err(e) => {
                        self.log(format!("连接失败: {e}"));
                        self.schedule_reconnect();
                    }
                }
            } else if thread_gone {
                // 旧实现只处理 Ok/Err:连接线程 panic 时 rx 断开,界面会永远
                // 停在"连接中..."且没有任何出口 —— 归到失败路径重试。
                self.connect_rx = None;
                self.log("连接线程意外退出(未返回结果)");
                self.schedule_reconnect();
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
                        match write_atomic(&p, &content) {
                            Ok(_) => self.log(format!("日志已保存到 {}", p.display())),
                            Err(e) => self.log(format!("日志保存失败: {e}")),
                        }
                    }
                    (DialogPurpose::SaveProfileAs, Some(p)) => {
                        self.stamp_profile_meta();
                        let text = render_config(&self.config_doc());
                        match text {
                            Ok(text) => match write_atomic(&p, &text) {
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
                                // 记住这次选择:否则重启后回到默认配置(W0-9)
                                self.remember_now();
                                self.apply_profile_switch(doc);
                                // 先取数、释放锁,再 log:避免 format! 参数里两次 lock 死锁
                                let (nb, nw) = {
                                    let g = lock_shared(&self.shared);
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
                            // 新建的配置同样要活过重启(W0-9)
                            self.remember_now();
                            self.apply_profile_switch(ConfigFile::default());
                            self.log(format!("已新建空配置并切换: {}", p.display()));
                        }
                        Err(e) => self.log(format!("新建失败: {e}")),
                    },
                    (DialogPurpose::PickBackground, Some(p)) => {
                        let path = p.display().to_string();
                        let before = lock_shared(&self.shared).profile.clone();
                        // 顶栏+左栏+中央区几乎铺满窗口,背景图只能透过面板显出来。
                        // 若"面板不透明度 × 压暗"已经把图压到基本看不见(用户会以为
                        // 选图没生效),就自动调到能看见的档位并写进日志说明。
                        let mut adjusted: Vec<String> = Vec::new();
                        {
                            let mut g = lock_shared(&self.shared);
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
            // ---- 「按下取消的那一刻」把这一下整体丢掉(用户 2026-10-10 第 1 条)----
            // 用户用来**退出**捕获/录制的那个按钮,自己也是一次鼠标按键;这几行就是
            // 让"点取消"不留下任何录入痕迹。放在**所有**录入路径(宏录制 / 组合键 /
            // 单键)之前 —— 三条路都会把这一下当成用户的输入。
            if let CaptureEvent::Button { code, pressed, .. } = ev {
                if self.swallow_cancel_click(ui.ctx(), code, pressed) {
                    continue;
                }
            }
            if let Some(rec) = self.macro_recording.as_mut() {
                if let CaptureEvent::Button { code, pressed, .. } = ev {
                    // W1-1:按事件的**捕获时刻**记步骤 —— 以前用收到事件的时刻,
                    // 队列排队与界面卡顿都会被算进两步之间的间隔里。
                    Self::record_macro_button(rec, code, pressed, ev.at());
                }
                continue;
            }
            if let CaptureEvent::Button { code, pressed, .. } = ev {
                // ---- 组合键捕获(系统键那类槽位;见 `waiting_keys`) ----
                // 与下面的单键捕获互斥:一次只会武装其中一个。
                if let Some(slot) = self.waiting_keys {
                    if crate::keymap::is_wheel_code(code) {
                        // 滚轮没有"抬起"边沿,做不了组合键;与单键那条路同一句提示。
                        self.log("滚轮不能这样绑定:鼠标键位请用[鼠标映射]下拉选择");
                        continue;
                    }
                    if pressed {
                        if self.capture_seen.len() < 2 && !self.capture_seen.contains(&code) {
                            self.capture_seen.push(code);
                        }
                        if !self.capture_down.contains(&code) {
                            self.capture_down.push(code);
                        }
                    } else {
                        self.capture_down.retain(|k| *k != code);
                        // 全部松开 = 这一轮输入结束:按**规范顺序**落定(修饰键在前),
                        // 于是界面上显示成 `Ctrl+X` 而不是用户真实的按下顺序。
                        if self.capture_down.is_empty() && !self.capture_seen.is_empty() {
                            let keys = crate::keymap::canonical_chord(&self.capture_seen);
                            self.waiting_keys = None;
                            self.capture_down.clear();
                            self.capture_seen.clear();
                            self.assign_keys(slot, keys);
                        }
                    }
                    continue;
                }
                if let (Some(slot), true) = (self.waiting_key, pressed) {
                    // 用户 2026-10-09(第 3 条"滚动"):滚轮不从这条路写进任何等待槽。
                    // 否则在清单上滚一下鼠标,当时等着接收的那个键位就被改成"滚轮上滚/下滚"了
                    // —— 鼠标滚轮只认[鼠标映射]那份下拉(`mouse_key_choices`),那个入口不走这里。
                    // **保持武装**:提示一句,用户接着按真正的键仍然有效,不必重新点一次。
                    if crate::keymap::is_wheel_code(code) {
                        self.log("滚轮不能这样绑定:鼠标键位请用[鼠标映射]下拉选择;若要绑键盘键请直接按键");
                    } else {
                        self.waiting_key = None;
                        self.assign_key(slot, code);
                    }
                }
            }
        }
        if self
            .macro_recording
            .as_ref()
            .is_some_and(|rec| rec.should_auto_stop())
        {
            self.finish_macro_recording();
        }

        // ---- 引擎侧的解释性消息(映射开关、触点池满、配置重建等)写进日志 ----
        // 总开关键是在引擎线程里处理的,以前不留任何痕迹 —— 于是"映射到底开没开、
        // 刚才是谁把它关了"完全看不出来,用户只能反复按 F8 试。
        {
            let (msgs, recheck) = {
                let mut g = lock_shared(&self.shared);
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
            let g = lock_shared(&self.shared);
            (
                g.enabled,
                g.control
                    .as_ref()
                    .map(|c| c.is_connected())
                    .unwrap_or(false),
            )
        };
        let grab_now = enabled && self.grab_enabled;
        self.grab_flag.store(grab_now, Ordering::Relaxed);
        // ---- 同步「拦截系统默认行为」掩码(2026-10-07) ----
        // 位定义见 capture::swallow_bit。规则:映射开启时,把"有系统默认功能
        // 且在配置里被绑定"的候选键交给捕获层吞掉(Esc 不再退出全屏、右键不再
        // 弹菜单)——"被绑定"就是用户的逐键选择,不额外要求 grab 勾选
        // (grab 在 Windows 上只有 Linux 的 EVIOCGRAB 语义、且不落盘,拿它当
        // 前置会让这个功能默认静默失效);FPS 模式的滚轮缩放在生效时也吞滚轮。
        // 钩子回调只读这一份预计算掩码,不做锁/不做遍历。
        {
            let bits = if enabled {
                let g = lock_shared(&self.shared);
                let live = g.aim_live;
                let aim = &g.profile.aim;
                // FPS 模式是否正在生效(与 engine 的 fps_is_active 同口径,
                // 含 anchor_set:未取锚点时引擎不会缩滚轮,捕获层也不能吞)
                let fps_active =
                    aim.enabled && live.mode_active && !live.suspended && aim.anchor_set();
                let mut bits = 0u16;
                for b in &g.profile.binds {
                    // 仅 FPS 的绑定在非 FPS 模式里并不生效,不能占着吞掉位 ——
                    // 否则用户平时按 Esc 也会被吞(映射里那条 fps_only 还没生效)。
                    if b.fps_only && !fps_active {
                        continue;
                    }
                    if let Some(bit) = crate::capture::swallow_bit(b.key) {
                        bits |= bit;
                    }
                }
                if aim.wheel_zoom && fps_active {
                    if let (Some(up), Some(down)) = (
                        crate::capture::swallow_bit(crate::keymap::BTN_WHEEL_UP),
                        crate::capture::swallow_bit(crate::keymap::BTN_WHEEL_DOWN),
                    ) {
                        bits |= up | down;
                    }
                }
                bits
            } else {
                0
            };
            self.swallow_flag.store(bits, Ordering::Relaxed);
        }
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
                let g = lock_shared(&self.shared);
                g.live.pointers
            };
            self.server = None;
            lock_shared(&self.shared).control = None;
            if pending > 0 {
                self.log(format!(
                    "控制通道已断开(当时有 {pending} 个触点未抬起;重连后会自动重建,不必手动收拾)"
                ));
                crate::diag_warn!("app", "控制通道断开时仍有 {pending} 个触点未抬起");
            } else {
                self.log("控制通道已断开");
            }
            // 自动重连(2026-10-06):旧实现断线后只清状态、等用户手动点按钮。
            // 真机日志里断出过一次 4 分钟空窗(注入全死:摇杆停在上一个方向、
            // 技能按不出来)。这里排下第一班重连;用户点[断开]即取消。
            self.schedule_reconnect();
            if self.reconnect_armed {
                self.log("将在 1 秒后自动重连(不想重连可点[断开])");
                crate::diag_warn!("app", "控制通道断开:已排入自动重连");
            }
        }
        self.tick_auto_reconnect();

        // ================= 布局分派 =================
        // 布局只有一套:顶栏 + 左栏 + 中央标签页(默认可视化共用),
        // 可视化风格改变的是页面内容(虚拟键盘),不是整体骨架。
        // (历史上还有第三种"鸿蒙"风格:顶栏 + 左侧导航 + 中央卡片 + 右侧预览
        //  + 底部状态栏的整套独立布局,2026-10-06 按用户决策 W2-7 删除。)
        {
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
                            if self.reconnect_attempts > 0 {
                                ui.label(format!("重连中…(第 {} 次)", self.reconnect_attempts));
                            } else {
                                ui.label("连接中...");
                            }
                        } else if ui.button("连接控制").clicked() {
                            // 手动连接 = 重新开始:清零退避计数
                            self.reconnect_attempts = 0;
                            self.reconnect_due = None;
                            self.connect_control();
                        }

                        ui.separator();
                        let tk = lock_shared(&self.shared).profile.toggle_key;
                        let tkn = if tk.is_empty() {
                            "未绑定".to_string()
                        } else {
                            tk.label().replace("KEY_", "")
                        };
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
                        // 这个按钮随任务出现/消失,标签还长短不一。按内容排布的话,
                        // 它每弹一次都会把右边的[关于]和这一排挤得左右横跳、甚至换行。
                        // 所以按**最长**的那个标签预留固定宽度的槽位:没任务时槽位留空,
                        // 位置一点不动(用户 2026-10-09)。
                        let slot = egui::vec2(
                            Self::cancel_button_slot_width(ui),
                            ui.spacing().interact_size.y,
                        );
                        let (rect, _) = ui.allocate_exact_size(slot, egui::Sense::hover());
                        // 有任务时这一个按钮也是"就地取消"控件:按下它的那一下同样
                        // 不该被记进任何捕获/录制(见 `note_cancel_zone`)。
                        let mut cancel_resp: Option<egui::Response> = None;
                        if let Some(label) = self.pending_task_label() {
                            let r = ui.put(
                                rect,
                                egui::Button::new(format!("取消{label}"))
                                    .fill(th.danger.gamma_multiply(0.35))
                                    .stroke(egui::Stroke::new(
                                        1.0,
                                        theme::with_alpha(th.danger, 190),
                                    )),
                            );
                            Self::note_cancel_zone(ui, &r);
                            cancel_resp = Some(r);
                        }
                        if cancel_resp.is_some_and(|r| r.clicked()) {
                            self.cancel_pending_tasks();
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
                            // 用户 2026-10-10(小改动 2):"使用说明"按钮长宽各 ×1.5 ——
                            // 它是"不会用的时候第一个要找的按钮"。按按钮自己的排版量出
                            // 常规尺寸再乘 1.5,字号、内边距跟主题走,不写死像素。
                            let help_size = {
                                let font = egui::TextStyle::Button.resolve(ui.style());
                                let text_w = ui
                                    .painter()
                                    .layout_no_wrap(
                                        "使用说明".to_owned(),
                                        font,
                                        egui::Color32::WHITE,
                                    )
                                    .size()
                                    .x;
                                let sp = ui.spacing();
                                [
                                    (text_w + 2.0 * sp.button_padding.x) * 1.5,
                                    sp.interact_size.y * 1.5,
                                ]
                            };
                            if ui
                                .add_sized(help_size, egui::Button::new("使用说明"))
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
                            // 默认风格:维持折叠头(现状不变)。
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
                                egui::CollapsingHeader::new("外观")
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
        } // ← 布局(顶栏 + 左栏 + 中央标签页)到这里结束

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
        // 截图小窗放在扩展宏弹窗**之后**:两者都在时,小窗要压在上面(它本来就是
        // "浮在最上面看"的用途)。
        self.ui_shot_window(ctx);

        // ================= 参数助手窗口 =================
        self.ui_args_helper(ctx);

        // ================= adb 命令助手窗口 =================
        self.ui_adb_helper(ctx);

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
            let g = lock_shared(&self.shared);
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
        let mut fast = lock_shared(&self.shared).fast_switch_enabled;
        if ui
            .checkbox(&mut fast, "启用快速切换")
            .on_hover_text("只有勾选后，切换键才会真正切换组合；不勾选也可在[其他功能]里预先设置")
            .changed()
        {
            lock_shared(&self.shared).fast_switch_enabled = fast;
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
            let mut g = lock_shared(&self.shared);
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
        // 新增/选中后滚动到光标。以前这是"非鸿蒙才做"(鸿蒙是独立布局,滚的是别的容器);
        // 现在只剩一套布局,一律滚动。
        if self.scroll_to_new {
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
        let bind_count = lock_shared(&self.shared).profile.binds.len();

        for i in 0..bind_count {
            let is_macro = {
                let g = lock_shared(&self.shared);
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
                let g = lock_shared(&self.shared);
                // 列表长度是**上一次**加锁时读的:期间引擎可能用切换键整份换掉
                // profile(换成键位更少的组合)→ 直接下标会 panic。
                // 取不到就整行不渲染(下一帧会按新长度重建列表)。
                let Some(b) = g.profile.binds.get(i) else {
                    return;
                };
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
                    let mut g = lock_shared(&self.shared);
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
                    let mut g = lock_shared(&self.shared);
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
                    let mut g = lock_shared(&self.shared);
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
                    let g = lock_shared(&self.shared);
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
                    let mut g = lock_shared(&self.shared);
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
                    let g = lock_shared(&self.shared);
                    // 渲染中途索引失效(引擎换组合):给占位文本而不是 panic
                    g.profile.binds.get(i).map(|b| b.action.describe())
                };
                ui.label(desc.unwrap_or_else(|| "(该键位已不存在)".to_string()));
                let mut g = lock_shared(&self.shared);
                if let Some(b) = g.profile.binds.get_mut(i) {
                    if let Action::AndroidKey { keycode } = &mut b.action {
                        ui.label("keycode:");
                        ui.add(egui::DragValue::new(keycode).range(0..=999));
                    }
                }
            }

            // 「按后延迟」(用户 2026-10-10 第 2 条):这条键位自己的冷却,所有
            // 动作类型(含宏触发)通吃,所以放在行尾、与动作细节无关的位置。
            if let Some(old) = {
                let g = lock_shared(&self.shared);
                g.profile.binds.get(i).map(|b| b.tail_delay_ms)
            } && let Some(new) = Self::tail_delay_widget(ui, old)
            {
                // 快照必须是"改动之前":先取整份配置,再把这一条换回旧值。
                let mut snapshot = lock_shared(&self.shared).profile.clone();
                if let Some(b) = snapshot.binds.get_mut(i) {
                    b.tail_delay_ms = old;
                }
                self.push_undo_snapshot(snapshot);
                if let Some(b) = lock_shared(&self.shared).profile.binds.get_mut(i) {
                    b.tail_delay_ms = new;
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
        // 下标核查见 remove_indexed:引擎可能刚用切换键换掉整份 profile
        let removed = {
            let mut g = lock_shared(&self.shared);
            remove_indexed(&mut g.profile.binds, i)
        };
        if !removed {
            self.log("该键位已不在当前组合里(组合刚被切换),未删除");
            return;
        }
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
            // 「按后延迟」(用户 2026-10-10 第 2 条):与可视化页的草稿编辑器同一份控件。
            if let Some(v) = Self::tail_delay_widget(ui, self.draft.tail_delay_ms) {
                self.draft.tail_delay_ms = v;
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
        lock_shared(&self.shared).profile.binds.push(KeyBind {
            key,
            action,
            // FPS 页建的草稿只入"仅 FPS"键位;键位页建的为普通键位
            fps_only: self.draft.fps_only,
            tail_delay_ms: self.draft.tail_delay_ms,
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
                ui.horizontal(|ui| {
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
                    // 鼠标键位**不走**"按任意键"那条 UI:按下鼠标就等于在截图上点了
                    // 一下,会和取点打架。这里下拉直接选是哪个键(用户 2026-10-09)。
                    // 选完与键盘那条路完全一致 —— 草稿照样从这里开始,后面照常取点/设动作。
                    ui.menu_button("＋ 新增鼠标按键映射", |ui| {
                        for (code, name) in mouse_key_choices() {
                            if ui.button(name).clicked() {
                                self.cancel_draft();
                                self.reset_draft_to_screen();
                                self.draft.key = Some(code);
                                self.draft.fps_only = false;
                                self.draft_active = true;
                                self.vk_sel = None;
                                self.waiting_key = None;
                                self.vk_bottom_add_active = true;
                                self.scroll_to_new = true;
                                ui.close();
                            }
                        }
                    })
                    .response
                    .on_hover_text("触发键用鼠标某个键位(其余与[新增按键映射]完全一样)");
                });
            } else {
                self.ui_vk_draft(ui);
            }
        }
        self.apply_pending_scroll(ui);
    }

    /// FPS 页:虚拟键盘(与键位页同一套显示能力,默认只亮 FPS 键位)
    /// + 既有 FPS 面板(全保留)。功能上只能设置 FPS 相关键。
    ///
    /// 布局(用户 2026-10-07 拍板):「鼠标瞄准」整串**常驻页面最上** ——
    /// 它本来就是 FPS 页的核心,不再等切到"瞄准锚点"才展开;顺序 =
    /// 瞄准 → 虚拟键盘 → 过滤/新增下拉行 → 下拉栏下方显示所选项目的设置。
    fn ui_visual_fps(&mut self, ui: &mut egui::Ui, captured: bool) {
        self.ui_aim(ui, captured);
        ui.separator();
        self.ui_vk_area(ui, true);
        ui.horizontal(|ui| self.vk_fps_filters.ui(ui, "vk_filters_fps"))
            .response
            .on_hover_text(
                "与[键位]页一样:想在这里看到普通映射/组合键/摇杆,勾上即可。\n\
                 默认只显示 FPS 独有的键位。",
            );
        self.ui_vk_panel(ui, true);
        self.apply_pending_scroll(ui);
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
        /// 组合键槽位:集合里的每个键都亮起,悬停显示同一条说明(2026-10-09 第 4 条)。
        fn put_set(
            m: &mut std::collections::HashMap<u16, (egui::Color32, String)>,
            keys: &KeySet,
            c: egui::Color32,
            tip: String,
        ) {
            for k in keys.iter() {
                put(m, *k, c, tip.clone());
            }
        }
        let mut m = std::collections::HashMap::new();
        let th = self.theme();
        let g = lock_shared(&self.shared);
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
                (filters.fps, th.warn, "仅 FPS")
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
            for (_, keys) in w.active_dirs() {
                for code in keys {
                    put(&mut m, code, c, format!("摇杆#{}({tag}) 方向键", wi + 1));
                }
            }
            if let Some(t) = w.temp.as_ref() {
                for code in t.key {
                    put(
                        &mut m,
                        code,
                        th.wheel_enable,
                        format!("摇杆#{} {tag}启用键", wi + 1),
                    );
                }
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
                put_set(
                    &mut m,
                    &s.effective_keys(),
                    th.accent,
                    format!("切换键位#{}(按下切到它指向的组合)", si + 1),
                );
            }
        }
        // 瞄准三键:属于 FPS 语义,受"显示FPS"勾选控制(FPS 页默认为真)
        if filters.fps {
            put_set(
                &mut m,
                &p.aim.hold_key,
                th.danger,
                "FPS 瞄准门控键(按住开镜)".into(),
            );
            put_set(
                &mut m,
                &p.aim.toggle_key,
                th.danger,
                "FPS 模式独立开关".into(),
            );
            put_set(
                &mut m,
                &p.aim.suspend_key,
                th.danger,
                "按住暂时退出 FPS 并显示鼠标".into(),
            );
        }
        put_set(
            &mut m,
            &p.cursor_toggle_key,
            th.accent,
            "全局鼠标消隐切换键".into(),
        );
        // 总开关键两页都亮(全局键)
        put_set(&mut m, &p.toggle_key, th.danger, "映射总开关键".into());
        m
    }

    /// 取点中应当呼吸闪烁的键码(取点目标本身在哪条键上就闪哪条)
    fn vk_pulse_code(&self) -> Option<u16> {
        let g = lock_shared(&self.shared);
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
                .and_then(|w| w.temp.as_ref())
                .and_then(|t| t.key.first().copied()),
            _ => None,
        }
    }

    /// 操作栏正在编辑的目标对应的键码(用于加亮显示)
    fn vk_sel_code(&self) -> Option<u16> {
        let g = lock_shared(&self.shared);
        match self.vk_sel? {
            VkSel::New => self.draft.key,
            VkSel::Bind(i) => g.profile.binds.get(i).map(|b| b.key),
            VkSel::Macro(i) => g.profile.binds.get(i).map(|b| b.key),
            // 组合键槽位:虚拟键盘只能加亮**一个**键,所以只取"正好单键"的那种
            // (老配置与绝大多数槽位都是单键);组合键不在虚拟键盘上加亮。
            VkSel::Toggle => g.profile.toggle_key.only(),
            VkSel::CursorToggle => g.profile.cursor_toggle_key.only(),
            VkSel::AimHold => g.profile.aim.hold_key.only(),
            VkSel::AimToggle => g.profile.aim.toggle_key.only(),
            VkSel::AimSuspend => g.profile.aim.suspend_key.only(),
            VkSel::WheelDir { wheel, dir } => g.profile.wheels.get(wheel).and_then(|w| {
                w.active_dirs()
                    .get(dir)
                    .and_then(|(_, key)| key.first().copied())
            }),
            VkSel::WheelEnable(i) => g
                .profile
                .wheels
                .get(i)
                .and_then(|w| w.temp.as_ref())
                .and_then(|t| t.key.first().copied()),
            VkSel::SwitchKey(i) => g
                .switch_keys
                .get(i)
                .map(|s| s.effective_keys())
                .and_then(|k| k.only()),
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
        let g = lock_shared(&self.shared);
        let p = &g.profile;
        if let Some(i) = p.binds.iter().position(|b| b.key == code) {
            return Some(if matches!(p.binds[i].action, Action::Macro(_)) {
                VkSel::Macro(i)
            } else {
                VkSel::Bind(i)
            });
        }
        // 组合键槽位按"成员"命中(点组合里的任一键都能落到这一槽)。
        if p.toggle_key.contains(&code) {
            return Some(VkSel::Toggle);
        }
        if p.cursor_toggle_key.contains(&code) {
            return Some(VkSel::CursorToggle);
        }
        if p.aim.hold_key.contains(&code) {
            return Some(VkSel::AimHold);
        }
        if p.aim.toggle_key.contains(&code) {
            return Some(VkSel::AimToggle);
        }
        if p.aim.suspend_key.contains(&code) {
            return Some(VkSel::AimSuspend);
        }
        for (wi, w) in p.wheels.iter().enumerate() {
            // 组合键方向键按**成员**命中(点组合里的任一键都能落到这一槽),
            // 与上面几个 `KeySet` 槽位同口径。
            for (dir, (_, kc)) in w.active_dirs().iter().enumerate() {
                if kc.contains(&code) {
                    return Some(VkSel::WheelDir { wheel: wi, dir });
                }
            }
            if w.temp.as_ref().is_some_and(|t| t.key.contains(&code)) {
                return Some(VkSel::WheelEnable(wi));
            }
        }
        if let Some(i) = g
            .switch_keys
            .iter()
            .position(|s| s.effective_keys().contains(&code))
        {
            return Some(VkSel::SwitchKey(i));
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
        let g = lock_shared(&self.shared);
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
                        let g = lock_shared(&self.shared);
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
        // W2-2:守卫拒绝时不留下"没人要的草稿"(撤销掉刚建的一切,日志已给出原因)
        if !self.begin_pick(CoordSlot::NewBind) {
            self.cancel_draft();
            return;
        }
        self.vk_sel = Some(VkSel::New);
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
            let mut g = lock_shared(&self.shared);
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
                tail_delay_ms: crate::keymap::DEFAULT_TAIL_DELAY_MS,
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
                        let mut g = lock_shared(&self.shared);
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
                            tail_delay_ms: crate::keymap::DEFAULT_TAIL_DELAY_MS,
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

    /// FPS 页专用添加栏：只允许新增 FPS 独有键位，不暴露普通键位/组合键/轮盘。
    /// 「鼠标瞄准」设置自 2026-10-07 起常驻页面上方，这里不再提供"瞄准锚点"条目
    /// (锚点取点按钮就在上方瞄准分组里)。
    fn ui_fps_add_bar(&mut self, ui: &mut egui::Ui) {
        if self.vk_add_kind != VkAddKind::Key {
            self.vk_add_kind = VkAddKind::Key;
            self.vk_sel = None;
            self.waiting_key = None;
        }
        let th = self.theme();
        ui.horizontal(|ui| {
            if Self::tab_button(ui, "＋ 新增 FPS 键位", false, th.ok) {
                self.cancel_draft();
                self.reset_draft_to_screen();
                self.draft.key = None;
                self.draft.fps_only = true;
                self.draft_active = true;
                self.vk_sel = None;
                self.waiting_key = Some(KeySlot::NewBind);
            }
            ui.label("也可直接点击键盘/鼠标上的空闲键创建仅 FPS 键位；鼠标瞄准设置常驻在页面上方");
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
                let cancel = Self::tab_button_resp(ui, "取消取点", false, self.theme().danger);
                Self::note_cancel_zone(ui, &cancel);
                if cancel.clicked() {
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
                        let g = lock_shared(&self.shared);
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
                                false,
                                true,
                            );
                        } else {
                            ui.small("该宏只有录制步骤；展开后可查看，编辑请点[载入编辑]。");
                        }
                        if ui.button("保存宏修改").clicked() {
                            let mut g = lock_shared(&self.shared);
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
            VkSel::AimToggle => self.ui_vk_special(ui, KeySlot::AimToggle, "FPS 模式开关键"),
            VkSel::AimSuspend => self.ui_vk_special(ui, KeySlot::AimSuspend, "FPS 暂时退出键"),
            VkSel::WheelDir { wheel, dir } => {
                self.ui_vk_special(ui, KeySlot::WheelDir { wheel, dir }, "摇杆方向键")
            }
            VkSel::WheelEnable(i) => {
                self.ui_vk_special(ui, KeySlot::WheelEnable(i), "摇杆临时启用键")
            }
            VkSel::SwitchKey(i) => self.ui_vk_special(ui, KeySlot::SwitchKey(i), "组合切换键"),
            VkSel::ComboKey { combo, slot } => {
                self.ui_vk_special(ui, KeySlot::ComboKey { combo, slot }, "组合键成员")
            }
        }
    }

    /// 独立的宏页面：录制、虚拟键盘取键、宏列表和触发键绑定都在这里完成。
    fn ui_macro_page(&mut self, ui: &mut egui::Ui) {
        let th = self.theme();
        let visual = self.ui_style() == theme::UiStyle::Visual;
        ui.heading("宏");
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
                // 登记取消区:录制期间点[停止录制]的那一下鼠标按键**本身也会被录成
                // 一步** —— 输入事件在同一帧的绘制之前处理,等按钮亮起时它已经进
                // `record_macro_button` 了。登记之后 `swallow_cancel_click` 会把这一下
                // (连同它的抬起)整体丢掉(与顶栏[取消按键捕获]是同一个坑)。
                let stop = Self::tab_button_resp(ui, "停止录制", false, th.danger);
                Self::note_cancel_zone(ui, &stop);
                if stop.clicked() {
                    self.finish_macro_recording();
                }
                // 录制中也能看步骤:先看步骤、再决定要不要保存(用户 2026-10-07)
                let showing = self.macro_show_recording_steps;
                if Self::tab_button(
                    ui,
                    if showing {
                        "隐藏步骤"
                    } else {
                        "显示步骤"
                    },
                    showing,
                    th.accent,
                ) {
                    self.macro_show_recording_steps = !showing;
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
                self.log(
                    "宏录制开始：等待第一个按键……（从第一个按键才开始计时与空闲停止；\
                     之后空闲会自动停止）",
                );
            }
            if Self::tab_button(ui, "清空", false, th.accent) {
                self.reset_macro_draft();
            }
            if Self::tab_button(ui, "扩展宏...", false, th.accent) {
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
            // 「按后延迟」(用户 2026-10-10 第 2 条):宏触发键自己的一份。
            // 整条宏跑完才开始算冷却 —— 连按宏键不会让两次执行叠在一起。
            if let Some(v) = Self::tail_delay_widget(ui, self.macro_page_tail_delay_ms) {
                self.macro_page_tail_delay_ms = v;
            }
            let ready = self.macro_page_key.is_some()
                && (!self.macro_page_steps.is_empty() || !self.macro_page_instructions.is_empty());
            // 用户 2026-10-10(第 1 条):载入编辑后,这一格就是"改这一条宏"。
            // 于是按钮按状态改名 —— 编辑中叫[保存宏](覆盖载入的那条),
            // 同时多出[另存为宏](行为 = 从前的[新建宏]:另 push 一条新的)。
            // 没载入时仍是[新建宏]一条路,不出现多余按钮。
            let editing = self.macro_loaded_index.is_some();
            ui.add_enabled_ui(ready, |ui| {
                if Self::tab_button(ui, if editing { "保存宏" } else { "新建宏" }, false, th.ok)
                {
                    if editing {
                        self.macro_save_over_loaded();
                    } else {
                        self.macro_add_from_page();
                    }
                }
                if editing && Self::tab_button(ui, "另存为宏", false, th.accent) {
                    self.macro_add_from_page();
                }
            });
            if Self::tab_button(ui, "取消编辑", false, th.danger) {
                self.reset_macro_draft();
                self.log("已取消宏编辑");
            }
        });

        // ---------- 2026-10-09:宏草稿库(草稿长期保存 / 之后自如取用) ----------
        // 编辑区这一份(步骤 + 设置项 + 虚拟键位层 + 触发键)就是"草稿"。想留着下次接着改,
        // 就在这里起个名字存起来;下拉选中后[载入]取回。操作与「手动 adb 命令」的用户预设
        // 一模一样,学一个会一个。
        ui.horizontal(|ui| {
            ui.label("草稿库:").on_hover_text(
                "把编辑区现在这份内容(步骤 + 设置项 + 触发键 + 扩展宏虚拟键位)\
                 起个名字长期留着,以后选中它点[载入]就能接着改",
            );
            let sel_text = self
                .macro_draft_sel
                .and_then(|i| self.macro_drafts.get(i))
                .map(|d| d.name.clone())
                .unwrap_or_else(|| "（未选择）".into());
            egui::ComboBox::from_id_salt("macro_draft_pick")
                .selected_text(sel_text)
                .show_ui(ui, |ui| {
                    if self.macro_drafts.is_empty() {
                        ui.weak("（还没有存过草稿）");
                    }
                    for (i, d) in self.macro_drafts.iter().enumerate() {
                        ui.selectable_value(&mut self.macro_draft_sel, Some(i), &d.name);
                    }
                });
            let picked = self
                .macro_draft_sel
                .is_some_and(|i| i < self.macro_drafts.len());
            ui.add_enabled_ui(picked, |ui| {
                if Self::tab_button(ui, "载入", false, th.accent) {
                    self.macro_draft_load();
                }
            });
            if Self::tab_button(ui, "另存当前草稿…", false, th.ok) {
                self.macro_draft_save_open = !self.macro_draft_save_open;
            }
            ui.add_enabled_ui(picked, |ui| {
                if Self::tab_button(ui, "删除", false, th.danger) {
                    self.macro_draft_delete();
                }
            });
        });
        if self.macro_draft_save_open {
            ui.horizontal(|ui| {
                ui.label("草稿名:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.macro_draft_name)
                        .desired_width(160.0)
                        .hint_text("给这份草稿起个名字"),
                );
                if Self::tab_button(ui, "保存", false, th.ok) {
                    self.macro_draft_save();
                }
                if Self::tab_button(ui, "取消", false, th.accent) {
                    self.macro_draft_save_open = false;
                }
                ui.small("同名草稿要先删除,再存一次");
            });
        }
        if let Some((ok, msg)) = self.macro_draft_msg.clone() {
            ui.colored_label(if ok { th.ok } else { th.danger }, msg);
        }

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
                if Self::tab_button(ui, "编辑扩展宏...", false, th.accent) {
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

        if let Some(rec) = self.macro_recording.as_ref() {
            if rec.steps.is_empty() {
                // 第一个键之前只是"等待":不发计时、不空闲停止、不计入宏
                ui.colored_label(th.warn, "录制中… 等待第一个按键（第一键按下后才开始计时）");
            } else {
                ui.colored_label(
                    th.warn,
                    format!(
                        "录制中… 已记录 {} 条结果（长按重复已合并）",
                        rec.steps.len()
                    ),
                );
            }
            if self.macro_show_recording_steps && !rec.steps.is_empty() {
                Self::ui_macro_recorded_steps(ui, &rec.steps);
            }
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
            if Self::tab_button(ui, "扩展宏...", false, th.accent) {
                self.open_macro_virtual_editor();
            }
        });
        let mut instructions = std::mem::take(&mut self.macro_page_instructions);
        // 宏页这份草稿不用返回值:传 false,省掉每帧一次整表克隆(W2-3)
        self.ui_macro_instruction_list(ui, &mut instructions, "macro_page", true, false);
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
            let g = lock_shared(&self.shared);
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
                    let g = lock_shared(&self.shared);
                    g.profile.binds.get(i).and_then(|b| match &b.action {
                        Action::Macro(m) => Some(m.clone()),
                        _ => None,
                    })
                };
                if let Some(action) = action.as_mut() {
                    // 这一步原先克隆整份 action(带录制步骤/virtual_profile)只为比较。
                    // 本块里唯一可改的就是 `instructions`(步骤与扩展键位表都是只读),
                    // 所以直接用列表自己的"改了没有"返回值代替(W2-3)。
                    // 注意:列表没被渲染时(只有录制步骤)返回 false,与旧比较行为一致。
                    let mut changed = false;
                    if !action.steps.is_empty() {
                        Self::ui_macro_recorded_steps(ui, &action.steps);
                    }
                    if let Some(vp) = &action.virtual_profile {
                        Self::ui_macro_virtual_profile_info(ui, vp);
                    }
                    if !action.instructions.is_empty() || action.virtual_profile.is_some() {
                        ui.small("设置动作:");
                        changed = self.ui_macro_instruction_list(
                            ui,
                            &mut action.instructions,
                            &format!("macro_expand_{i}"),
                            false,
                            true,
                        );
                    } else {
                        ui.small("该宏只有录制步骤，录制步骤只读。");
                    }
                    if changed {
                        {
                            let mut g = lock_shared(&self.shared);
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
                let g = lock_shared(&self.shared);
                g.profile.binds.get(i).and_then(|b| match &b.action {
                    Action::Macro(m) => Some((b.key, m.clone(), b.fps_only, b.tail_delay_ms)),
                    _ => None,
                })
            };
            if let Some((key, action, fps, tail)) = loaded {
                self.macro_page_key = Some(key);
                self.macro_page_steps = action.steps;
                // 旧文件/手工编辑来的宏可能第一步带延迟(甚至是在"录制起点"
                // 修复之前录的):进了编辑区就按不变量归一化,让它可见可控。
                Self::normalize_macro_start(&mut self.macro_page_steps);
                self.macro_page_instructions = action.instructions;
                self.macro_page_virtual_profile = action.virtual_profile.as_deref().cloned();
                self.macro_page_fps_only = fps;
                self.macro_page_tail_delay_ms = tail;
                self.macro_loaded_index = Some(i);
                self.waiting_key = None;
                self.log("已载入宏步骤到编辑区；改完点[保存宏]覆盖这一条，或[另存为宏]存成新宏");
            }
        }
        if let Some(i) = delete {
            self.push_undo();
            // 下标核查见 remove_indexed(引擎可能刚换过组合)
            let removed = {
                let mut g = lock_shared(&self.shared);
                remove_indexed(&mut g.profile.binds, i)
            };
            if !removed {
                self.log("该宏已不在当前组合里(组合刚被切换),未删除");
                return;
            }
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
        self.macro_page_tail_delay_ms = crate::keymap::DEFAULT_TAIL_DELAY_MS;
        self.macro_loaded_index = None;
        self.picking = None;
        self.waiting_key = None;
    }

    /// 「宏草稿库」:把编辑区当前这一份草稿按名字存起来(落 `macro_drafts.json`)。
    ///
    /// 与「手动 adb 命令」的[另存为预设]同一套规矩:名字不能空、同名不让覆盖
    /// (要改就先删掉再存),写盘失败就**回滚内存**,不让界面和文件不一致。
    fn macro_draft_save(&mut self) {
        let name = self.macro_draft_name.trim().to_string();
        if name.is_empty() {
            self.macro_draft_msg = Some((false, "草稿名不能为空".into()));
            return;
        }
        if self.macro_recording.is_some() {
            self.macro_draft_msg = Some((false, "正在录制中,先停止录制再保存草稿".into()));
            return;
        }
        if self.macro_page_key.is_none()
            && self.macro_page_steps.is_empty()
            && self.macro_page_instructions.is_empty()
            && self.macro_page_virtual_profile.is_none()
        {
            self.macro_draft_msg = Some((false, "当前草稿是空的,没有可保存的内容".into()));
            return;
        }
        if self.macro_drafts.iter().any(|d| d.name == name) {
            self.macro_draft_msg =
                Some((false, format!("已存在同名草稿 {name}(先删除它或换个名字)")));
            return;
        }
        self.macro_drafts.push(MacroDraft {
            name: name.clone(),
            key: self.macro_page_key,
            fps_only: self.macro_page_fps_only,
            tail_delay_ms: self.macro_page_tail_delay_ms,
            steps: self.macro_page_steps.clone(),
            instructions: self.macro_page_instructions.clone(),
            virtual_profile: self.macro_page_virtual_profile.clone(),
        });
        match save_macro_drafts(&macro_drafts_path(), &self.macro_drafts) {
            Ok(()) => {
                self.macro_draft_sel = Some(self.macro_drafts.len() - 1);
                self.macro_draft_save_open = false;
                self.macro_draft_name.clear();
                self.macro_draft_msg = Some((true, format!("已保存草稿: {name}")));
                self.log(format!("宏草稿已保存: {name}"));
            }
            Err(e) => {
                self.macro_drafts.pop();
                self.macro_draft_msg = Some((false, format!("草稿保存失败: {e}")));
            }
        }
    }

    /// 「宏草稿库」:把下拉选中的草稿取回编辑区(整份替换当前草稿)。
    fn macro_draft_load(&mut self) {
        let Some(i) = self
            .macro_draft_sel
            .filter(|i| *i < self.macro_drafts.len())
        else {
            return;
        };
        if self.macro_recording.is_some() {
            self.macro_draft_msg = Some((false, "正在录制中,先停止录制再载入草稿".into()));
            return;
        }
        let d = self.macro_drafts[i].clone();
        self.macro_page_key = d.key;
        self.macro_page_fps_only = d.fps_only;
        self.macro_page_tail_delay_ms = d.tail_delay_ms;
        self.macro_page_steps = d.steps;
        self.macro_page_instructions = d.instructions;
        self.macro_page_virtual_profile = d.virtual_profile;
        // 取回来的是**草稿**,不是列表里已有的那条宏 —— "载入编辑/取消编辑"要复位,
        // 否则按钮会显示成"取消编辑"却取消了一条跟当前草稿无关的宏。
        self.macro_loaded_index = None;
        self.macro_draft_msg = Some((true, format!("已载入草稿: {}", d.name)));
        self.log(format!("宏草稿已载入: {}", d.name));
    }

    /// 「宏草稿库」:删除下拉选中的草稿(写盘失败就放回去)。
    fn macro_draft_delete(&mut self) {
        let Some(i) = self
            .macro_draft_sel
            .filter(|i| *i < self.macro_drafts.len())
        else {
            return;
        };
        let removed = self.macro_drafts.remove(i);
        self.macro_draft_sel = None;
        match save_macro_drafts(&macro_drafts_path(), &self.macro_drafts) {
            Ok(()) => {
                self.macro_draft_msg = Some((true, format!("已删除草稿: {}", removed.name)));
                self.log(format!("宏草稿已删除: {}", removed.name));
            }
            Err(e) => {
                // 写盘失败:放回去,别让界面和文件不一致
                self.macro_drafts.insert(i, removed);
                self.macro_draft_msg = Some((false, format!("删除失败(文件没写成功): {e}")));
            }
        }
    }

    fn open_macro_virtual_editor(&mut self) {
        // 已经开着就**不要**重建:重建会把用户没保存的虚拟键位悄悄丢掉,
        // 还会把正进行中的取点/选键留在一个指向旧 profile 的下标上
        // (用户要求:取消必须无残留、可逆、可预期 —— 静默丢内容不合这条)。
        // 想重来请用弹窗自己的「取消」/「保存」,那两条路都是清干净的。
        if self.macro_virtual_editor.is_some() {
            self.log("扩展宏设置窗口已经打开(要重来请先在窗口里保存或取消)");
            return;
        }
        let (source_scheme, profile) = {
            let g = lock_shared(&self.shared);
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
            newest: None,
        });
    }

    fn ui_macro_virtual_window(&mut self, ctx: &egui::Context) {
        let Some(mut editor) = self.macro_virtual_editor.take() else {
            return;
        };
        let schemes: Vec<String> = {
            let g = lock_shared(&self.shared);
            g.schemes.iter().map(|p| p.name.clone()).collect()
        };
        let mut open = true;
        let mut save = false;
        let mut cancel = false;
        // R4(2026-10-08):本帧在弹窗里点出来的操作,统一攒到这里、等窗口收尾时落地
        // (见 `apply_virtual_act`)。攒下来的另一个好处:关窗/取消时**不用逐个回滚** ——
        // 直接丢掉这个列表,什么都没发生。
        let mut acts: Vec<MacroVirtualAct> = Vec::new();
        let mut add_combo = false;
        let mut add_wheel = false;
        let mut arm_new_key = false;
        // 「鼠标映射」下拉里选中的键码(鼠标键按下=在图上点了一下,不能靠"按任意键")
        let mut mouse_new_key: Option<u16> = None;
        let mut shot = false;
        // 本帧点了虚拟键盘上的键(虚拟取键的第二步);`(槽位, 键码)`
        let mut virtual_key_hit: Option<(KeySlot, u16)> = None;
        // 点了[取消取点]/[取消选键]:取消必须无残留 —— 只清虚拟槽位,不碰主界面的取点
        let mut cancel_pending = false;
        // 主题与换算器都在加锁之前取好(它们内部要读配置)。
        let th = self.theme();
        let am = self.mapper();
        egui::Window::new("扩展宏：虚拟键位")
            .open(&mut open)
            .default_size([760.0, 600.0])
            .min_size([520.0, 360.0])
            .resizable(true)
            // 2026-10-09:窗口里多了"拼在下方的截图",内容高度会超过屏幕(竖屏截图尤其)。
            // 允许整窗纵向滚动,保证最下面的[保存到宏草稿]/[取消]永远够得着 ——
            // 否则窗口自己长到屏幕外,用户就"只能保存不了也取消不了"了。
            .vscroll(true)
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
                        let mut inherited = lock_shared(&self.shared)
                            .schemes
                            .get(source)
                            .cloned()
                            .unwrap_or_default();
                        sanitize_virtual_profile(&mut inherited);
                        editor.profile = inherited;
                        editor.source_scheme = source;
                        editor.selected_key = None;
                        // 换了基底,旧的下标没有意义了(用户 2026-10-10 第 4 条)
                        editor.newest = None;
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
                    if let Some(slot) = self.waiting_key.filter(|s| s.is_virtual()) {
                        // R4:举着"等一次键"的虚拟槽位时,这次点击交给它
                        // (新建虚拟键位 / 组合键的某个键 / 轮盘的方向键或启用键)。
                        virtual_key_hit = Some((slot, code));
                    } else if editor.profile.binds.iter().any(|b| b.key == code) {
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
                            tail_delay_ms: crate::keymap::DEFAULT_TAIL_DELAY_MS,
                        });
                        editor.selected_key = Some(code);
                    }
                }
                ui.separator();
                // ---------- R4(2026-10-08):新增项目 ----------
                // 与主界面的"新增项目"栏同一套四类(按键/组合键/轮盘/锚点),也同一套
                // 后续流程(点键 → 截图取点);区别只有一个:这里建出来的东西落在
                // **虚拟层**(宏草稿)里,不碰实时配置。四类都能在截图上取点摆放。
                // (2026-10-09 起截屏按钮不在这里了:截图整块挪到下方与本栏并列,
                //  见下面"截图模块"那一段。)
                ui.horizontal(|ui| {
                    ui.label("新增项目:");
                    if ui.button("＋ 按键").clicked() {
                        arm_new_key = true;
                    }
                    if ui.button("＋ 组合键").clicked() {
                        add_combo = true;
                    }
                    if ui.button("＋ 轮盘").clicked() {
                        add_wheel = true;
                    }
                    if ui
                        .button("＋ 锚点")
                        .on_hover_text("锚点 = 鼠标瞄准时虚拟手指落下的起点(与主界面「取锚点」同一个东西)")
                        .clicked()
                    {
                        acts.push(MacroVirtualAct::PickAimAnchor);
                    }
                    // 鼠标键位不走"按任意键":按下它本身就是在图上点了一下,会和取点打架。
                    // 下拉里直接选,选完接**同一条**「＋按键」流程(建虚拟键位 → 取点)。
                    ui.menu_button("鼠标映射", |ui| {
                        for (code, name) in mouse_key_choices() {
                            if ui.button(name).clicked() {
                                mouse_new_key = Some(code);
                                ui.close();
                            }
                        }
                    })
                    .response
                    .on_hover_text("把鼠标某个键位当作虚拟触发键(与「＋按键」同一条流程)");
                });
                // 待完成操作的状态条:每条等待都配一个[取消](有始必有终)。
                let waiting_virtual = self.waiting_key.filter(|s| s.is_virtual());
                if let Some(slot) = waiting_virtual {
                    ui.horizontal(|ui| {
                        ui.colored_label(
                            th.warn,
                            format!("等待按键: {}。", Self::virtual_wait_hint(slot)),
                        );
                        if ui.button("取消选键").clicked() {
                            cancel_pending = true;
                        }
                    });
                }
                if let Some(CoordSlot::MacroVirtual(pick)) = self.picking {
                    ui.horizontal(|ui| {
                        ui.colored_label(
                            th.warn,
                            format!(
                                "取点中: {}。请在下方截图(或小窗)上点击选点,或",
                                Self::virtual_pick_hint(&editor.profile, pick)
                            ),
                        );
                        let cancel = ui.button("取消取点");
                        Self::note_cancel_zone(ui, &cancel);
                        if cancel.clicked() {
                            cancel_pending = true;
                        }
                    });
                }
                ui.separator();
                if let Some(key) = editor.selected_key {
                    ui.horizontal(|ui| {
                        ui.strong(format!("虚拟键位: {}", key_name(key)));
                        if let Some(index) = editor.profile.binds.iter().position(|b| b.key == key)
                        {
                            if ui.button("取点").clicked() {
                                acts.push(MacroVirtualAct::PickBind(index));
                            }
                        }
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
                // ---------- R4(2026-10-08):虚拟层清单(组合键/轮盘/锚点) ----------
                // 按键在上面那一段单独编辑(它有自己的选中态);这里把另外三类列出来,
                // 每一项都能取点/取键/删除。清单行都是纯函数,动作回抛到 `acts`。
                ui.label(format!(
                    "虚拟层清单: {} 个按键 / {} 个组合键 / {} 个轮盘 / 锚点{}",
                    editor.profile.binds.len(),
                    editor.profile.combos.len(),
                    editor.profile.wheels.len(),
                    if editor.profile.aim.anchor_set() {
                        "已设置"
                    } else {
                        "未设置"
                    },
                ));
                egui::ScrollArea::vertical()
                    .id_salt("macro_virtual_list")
                    .max_height(220.0)
                    .show(ui, |ui| {
                        for (i, combo) in editor.profile.combos.iter_mut().enumerate() {
                            if let Some(act) =
                                Self::ui_macro_virtual_combo(ui, i, combo, self.waiting_key)
                            {
                                acts.push(act);
                            }
                        }
                        for (i, wheel) in editor.profile.wheels.iter_mut().enumerate() {
                            if let Some(act) =
                                Self::ui_macro_virtual_wheel(ui, i, wheel, &am, self.waiting_key)
                            {
                                acts.push(act);
                            }
                        }
                        if let Some(act) = Self::ui_macro_virtual_aim(
                            ui,
                            &mut editor.profile.aim,
                            &am,
                            self.picking,
                        ) {
                            acts.push(act);
                        }
                    });
                ui.separator();
                // ---------- 2026-10-09:截图模块(拼在虚拟键位/虚拟层下方) ----------
                // 用户反馈:取点跑到"外面的窗口"上去点太拧巴。这里把截图画布直接拼进弹窗,
                // 「取点」之后鼠标不用离开窗口。摘出共用:`ui_shot_canvas` 与下方面板、
                // 截图小窗是同一份(见它的文档注释),这里不复制第二套画布逻辑。
                ui.horizontal(|ui| {
                    let taking = self.shot_rx.is_some();
                    if ui
                        .button(if taking {
                            "截图中..."
                        } else {
                            "截取手机屏幕"
                        })
                        .on_hover_text("重新抓一张手机截图;取点会落在新截图上")
                        .clicked()
                        && !taking
                    {
                        shot = true;
                    }
                    self.ui_shot_zoom_buttons(ui);
                    // 同一个位置、两个名字的按钮来回切 —— 与下方面板里那个一模一样。
                    let (win_label, win_hint) = if self.shot_window_open {
                        ("关闭小窗", "把截图放回弹窗下方")
                    } else {
                        ("小窗悬浮", "把截图独立成一个可移动的小窗")
                    };
                    if ui.button(win_label).on_hover_text(win_hint).clicked() {
                        self.shot_window_open = !self.shot_window_open;
                        // 从扩展宏弹窗开的:小窗也画虚拟键位层(用户 2026-10-10 第 4 条)
                        self.shot_window_virtual = self.shot_window_open;
                        // 窗口尺寸由小窗自己按当前倍率算(见 `ui_shot_window`),这里不传。
                    }
                    if self.shot_window_open {
                        ui.checkbox(&mut self.shot_window_pin, "置顶")
                            .on_hover_text("勾上:小窗固定在其他窗口上方;不勾:可以被其他窗口盖住");
                    }
                });
                if !self.shot_window_open {
                    // 画布与取点都要"看见"这份虚拟键位表:`draw_overlay` 靠
                    // `self.macro_virtual_editor` 决定画哪一层,`assign_virtual_coord` 也写在
                    // 它上面。而这个函数开头把编辑器取成了局部变量,所以这里**整段弹窗期间**
                    // 都把这份表放回 `App`,画完再把 profile 合并回来(只有 profile 会被画布改写)。
                    //
                    // 用户 2026-10-10(第 4 条):旧写法只在"正在虚拟取点"那一帧才注入,于是
                    // ①换成继承键位后截图上看不到继承来的键位 ②新加的点取完就闪没了 ——
                    // 两个现象同一个根因:不取点的那些帧画的是实时配置。现在无条件注入,
                    // 画布始终画这份虚拟层(不取点时它不会被改写,合并回来是等价的)。
                    self.macro_virtual_editor = Some(editor.clone());
                    // 分界条 + 定高的画布区:高度由用户拖出来(`macro_shot_height`),
                    // 画布按这块地方等比内接(见 `ui_shot_canvas` / `screenshot_fit_scale`),
                    // 所以图不会被上面的文字压住,也不会缩在一个固定高度的滚动区里出不来。
                    self.ui_shot_divider(ui);
                    let h = self
                        .macro_shot_height
                        .clamp(MACRO_SHOT_MIN_H, MACRO_SHOT_MAX_H);
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), h),
                        egui::Layout::top_down(egui::Align::Center),
                        |ui| {
                            self.ui_shot_canvas(ui, ShotCanvasKind::Macro, true);
                        },
                    );
                    if let Some(drawn) = self.macro_virtual_editor.take() {
                        editor.profile = drawn.profile;
                    }
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
            // 保存 = 这一步结束:待完成的取点/取键一并作废,截图浮层随即复原。
            self.clear_virtual_pick();
        } else if !cancel && open {
            // ① 本帧清单里点出来的"新增"先落到这份副本上(只改宏草稿,不碰实时配置)
            if add_combo {
                // 与主界面 [＋ 新增组合键] 同款默认值,只是写进虚拟层
                let (x, y) = (am.rel_x(540), am.rel_y(960));
                editor.profile.combos_enabled = true;
                editor.profile.combos.push(KeyCombo {
                    keys: vec![0, 0],
                    action: Action::Tap {
                        x,
                        y,
                        duration_ms: crate::keymap::DEFAULT_TAP_DURATION_MS,
                        radius: crate::keymap::DEFAULT_RADIUS,
                    },
                    fps_only: false,
                    tail_delay_ms: crate::keymap::DEFAULT_TAIL_DELAY_MS,
                });
                editor.newest = Some(MacroVirtualPick::Combo(editor.profile.combos.len() - 1));
            }
            if add_wheel {
                // 与主界面 [＋ 新增轮盘] 同一套:自动找一个空位,按默认几何建
                let (cx, cy) = crate::keymap::next_wheel_spot(&editor.profile.wheels);
                editor.profile.wheels.push(Wheel::new_default(&am, cx, cy));
                editor.newest = Some(MacroVirtualPick::WheelCenter(
                    editor.profile.wheels.len() - 1,
                ));
            }
            // ② 把这份副本放回 App —— 后面几步(取点/取键)都写在它上面
            self.macro_virtual_editor = Some(editor);
            // ③ 清单行回抛的动作
            for act in acts {
                self.apply_virtual_act(act);
            }
            // ④ 虚拟键盘上刚按下的那个键
            if let Some((slot, code)) = virtual_key_hit {
                if let Some(msg) = self.assign_virtual_key(slot, code) {
                    self.log(msg);
                }
            }
            // ④b [鼠标映射]:下拉里选好的鼠标键位,直接走「＋按键」那条路的第一步
            //     (键已经定了,不再等按键,直接进截图取点)。
            if let Some(code) = mouse_new_key
                && let Some(msg) = self.assign_virtual_key(KeySlot::MacroVirtualNewBind, code)
            {
                self.log(msg);
            }
            // ⑤ [＋ 按键] = 主界面同款的第一步:等一次键盘点击,再进入截图取点
            if arm_new_key {
                self.picking = None;
                self.waiting_key = Some(KeySlot::MacroVirtualNewBind);
                self.log("请在弹窗里的虚拟键盘上点一个键,新建虚拟键位");
            }
            if shot {
                self.take_screenshot();
            }
            // ⑥ 取消必须无残留:只清虚拟槽位(主界面自己的取点不动)
            if cancel_pending {
                self.clear_virtual_pick();
                self.log("已取消扩展宏取点/选键");
            }
        } else {
            self.clear_virtual_pick();
            self.log("已取消扩展宏设置，未保留任何修改");
        }
    }

    /// R4(2026-10-08):虚拟取键等待中的提示文案(点哪一类的哪个键)。
    fn virtual_wait_hint(slot: KeySlot) -> String {
        match slot {
            KeySlot::MacroVirtualNewBind => "点虚拟键盘上的任意一个键,新建虚拟键位".to_string(),
            KeySlot::MacroVirtualComboKey { combo, slot } => {
                format!("虚拟组合键#{} 的第 {} 个键", combo + 1, slot + 1)
            }
            KeySlot::MacroVirtualWheelDir { wheel, dir } => {
                format!("虚拟轮盘#{} 的第 {} 个方向键", wheel + 1, dir + 1)
            }
            KeySlot::MacroVirtualWheelEnable(i) => format!("虚拟轮盘#{} 的启用键", i + 1),
            _ => "点虚拟键盘上的键".to_string(),
        }
    }

    /// R4(2026-10-08):虚拟取点等待中的提示文案(标出点的是哪一项)。
    fn virtual_pick_hint(profile: &Profile, pick: MacroVirtualPick) -> String {
        match pick {
            MacroVirtualPick::Bind(i) => match profile.binds.get(i) {
                Some(b) => format!("虚拟按键 {} 的落点", key_name(b.key)),
                None => "虚拟按键的落点(该项已删除)".to_string(),
            },
            MacroVirtualPick::Combo(i) => format!("虚拟组合键#{} 的落点", i + 1),
            MacroVirtualPick::WheelCenter(i) => format!("虚拟轮盘#{} 的圆心", i + 1),
            MacroVirtualPick::AimAnchor => "虚拟瞄准锚点".to_string(),
        }
    }

    /// R4(2026-10-08):弹窗里一行「虚拟组合键」。
    ///
    /// 纯函数:只读写这一个 `KeyCombo` 自己;"取键/取点/删除"这类要动 `App` 的
    /// 请求回抛成 [`MacroVirtualAct`]。动作编辑器与虚拟按键共用
    /// [`Self::ui_action_editor`],不复制第二份。
    fn ui_macro_virtual_combo(
        ui: &mut egui::Ui,
        i: usize,
        combo: &mut KeyCombo,
        waiting: Option<KeySlot>,
    ) -> Option<MacroVirtualAct> {
        let mut act = None;
        ui.horizontal(|ui| {
            ui.label(format!("组合键#{}", i + 1));
            ui.label("按键:");
            // 至少两个键(与主界面同款:0 显示成"未绑定",不是键码 0 的名字)
            for slot in 0..combo.keys.len().max(2) {
                let key = combo.keys.get(slot).copied().unwrap_or(0);
                let want = KeySlot::MacroVirtualComboKey { combo: i, slot };
                let shown = (key != 0).then_some(key);
                if Self::key_button(ui, waiting == Some(want), shown).clicked() {
                    act = Some(MacroVirtualAct::TakeKey(want));
                }
            }
            if combo.keys.len() < 4
                && ui
                    .small_button("＋")
                    .on_hover_text("再加一个成员键(最多 4 个)")
                    .clicked()
            {
                combo.keys.push(0);
            }
            if combo.keys.len() > 2
                && ui
                    .small_button("－")
                    .on_hover_text("去掉最后一个成员键")
                    .clicked()
            {
                combo.keys.pop();
            }
            if ui
                .button("落点")
                .on_hover_text("在截图上点出这个组合键的落点")
                .clicked()
            {
                act = Some(MacroVirtualAct::PickComboPoint(i));
            }
            if ui.small_button("删除").clicked() {
                act = Some(MacroVirtualAct::RemoveCombo(i));
            }
        });
        ui.indent(("macro_virtual_combo_body", i), |ui| {
            Self::ui_action_editor(ui, ("macro_virtual_combo_action", i), &mut combo.action);
            ui.checkbox(&mut combo.fps_only, "仅 FPS");
            if let Some(v) = Self::tail_delay_widget(ui, combo.tail_delay_ms) {
                combo.tail_delay_ms = v;
            }
        });
        act
    }

    /// R4(2026-10-08):弹窗里一行「虚拟轮盘」。
    ///
    /// 纯函数(只读写这一个 `Wheel`);取点/取键/删除回抛成 [`MacroVirtualAct`]。
    /// 是主界面轮盘编辑器的**紧凑版**:同一套字段与同一套标签(`kind.label()`、
    /// `mode.label()`、`wheel_dir_label`),但省掉只在主界面才有意义的部分
    /// (手改终点、点信息卡)。半径/影响范围与主界面同一个口径:半径按**像素**
    /// 编辑(经 `Mapper` 换算回相对值),影响范围是"推多远 = 半径 × 系数"的系数。
    fn ui_macro_virtual_wheel(
        ui: &mut egui::Ui,
        i: usize,
        w: &mut Wheel,
        am: &Mapper,
        waiting: Option<KeySlot>,
    ) -> Option<MacroVirtualAct> {
        let mut act = None;
        ui.horizontal(|ui| {
            ui.label(format!("轮盘#{}", i + 1));
            let mut kind = w.kind;
            egui::ComboBox::from_id_salt(("macro_virtual_wheel_kind", i))
                .selected_text(kind.label())
                .show_ui(ui, |ui| {
                    for k in [WheelKind::Standard, WheelKind::Custom, WheelKind::Execute] {
                        ui.selectable_value(&mut kind, k, k.label());
                    }
                });
            if kind != w.kind {
                w.kind = kind;
                // 与主界面同一条规则:切到多向/执行轮盘时补齐自定义方向
                w.ensure_custom_directions();
            }
            if w.kind == WheelKind::Standard {
                let mut mode = w.mode;
                egui::ComboBox::from_id_salt(("macro_virtual_wheel_mode", i))
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
                w.mode = mode;
            }
            ui.label(format!("圆心 ({:.0}%,{:.0}%)", w.cx * 100.0, w.cy * 100.0));
            if ui
                .button("取圆心")
                .on_hover_text("在截图上点出这个轮盘的圆心")
                .clicked()
            {
                act = Some(MacroVirtualAct::PickWheelCenter(i));
            }
            if ui.small_button("删除").clicked() {
                act = Some(MacroVirtualAct::RemoveWheel(i));
            }
        });
        ui.indent(("macro_virtual_wheel_body", i), |ui| {
            ui.horizontal(|ui| {
                ui.label("启用键:");
                let want = KeySlot::MacroVirtualWheelEnable(i);
                let ek = w.temp.as_ref().map(|t| t.key).unwrap_or_default();
                if Self::keys_button(ui, waiting == Some(want), &ek).clicked() {
                    act = Some(MacroVirtualAct::TakeKey(want));
                }
                if let Some(t) = w.temp.as_ref() {
                    ui.label(if t.mode == TempMode::Hold {
                        "长按启用(临时摇杆)"
                    } else {
                        "再按切换(临时摇杆)"
                    });
                } else {
                    ui.label("(永久摇杆)");
                }
                if w.temp.is_some() {
                    if ui
                        .button(if w.temp.as_ref().map(|t| t.mode) == Some(TempMode::Hold) {
                            "模式:长按"
                        } else {
                            "模式:切换"
                        })
                        .on_hover_text("在「按住才启用」与「按一下锁定/再按解除」之间切换")
                        .clicked()
                    {
                        if let Some(t) = w.temp.as_mut() {
                            t.mode = match t.mode {
                                TempMode::Hold => TempMode::Toggle,
                                TempMode::Toggle => TempMode::Hold,
                            };
                        }
                    }
                    if ui
                        .small_button("清除启用键")
                        .on_hover_text("清掉后变回永久摇杆")
                        .clicked()
                    {
                        w.temp = None;
                    }
                }
            });
            if w.kind != WheelKind::Standard {
                ui.horizontal(|ui| {
                    ui.label("方向数量:");
                    let mut count = w.active_dirs().len().clamp(2, 8);
                    if ui
                        .add(egui::DragValue::new(&mut count).range(2..=8))
                        .changed()
                    {
                        w.set_direction_count(count);
                    }
                });
            }
            // 方向键:一行两个,免得 8 个方向横向顶出窗口
            let dirs: Vec<(usize, f32, KeySet)> = w
                .active_dirs()
                .into_iter()
                .enumerate()
                .map(|(pos, (angle, key))| (pos, angle, key))
                .collect();
            for chunk in dirs.chunks(2) {
                ui.horizontal(|ui| {
                    for &(pos, angle, key) in chunk {
                        ui.label(wheel_dir_label(angle, pos));
                        let want = KeySlot::MacroVirtualWheelDir { wheel: i, dir: pos };
                        if Self::keys_button(ui, waiting == Some(want), &key).clicked() {
                            act = Some(MacroVirtualAct::TakeKey(want));
                        }
                    }
                });
            }
            ui.horizontal(|ui| {
                ui.label("半径:");
                let mut pr = am.len(w.radius);
                if ui
                    .add(egui::DragValue::new(&mut pr).range(10..=1000))
                    .changed()
                {
                    w.radius = am.rel_len(pr);
                }
                ui.label("影响范围:");
                let mut scope = w.scope();
                if ui
                    .add(
                        egui::DragValue::new(&mut scope)
                            .speed(0.02)
                            .range(crate::keymap::SCOPE_MIN..=crate::keymap::SCOPE_MAX),
                    )
                    .on_hover_text("触点实际推出的距离 = 半径 × 系数")
                    .changed()
                {
                    w.scope = crate::keymap::clamp_scope(scope);
                }
            });
        });
        act
    }

    /// R4(2026-10-08):弹窗里一行「虚拟锚点」(= 鼠标视角的瞄准锚点)。
    ///
    /// 与主界面"瞄准锚点"那一段同一套三个动作:取点 / 恢复默认(屏幕正中央)/
    /// 清除(未设置)。注意:宏回放目前**不消费**虚拟层的锚点(引擎只按虚拟层解析
    /// 按键/组合键/轮盘),这里保留设置入口与数据,是"整套体系都能在弹窗里调用"
    /// 这一条要求的落点,也是给后续接入留的接口。
    fn ui_macro_virtual_aim(
        ui: &mut egui::Ui,
        aim: &mut Aim,
        am: &Mapper,
        picking: Option<CoordSlot>,
    ) -> Option<MacroVirtualAct> {
        let mut act = None;
        ui.horizontal(|ui| {
            ui.label("锚点:");
            if aim.anchor_set() {
                let (ax, ay) = am.point(aim.anchor_x, aim.anchor_y);
                ui.label(format!("({ax}, {ay})"));
            } else {
                ui.label("未设置");
            }
            let want = CoordSlot::MacroVirtual(MacroVirtualPick::AimAnchor);
            if ui
                .button(if picking == Some(want) {
                    "点击截图..."
                } else {
                    "取点"
                })
                .on_hover_text("锚点是虚拟手指落下的起点,请取在游戏 UI 之外的干净区域")
                .clicked()
            {
                act = Some(MacroVirtualAct::PickAimAnchor);
            }
            if ui
                .small_button("恢复默认")
                .on_hover_text("放回手机屏幕正中央")
                .clicked()
            {
                // 与主界面同款:默认值 = 屏幕正中央,仍按配置的坐标单位换算
                aim.anchor_x = am.rel_x((am.w / 2.0) as i32);
                aim.anchor_y = am.rel_y((am.h / 2.0) as i32);
            }
            if ui
                .small_button("清除")
                .on_hover_text("清成「未设置」(0,0)")
                .clicked()
            {
                aim.anchor_x = 0.0;
                aim.anchor_y = 0.0;
            }
        });
        act
    }

    fn ui_macro_virtual_bind(ui: &mut egui::Ui, bind: &mut KeyBind) {
        Self::ui_action_editor(ui, ("macro_virtual_action", bind.key), &mut bind.action);
        ui.checkbox(&mut bind.fps_only, "仅 FPS");
        if let Some(v) = Self::tail_delay_widget(ui, bind.tail_delay_ms) {
            bind.tail_delay_ms = v;
        }
    }

    /// 「按后延迟」数字框(用户 2026-10-10 第 2 条)。
    ///
    /// 语义:这一条上一次**抬起之后**至少再等这么久才准下一次按下 —— 快速连点 /
    /// 连续划动时,防止上一动作还没抬起、下一动作已经按下。冷却没过就按下来的
    /// 那一次**不丢**,引擎会把它推迟到冷却结束再执行。`0` = 不等(默认)。
    ///
    /// 按键 / 组合键 / 宏触发三类设置共用这一份控件(用户要求"所有出现这三类设置
    /// 的位置都要加" —— 共用一份才不会三处各写一遍、改了语义只改一处)。
    ///
    /// **2026-10-10 晚:整项改成可选功能**,于是这里还要看**总开关**
    /// ([`Profile::tail_delay_enabled`],默认关闭):关着时数字框灰显不可改
    /// (数值照留,打开开关就恢复),悬停提示也换成"去左栏打开总开关"。
    /// 返回用户改出来的新值;没改(或没开着)就是 `None`。
    fn tail_delay_widget(ui: &mut egui::Ui, current: u32) -> Option<u32> {
        let on = ui
            .memory(|m| m.data.get_temp::<bool>(tail_delay_on_id()))
            .unwrap_or(false);
        let mut v = current;
        let r = ui
            .add_enabled_ui(on, |ui| {
                ui.label("按后延迟ms:");
                ui.add(
                    egui::DragValue::new(&mut v)
                        .range(0..=crate::keymap::MAX_TAIL_DELAY_MS)
                        .speed(1),
                )
                .on_hover_text(if on {
                    "这一条上一次抬起之后再等这么久才准下一次按下。\n\
                     冷却没过就按下的那一次会被推迟到冷却结束执行,不会丢。\n\
                     0 = 不等。数值原样保存在配置里。"
                } else {
                    "「按后延迟」当前未启用(默认关闭)。\n\
                     到左栏「按后延迟」勾上总开关后,这一条才会按这个数值生效。"
                })
            })
            .inner;
        (on && r.changed()).then_some(v)
    }

    /// 动作编辑器(点按/长按/滑动/系统键 + 各自的几何参数;不含"仅 FPS")。
    ///
    /// R4(2026-10-08):原先是 `ui_macro_virtual_bind` 的正文,虚拟组合键也要用
    /// 同一套控件,于是摊出来共用一份 —— 以后加动作类型只改这里,不必改两处。
    /// `salt` 必须是各调用点互不相同的 id(同一帧里两个 ComboBox 撞 id 会互相干扰)。
    fn ui_action_editor<H: std::hash::Hash + std::fmt::Debug>(
        ui: &mut egui::Ui,
        salt: H,
        action: &mut Action,
    ) {
        let kind = match action {
            Action::Tap { .. } => 0,
            Action::Hold { .. } => 1,
            Action::Swipe(_) => 2,
            Action::AndroidKey { .. } => 3,
            Action::Macro(_) => 0,
        };
        let mut kind = kind;
        ui.horizontal(|ui| {
            ui.label("动作:");
            egui::ComboBox::from_id_salt(salt)
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
        let old_kind = match action {
            Action::Tap { .. } => 0,
            Action::Hold { .. } => 1,
            Action::Swipe(_) => 2,
            Action::AndroidKey { .. } => 3,
            Action::Macro(_) => 0,
        };
        if kind != old_kind {
            // 换动作类型时**保留已经取好的落点**(用户 2026-10-09 反馈:选完动作,
            // 之前取的点会被抛弃、退回屏幕中央)。点按与长按共用同一个 (x, y),
            // 所以两者互换时把旧坐标带过去;从滑动切过来时用它的起点。
            // 滑动本身是"起点+终点"两个点,不属于"一个落点",仍按默认几何新建。
            let (px, py) = match action {
                Action::Tap { x, y, .. } | Action::Hold { x, y, .. } => (*x, *y),
                Action::Swipe(s) => s.start,
                _ => (0.5, 0.5),
            };
            *action = match kind {
                1 => Action::Hold {
                    x: px,
                    y: py,
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
                    x: px,
                    y: py,
                    duration_ms: crate::keymap::DEFAULT_TAP_DURATION_MS,
                    radius: crate::keymap::DEFAULT_RADIUS,
                },
            };
        }
        match action {
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
                .map(|(_, key)| {
                    if key.is_empty() {
                        "未绑定".to_string()
                    } else {
                        key.label()
                    }
                })
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
        // 是否允许"取点":只有宏页的列表能直接写回(self.macro_page_instructions);
        // 键位页里展开的宏编辑的是副本,取点会落错地方,故传 false。
        pick_enabled: bool,
        // 是否需要"这一帧改了没有"的返回值。整表克隆只是为了做这次比较
        // (W2-3 度量:20 条约 451ns/帧),不需要返回值的调用方传 false 就整个省掉。
        detect_change: bool,
    ) -> bool {
        let before = detect_change.then(|| instructions.clone());
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
                    self.ui_macro_instruction(ui, instruction, i, id, pick_enabled);
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
        match before {
            Some(b) => b != *instructions,
            None => false,
        }
    }

    /// 单个设置宏动作的参数编辑器。
    fn ui_macro_instruction(
        &mut self,
        ui: &mut egui::Ui,
        instruction: &mut MacroInstruction,
        index: usize,
        id: &str,
        pick_enabled: bool,
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
                duration_ms,
                delay_ms,
            } => {
                ui.horizontal(|ui| {
                    ui.label("按键");
                    let waiting = self.waiting_key == Some(KeySlot::MacroInstructionKey(index));
                    if Self::key_button(ui, waiting, Some(*code)).clicked() {
                        pending_key = Some(KeySlot::MacroInstructionKey(index));
                    }
                    ui.label("持续").on_hover_text(
                        "按键步骤是自包含的\"按下 → 持续 → 抬起\"(W2-8):\n\
                         宏不需要长按——点一下,需要的操作按部就班地执行,\n\
                         宏结束时不会在手机上留下任何按着的触点。",
                    );
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
                    let g = lock_shared(&self.shared);
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
                    if pick_enabled {
                        let waiting = self.picking == Some(CoordSlot::MacroClickPoint(index));
                        if ui
                            .button(if waiting { "点击取点..." } else { "取点" })
                            .clicked()
                        {
                            self.begin_pick(CoordSlot::MacroClickPoint(index));
                        }
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
                    if pick_enabled {
                        let waiting = self.picking == Some(CoordSlot::MacroSwipeStart(index));
                        if ui
                            .button(if waiting {
                                "点击取起点..."
                            } else {
                                "取起点"
                            })
                            .clicked()
                        {
                            self.begin_pick(CoordSlot::MacroSwipeStart(index));
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("终点 X/Y");
                    ui.add(egui::DragValue::new(end_x).range(0.0..=1.0).speed(0.005));
                    ui.add(egui::DragValue::new(end_y).range(0.0..=1.0).speed(0.005));
                    if pick_enabled {
                        let waiting = self.picking == Some(CoordSlot::MacroSwipeEnd(index));
                        if ui
                            .button(if waiting {
                                "点击取终点..."
                            } else {
                                "取终点"
                            })
                            .clicked()
                        {
                            self.begin_pick(CoordSlot::MacroSwipeEnd(index));
                        }
                    }
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
                    let g = lock_shared(&self.shared);
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
    fn record_macro_button(rec: &mut MacroRecording, code: u16, pressed: bool, at: Instant) {
        rec.last_event = at;
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
            // Instant::duration_since 在"乱序"(后一条事件的时刻更早)时饱和为 0,
            // 不会 panic,也不会记出负延迟。
            at.duration_since(rec.last_step_at).as_millis().min(2000) as u32
        };
        rec.steps.push(MacroStep {
            code,
            pressed,
            delay_ms: delay,
        });
        rec.last_step_at = at;
    }

    /// 宏的起始点归一化:**宏从第一个按键事件开始算**。
    ///
    /// 用户点[开始录制]之后可能要去翻键盘、想内容,空档期**不该**算进宏里 ——
    /// 否则回放时会先愣上几秒才动,而且用户完全看不出那段时间是哪来的。
    /// 录制侧本来就在 `record_macro_button` / 虚拟键盘录入里把第一步短路成 0,
    /// 这里再兜一道,是因为宏还可能来自旧版配置文件、导入的 YAML 或手工编辑:
    /// 统一在"进入草稿"和"保存成键位"两处归一化,保证不变量对所有来源都成立。
    ///
    /// 只动**第一个步骤**的延迟;用户手工编排的"间隔"指令(`MacroInstruction::Delay`)
    /// 是明确写出来的动作,不能碰。
    fn normalize_macro_start(steps: &mut [MacroStep]) {
        if let Some(first) = steps.first_mut() {
            first.delay_ms = 0;
        }
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
        Self::normalize_macro_start(&mut self.macro_page_steps);
        lock_shared(&self.shared).profile.binds.push(KeyBind {
            key,
            action: Action::Macro(MacroAction {
                steps: self.macro_page_steps.clone(),
                instructions: self.macro_page_instructions.clone(),
                virtual_profile: self.macro_page_virtual_profile.clone().map(Box::new),
            }),
            fps_only: self.macro_page_fps_only,
            // 「按后延迟」(用户 2026-10-10 第 2 条):宏页上配的那一份跟着宏走
            tail_delay_ms: self.macro_page_tail_delay_ms,
        });
        let was_editing = self.macro_loaded_index.is_some();
        self.reset_macro_draft();
        self.log(if was_editing {
            "已另存为新宏"
        } else {
            "已新建宏"
        });
    }

    /// 把编辑区当前这一份**覆盖**到[载入编辑]载入的那条宏上(用户 2026-10-10 第 1 条)。
    ///
    /// 与 [`Self::macro_add_from_page`] 的区别只有一个:不 push 新键位,而是原地改写
    /// `binds[i].action`(与可视化键位页的[保存宏修改]同一条路)。
    /// 载入的那条要是已经被删/被组合切换挤掉了(下标失效或不再是宏),就退回"另存为新宏"
    /// 并说明一句 —— 绝不静默丢改动。
    fn macro_save_over_loaded(&mut self) {
        let Some(index) = self.macro_loaded_index else {
            self.macro_add_from_page();
            return;
        };
        let Some(key) = self.macro_page_key else {
            self.log("请先选择宏触发键");
            return;
        };
        if self.macro_page_steps.is_empty() && self.macro_page_instructions.is_empty() {
            self.log("宏没有录制步骤或设置动作");
            return;
        }
        // 先确认那一格还在、且仍是宏 —— 免得白记一次撤销
        let still_there = {
            let g = lock_shared(&self.shared);
            g.profile
                .binds
                .get(index)
                .is_some_and(|b| matches!(b.action, Action::Macro(_)))
        };
        if !still_there {
            self.macro_loaded_index = None;
            self.log("原宏已不在当前组合里，已改为另存为新宏");
            self.macro_add_from_page();
            return;
        }
        self.push_undo();
        Self::normalize_macro_start(&mut self.macro_page_steps);
        let action = Action::Macro(MacroAction {
            steps: self.macro_page_steps.clone(),
            instructions: self.macro_page_instructions.clone(),
            virtual_profile: self.macro_page_virtual_profile.clone().map(Box::new),
        });
        let fps_only = self.macro_page_fps_only;
        let tail_delay_ms = self.macro_page_tail_delay_ms;
        {
            let mut g = lock_shared(&self.shared);
            if let Some(b) = g.profile.binds.get_mut(index) {
                b.key = key;
                b.action = action;
                b.fps_only = fps_only;
                // 「按后延迟」(用户 2026-10-10 第 2 条):覆盖保存时一并写回,
                // 否则"载入编辑 → 改了延迟 → 保存宏"会静默丢掉这次改动。
                b.tail_delay_ms = tail_delay_ms;
            }
        }
        self.reset_macro_draft();
        self.log("宏已保存(覆盖刚才[载入编辑]的那一条)");
    }
    fn finish_macro_recording(&mut self) {
        if let Some(rec) = self.macro_recording.take() {
            self.macro_page_steps = rec.steps;
            // 起始点不变量:第一个动作 = 用户按下第一个键的那一刻
            // (点[开始录制]到那一刻之间的空档永远不算宏的一部分)。
            Self::normalize_macro_start(&mut self.macro_page_steps);
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
            // 「按后延迟」(用户 2026-10-10 第 2 条):草稿阶段就定,入配置时照抄。
            if let Some(v) = Self::tail_delay_widget(ui, self.draft.tail_delay_ms) {
                self.draft.tail_delay_ms = v;
            }
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
            if slot.takes_chord() {
                // 系统键那类槽位(用户 2026-10-09 第 4 条):整组键一个按钮,
                // 捕获 `Ctrl+X`、显示 `Ctrl+X`,没有额外的时长/间隔参数。
                let keys = self.vk_slot_keys(&slot);
                let waiting = self.waiting_keys == Some(slot);
                if Self::keys_button(ui, waiting, &keys).clicked() {
                    self.begin_keys_capture(slot);
                }
                if !keys.is_empty() && ui.small_button("清除").clicked() {
                    self.assign_keys(slot, KeySet::new());
                }
            } else {
                let waiting = self.waiting_key == Some(slot);
                let code = self.vk_slot_keys(&slot).only().unwrap_or(0);
                if Self::key_button(ui, waiting, Some(code)).clicked() {
                    self.begin_key_capture(slot);
                }
            }
            let cancel = Self::tab_button_resp(ui, "取消选择", false, self.theme().accent);
            Self::note_cancel_zone(ui, &cancel);
            // 与[取消设置]同一条纪律:**按下**即退出捕获,不等松开 —— 否则这一下
            // 点击的"抬起"边沿会先把组合键落定成绑定(见 `cancel_bind_button`)。
            if cancel.is_pointer_button_down_on() || cancel.clicked() {
                self.vk_sel = None;
                self.cancel_key_capture();
            }
        });
        if self.waiting_keys == Some(slot) {
            ui.small("等待组合键中... 按住 Ctrl 再按另一个键,全部松开即生效(最多两个键)");
        } else if self.waiting_key == Some(slot) {
            ui.small("等待按键中... 按任意键完成改绑");
        }
    }

    /// 读取一个 KeySlot 当前持有的键(组合键槽位返回整个集合)。
    fn vk_slot_keys(&self, slot: &KeySlot) -> KeySet {
        let g = lock_shared(&self.shared);
        match slot {
            KeySlot::Toggle => g.profile.toggle_key,
            KeySlot::CursorToggle => g.profile.cursor_toggle_key,
            KeySlot::AimHold => g.profile.aim.hold_key,
            KeySlot::AimToggle => g.profile.aim.toggle_key,
            KeySlot::AimSuspend => g.profile.aim.suspend_key,
            KeySlot::RecoilTrigger => g.profile.aim.recoil.trigger_key,
            KeySlot::RecoilSwitch => g.profile.aim.recoil.switch_key,
            KeySlot::WheelDir { wheel, dir } => g
                .profile
                .wheels
                .get(*wheel)
                .and_then(|w| w.active_dirs().get(*dir).map(|(_, key)| *key))
                .unwrap_or_default(),
            KeySlot::WheelEnable(i) => g
                .profile
                .wheels
                .get(*i)
                .and_then(|w| w.temp.as_ref().map(|t| t.key))
                .unwrap_or_default(),
            KeySlot::SwitchKey(i) => g
                .switch_keys
                .get(*i)
                .map(|s| s.effective_keys())
                .unwrap_or_default(),
            KeySlot::ComboKey { combo, slot } => KeySet::single(
                g.profile
                    .combos
                    .get(*combo)
                    .and_then(|c| c.keys.get(*slot))
                    .copied()
                    .unwrap_or(0),
            ),
            KeySlot::MacroTrigger => KeySet::single(self.macro_page_key.unwrap_or(0)),
            KeySlot::MacroInstructionKey(index) => KeySet::single(
                self.macro_page_instructions
                    .get(*index)
                    .and_then(|instruction| match instruction {
                        MacroInstruction::Key { code, .. } => Some(*code),
                        _ => None,
                    })
                    .unwrap_or(0),
            ),
            KeySlot::MacroInstructionComboKey { instruction, slot } => KeySet::single(
                self.macro_page_instructions
                    .get(*instruction)
                    .and_then(|item| match item {
                        MacroInstruction::Combo { keys, .. } => keys.get(*slot).copied(),
                        _ => None,
                    })
                    .unwrap_or(0),
            ),
            KeySlot::NewBind | KeySlot::Bind(_) => KeySet::new(),
            // R4:虚拟槽位的值在宏草稿里(不是实时配置),这里读不到也不该读
            // —— 见 `assign_virtual_key` / `assign_virtual_coord`。
            KeySlot::MacroVirtualNewBind
            | KeySlot::MacroVirtualComboKey { .. }
            | KeySlot::MacroVirtualWheelDir { .. }
            | KeySlot::MacroVirtualWheelEnable(_) => KeySet::new(),
        }
    }

    /// Optional simultaneous chords.  The first key is the chord leader: it is
    /// held briefly so a following key can complete the chord without firing
    /// the leader's single-key action first.
    fn ui_combos(&mut self, ui: &mut egui::Ui) {
        let (mut enabled, combos, mapper) = {
            let g = lock_shared(&self.shared);
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
            lock_shared(&self.shared).profile.combos_enabled = enabled;
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
                        let fps_changed = ui.checkbox(&mut combo.fps_only, "仅 FPS").changed();
                        // 「按后延迟」(用户 2026-10-10 第 2 条):组合键自己的一份。
                        let tail_changed = Self::tail_delay_widget(ui, combo.tail_delay_ms)
                            .map(|v| combo.tail_delay_ms = v)
                            .is_some();
                        if action_changed || fps_changed || tail_changed {
                            snapshots.push((index, combo.clone()));
                        }
                    });
            });
        }
        self.picking = combo_pick;
        self.easing_edit = combo_easing;
        if let Some(index) = delete_combo {
            self.push_undo();
            let mut g = lock_shared(&self.shared);
            if index < g.profile.combos.len() {
                g.profile.combos.remove(index);
            }
            self.waiting_key = None;
        }
        if let Some((combo_index, slot)) = delete_slot {
            self.push_undo();
            let mut g = lock_shared(&self.shared);
            if let Some(combo) = g.profile.combos.get_mut(combo_index)
                && slot < combo.keys.len()
            {
                combo.keys.remove(slot);
            }
            self.waiting_key = None;
        }
        if let Some(index) = add_slot {
            self.push_undo();
            let mut g = lock_shared(&self.shared);
            if let Some(combo) = g.profile.combos.get_mut(index) {
                combo.keys.push(0);
            }
        }
        for (index, combo) in snapshots {
            self.push_undo();
            if let Some(target) = lock_shared(&self.shared).profile.combos.get_mut(index) {
                *target = combo;
            }
        }
        if ui.button("＋ 新增组合键").clicked() {
            self.push_undo();
            let m = self.mapper();
            let (x, y) = (m.rel_x(540), m.rel_y(960));
            lock_shared(&self.shared).profile.combos.push(KeyCombo {
                keys: vec![0, 0],
                action: Action::Tap {
                    x,
                    y,
                    duration_ms: crate::keymap::DEFAULT_TAP_DURATION_MS,
                    radius: crate::keymap::DEFAULT_RADIUS,
                },
                fps_only: false,
                tail_delay_ms: crate::keymap::DEFAULT_TAIL_DELAY_MS,
            });
            self.scroll_to_new = true;
        }
        self.apply_pending_scroll(ui);
    }

    fn ui_wheels(&mut self, ui: &mut egui::Ui) {
        let th = self.theme();
        ui.heading("轮盘(虚拟摇杆)");
        ui.label("设置[启用键]后变为临时轮盘:仅在启用期间生效,期间方向键的其它绑定让位");
        let mut to_delete: Option<usize> = None;
        let wheel_count = lock_shared(&self.shared).profile.wheels.len();

        for i in 0..wheel_count {
            ui.horizontal(|ui| {
                let temp_info = {
                    let g = lock_shared(&self.shared);
                    // 列表长度是上一次加锁时读的:引擎换组合后可能越界(W0-10),
                    // 取不到就整行不渲染
                    let Some(w) = g.profile.wheels.get(i) else {
                        return;
                    };
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
                        let mut g = lock_shared(&self.shared);
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
                            let mut g = lock_shared(&self.shared);
                            if let Some(w) = g.profile.wheels.get_mut(i) {
                                w.mode = mode;
                            }
                        }
                        self.log(format!("轮盘模式已切换为: {}", mode.label()));
                    }
                } else {
                    ui.label("双键取中点方向")
                        .on_hover_text("两个方向键同时按下时，最终响应目标 = 两键方向的中点（仍落在影响范围圆上）。");
                }

                // 启用键(设置后变为临时轮盘)
                ui.label("启用键:");
                let ek = temp.map(|(k, _)| k).unwrap_or_default();
                let waiting_e = self.waiting_keys == Some(KeySlot::WheelEnable(i));
                if Self::keys_button(ui, waiting_e, &ek).clicked() {
                    self.begin_keys_capture(KeySlot::WheelEnable(i));
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
                        let mut g = lock_shared(&self.shared);
                        if let Some(t) = g.profile.wheels.get_mut(i).and_then(|w| w.temp.as_mut()) {
                            t.mode = match t.mode {
                                TempMode::Hold => TempMode::Toggle,
                                TempMode::Toggle => TempMode::Hold,
                            };
                        }
                    }
                    if ui.button("设为永久").clicked() {
                        self.push_undo();
                        let done = {
                            let mut g = lock_shared(&self.shared);
                            match g.profile.wheels.get_mut(i) {
                                Some(w) => {
                                    w.temp = None;
                                    true
                                }
                                None => false,
                            }
                        };
                        if done {
                            self.log("已设为永久轮盘");
                        }
                    }
                } else {
                    ui.label("(永久)");
                }
            });
            let (dirs, kind, manuals) = {
                let g = lock_shared(&self.shared);
                // 索引失效(引擎刚换组合)则跳过这一行,继续渲染后面的
                let Some(w) = g.profile.wheels.get(i) else {
                    continue;
                };
                // 手动终点按下标取(自定义/执行轮盘的 active_dirs 与 directions 一一对应)
                let manuals: Vec<Option<(f32, f32)>> = (0..8)
                    .map(|d| w.directions.get(d).and_then(|x| x.manual))
                    .collect();
                (w.active_dirs(), w.kind, manuals)
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
                        let mut g = lock_shared(&self.shared);
                        if let Some(w) = g.profile.wheels.get_mut(i) {
                            w.set_direction_count(count);
                        }
                    }
                    ui.label("最多 8 个；同时只接受最早按下的 2 个方向，取两键方向的中点");
                });
            }
            for (d, (angle, code)) in dirs.into_iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(wheel_dir_label(angle, d));
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
                            let mut g = lock_shared(&self.shared);
                            if let Some(dir) = g
                                .profile
                                .wheels
                                .get_mut(i)
                                .and_then(|w| w.directions.get_mut(d))
                            {
                                dir.angle_deg = a;
                            }
                        }
                    }
                    ui.label("按键:");
                    let waiting = self.waiting_keys == Some(KeySlot::WheelDir { wheel: i, dir: d });
                    if Self::keys_button(ui, waiting, &code).clicked() {
                        self.begin_keys_capture(KeySlot::WheelDir { wheel: i, dir: d });
                    }
                    // 「设置位置」:在截图上直接点一个终点,替代"角度 × 影响范围"。
                    // 手改点可以超出/不足影响范围圆 —— 影响范围只是基准,
                    // 之后改它不会挪动手改点;只有「重置」才会把点贴回基准圆。
                    if kind != WheelKind::Standard {
                        let slot = CoordSlot::WheelDirEnd { wheel: i, dir: d };
                        let waiting_pos = self.picking == Some(slot);
                        match manuals.get(d).copied().flatten() {
                            Some((mx, my)) => {
                                let (px, py) = self.mapper().point(mx, my);
                                ui.colored_label(th.ok, format!("位置:自定义({px},{py})"))
                                    .on_hover_text(
                                        "这个方向已手改终点：触点就推到这里，不再受角度/影响范围影响。",
                                    );
                                if ui
                                    .button("重置")
                                    .on_hover_text("清除手改终点，重新贴回影响范围圆（之后改影响范围才会再次生效）")
                                    .clicked()
                                {
                                    self.push_undo();
                                    {
                                        let mut g = lock_shared(&self.shared);
                                        if let Some(dir) = g
                                            .profile
                                            .wheels
                                            .get_mut(i)
                                            .and_then(|w| w.directions.get_mut(d))
                                        {
                                            dir.manual = None;
                                        }
                                    }
                                    self.log("已重置该方向的手改终点(回到影响范围圆)");
                                }
                            }
                            None => {
                                if ui
                                    .button(if waiting_pos {
                                        "点击截图..."
                                    } else {
                                        "设置位置"
                                    })
                                    .on_hover_text(
                                        "在截图上点一个点作为这个方向的终点：可以超出或不足影响范围圆。",
                                    )
                                    .clicked()
                                {
                                    self.begin_pick(slot);
                                }
                            }
                        }
                    }
                });
            }
            // 半径 / 影响范围:两行放不下(双向滚动区里横排太长),故半径独占一行、
            // 影响范围另起一行,并给出"实际推出的像素距离"便于和游戏里的判定圈对照。
            ui.horizontal(|ui| {
                let waiting_p = self.picking == Some(CoordSlot::WheelCenter(i));
                let m = self.mapper();
                {
                    let mut g = lock_shared(&self.shared);
                    // 撤销快照只取这一个轮盘(理由同按键区:避免每帧深拷贝整份配置)
                    let before = g.profile.wheels.get(i).cloned();
                    let mut wheel_edit = false;
                    // 索引失效(引擎刚换组合)则不再渲染这一行的参数编辑
                    let Some(w) = g.profile.wheels.get_mut(i) else {
                        return;
                    };
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
                    let mut g = lock_shared(&self.shared);
                    if let Some(w) = g.profile.wheels.get_mut(i) {
                        w.scope = crate::keymap::DEFAULT_WHEEL_SCOPE;
                    }
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
            // 下标核查见 remove_indexed(引擎可能刚换过组合)
            let removed = {
                let mut g = lock_shared(&self.shared);
                remove_indexed(&mut g.profile.wheels, i)
            };
            if !removed {
                self.log("该轮盘已不在当前组合里(组合刚被切换),未删除");
            } else {
                if self.resizing == Some(ResizeTarget::Wheel(i)) {
                    self.resizing = None;
                }
                self.log("已删除轮盘");
            }
        }
        if ui.button("新增轮盘").clicked() {
            self.push_undo();
            // 半径固定 150px、落点避开已有摇杆(见 keymap::Wheel::new_default /
            // next_wheel_spot):新建出来的摇杆不该比用户辛苦调小的那个大一圈,
            // 也不该叠在别的摇杆上让人分不清
            let m = self.mapper();
            let (cx, cy) = {
                let g = lock_shared(&self.shared);
                crate::keymap::next_wheel_spot(&g.profile.wheels)
            };
            let wheel = Wheel::new_default(&m, cx, cy);
            let r_px = m.len(wheel.radius);
            let n = {
                let mut g = lock_shared(&self.shared);
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
            let g = lock_shared(&self.shared);
            crate::keymap::next_wheel_spot(&g.profile.wheels)
        };
        let wheel = Wheel::new_default(&m, cx, cy);
        let r_px = m.len(wheel.radius);
        let n = {
            let mut g = lock_shared(&self.shared);
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
    /// `virtual_src` = 这张画布该画**扩展宏弹窗那份虚拟键位表**还是实时配置。
    ///
    /// 由调用方决定(见 [`Self::ui_shot_canvas`] 的 `virtual_layer` 参数):
    /// 弹窗自己的那块画布恒为 true,主界面下方面板恒为 false,
    /// 截图小窗看它是"从弹窗开的小窗"还是"从主界面开的小窗"。
    fn draw_overlay(&self, ui: &egui::Ui, rect: egui::Rect, scale: f32, virtual_src: bool) {
        use egui::{Align2, FontId, Stroke, vec2};
        use theme::size;
        let painter = ui.painter();
        let to_screen = |x: i32, y: i32| rect.min + vec2(x as f32 * scale, y as f32 * scale);
        let short_name = |code: u16| key_name(code).replace("KEY_", "");

        // 坐标换算器必须**先**构造好(它内部会加配置锁);
        // 若先拿到配置锁再调 self.mapper() 会自锁,界面会直接卡死。
        let m = self.mapper();
        let g = lock_shared(&self.shared);
        // R4(2026-10-08):浮层的"内容来源"。
        //
        // 平时画的是实时配置;当**扩展宏弹窗**正在取点(`picking` 是虚拟取点、
        // 而且弹窗还开着)时,改画弹窗里那份虚拟键位表 —— 用户要"在截图上给
        // 虚拟按键/组合键/轮盘/锚点取点",就得先看见它们。
        //
        // 这条切换是**临时**的:取点一结束(放到点上 / 取消取点 / 关窗 / 保存)
        // `picking` 立刻不再是虚拟取点,下一帧这里就回落到实时配置 ——
        // 截图上的标识于是完全复原,不留"键位垃圾"(用户 2026-10-08 明确要求)。
        // 于是弹窗没开、或没在取点时,这里的观感与加这段代码之前一模一样。
        //
        // 说明:`resizing` 与 `picking` 互斥(`begin_pick`/`begin_resize*` 各自清掉
        // 对方),所以切到虚拟层时不会出现"按实时下标高亮的黄色圈"这种错位。
        let virtual_editor = if virtual_src {
            self.macro_virtual_editor.as_ref()
        } else {
            None
        };
        let src: &Profile = match virtual_editor {
            Some(e) => &e.profile,
            None => &g.profile,
        };
        // 最近一次新增的虚拟项(只画一圈强调 + 「新」角标,不改数据)
        let newest = virtual_editor.and_then(|e| e.newest);
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

        // 「新」角标(用户 2026-10-10 第 4 条"每次新增点需标明"):给最近一次新增
        // 或刚取点的那个虚拟项套一圈强调 + 一个「新」字,用户一眼就能找到它。
        // 纯画法,不改任何数据;`newest` 为 None(非虚拟层/刚换过基底)时从不触发。
        let mark_new = |p: egui::Pos2, r: f32| {
            painter.circle_stroke(p, r + 5.0, Stroke::new(3.0, theme::casing(th.accent)));
            painter.circle_stroke(p, r + 5.0, Stroke::new(1.5, th.accent));
            theme::paint_label(
                painter,
                p + vec2(0.0, -(r + 18.0)),
                Align2::CENTER_CENTER,
                "新",
                FontId::proportional(size::LABEL_FONT),
                th.accent,
            );
        };

        // 键位(含草稿标记)仅在"全部/仅键位"时显示
        if self.overlay_filter.keys {
            let fps_overlay_active = g.aim_live.mode_active && !g.aim_live.suspended;
            for (i, b) in src.binds.iter().enumerate() {
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
                        if newest == Some(MacroVirtualPick::Bind(i)) {
                            mark_new(p, r);
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
            // R4:浮层正在画扩展宏的虚拟层时不画实时草稿 —— 两套标记混在一张截图上
            // 谁都认不出是谁的(实时草稿本身没被改动,取点结束照旧显示)。
            let draft_kind = if !virtual_src
                && (self.draft_active || self.waiting_key == Some(KeySlot::NewBind))
            {
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
            for (ci, combo) in src.combos.iter().enumerate() {
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
                        if newest == Some(MacroVirtualPick::Combo(ci)) {
                            mark_new(p, r);
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
        // 宏清单(R4):永远读**实时配置**,不受上面的虚拟层切换影响 ——
        // 虚拟层按设计不继承宏(`sanitize_virtual_profile` 会把宏步骤剔掉),
        // 而这一块说的是"本机绑了哪些宏",属于实时配置的说明。
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
        if self.overlay_filter.aim && src.aim.anchor_set() {
            let aim = &src.aim;
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
            let label = if aim.hold_key.is_empty() {
                "瞄准锚点".to_string()
            } else {
                format!("瞄准锚点 [{}]", aim.hold_key.label())
            };
            theme::paint_label(
                painter,
                p + vec2(0.0, -(arm + 6.0)),
                Align2::CENTER_CENTER,
                &label,
                FontId::proportional(size::LABEL_FONT),
                theme::tone_text(tone),
            );
            if newest == Some(MacroVirtualPick::AimAnchor) {
                mark_new(p, ring);
            }
        }

        // 轮盘按过滤条件显示;临时轮盘用虚线圆环区分
        if self.overlay_filter.wheels_perm || self.overlay_filter.wheels_temp {
            for (wi, w) in src.wheels.iter().enumerate() {
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
                if newest == Some(MacroVirtualPick::WheelCenter(wi)) {
                    mark_new(c, r);
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
                // 手改终点(在方向行点过「设置位置」的那个点):从圆心拉一条虚线到终点,
                // 终点画**一个圆圈**(不是圆点),并标出是哪个方向。它**不是**
                // "角度 × 影响范围"的结果,所以用与影响范围环不同的颜色,
                // 一眼能看出哪些方向被手改过。
                //
                // 圆圈的半径 = min(影响范围, 按键大小),见 [`wheel_dir_end_radius_px`]
                // —— 那个函数就是"落点圆圈大小"的**预留接口**(现固定、未来可调)。
                for (d, dir) in w.directions.iter().enumerate() {
                    let Some((mx, my)) = dir.manual else { continue };
                    let (px, py) = m.point(mx, my);
                    let ep = to_screen(px, py);
                    let ink = theme::tone_color(th.key_hold, tone);
                    for shape in egui::Shape::dashed_line(
                        &[c, ep],
                        Stroke::new(3.5, theme::casing(ink)),
                        5.0,
                        4.0,
                    ) {
                        painter.add(shape);
                    }
                    for shape in egui::Shape::dashed_line(&[c, ep], Stroke::new(1.5, ink), 5.0, 4.0)
                    {
                        painter.add(shape);
                    }
                    let end_r = wheel_dir_end_radius_px(&m, w, scale);
                    painter.circle_filled(ep, end_r, theme::with_alpha(ink, 45));
                    painter.circle_stroke(
                        ep,
                        end_r,
                        Stroke::new(size::KEY_STROKE + 2.5, theme::casing(ink)),
                    );
                    painter.circle_stroke(ep, end_r, Stroke::new(size::KEY_STROKE, ink));
                    theme::paint_label(
                        painter,
                        ep + vec2(0.0, -end_r - 10.0),
                        Align2::CENTER_CENTER,
                        &wheel_dir_label(dir.angle_deg, d),
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
                let g = lock_shared(&self.shared);
                match g.profile.binds.get(i).map(|b| &b.action) {
                    Some(Action::Swipe(s)) => s.easing,
                    _ => {
                        self.easing_edit = None;
                        return;
                    }
                }
            }
            EasingEditTarget::Combo(i) => {
                let g = lock_shared(&self.shared);
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
            let mut g = lock_shared(&self.shared);
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
        let g = lock_shared(&self.shared);
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
        let g = lock_shared(&self.shared);
        g.control
            .as_ref()
            .map(|c| (c.screen_w, c.screen_h))
            .filter(|(w, h)| *w > 0 && *h > 0)
    }

    /// W2-2 坐标空间守卫:截图与注入空间的纵横比不一致时返回证据。
    /// 每帧现算,没有需要清理的状态 —— 两空间一对齐(重新截图/重查尺寸)自动恢复。
    fn space_guard(&self) -> Option<SpaceMismatch> {
        let (_, w, h) = self.shot.as_ref()?;
        space_mismatch((*w, *h), self.inject_space()?)
    }

    /// W2-2 一键对齐:以当前截图空间为准 —— 注入坐标空间(含协议声明尺寸)
    /// 与配置里记录的"设计尺寸"都重设为截图尺寸。
    ///
    /// 适用场景:截图是刚拍的、屏幕确实转了,而控制通道还停在旧方向。
    /// 截图本身是旧的(视频流没跟上)时该用[重新查询屏幕尺寸]或重新截图,
    /// 面板提示里已写明这两条路的区别。
    fn adopt_shot_space(&mut self, m: SpaceMismatch) {
        let (sw, sh) = m.shot;
        self.sync_display_space(sw, sh);
        {
            let mut g = lock_shared(&self.shared);
            if g.profile.format_version >= crate::keymap::PROFILE_VERSION {
                g.profile.screen = Some((sw, sh));
            }
        }
        self.log(format!(
            "已按截图对齐坐标空间: 注入与设计尺寸 → {sw}x{sh}(可重新[截取手机屏幕]复核)"
        ));
    }

    /// 坐标换算器:配置坐标(相对值) <-> 当前屏幕像素。
    /// 屏幕尺寸未知时按 1080x2400 估算,只影响界面显示,不影响注入。
    ///
    /// 内部会加配置锁,调用方**不要**在已持有配置锁时调用它(会自锁)。
    fn mapper(&self) -> Mapper {
        let unit = lock_shared(&self.shared).profile.coord_unit();
        Mapper::new(unit, self.screen_size().unwrap_or((1080, 2400)))
    }

    /// 当前主题的语义色(界面里一律用它,不写死颜色)
    fn theme(&self) -> Theme {
        lock_shared(&self.shared).profile.look.theme()
    }

    /// 当前界面风格(默认/可视化)。页面内容分支一律经它判断。
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
        let mut g = lock_shared(&self.shared);
        if g.profile.look.style != style {
            g.profile.look.style = style;
        }
    }

    /// 请求切换界面风格:写进配置并立即落盘(look.json),然后关闭窗口。
    /// main.rs 的重启循环随后用新风格重开 UI(见文件头 STYLE_RESTART 的说明)。
    fn request_style_restart(&mut self, ctx: &egui::Context, new_style: theme::UiStyle) {
        self.style = new_style;
        {
            let mut g = lock_shared(&self.shared);
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
        lock_shared(&self.shared).profile.look.clone()
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
        let mut g = lock_shared(&self.shared);
        if g.profile.format_version >= crate::keymap::PROFILE_VERSION {
            g.profile.screen = Some(space);
        }
    }

    /// 截图尺寸就是"当前屏幕方向"下的真实触摸坐标空间,用它校正控制通道。
    ///
    /// 只校正坐标空间,**绝不改动已取好的锚点**:手机临时切到别的方向
    /// (例如横屏配好后弹了一下竖屏、再切回来)必须原样可用。
    /// 锚点暂时超出当前空间时,由注入前的钳制兜底,并在界面上给出提示即可。
    fn sync_display_space(&mut self, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        let mut changed = false;
        {
            let mut g = lock_shared(&self.shared);
            if let Some(c) = g.control.as_mut() {
                if (c.screen_w, c.screen_h) != (w, h) {
                    // 走 set_screen:它会同时更新写线程用的原子量。
                    // 直接写字段的话,协议里声明的尺寸会永远停在连接时的那个值。
                    c.set_screen(w, h);
                    changed = true;
                }
            }
        }
        if changed {
            self.log(format!("触摸坐标空间已更新为 {w}x{h}"));
        }
    }

    /// 配置区(保存/另存为/选用/新建/默认/检测/保存日志)。
    /// 默认与可视化风格共用这一份(放在左栏)。
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
    /// 默认与可视化风格共用这一份(放在左栏)。
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

    /// 总开关键 + [映射时屏蔽原键] 勾选(默认/可视化风格共用,放在左栏中段)。
    /// 复选框只有 Linux 有实际语义(EVIOCGRAB 独占 /dev/input),Windows 上
    /// 隐藏并留一行说明(2026-10-07 V1-2 ③ 决策:死 UI 不留)。
    fn ui_toggle_key_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("总开关键:");
            // 用户 2026-10-09(第 4 条):总开关键与其它系统键统一成一个按钮,
            // 支持 `Ctrl+X` 这样的组合(顺序无关),没有额外参数。
            let tk = { lock_shared(&self.shared).profile.toggle_key };
            let waiting = self.waiting_keys == Some(KeySlot::Toggle);
            if Self::keys_button(ui, waiting, &tk).clicked() {
                self.begin_keys_capture(KeySlot::Toggle);
            }
            if !tk.is_empty() && ui.small_button("清除").clicked() {
                self.assign_keys(KeySlot::Toggle, KeySet::new());
            }
        });
        // 「按后延迟」总开关(用户 2026-10-10 晚追加要求:整项**改成可选功能**)。
        // 摆在这里与总开关键同一段:它俩都是**全局行为开关**,不是某一条键位的参数;
        // 逐条的数字框仍在各自那条键位/组合键/宏自己的行上(见 `tail_delay_widget`)。
        {
            let mut on = lock_shared(&self.shared).profile.tail_delay_enabled;
            if ui
                .checkbox(&mut on, "按后延迟")
                .on_hover_text(
                    "默认关闭。关闭时整项不生效 —— 每条键位/组合键/宏自己的毫秒值\
                     原样保留,但一律按 0(不等)处理,行为与没有这个功能时完全一致。\n\
                     打开后还要那条自己填了 >0 的数才真等:两层都愿意才生效。\n\
                     语义:那一条上一次**抬起**之后再等这么久才准下一次按下;\
                     冷却里按下的那一次不丢,会被推迟到冷却结束再执行。\n\
                     这个开关随当前键位组合(方案)一起保存 —— 每个方案各自一份。",
                )
                .changed()
            {
                self.push_undo();
                lock_shared(&self.shared).profile.tail_delay_enabled = on;
                self.log(if on {
                    "已启用「按后延迟」:逐条数值开始生效(各条自己填 >0 才真等)"
                } else {
                    "已关闭「按后延迟」:全部按“不等”处理,各条数值原样保留"
                });
            }
        }
        #[cfg(not(windows))]
        ui.checkbox(
            &mut self.grab_enabled,
            "映射时屏蔽原键(grab)\n注意:开启后映射期间键盘只对本程序生效",
        );
        // Windows 走全局钩子(拦截后不转发),没有"独占设备"这一档 ——
        // 勾选不产生任何效果,所以这里只说明去处。
        #[cfg(windows)]
        ui.small("映射时屏蔽原键(grab)仅 Linux 可用(EVIOCGRAB 独占设备);Windows 无需该项。");
    }

    /// 引擎运行状态一行(触点占用 / 因池满被放弃)。
    /// 默认/可视化风格共用(放在左栏中段)。
    fn ui_engine_status(&mut self, ui: &mut egui::Ui) {
        let g = lock_shared(&self.shared);
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

        // 捕获层健康:钩子回调耗时 p99 超过预警线时把原因摆出来。
        // 系统对低级钩子有 300ms 超时线,越接近它越可能丢事件 ——
        // 这条平时不出现,出现时一定是真出过毛刺(最近一轮 5 秒汇总)。
        if self.hook_lag_flag.load(Ordering::Relaxed) {
            ui.colored_label(
                th.danger,
                "⚠ 捕获层延迟异常:按键回调耗时超标,可能丢事件(详见运行日志)",
            )
            .on_hover_text(
                "Windows 对底层按键回调有 300ms 的超时线:回调太慢时系统会静默\n\
                 丢弃事件,严重时直接摘掉整个钩子(表现为按键突然全部失灵)。\n\
                 最近一轮统计里回调 p99 超过 5ms 就会亮这条警示。\n\
                 常见原因:CPU 被占满、安全软件也挂了同类钩子。",
            );
        }

        // 捕获层心跳(W1-4):过去"钩子被系统静默摘除"的表现是按键突然全部
        // 失灵,没有任何提示、只能重启程序;现在心跳会发现并就地重装。
        // 状态 2 会在自愈后 dwell 展示几秒(事件型,不是持续状态)。
        match self.hb_flag.load(Ordering::Relaxed) {
            1 => {
                ui.colored_label(
                    th.danger,
                    "⚠ 捕获层心跳中断:钩子线程无响应,事件可能正在丢失(详见运行日志)",
                )
                .on_hover_text(
                    "低级钩子的回调跑在捕获线程自己的消息循环里:那里一旦卡住,\n\
                     系统会先丢事件、再静默摘掉整个钩子。3 秒没有心跳就会亮这条。\n\
                     常见原因:CPU 被占满、安全软件的钩子拖慢整个钩子链。",
                );
            }
            2 => {
                ui.colored_label(
                    th.warn,
                    "⚠ 捕获层钩子曾失效:已自动重装恢复(期间可能丢过事件)",
                )
                .on_hover_text(
                    "Windows 会静默摘除超时的低级钩子,不通知程序 —— 以前的表现\n\
                     是按键突然全部失灵,只能重启。心跳探针发现无回音后已就地重装。\n\
                     若这条反复出现,检查安全软件,或把本程序设为管理员运行。",
                );
            }
            _ => {}
        }
    }

    /// 运行日志列表(默认/可视化风格共用,放在左栏底部)
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
    // 顶栏调用这几个方法,
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
        let mut launch_args = self.prepare_scrcpy_args();
        // 显示真实投屏帧率(2026-10-06 反馈):scrcpy 的 FPS 计数器每秒把
        // 实况帧率打到 stdout(见 adb::parse_scrcpy_fps),"显示帧率"里的
        // "投屏帧率"一行就是它。用户参数里已有 --print-fps 则尊重用户,不重复加。
        if !launch_args.split_whitespace().any(|a| a == "--print-fps") {
            if !launch_args.trim().is_empty() {
                launch_args.push(' ');
            }
            launch_args.push_str("--print-fps");
        }
        self.stream_fps = None; // 旧进程的读数作废,等新进程的第一行
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
            let mut g = lock_shared(&self.shared);
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
    /// 放在顶栏(默认/可视化风格共用这一份)。
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

    /// 外观设置面板:风格 / 配色 / 控件密度 / 背景图(改动可撤销,随配置保存)
    fn ui_look(&mut self, ui: &mut egui::Ui) {
        let mut pick_bg = false;
        let mut edit = false;
        let mut style_restart: Option<theme::UiStyle> = None;
        // 撤销快照的"改动前"只需要本页会改的那一份(`Look`,~17ns/帧);
        // 整份配置(~5µs/帧,见 W2-3 度量)推迟到真的改了东西那一刻再组装。
        let look_before = lock_shared(&self.shared).profile.look.clone();
        {
            let mut g = lock_shared(&self.shared);
            let look = &mut g.profile.look;

            // ---- 界面风格(默认/可视化) ----
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
            // 组装"改动前"的整份配置:除 `look` 用上面那份旧的,其余字段取当前值。
            // 本页不会动其余字段,所以这与"进页时整份克隆"等价;若同一帧里别处
            // 也改了配置,那些改动会保留 —— 撤销只回退外观,这正是想要的粒度。
            let mut before = lock_shared(&self.shared).profile.clone();
            before.look = look_before;
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
        let res = serde_json::to_string_pretty(look)
            .map_err(|e| e.to_string())
            .and_then(|text| write_atomic(&path, &text).map_err(|e| e.to_string()));
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

    /// 鼠标瞄准(FPS / 开放世界)。内容**直接展开**(不再套折叠栏 ——
    /// 用户明确要求把它放出来,少一层点击)。
    fn ui_aim(&mut self, ui: &mut egui::Ui, captured: bool) {
        ui.heading("鼠标瞄准（FPS / 开放世界）");
        self.ui_aim_body(ui, captured);
    }

    fn ui_aim_body(&mut self, ui: &mut egui::Ui, captured: bool) {
        let th = self.theme();
        let gamepad_mode = {
            let mode = lock_shared(&self.shared).profile.aim.input_mode;
            matches!(
                mode,
                ViewInputMode::VirtualGamepadContinuous | ViewInputMode::VirtualGamepadSegmented
            )
        };
        if gamepad_mode {
            ui.label("把鼠标的相对位移映射成虚拟 Xbox 手柄右摇杆。游戏必须支持手柄右摇杆视角；该模式不需要锚点。");
            ui.label("用法:先开启总映射,连接控制通道,按 FPS 模式开关键进入。");
        } else {
            ui.label("把鼠标的相对位移映射成手机上的手指拖动。FPS 模式用于开镜/射击；开放世界模式用于无需射击的无限水平转向。");
            ui.label("锚点默认不设置。锚点不是游戏准星，而是虚拟手指落下的起点；应放在游戏 UI 之外的干净区域。");
            ui.label("用法:先开启总映射，再到“瞄准锚点”取点，按 FPS 模式开关键进入。");
        }

        // —— 生效条件自检:直接告诉用户"现在为什么没反应" ——
        let (mapping_enabled, aim_on, anchor_ok, hold_key, connected, space, live) = {
            let g = lock_shared(&self.shared);
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
                let g = lock_shared(&self.shared);
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
                "映射总开关已开启（FPS 模式只在映射开启后生效）".to_string(),
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
        } else if !hold_key.is_empty() {
            ui.colored_label(
                th.warn,
                format!(
                    "→ 已就绪,但绑定了[按住才瞄准]:需按住 {} 时才会转动视角",
                    hold_key.label()
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
        let mut pick_recoil_key = false;
        let mut pick_recoil_switch = false;
        // 本帧被点[取消设置]且当时**正在等待捕获**的槽位(用户 2026-10-10 第 3 条):
        // 只取消这次捕获,不碰已有绑定。清空型(已绑定)不用它 —— 直接在上面写配置。
        let mut cancel_bind: Option<KeySlot> = None;
        // 本帧被点[上一档/下一档/第 N 档]要切到的挡位:挡位是引擎侧运行状态,
        // 界面只能提一个请求(见 engine::Shared::recoil_gear_req 的注释)。
        let mut recoil_gear_req: Option<usize> = None;
        let mut toggled = false;
        // 本帧修改前的快照:面板里任何一处改动都记一次撤销。
        // 连续拖动的数值控件只在"开始编辑"那一帧记录,避免每帧都产生一个撤销步。
        // 快照只要本页会改的那一份(`Aim`),整份配置推迟到真的改了再组装(W2-3)。
        let undo_before_aim: Aim;
        let mut undo_needed = false;
        // 锚点以像素显示(存储是相对值)
        let am = self.mapper();
        // 「恢复默认」的目标:手机屏幕正中央(仍按配置的坐标单位换算)。
        // 在加锁之前算好 —— `mapper()` 要读配置,持锁时再取会自锁。
        let default_anchor = (am.rel_x((am.w / 2.0) as i32), am.rel_y((am.h / 2.0) as i32));

        {
            let mut g = lock_shared(&self.shared);
            undo_before_aim = g.profile.aim.clone();
            // 压枪(V2-1)小节要读 aim_live 的运行态(是否生效/当前档),
            // 在 `aim` 的可变借用建立之前先拷一份(AimLive 是 Copy)。
            let recoil_live = g.aim_live;
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
                    aim.hold_key = KeySet::new();
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
                // [已废弃 2026-10-08] 后两个选项(虚拟手柄右摇杆 连续/分段回中)不再维护:
                // 仍然可选、行为不变,但不会再被修复或扩展(见 keymap.rs `ViewInputMode`)。
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
                ui.heading("瞄准锚点");
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
                    if ui
                        .button("恢复默认")
                        .on_hover_text("把锚点放回手机屏幕正中央（新建配置仍不自动设置锚点）")
                        .clicked()
                    {
                        aim.anchor_x = default_anchor.0;
                        aim.anchor_y = default_anchor.1;
                        undo_needed = true;
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
                        .checkbox(&mut aim.boundary, "限制在屏幕边界内")
                        .on_hover_text("关闭后为「无边界」：触点推不动时会无缝抬指/重按并接着推，上下左右都能无限自由旋转。触点始终留在屏幕内，不会推到屏幕外被系统丢弃。")
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
                        ui.label(
                            "（无边界模式下推到此半径就抬指重按；贴边时改按到屏幕边缘的距离）",
                        );
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
                if ui
                    .checkbox(&mut aim.wheel_zoom, "滚轮缩放(双指捏合)")
                    .on_hover_text(
                        "仅 FPS 模式生效：上滚 = 两指张开(放大)，下滚 = 两指捏合(缩小)。\
                         每齿注入一段瞬发双指手势(按下旧指距 → 移动到新指距 → 抬起)，\n\
                         屏幕上不会留下手指；缩放围绕锚点进行。勾选后滚轮在本程序内被拦截，\
                         不再触发系统滚动。",
                    )
                    .changed()
                {
                    undo_needed = true;
                }
                ui.label("每齿:");
                let r = ui
                    .add(
                        egui::DragValue::new(&mut aim.wheel_zoom_step)
                            .range(5.0..=40.0)
                            .speed(0.5)
                            .suffix(" %"),
                    )
                    .on_hover_text("一次滚轮齿改变指距的百分比(指数步进，缩小与放大对称)。");
                if r.drag_started() || r.gained_focus() {
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
                    let waiting = self.waiting_keys == Some(KeySlot::AimHold);
                    if Self::keys_button(ui, waiting, &hk).clicked() {
                        pick_hold_key = true;
                    }
                    if (waiting || !hk.is_empty()) && Self::cancel_bind_button(ui, waiting) {
                        if waiting {
                            cancel_bind = Some(KeySlot::AimHold);
                        } else {
                            aim.hold_key = KeySet::new();
                            undo_needed = true;
                        }
                    }
                    if ui
                        .small_button("用右键")
                        .on_hover_text("开镜时才转动视角(按住右键瞄准)")
                        .clicked()
                    {
                        aim.hold_key = KeySet::single(crate::keymap::BTN_RIGHT);
                        undo_needed = true;
                    }
                    ui.label("(可绑鼠标右键,开镜时才动视角)");
                });
            }

            ui.horizontal(|ui| {
                ui.label("FPS 模式开关键:");
                let tk = aim.toggle_key;
                let waiting = self.waiting_keys == Some(KeySlot::AimToggle);
                if Self::keys_button(ui, waiting, &tk).clicked() {
                    pick_toggle_key = true;
                }
                if (waiting || !tk.is_empty()) && Self::cancel_bind_button(ui, waiting) {
                    if waiting {
                        cancel_bind = Some(KeySlot::AimToggle);
                    } else {
                        aim.toggle_key = KeySet::new();
                        undo_needed = true;
                    }
                }
                ui.label("(只在总映射开启后可进入/退出)");
            });
            ui.horizontal(|ui| {
                ui.label("按住才退出:");
                let sk = aim.suspend_key;
                let waiting = self.waiting_keys == Some(KeySlot::AimSuspend);
                if Self::keys_button(ui, waiting, &sk).clicked() {
                    pick_suspend_key = true;
                }
                if (waiting || !sk.is_empty()) && Self::cancel_bind_button(ui, waiting) {
                    if waiting {
                        cancel_bind = Some(KeySlot::AimSuspend);
                    } else {
                        aim.suspend_key = KeySet::new();
                        undo_needed = true;
                    }
                }
                ui.label("(按住暂时退出 FPS,恢复普通映射并显示鼠标;松开回到 FPS)");
            });

            // ---- 压枪 / 后坐力补偿(V2-1;产品立场:默认关闭、UI 明示) ----
            // 参数语义对齐 K2er《鼠标宏》专页(原文抄在 keymap::Recoil 的注释里)。
            ui.separator();
            ui.horizontal(|ui| {
                if ui
                    .checkbox(&mut aim.recoil.enabled, "后坐力补偿")
                    .on_hover_text(
                        "按住触发键期间,按[控制频率]持续把视角向下拉,补偿枪械后坐力。\n\
                         需要配合瞄准模式使用(瞄准未激活时不生效);默认关闭、不改变任何既有行为。",
                    )
                    .changed()
                {
                    undo_needed = true;
                }
                if aim.recoil.enabled && recoil_live.recoil_active {
                    ui.colored_label(th.ok, "生效中");
                }
            });
            if aim.recoil.enabled {
                let n_strengths = aim.recoil.strengths.len();
                let cur_idx = if n_strengths == 0 {
                    0
                } else {
                    recoil_live.recoil_index.min(n_strengths - 1)
                };
                ui.horizontal(|ui| {
                    ui.label("触发键:");
                    let rk = aim.recoil.trigger_key;
                    let waiting = self.waiting_keys == Some(KeySlot::RecoilTrigger);
                    if Self::keys_button(ui, waiting, &rk).clicked() {
                        pick_recoil_key = true;
                    }
                    if (waiting || !rk.is_empty()) && Self::cancel_bind_button(ui, waiting) {
                        if waiting {
                            cancel_bind = Some(KeySlot::RecoilTrigger);
                        } else {
                            aim.recoil.trigger_key = KeySet::new();
                            undo_needed = true;
                        }
                    }
                    if ui
                        .small_button("用左键")
                        .on_hover_text("射击键一般是鼠标左键(K2er 原文:一般是鼠标左键)")
                        .clicked()
                    {
                        aim.recoil.trigger_key = KeySet::single(crate::keymap::BTN_LEFT);
                        undo_needed = true;
                    }
                });
                // 挡位切换键(用户 2026-10-10 第 3 条):与上面几个系统键同一个控件
                // ([`Self::keys_button`])—— 单键或最多两键的组合都收得下,按一下换一档。
                ui.horizontal(|ui| {
                    ui.label("挡位切换键:");
                    let sk = aim.recoil.switch_key;
                    let waiting = self.waiting_keys == Some(KeySlot::RecoilSwitch);
                    if Self::keys_button(ui, waiting, &sk).clicked() {
                        pick_recoil_switch = true;
                    }
                    if (waiting || !sk.is_empty()) && Self::cancel_bind_button(ui, waiting) {
                        if waiting {
                            cancel_bind = Some(KeySlot::RecoilSwitch);
                        } else {
                            aim.recoil.switch_key = KeySet::new();
                            undo_needed = true;
                        }
                    }
                    ui.label("(按一下换一档,循环;按住它时滚轮也能换档)");
                });
                ui.horizontal(|ui| {
                    ui.label("控制频率:");
                    let rr = ui
                        .add(
                            egui::DragValue::new(&mut aim.recoil.rate_hz)
                                .range(1.0..=240.0)
                                .speed(1.0)
                                .suffix(" 次/秒"),
                        )
                        .on_hover_text("每秒控制的次数(K2er 语义);默认 60。");
                    if rr.drag_started() || rr.gained_focus() {
                        undo_needed = true;
                    }
                    ui.label(format!(
                        "当前强度:第 {} 档(共 {} 档)",
                        cur_idx + 1,
                        n_strengths.max(1)
                    ));
                });
                // 界面手动换档(用户 2026-10-10 第 3 条:"支持界面手动切换挡位")。
                // 与换档键、滚轮换档是**同一档位**:都落引擎侧那一个 `recoil_index`。
                // 它是运行状态而不是配置 —— 所以不记撤销;配置里存的只有下面的档位表。
                ui.horizontal(|ui| {
                    ui.label("手动换档:");
                    if ui
                        .add_enabled(n_strengths >= 2, egui::Button::new("◀ 上一档"))
                        .clicked()
                    {
                        recoil_gear_req = Some((cur_idx + n_strengths - 1) % n_strengths);
                    }
                    if ui
                        .add_enabled(n_strengths >= 2, egui::Button::new("下一档 ▶"))
                        .clicked()
                    {
                        recoil_gear_req = Some((cur_idx + 1) % n_strengths);
                    }
                    ui.label("(点下面某档的[第 n 档]也能直接切过去)");
                });
                // [＋ 添加一档]与[滚轮换档]放在档位表**外面**:档位多时表是收起的,
                // 添加按钮跟着被藏起来就等于"越用越加不了档"。
                let mut add_gear = false;
                ui.horizontal(|ui| {
                    if ui
                        .button("＋ 添加一档")
                        .on_hover_text("K2er:控制强度可以增加多个强度")
                        .clicked()
                    {
                        add_gear = true;
                    }
                    ui.checkbox(&mut aim.recoil.wheel_switch, "滚轮换档")
                        .on_hover_text(
                            "触发键(或挡位切换键)按住期间,滚轮从\"缩放\"改为切换控制强度档\n\
                             (K2er:鼠标滚轮改变强度);这个键没按住时滚轮仍是缩放。\n\
                             两者都想用滚轮时,补偿换档优先。",
                        );
                });
                if add_gear {
                    let last = aim.recoil.strengths.last().copied().unwrap_or(6.0);
                    aim.recoil.strengths.push(last);
                    undo_needed = true;
                }
                // 控制强度档位表:每档一个像素值;删除后自动收敛当前档。
                // 档位过多时默认收起(非添加模式),见 RECOIL_TIER_COLLAPSE_AT;
                // 点[＋ 添加一档]的这一帧强制展开 —— 正在添加就不该藏。
                let n_now = aim.recoil.strengths.len();
                let mut remove_idx = None;
                egui::CollapsingHeader::new("控制强度档位表")
                    .default_open(n_now <= RECOIL_TIER_COLLAPSE_AT)
                    .open(add_gear.then_some(true))
                    .show(ui, |ui| {
                        for (i, s) in aim.recoil.strengths.iter_mut().enumerate() {
                            ui.horizontal(|ui| {
                                let current = i == cur_idx;
                                let label = if current {
                                    format!("▶ 第 {} 档", i + 1)
                                } else {
                                    format!("第 {} 档", i + 1)
                                };
                                if ui
                                    .selectable_label(current, label)
                                    .on_hover_text(
                                        "点一下把当前档切到这一档(与换档键、滚轮换档同一档位)",
                                    )
                                    .clicked()
                                    && !current
                                {
                                    recoil_gear_req = Some(i);
                                }
                                let d = ui
                                    .add(
                                        egui::DragValue::new(s)
                                            .range(0.0..=200.0)
                                            .speed(0.5)
                                            .suffix(" px/次"),
                                    )
                                    .on_hover_text(
                                        "每次控制的向下位移(设备像素);20ms 一拍 60 次/秒时,\
                                         6px ≈ 360px/s。",
                                    );
                                if d.drag_started() || d.gained_focus() {
                                    undo_needed = true;
                                }
                                if n_now > 1 && ui.small_button("删除").clicked() {
                                    remove_idx = Some(i);
                                }
                            });
                        }
                    });
                if let Some(i) = remove_idx {
                    aim.recoil.strengths.remove(i);
                    undo_needed = true;
                }
                ui.horizontal(|ui| {
                    ui.label("摇晃:");
                    let s = ui
                        .add(
                            egui::DragValue::new(&mut aim.recoil.shake_px)
                                .range(0.0..=200.0)
                                .speed(0.5)
                                .suffix(" px"),
                        )
                        .on_hover_text(
                            "每次控制在左右方向的随机抖动上限(K2er:随机左右摇晃);0 = 不摇晃。",
                        );
                    if s.drag_started() || s.gained_focus() {
                        undo_needed = true;
                    }
                    ui.label("覆盖灵敏度:");
                    let v = ui
                        .add(
                            egui::DragValue::new(&mut aim.recoil.sensitivity)
                                .range(0.0..=20.0)
                                .speed(0.1),
                        )
                        .on_hover_text(
                            "触发期间鼠标位移改用该灵敏度(K2er:覆盖瞄准的灵敏度);0 = 不覆盖。",
                        );
                    if v.drag_started() || v.gained_focus() {
                        undo_needed = true;
                    }
                });
            }

            if ui
                .checkbox(&mut aim.capture_mouse, "指针消隐(FPS 模式下隐藏系统光标)")
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
            // 组装"改动前"的整份配置:除 `aim` 用上面那份旧的,其余字段取当前值
            // (本页只改 `aim`,与"进页时整份克隆"等价,见 W2-3)
            let mut before = lock_shared(&self.shared).profile.clone();
            before.aim = undo_before_aim;
            self.push_undo_snapshot(before);
        }

        if let Some(slot) = to_pick {
            self.begin_pick(slot);
        }
        // 三个 FPS 系统键 + 压枪触发键/挡位切换键:统一按**组合键**捕获(2026-10-09 第 4 条)。
        if pick_hold_key {
            self.begin_keys_capture(KeySlot::AimHold);
        }
        if pick_toggle_key {
            self.begin_keys_capture(KeySlot::AimToggle);
        }
        if pick_suspend_key {
            self.begin_keys_capture(KeySlot::AimSuspend);
        }
        if pick_recoil_key {
            self.begin_keys_capture(KeySlot::RecoilTrigger);
        }
        if pick_recoil_switch {
            self.begin_keys_capture(KeySlot::RecoilSwitch);
        }
        // 本帧的[取消设置](用户 2026-10-10 第 3 条):等待捕获中 = 只退出捕获;
        // 已绑定 = 退出捕获并清空(已绑定那种在面板里直接写了 `aim`,不用来这里)。
        if cancel_bind.is_some() {
            self.cancel_key_capture();
            self.log("已取消设置:已退出按键捕获,原绑定保持不变");
        }
        // 界面手动换档:挡位是引擎侧运行状态,界面只能提一个请求(见
        // `engine::Shared::recoil_gear_req` 的注释);引擎下一轮取走并镜像回来。
        if let Some(req) = recoil_gear_req {
            let applied = {
                let mut g = lock_shared(&self.shared);
                let n = g.profile.aim.recoil.strengths.len();
                if n == 0 {
                    None
                } else {
                    let idx = req.min(n - 1);
                    g.recoil_gear_req = Some(idx);
                    Some((idx, n))
                }
            };
            if let Some((idx, n)) = applied {
                self.log(format!(
                    "后坐力补偿:控制强度切换到第 {} 档(共 {} 档)",
                    idx + 1,
                    n
                ));
            }
        }
        if toggled {
            // 用户明确要求锚点默认不设置。这里只提示，不再替用户放置。
            let needs_anchor = { lock_shared(&self.shared).profile.aim.enabled };
            if needs_anchor {
                self.log("鼠标视角已启用；锚点默认未设置，请在“瞄准锚点”中取点");
            }
        }
    }

    fn ui_switch_keys(&mut self, ui: &mut egui::Ui) {
        let (names, active, n) = {
            let g = lock_shared(&self.shared);
            (
                g.schemes.iter().map(|p| p.name.clone()).collect::<Vec<_>>(),
                g.active_scheme,
                g.schemes.len(),
            )
        };
        let rows = lock_shared(&self.shared).switch_keys.clone();
        ui.heading("切换键位");
        ui.small(
            "一个按钮搞定:点击后按住 Ctrl 再按另一个键即为组合键(最多两个键),\
             顺序无关。可在[按键组合]里启用快速切换。",
        );
        let mut delete = None;
        for (i, original) in rows.iter().enumerate() {
            let keys = original.effective_keys();
            let mut changed = false;
            ui.horizontal(|ui| {
                ui.label(format!("切换{}:", i + 1));
                // 用户 2026-10-09(第 4 条):不再有"第一个键/第二个键"两个按钮,
                // 也不再有[新增组合键]/[删除组合键] —— 整行就是一个组合键槽,
                // 与总开关键那类系统键完全同一套交互(见 `keys_button`)。
                let waiting = self.waiting_keys == Some(KeySlot::SwitchKey(i));
                if Self::keys_button(ui, waiting, &keys).clicked() {
                    self.begin_keys_capture(KeySlot::SwitchKey(i));
                }
                if !keys.is_empty() && ui.small_button("清除").clicked() {
                    self.assign_keys(KeySlot::SwitchKey(i), KeySet::new());
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
                    if let Some(s) = lock_shared(&self.shared).switch_keys.get_mut(i) {
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
                        if let Some(s) = lock_shared(&self.shared).switch_keys.get_mut(i) {
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
            lock_shared(&self.shared).switch_keys.push(SwitchKey {
                key: 0,
                keys: KeySet::new(),
                target: next,
                direction: SwitchDirection::Target,
            });
            self.scheme_dirty = true;
        }
        if let Some(i) = delete {
            // 下标核查见 remove_indexed(引擎可能刚换过组合)
            let removed = {
                let mut g = lock_shared(&self.shared);
                remove_indexed(&mut g.switch_keys, i)
            };
            if removed {
                self.scheme_dirty = true;
                if self.waiting_keys == Some(KeySlot::SwitchKey(i)) {
                    self.waiting_keys = None;
                    self.capture_down.clear();
                    self.capture_seen.clear();
                }
            } else {
                self.log("该切换键已不存在,未删除");
            }
        }
    }

    fn fps_text(info: &RemoteDebugInfo) -> String {
        info.fps
            .map(|v| format!("{v:.1} Hz"))
            .unwrap_or_else(|| "等待刷新".to_string())
    }

    /// 真实投屏帧率(来自 scrcpy --print-fps 的实况输出)的显示文本。
    ///
    /// 与 [`Self::fps_text`](物理面板刷新率,dumpsys 读数)是两回事:这里才是
    /// "画面现在每秒实际有多少帧"。scrcpy 每秒打一行;超过几秒没有新行就如实
    /// 说明(停投/被参数关掉),不拿旧数字糊弄 —— 用户要的正是这个区分。
    fn stream_fps_text(&self) -> String {
        match self.stream_fps {
            Some((v, at)) => {
                let age = at.elapsed();
                if age <= Duration::from_secs(5) {
                    format!("{v:.0} fps (scrcpy 实况输出)")
                } else {
                    format!(
                        "无数据(最后 {v:.0} fps,{:.0} 秒前;投屏已停或参数里没开 --print-fps)",
                        age.as_secs_f32()
                    )
                }
            }
            None => "等待 scrcpy 输出(未在投屏,或参数里没开 --print-fps)".to_string(),
        }
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
        // 分辨率 30s 才查一次(见 DEBUG_SIZE_TTL):没到点就这一轮不查,
        // poll 里沿用旧值。省下的是每轮一条最重的 `dumpsys window displays`。
        let need_size = self
            .debug_size_at
            .map(|t| t.elapsed() >= DEBUG_SIZE_TTL)
            .unwrap_or(true);
        let (tx, rx) = channel();
        self.debug_rx = Some(rx);
        std::thread::spawn(move || {
            let fps = adb::display_refresh_rate(&serial).ok();
            let resolution = if need_size {
                adb::display_size(&serial).ok()
            } else {
                None // "本轮没查",不是"查失败" —— poll 会区分处理
            };
            let error = if fps.is_none() || (need_size && resolution.is_none()) {
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
                Ok(mut info) => {
                    // 本轮没查分辨率 -> 沿用上一次取到的值,不让界面闪"等待刷新"。
                    match info.resolution {
                        Some(_) => self.debug_size_at = Some(Instant::now()),
                        None => info.resolution = self.debug_info.resolution,
                    }
                    self.debug_info = info;
                }
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
            && self.debug_last_query.elapsed() >= DEBUG_POLL_INTERVAL
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
        let mut cursor_key = KeySet::new();
        ui.horizontal(|ui| {
            ui.label("切换键:");
            cursor_key = lock_shared(&self.shared).profile.cursor_toggle_key;
            // 与其余系统键同一套:一个按钮捕获,支持 Ctrl+X(顺序无关)。
            let waiting = self.waiting_keys == Some(KeySlot::CursorToggle);
            if Self::keys_button(ui, waiting, &cursor_key).clicked() {
                self.begin_keys_capture(KeySlot::CursorToggle);
            }
            if waiting {
                // 等待捕获中:这一个按钮只退出捕获,不动已有的绑定(见 [`Self::cancel_bind_button`])
                if Self::cancel_bind_button(ui, true) {
                    self.cancel_key_capture();
                    self.log("已取消设置:已退出按键捕获,原绑定保持不变");
                }
            } else if !cursor_key.is_empty() && Self::cancel_bind_button(ui, false) {
                self.assign_keys(KeySlot::CursorToggle, KeySet::new());
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
            ui.checkbox(&mut self.debug_show_fps, "显示帧率(面板刷新率/投屏实况)");
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
            ui.label(format!(
                "手机面板刷新率: {}(系统读数,物理屏幕能力,不是投屏帧率)",
                Self::fps_text(&info)
            ));
            ui.label(format!("投屏帧率: {}", self.stream_fps_text()));
        }
        if self.debug_show_resolution {
            ui.label(format!("手机分辨率: {}", Self::resolution_text(&info)));
        }
        if self.debug_show_aim {
            let live = lock_shared(&self.shared).aim_live;
            ui.label(format!(
                "鼠标视角: 位移 {} 次，最近 ({:.1}, {:.1})",
                live.motions, live.last_dx, live.last_dy
            ));
        }
        if let Some(updated) = info.updated_at {
            ui.small(format!("最后刷新: {}", fmt_timestamp(updated)));
        }
        // 「正在刷新...」与"读取失败:..."是**条件出现**的,而且刷新是每 2 秒一轮
        // (见 `DEBUG_POLL_INTERVAL`)。按内容排布的话,这一行每 2 秒出现一次,把下面整块
        // (手动 adb 命令)连人带按钮推下去又弹回来 —— 用户 2026-10-09 要求这种抖动必须消失。
        // 做法:给它们一个**固定高度的单行槽位**,有内容没内容、内容换不换,位置都不动;
        // 超宽就单行截断,完整文字挂在 hover 提示里。
        let status = match self.debug_rx.is_some() {
            true => Some((
                th.warn,
                "正在刷新...".to_string(),
                "正在向设备读取调试信息".to_string(),
            )),
            false => info.error.map(|e| (th.danger, e.clone(), e)),
        };
        let (status_rect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), ui.spacing().interact_size.y),
            egui::Sense::hover(),
        );
        if let Some((color, text, full)) = status {
            ui.put(
                status_rect,
                egui::Label::new(egui::RichText::new(text).color(color))
                    .wrap_mode(egui::TextWrapMode::Truncate)
                    .sense(egui::Sense::hover()),
            )
            .on_hover_text(full);
        }
        self.ui_manual_adb(ui);
    }

    /// 「其他功能 → 手动 adb 命令」:像搭 scrcpy 参数一样拼一条 adb 命令并执行。
    ///
    /// 命令栏是 token 列表:加入时查冲突(`adbcmd::check_fragment`),单个参数
    /// 点一下即移除;内容随 settings.json 持久化(`adb_command`),命名预设
    /// 另存 `adb_presets.json`(与内置预设同列在一个下拉里)。
    fn ui_manual_adb(&mut self, ui: &mut egui::Ui) {
        let th = self.theme();
        ui.separator();
        ui.heading("手动 adb 命令");
        ui.label("像搭 scrcpy 参数一样拼一条 adb 命令(不用写开头的 adb),加入时会检查冲突,点[执行]立即发送;输出显示在下方。");
        ui.small("参数按空格拆分,引号内的空格不拆(如 input text \"hello world\");未指定 -s/-d/-e/-t 时自动带上当前选中的设备。");

        // ---- 预设行:内置(★)+ 我的(☆)同一个下拉,选中即载入(替换整条) ----
        let builtins: Vec<&adbcmd::CmdEntry> = adbcmd::builtin_entries()
            .iter()
            .filter(|e| e.preset)
            .collect();
        let builtin_count = builtins.len();
        let sel = self.adb_preset_sel;
        let sel_text = match sel {
            Some(i) if i < builtin_count => format!("★ {}", builtins[i].name),
            Some(i) if i - builtin_count < self.adb_user_presets.len() => {
                format!("☆ {}", self.adb_user_presets[i - builtin_count].name)
            }
            _ => "选择预设…".to_string(),
        };
        let mut newly_selected: Option<usize> = None;
        ui.horizontal(|ui| {
            ui.label("预设:");
            egui::ComboBox::from_id_salt("adb_preset_combo")
                .selected_text(sel_text)
                .width(260.0)
                .show_ui(ui, |ui| {
                    for (i, e) in builtins.iter().enumerate() {
                        if ui
                            .selectable_label(sel == Some(i), format!("★ {}", e.name))
                            .clicked()
                        {
                            newly_selected = Some(i);
                            ui.close(); // 选中即收起下拉(载入是"动作"不是"切换显示")
                        }
                    }
                    if !self.adb_user_presets.is_empty() {
                        ui.separator();
                        ui.label("我的预设:");
                        for (j, p) in self.adb_user_presets.iter().enumerate() {
                            let i = builtin_count + j;
                            if ui
                                .selectable_label(sel == Some(i), format!("☆ {}", p.name))
                                .clicked()
                            {
                                newly_selected = Some(i);
                                ui.close();
                            }
                        }
                    }
                });
            if ui.small_button("另存当前为预设").clicked() {
                self.adb_preset_save_open = !self.adb_preset_save_open;
            }
            let is_user = sel.map(|i| i >= builtin_count).unwrap_or(false);
            if ui
                .add_enabled(is_user, egui::Button::new("删除选中预设").small())
                .clicked()
            {
                self.adb_delete_preset();
            }
        });
        if let Some(i) = newly_selected {
            let (name, tokens) = if i < builtin_count {
                let e = builtins[i];
                (
                    e.name.to_string(),
                    e.tokens.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                )
            } else {
                let p = &self.adb_user_presets[i - builtin_count];
                (p.name.clone(), p.tokens.clone())
            };
            self.adb_bar = tokens;
            self.adb_preset_sel = Some(i);
            self.adb_msg = Some((true, format!("已载入预设: {name}(替换了整条命令)")));
            self.remember_now();
        }
        if self.adb_preset_save_open {
            ui.horizontal(|ui| {
                ui.label("预设名:");
                ui.add(egui::TextEdit::singleline(&mut self.adb_preset_name).desired_width(160.0));
                if ui.button("保存").clicked() {
                    self.adb_save_preset();
                }
                if ui.small_button("取消").clicked() {
                    self.adb_preset_save_open = false;
                    self.adb_preset_name.clear();
                }
            });
        }
        // ---- 命令栏:token 芯片,点一下移除 ----
        ui.label("命令栏:");
        if self.adb_bar.is_empty() {
            ui.colored_label(th.muted, "(空 —— 从下面加入参数,或载入一个预设)");
        } else {
            let mut remove: Option<usize> = None;
            ui.horizontal_wrapped(|ui| {
                for (i, tok) in self.adb_bar.iter().enumerate() {
                    if ui
                        .small_button(format!("{tok} ✕"))
                        .on_hover_text("点击移除该参数")
                        .clicked()
                    {
                        remove = Some(i);
                    }
                }
            });
            if let Some(i) = remove {
                let tok = self.adb_bar.remove(i);
                self.adb_msg = Some((true, format!("已移除: {tok}")));
                self.log(format!("adb 命令栏移除: {tok}"));
                self.remember_now();
            }
        }

        // ---- 加入参数 / 清空 / 助手 ----
        ui.horizontal(|ui| {
            ui.label("加入参数:");
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.adb_add_input)
                    .hint_text("如 settings put system min_refresh_rate 120")
                    .desired_width(300.0),
            );
            let entered = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui.button("添加").clicked() || entered {
                self.adb_try_add_input();
                if entered {
                    resp.request_focus(); // 回车后保持焦点,方便连续输入
                }
            }
            if ui.small_button("清空命令栏").clicked() {
                self.adb_clear_bar();
            }
            if ui.button("常用命令助手").clicked() {
                self.adb_helper = Some(AdbHelperState {
                    search: String::new(),
                    selected: 0,
                });
            }
        });

        // ---- 预览 + 执行 ----
        let serial = self.serial();
        if !self.adb_bar.is_empty() {
            let (_, display) = adbcmd::build_command(&serial, &self.adb_bar);
            ui.monospace(format!("将执行: {display}"));
            if serial.trim().is_empty() && !adbcmd::has_device_selector(&self.adb_bar) {
                ui.colored_label(
                    th.warn,
                    "提示: 当前没有选中的设备(左栏选一台,或自行加入 -s 序列号)",
                );
            }
        }
        let running = self.adb_run.is_some();
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !running && !self.adb_bar.is_empty(),
                    egui::Button::new("执行"),
                )
                .clicked()
            {
                self.adb_execute();
            }
            if running {
                if ui.button("停止").clicked() {
                    if let Some(run) = self.adb_run.as_ref() {
                        run.stop();
                    }
                    self.log(
                        "已请求停止 adb 命令(终止本机 adb 进程;设备端已开始的子命令会继续跑完)",
                    );
                }
                let secs = self
                    .adb_run_started
                    .map(|t| t.elapsed().as_secs_f32())
                    .unwrap_or(0.0);
                ui.colored_label(th.warn, format!("执行中… 已运行 {secs:.1}s"));
                ui.ctx().request_repaint();
            }
        });
        if let Some((ok, text)) = &self.adb_msg {
            ui.colored_label(if *ok { th.ok } else { th.danger }, text);
        }

        // ---- 输出区 ----
        if let Some(last) = &self.adb_last {
            ui.separator();
            let mut clear_last = false;
            match &last.done {
                Err(e) => {
                    ui.colored_label(th.danger, format!("启动失败: {e}"));
                }
                Ok(d) => {
                    let mut status = format!(
                        "上次执行: {} — 退出码 {}",
                        last.display,
                        d.exit_code
                            .map(|c| c.to_string())
                            .unwrap_or_else(|| "无".into())
                    );
                    status.push_str(&format!(",用时 {} ms", d.millis));
                    if d.killed {
                        status.push_str(",已手动停止");
                    }
                    if d.truncated {
                        status.push_str(&format!(
                            ",输出超过 {} KB 已截断",
                            adbcmd::OUTPUT_CAP / 1024
                        ));
                    }
                    ui.label(status);
                    let mut text = d.stdout.clone();
                    if !d.stderr.is_empty() {
                        if !text.is_empty() && !text.ends_with('\n') {
                            text.push('\n');
                        }
                        text.push_str("── stderr ──\n");
                        text.push_str(&d.stderr);
                    }
                    if text.trim().is_empty() {
                        text.push_str("(无输出)");
                    }
                    egui::ScrollArea::vertical()
                        .max_height(170.0)
                        .id_salt("adb_out_scroll")
                        .show(ui, |ui| {
                            ui.add(
                                egui::TextEdit::multiline(&mut text.as_str())
                                    .code_editor()
                                    .desired_width(f32::INFINITY),
                            );
                        });
                    ui.horizontal(|ui| {
                        if ui.small_button("复制输出").clicked() {
                            ui.ctx().copy_text(text.clone());
                        }
                        if ui.small_button("清空结果").clicked() {
                            clear_last = true;
                        }
                    });
                }
            }
            if clear_last {
                self.adb_last = None;
            }
        }
    }
    /// 「常用命令助手」窗口(仿 scrcpy 参数助手的 take/放回模式)。
    fn ui_adb_helper(&mut self, ctx: &egui::Context) {
        let Some(mut h) = self.adb_helper.take() else {
            return;
        };
        let mut open = true;
        egui::Window::new("常用 adb 命令助手")
            .open(&mut open)
            .default_width(560.0)
            .show(ctx, |ui| self.adb_helper_panel(ui, &mut h));
        if open {
            self.adb_helper = Some(h);
        }
    }

    fn adb_helper_panel(&mut self, ui: &mut egui::Ui, h: &mut AdbHelperState) {
        let th = self.theme();
        ui.label("内置离线命令库(不联网)。搜索或翻列表,选中看说明,再[加入命令栏]。★ = 同时出现在预设下拉里。");
        ui.small("加入规则: 设备定向参数(-s/-d/-e/-t)最多一个且必须在子命令之前;开头参数区不允许同名参数重复;相同片段不能重复加入;设备侧子命令自身的参数(grep -i、pm -3 等)不受限制。");
        ui.horizontal(|ui| {
            ui.label("搜索:");
            ui.add(
                egui::TextEdit::singleline(&mut h.search)
                    .hint_text("名称 / 说明 / 参数")
                    .desired_width(240.0),
            );
            if ui.small_button("清除").clicked() {
                h.search.clear();
            }
        });
        let entries = adbcmd::builtin_entries();
        let q = h.search.trim().to_lowercase();
        let matched: Vec<usize> = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| q.is_empty() || e.matches(&q))
            .map(|(i, _)| i)
            .collect();
        // 过滤结果变化时把选中项拉回可见范围(搜没了就选第一条)
        if !matched.is_empty() && !matched.contains(&h.selected) {
            h.selected = matched[0];
        }
        egui::ScrollArea::vertical()
            .max_height(230.0)
            .id_salt("adb_helper_list")
            .show(ui, |ui| {
                if matched.is_empty() {
                    ui.colored_label(th.muted, "没有匹配的命令");
                }
                for &i in &matched {
                    let e = &entries[i];
                    let label = format!("{} {}", if e.preset { "★" } else { "·" }, e.name);
                    if ui.selectable_label(h.selected == i, label).clicked() {
                        h.selected = i;
                    }
                }
            });
        if matched.contains(&h.selected) {
            let e = &entries[h.selected];
            ui.separator();
            ui.strong(e.name);
            ui.monospace(e.tokens.join(" "));
            ui.label(e.desc);
            ui.small(format!("使用/注意: {}", e.usage));
            ui.horizontal(|ui| {
                if ui.button("加入命令栏").clicked() {
                    self.adb_helper_apply(e.tokens, false);
                }
                if ui.small_button("替换命令栏").clicked() {
                    self.adb_helper_apply(e.tokens, true);
                }
                if ui.small_button("撤去该片段").clicked() {
                    self.adb_helper_remove(e.tokens);
                }
            });
        }
        ui.separator();
        ui.hyperlink_to(
            "adb 官方文档 (Android Developers)",
            "https://developer.android.com/tools/adb",
        );
    }

    /// 手动 adb 命令:把输入框内容作为一段参数加入(冲突则拒绝并说明)。
    fn adb_try_add_input(&mut self) {
        let frag = adbcmd::split_tokens(&self.adb_add_input);
        if frag.is_empty() {
            self.adb_msg = Some((false, "输入为空,没有可加入的参数".into()));
            return;
        }
        match adbcmd::check_fragment(&self.adb_bar, &frag) {
            Ok(()) => {
                let text = frag.join(" ");
                self.adb_bar.extend(frag);
                self.adb_add_input.clear();
                self.adb_msg = Some((true, format!("已加入: {text}")));
                self.remember_now();
            }
            Err(why) => {
                self.adb_msg = Some((false, format!("拒绝加入: {why}")));
                self.log(format!("adb 命令加入被拒: {why}"));
            }
        }
    }

    /// 手动 adb 命令:内置库条目 → 命令栏(false=追加,true=替换整条)。
    fn adb_helper_apply(&mut self, tokens: &[&'static str], replace: bool) {
        let frag: Vec<String> = tokens.iter().map(|s| s.to_string()).collect();
        if replace {
            self.adb_bar = frag.clone();
            self.adb_msg = Some((true, format!("命令栏已替换为: {}", frag.join(" "))));
            self.remember_now();
            return;
        }
        match adbcmd::check_fragment(&self.adb_bar, &frag) {
            Ok(()) => {
                self.adb_bar.extend(frag.clone());
                self.adb_msg = Some((true, format!("已加入: {}", frag.join(" "))));
                self.remember_now();
            }
            Err(why) => {
                self.adb_msg = Some((false, format!("拒绝加入: {why}")));
                self.log(format!("adb 命令加入被拒: {why}"));
            }
        }
    }

    /// 手动 adb 命令:从命令栏撤去一段片段(完全匹配才撤)。
    fn adb_helper_remove(&mut self, tokens: &[&'static str]) {
        let frag: Vec<String> = tokens.iter().map(|s| s.to_string()).collect();
        let missing = || Some((false, "命令栏里没有这段参数".to_string()));
        if frag.is_empty() || frag.len() > self.adb_bar.len() {
            self.adb_msg = missing();
            return;
        }
        let Some(pos) = self
            .adb_bar
            .windows(frag.len())
            .position(|w| w == frag.as_slice())
        else {
            self.adb_msg = missing();
            return;
        };
        self.adb_bar.drain(pos..pos + frag.len());
        self.adb_msg = Some((true, format!("已撤去: {}", frag.join(" "))));
        self.remember_now();
    }

    fn adb_clear_bar(&mut self) {
        if self.adb_bar.is_empty() {
            self.adb_msg = Some((false, "命令栏本来就是空的".into()));
            return;
        }
        self.adb_bar.clear();
        self.adb_msg = Some((true, "命令栏已清空".into()));
        self.log("adb 命令栏已清空");
        self.remember_now();
    }

    /// 把当前命令栏另存为命名预设(落 adb_presets.json)。
    fn adb_save_preset(&mut self) {
        let name = self.adb_preset_name.trim().to_string();
        if name.is_empty() {
            self.adb_msg = Some((false, "预设名不能为空".into()));
            return;
        }
        if self.adb_bar.is_empty() {
            self.adb_msg = Some((false, "命令栏为空,没有可保存的内容".into()));
            return;
        }
        if self.adb_user_presets.iter().any(|p| p.name == name) {
            self.adb_msg = Some((false, format!("已存在同名预设 {name}(先删除它或换个名字)")));
            return;
        }
        self.adb_user_presets.push(adbcmd::UserPreset {
            name: name.clone(),
            tokens: self.adb_bar.clone(),
        });
        match adbcmd::save_user_presets(&adb_presets_path(), &self.adb_user_presets) {
            Ok(()) => {
                let builtin_count = adbcmd::builtin_entries()
                    .iter()
                    .filter(|e| e.preset)
                    .count();
                self.adb_preset_sel = Some(builtin_count + self.adb_user_presets.len() - 1);
                self.adb_preset_save_open = false;
                self.adb_preset_name.clear();
                self.adb_msg = Some((true, format!("已保存预设: {name}")));
                self.log(format!("adb 预设已保存: {name}"));
            }
            Err(e) => {
                self.adb_user_presets.pop();
                self.adb_msg = Some((false, format!("预设保存失败: {e}")));
            }
        }
    }

    /// 删除下拉里选中的用户预设(内置预设不可删)。
    fn adb_delete_preset(&mut self) {
        let builtin_count = adbcmd::builtin_entries()
            .iter()
            .filter(|e| e.preset)
            .count();
        let Some(sel) = self.adb_preset_sel else {
            return;
        };
        if sel < builtin_count || sel - builtin_count >= self.adb_user_presets.len() {
            self.adb_msg = Some((false, "只有『我的预设』可以删除".into()));
            return;
        }
        let idx = sel - builtin_count;
        let removed = self.adb_user_presets.remove(idx);
        self.adb_preset_sel = None;
        match adbcmd::save_user_presets(&adb_presets_path(), &self.adb_user_presets) {
            Ok(()) => {
                self.adb_msg = Some((true, format!("已删除预设: {}", removed.name)));
                self.log(format!("adb 预设已删除: {}", removed.name));
            }
            Err(e) => {
                // 写盘失败:放回去,别让界面和文件不一致
                self.adb_user_presets.insert(idx, removed);
                self.adb_msg = Some((false, format!("删除失败(文件没写成功): {e}")));
            }
        }
    }

    /// 执行命令栏:组装 argv(缺设备定向参数时自动补 -s)并启动。
    fn adb_execute(&mut self) {
        if self.adb_run.is_some() {
            return;
        }
        if self.adb_bar.is_empty() {
            self.adb_msg = Some((false, "命令栏为空,先加入参数或载入预设".into()));
            return;
        }
        let exe = if self.adb_path.trim().is_empty() {
            adb::adb_bin_now().unwrap_or_else(|| "adb".to_string())
        } else {
            self.adb_path.trim().to_string()
        };
        let (args, display) = adbcmd::build_command(&self.serial(), &self.adb_bar);
        self.log(format!("执行 adb 命令: {display}"));
        self.adb_last = None;
        self.adb_run = Some(adbcmd::run(&exe, args, display));
        self.adb_run_started = Some(Instant::now());
    }

    /// 每帧看一眼正在执行的 adb 命令:完成后收尾、写日志、结果留给输出区。
    fn poll_adb_run(&mut self) {
        let Some(run) = self.adb_run.as_ref() else {
            return;
        };
        let Some(outcome) = run.poll() else {
            return;
        };
        self.adb_run = None;
        self.adb_run_started = None;
        match &outcome.done {
            Ok(d) => self.log(format!(
                "adb 命令结束: 退出码 {},用时 {} ms{}",
                d.exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "无".into()),
                d.millis,
                if d.killed { ",已手动停止" } else { "" }
            )),
            Err(e) => self.log(format!("adb 命令启动失败: {e}")),
        }
        self.adb_last = Some(outcome);
    }

    fn ui_debug_overlay(&mut self, ctx: &egui::Context) {
        if !self.debug_overlay_open {
            return;
        }
        let info = self.debug_info.clone();
        let live = lock_shared(&self.shared).aim_live;
        let mut close = false;
        let data = DebugOverlayData {
            show_fps: self.debug_show_fps,
            show_resolution: self.debug_show_resolution,
            show_aim: self.debug_show_aim,
            fps: Self::fps_text(&info),
            stream_fps: self.stream_fps_text(),
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
                .with_inner_size([340.0, 175.0])
                .with_min_inner_size([220.0, 90.0])
                .with_resizable(true),
            |ui, _class| {
                ui.heading("调试信息");
                ui.separator();
                if data.show_fps {
                    ui.label(format!("手机面板刷新率: {}", data.fps));
                    ui.label(format!("投屏帧率: {}", data.stream_fps));
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

    /// 截图**抬头**:按钮行(截图 / 缩放 / 小窗 / 显示项目…)+ 状态提示(取点中 /
    /// 取消取点 / 修改范围中)+ 坐标空间一致性提示 —— 下方面板与截图小窗**共用同一份**。
    ///
    /// 用户 2026-10-10(第 3 条):"截图框要带上原有整套抬头('显示项目''取消取点'等)"。
    /// 所以这段从 `ui_picker_body` 里摊出来,谁画截图画布谁就带上它 —— 各写一份必然
    /// 越走越远(加了新按钮只在一个地方生效)。用 `horizontal_wrapped` 是因为小窗可能
    /// 比这行按钮还窄,硬排会直接把右边的控件切掉。
    fn ui_shot_header(&mut self, ui: &mut egui::Ui) {
        // 百分比直接取画布上一帧实际用的基准(见 `ui_shot_canvas`),不再另算一份
        // —— 两处各算一份时容易悄悄跑偏(容器宽高不同,显示的百分比就不等于真实倍率)。
        // 小窗开着时画布在小窗里,倍率走的是小窗自己那份绝对基准,这里也跟着切过去,
        // 否则抬头会显示下方面板的倍率、跟眼前这张图对不上。
        let preview_scale_base = if self.shot_window_open {
            self.shot_popup_base
        } else {
            self.shot_last_base
        };
        ui.horizontal_wrapped(|ui| {
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
            self.ui_shot_zoom_buttons(ui);
            // 截图小窗(用户 2026-10-09):同一个位置、两个名字的按钮来回切 ——
            // 悬浮时内容搬进独立窗口,再点一次就拼回下方面板(布局与之前一字不差)。
            let (win_label, win_hint) = if self.shot_window_open {
                ("关闭小窗", "把截图放回下方面板(恢复原来的布局)")
            } else {
                ("小窗悬浮", "把截图独立成一个可移动的小窗")
            };
            if ui.button(win_label).on_hover_text(win_hint).clicked() {
                self.shot_window_open = !self.shot_window_open;
                // 从主界面/小窗自己这儿开的:小窗画实时配置(扩展宏弹窗开的走它自己那条路)
                self.shot_window_virtual = false;
                // 窗口尺寸不在这里定:小窗每帧按当前倍率自己算(见 `ui_shot_window`),
                // 于是 `±` 一改倍率窗口就跟着变,不必再记"开窗那一刻该多大"。
            }
            if self.shot_window_open {
                ui.checkbox(&mut self.shot_window_pin, "置顶")
                    .on_hover_text("勾上:小窗固定在其他窗口上方;不勾:可以被其他窗口盖住");
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
                let cancel = ui.button("取消取点");
                Self::note_cancel_zone(ui, &cancel);
                if cancel.clicked() {
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
                let cancel = ui.button("取消取点");
                Self::note_cancel_zone(ui, &cancel);
                if cancel.clicked() {
                    self.cancel_draft();
                    self.log("已取消新增");
                }
            } else if let Some(target) = self.resizing {
                // 直接显示当前半径(像素),改没改一眼就能看出来
                let space = self.screen_size().unwrap_or((1080, 2400));
                let cur_px = {
                    let g = lock_shared(&self.shared);
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

        // W2-2 坐标空间一致性守卫:浮层与取点按**截图**画,注入按**坐标空间**走 ——
        // 纵横比不一致时取点会落偏。不一致 → 醒目提示 + 一键对齐 + 禁用取点
        // (禁用入口:`begin_pick` 与画布点击两处);对齐后自动恢复,无残留状态。
        if let Some(m) = self.space_guard() {
            let th = self.theme();
            ui.colored_label(th.danger, format!("坐标空间未对齐:{}", m.describe()));
            ui.colored_label(
                th.warn,
                "这种状态下取点会落偏(浮层按截图绘制,注入按坐标空间走),已暂时禁用取点。\
                 刚截的图 → 点[以截图为准];截图是旧的(刚转过屏) → 点[重新查询屏幕尺寸]或重新截图。",
            );
            ui.horizontal(|ui| {
                if ui
                    .button("以截图为准(重设坐标空间)")
                    .on_hover_text("把注入用的触摸坐标空间与配置记录的「设计尺寸」都对齐到当前截图")
                    .clicked()
                {
                    self.adopt_shot_space(m);
                }
                if ui
                    .button("重新查询屏幕尺寸")
                    .on_hover_text("让 adb 重新读取设备当前显示尺寸,并把坐标空间对齐到它")
                    .clicked()
                {
                    self.refresh_display_space();
                    self.log("正在重新查询设备屏幕尺寸...");
                }
            });
        } else if let Some((sw, sh)) = self.shot.as_ref().map(|(_, w, h)| (*w, *h)) {
            // 分辨率不同但比例一致:相对坐标换算不受影响,只留一句说明。
            // (以前这条与"转过屏"混在同一个警告里,同比例的窗口缩放会被误报成"取点会落偏")
            if let Some((iw, ih)) = self.inject_space() {
                if (iw, ih) != (sw, sh) {
                    ui.small(format!(
                        "截图 {sw}x{sh} 与触摸坐标空间 {iw}x{ih} 分辨率不同但比例一致:取点按相对坐标换算,不受影响"
                    ));
                }
            }
        }
    }

    /// 截图取点的按钮行 + 预览画布(不含标题)。
    /// 放在"截图取点"标题下(默认/可视化风格共用这一份)。
    fn ui_picker_body(&mut self, ui: &mut egui::Ui) {
        self.ui_shot_header(ui);
        // 小窗模式:画布整个搬进独立窗口(见 `ui_shot_window`),面板里不再重复画。
        // 按钮行仍留在原处 —— 所以"再点一次同一个位置、另一个名字的按钮"就能拼回来。
        if !self.shot_window_open {
            // 下方面板恒画实时配置(虚拟层只属于扩展宏弹窗,见 `ui_shot_canvas`)
            self.ui_shot_canvas(ui, ShotCanvasKind::Panel, false);
        }
    }

    /// 放大/缩小/重置预览缩放(截图面板与截图小窗共用一份,不复制第二份逻辑)。
    ///
    /// 只改 `shot_zoom` 这一个数:图的大小由它算,**小窗的窗口大小**也由它算
    /// (见 [`Self::shot_window_size`],每帧按倍率给一次 `with_inner_size`)——
    /// 于是"`±` 同时缩放截图大小与弹窗大小"是自然结果,不需要在这里额外去动窗口。
    fn ui_shot_zoom_buttons(&mut self, ui: &mut egui::Ui) {
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
    }

    /// 小窗里这张图现在的**绝对**倍率:小窗专用基准 × 用户倍率。
    ///
    /// 与下方面板/扩展宏弹窗那条路([`Self::shot_last_base`],按容器内接)刻意分开:
    /// 小窗的窗口尺寸是由这个倍率**算出来**的,若倍率又反过来由窗口尺寸决定,两者
    /// 就会互相追着放大(见 `shot_popup_base` 的注释)。
    fn shot_popup_scale(&self) -> f32 {
        (self.shot_popup_base * self.shot_zoom).clamp(0.05, 4.0)
    }

    /// 小窗此刻应有的**内容尺寸**(pt):图按 [`Self::shot_popup_scale`] 占多大 + 抬头一行。
    ///
    /// 每帧都给 `with_inner_size` 同一个值不会把用户手动拉伸的尺寸顶回去(框架只在
    /// 值**变了**的时候下发 `ViewportCommand::InnerSize`),所以这里可以放心地按状态算 ——
    /// 于是"`±` 同时缩放截图大小与弹窗大小"就是自然结果:`shot_zoom` 一变,窗口跟着变。
    ///
    /// 尺寸**封顶**在 [`shot_popup_area`](程序窗口 ∩ 系统屏幕):手动放大到放不下时,
    /// 让图比窗口大、靠滚轮翻着看,而不是把窗口撑出屏幕外面去。
    fn shot_window_size(&self, ctx: &egui::Context) -> egui::Vec2 {
        let area = shot_popup_area(ctx);
        let scale = self.shot_popup_scale();
        // 没截图时给一个能用的默认尺寸(与旧写死的 460x820 相近,但按行高算)
        let (iw, ih) = self
            .shot
            .as_ref()
            .map(|(_, w, h)| (*w as f32 * scale, *h as f32 * scale))
            .unwrap_or((440.0, 760.0));
        // 抬头高度取上一帧的实测值(它会换行,不是一个常数);还没量过时按两行估。
        let header = if self.shot_window_header_h > 0.0 {
            self.shot_window_header_h
        } else {
            SHOT_POPUP_HEADER_EST
        };
        // 取整到 1pt:否则浮点尾数每帧抖一点就够触发一次多余的窗口尺寸下发。
        egui::vec2(
            (iw + SHOT_POPUP_PAD).ceil().min(area.x),
            (ih + header + SHOT_POPUP_PAD).ceil().min(area.y),
        )
    }

    /// 截图区上方的**可拖动分界条**(扩展宏弹窗专用):上下拖动改下方画布区的高度。
    ///
    /// 用户 2026-10-09 第三轮:"宏截图…文字与图像分界可拖动调整,布局/尺寸正确,不被文字压住"。
    /// 所以它自己占一条细缝、自己接拖拽,画布区按拖出来的高度铺开 ——
    /// 文字在上、图在下,各占各的地方,谁也不压谁。
    fn ui_shot_divider(&mut self, ui: &mut egui::Ui) {
        let (rect, resp) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), MACRO_SHOT_DIVIDER_H),
            egui::Sense::drag(),
        );
        let active = resp.hovered() || resp.dragged();
        if active {
            // 光标换成"上下可调",不用提示文字也知道这里能拖
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
        }
        if resp.dragged() {
            // 向下拖 = 分界下移 = 画布区变小(与手感一致:抓住的是分界线本身)
            self.macro_shot_height = (self.macro_shot_height + resp.drag_delta().y)
                .clamp(MACRO_SHOT_MIN_H, MACRO_SHOT_MAX_H);
        }
        let th = self.theme();
        let color = if active { th.accent } else { th.muted };
        let y = rect.center().y;
        let painter = ui.painter();
        painter.line_segment(
            [
                egui::pos2(rect.left(), y),
                egui::pos2(rect.center().x - 18.0, y),
            ],
            egui::Stroke::new(1.0, color),
        );
        painter.line_segment(
            [
                egui::pos2(rect.center().x + 18.0, y),
                egui::pos2(rect.right(), y),
            ],
            egui::Stroke::new(1.0, color),
        );
        // 中间三段小横线当握把(比单线更"看得出是能抓的东西")
        for i in -1..=1 {
            let cx = rect.center().x + i as f32 * 6.0;
            painter.line_segment(
                [egui::pos2(cx, y - 3.0), egui::pos2(cx, y + 3.0)],
                egui::Stroke::new(1.5, color),
            );
        }
        if resp.hovered() {
            resp.on_hover_text("拖动这条分界可以改截图区的高矮");
        }
    }

    /// 截图画布本体:图片 + 浮层 + 取点 / 拖拽改范围的命中处理(不含按钮行与状态提示)。
    ///
    /// 2026-10-09 从 `ui_picker_body` 摊出来,好让三处共用同一份:
    /// ①下方面板 ②截图小窗 ③扩展宏弹窗里"拼在虚拟键位下方"的那块。
    ///
    /// **尺寸与滚动的分工(用户 2026-10-10 第 3 条)**:画布只按 `kind` 决定"图多大",
    /// 与"这块地方多高"彻底解耦 ——
    /// - 图比可视区小:图居中,没有滚动条(旧实现把图缩到与容器一样大,于是放大
    ///   这件事根本看不出来,还被误当成"上下滚动被禁用");
    /// - 图比可视区大:**滚轮上下(左右)翻动**去取点,`±` 调大小,两者互不牵制;
    /// - 可视区多高由**外层**决定(下方面板靠拖分隔条、扩展宏弹窗靠拖分界条),
    ///   图再大也撑不动它 —— "截图大小不得决定控件高度,控件高度只由鼠标拖拽决定"。
    ///
    /// `kind`:这块画布画在哪 —— 决定"自动倍率"怎么算(见 [`ShotCanvasKind`]),
    /// 也决定滚动位置各自独立(一份 salt 一份记忆,互不串)。
    ///
    /// `virtual_layer`:这块画布要不要画**扩展宏弹窗**那份虚拟键位表。
    /// 用户 2026-10-10(第 4 条)报的两个现象(换成继承键位后截图不同步、新加的取点
    /// 闪一帧就没了)是同一个根因:旧实现只在"正在虚拟取点"那一帧才切到虚拟层,
    /// 于是弹窗开着的时候画布画的仍是实时配置。现在由调用方按**画的这块地方属于谁**
    /// 来定,弹窗那块恒为 true。
    fn ui_shot_canvas(&mut self, ui: &mut egui::Ui, kind: ShotCanvasKind, virtual_layer: bool) {
        let shot = self.shot.as_ref().map(|(t, w, h)| (t.id(), *w, *h));
        if let Some((tex_id, w, h)) = shot {
            // 这块地方能给出多少地方:小窗/弹窗/下方面板各按自己的容器算。
            let view = ui.available_size();
            let base_scale = match kind {
                ShotCanvasKind::Popup => {
                    // 小窗:基准由**屏幕与程序窗口边界**定(见 [`Self::shot_popup_scale`]),
                    // 刻意不看小窗自己多大 —— 否则 `±` 改窗口、窗口又改倍率会互相追着放大。
                    self.shot_popup_base
                }
                ShotCanvasKind::Panel | ShotCanvasKind::Macro => {
                    let b = shot_auto_base(view.x, view.y, w, h);
                    // 抬头那个百分比显示的就是它 × `shot_zoom`(见 `ui_shot_zoom_buttons`)。
                    self.shot_last_base = b;
                    b
                }
            };
            let scale = (base_scale * self.shot_zoom).clamp(0.05, 4.0);
            let size = egui::vec2(w as f32 * scale, h as f32 * scale);
            // 取点或修改响应范围时需要拖拽响应
            let sense = if self.resizing.is_some() {
                egui::Sense::drag()
            } else {
                egui::Sense::click()
            };
            // 内容尺寸 = 图与可视区里的**大者**:图小就撑满可视区再把图居中
            // (等比内接留在某一维的余量均匀落两侧,不堆在右/下),图大就出滚动条。
            let content = size.max(view);
            let scroll = egui::ScrollArea::both()
                .id_salt(kind.scroll_salt())
                .auto_shrink([false, false]);
            scroll.show(ui, |ui| {
                let (area, resp) = ui.allocate_exact_size(content, sense);
                let rect = egui::Rect::from_center_size(area.center(), size);
                ui.painter().image(
                    tex_id,
                    rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
                // 虚拟层画布 = 调用方点名的那块 + "正举着一次虚拟取点"这条老口子
                // (取点可能在主界面下方面板上继续,那种帧也得看得见虚拟层)。
                let virtual_src = virtual_layer || self.picking.is_some_and(CoordSlot::is_virtual);
                self.draw_overlay(ui, rect, scale, virtual_src);

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
                    // 命中区就是整块内容区(含图小的时候内接留下的余量):不必"精确点中
                    // 那张小图"才能拖动,点在旁边的空白里同样算数。
                    let hit = area;
                    if down && origin.map(|o| hit.contains(o)).unwrap_or(false) {
                        if let Some(pos) = pos {
                            // 目标圆心(像素):键位取自己的坐标,轮盘取圆心
                            let (cx, cy) = {
                                let g = lock_shared(&self.shared);
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
                            let mut g = lock_shared(&self.shared);
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
                        // 等比内接必然在某一维留下余量:点在余量里不算"点到截图",
                        // 否则会取到一个屏幕外的坐标(以前命中区就等于图片本身,没这问题)。
                        let on_image = rect.contains(pos);
                        if on_image && let Some(slot) = self.picking {
                            // W2-2:武装期间空间失去对齐(手机转了屏 / 截图被换)也要拦住。
                            // 保持武装不消费 —— 对齐后直接再点即可,不必重新[取点]。
                            if let Some(m) = self.space_guard() {
                                self.log(format!("取点被阻止:{},本点未写入", m.describe()));
                            } else {
                                self.picking = None;
                                self.assign_coord(slot, px, py);
                                // 新增取点后若尚未设键位,立即进入等待按键
                                if slot == CoordSlot::NewBind && self.draft.key.is_none() {
                                    self.waiting_key = Some(KeySlot::NewBind);
                                    self.log("已取点,请按下要绑定的按键");
                                }
                            }
                        } else if on_image && let Some(i) = self.wheel_at(px, py) {
                            // 点到某个摇杆的响应圈:弹出/收起它的方向键信息卡
                            // (画布上只留"摇杆N",细节按需查看)
                            self.wheel_info = if self.wheel_info == Some(i) {
                                None
                            } else {
                                Some(i)
                            };
                        } else {
                            // 点到图上空白处:收起信息卡,并照旧报一次坐标。
                            // 点在图片外的余量里(等比内接留下的边):只收卡片,不报坐标。
                            self.wheel_info = None;
                            if on_image {
                                self.log(format!("截图坐标: ({px}, {py})"));
                            }
                        }
                    }
                }
            });
        }
    }

    /// 截图小窗(2026-10-09):把截图画布放进一个**独立窗口**,可勾选置顶。
    ///
    /// 用独立视口(`show_viewport_immediate`)而不是 `egui::Window`,只有一个理由:
    /// 只有视口能真正"固定在其他窗口上方"(`with_always_on_top`)—— 同一个进程里的
    /// `egui::Window` 永远盖不住别的应用。调试信息悬浮窗走的也是这一套,此处不另起炉灶。
    ///
    /// 用户 2026-10-10(第 3 条)之后,这个小窗:
    /// - **按屏幕分辨率自动定比例** —— 竖屏纵向、横屏横向默认 100%,放不下就按
    ///   "程序窗口 ∩ 系统屏幕"缩到放得下(见 [`shot_popup_area`] / [`shot_auto_base`]);
    /// - **`±` 同时缩放截图与窗口** —— 每帧按 `shot_zoom` 算出应有的窗口尺寸交给
    ///   `with_inner_size`(框架只在值变了时才真的下发命令,所以不会顶掉手动拉伸);
    /// - **带上整套抬头** —— 与下方面板共用 [`Self::ui_shot_header`],不再只有三个按钮。
    fn ui_shot_window(&mut self, ctx: &egui::Context) {
        if !self.shot_window_open {
            return;
        }
        // 小窗的绝对基准(屏幕/程序窗口边界下的合适大小)。**每帧重算**:主窗口被
        // 拉小、或换了截图,小窗里的图跟着变到"仍然放得下"的倍率。
        //
        // 高度预算要**先扣掉抬头**(小窗的窗口 = 图 + 抬头):抬头用估算值而不是上一帧
        // 实测值 —— 实测值随窗口宽度换行、窗口宽度又由这个基准算,两个都取实测就会互相追。
        let area = shot_popup_area(ctx);
        let budget = egui::vec2(
            (area.x - SHOT_POPUP_PAD).max(160.0),
            (area.y - SHOT_POPUP_HEADER_EST - SHOT_POPUP_PAD).max(160.0),
        );
        self.shot_popup_base = match self.shot.as_ref() {
            Some((_, w, h)) => shot_auto_base(budget.x, budget.y, *w, *h),
            None => 1.0,
        };
        let size = self.shot_window_size(ctx);
        let mut vb = egui::ViewportBuilder::default()
            .with_title("scrcpy-pad 截图")
            .with_inner_size([size.x, size.y])
            .with_min_inner_size([160.0, 160.0])
            .with_resizable(true);
        if self.shot_window_pin {
            vb = vb.with_always_on_top();
        }
        let _ = ctx.show_viewport_immediate(shot_window_viewport_id(), vb, |ui, _class| {
            // 点右上角的 X(用户 2026-10-09:"点 X:关闭小窗,主程序恢复显示")。
            // 独立视口的关闭请求要自己接住 —— 不接就等于"点了没反应";
            // 接住 = 只关小窗(内容随即拼回下方面板),主程序照常在那儿。
            // 与调试信息悬浮窗同一套写法。
            if ui.input(|i| i.viewport().close_requested()) {
                self.shot_window_open = false;
                // 视口在这一帧里被请求关闭,后面就别再往里画东西了
                return;
            }
            // 抬头 = 与下方面板**同一份**(按钮行 / 显示项目 / 取点中→取消取点…)。
            // 顺手量一下它实际占多高:`shot_window_size` 要用它把图高加上去,
            // 否则抬头会把图挤掉一截(抬头会换行,高度不是常数)。
            let y0 = ui.cursor().top();
            self.ui_shot_header(ui);
            self.shot_window_header_h = (ui.cursor().top() - y0).max(ui.spacing().interact_size.y);
            // 画布自己带滚动区(见 `ui_shot_canvas`),这里不再套一层。
            // 小窗画哪一层,看它是"从扩展宏弹窗打开"还是"从主界面打开"
            // (见 `shot_window_virtual`);倍率走小窗专用的绝对基准。
            self.ui_shot_canvas(ui, ShotCanvasKind::Popup, self.shot_window_virtual);
        });
    }

    /// 命中测试:截图坐标 (px, py) 落在哪个摇杆的响应圈里(没有则 None)。
    ///
    /// 命中半径取"该摇杆的半径"与一个最小手感半径(24px)的较大者 ——
    /// 用户可能把摇杆调得很小,但"点一下看键位"这个动作不该要求点得那么准。
    /// 多个摇杆重叠时取**最后绘制**的那个(与看到的上下层一致)。
    fn wheel_at(&self, px: i32, py: i32) -> Option<usize> {
        let (profile, space) = {
            let g = lock_shared(&self.shared);
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

/// 扩展宏弹窗里截图画布区的高度:默认值 / 可拖范围(pt)。
/// 用户 2026-10-09 第三轮:"文字与图像分界可拖动调整,布局/尺寸正确,不被文字压住" ——
/// 所以这里不再写死一个 `max_height`,而是留一条能让用户自己定的分界。
const MACRO_SHOT_DEFAULT_H: f32 = 320.0;
const MACRO_SHOT_MIN_H: f32 = 120.0;
const MACRO_SHOT_MAX_H: f32 = 1200.0;
/// 那条可拖分界条的厚度(pt):够宽好抓,又不至于占地方。
const MACRO_SHOT_DIVIDER_H: f32 = 10.0;

/// 还没量到截图小窗抬头的实际高度时,按这个估(约两行)。抬头会随窗口宽度换行,
/// 高度不是常数 —— 但它是"窗口宽度"的函数而不是反过来,所以估算值不会引起来回抖
/// (见 [`PadApp::shot_window_size`])。
const SHOT_POPUP_HEADER_EST: f32 = 60.0;
/// 截图小窗四周留的边(pt):贴着边界算会因窗口装饰与取整差出几个点,
/// 明明"刚好放得下"却弹出滚动条。
const SHOT_POPUP_PAD: f32 = 8.0;

/// 截图画布画在哪块地方(用户 2026-10-10 第 3 条)。决定两件互不相干的事:
/// ①自动倍率怎么算 ②滚动位置各自独立(三块地方互不串)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShotCanvasKind {
    /// 主界面下方面板:按容器内接(容器拉大图跟着大)。
    Panel,
    /// 扩展宏弹窗里拼在虚拟键位下方的那块:同上,只是另占一份滚动位置。
    Macro,
    /// 截图小窗:按**屏幕/程序窗口边界**算绝对倍率 —— 不看自己多大,
    /// 否则"`±` 改窗口尺寸、窗口尺寸又改倍率"会互相追着放大(见 `shot_popup_base`)。
    Popup,
}

impl ShotCanvasKind {
    /// 滚动位置用的 salt:每块画布一份,互不串。
    fn scroll_salt(self) -> &'static str {
        match self {
            Self::Panel => "shot_canvas_scroll_panel",
            Self::Macro => "shot_canvas_scroll_macro",
            Self::Popup => "shot_canvas_scroll_popup",
        }
    }
}

/// 截图小窗的视口 Id。创建与"按倍率下发尺寸"必须用同一个,所以收在一处。
fn shot_window_viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("scrcpy_pad_shot_window")
}

/// 截图小窗最多能占多大地方(pt):程序窗口内容区 ∩ 系统屏幕,四周再留一圈边。
///
/// 用户 2026-10-10(第 3 条):"放不下时按程序窗口与系统屏幕边界调整缩放" ——
/// 判据就是这块地方。留边是因为标题栏/任务栏不归内容区管,贴着算会让窗口少一截。
/// 取不到系统屏幕尺寸(个别平台不给)时就只按程序窗口算。
fn shot_popup_area(ctx: &egui::Context) -> egui::Vec2 {
    let screen = ctx.content_rect().size();
    let monitor = ctx.input(|i| i.viewport().monitor_size).unwrap_or(screen);
    let avail =
        egui::vec2(screen.x.min(monitor.x), screen.y.min(monitor.y)) - egui::vec2(24.0, 24.0);
    // 兜一个下限:算成 0 或负数会让下面的内接倍率变成 0,图直接看不见。
    egui::vec2(avail.x.max(200.0), avail.y.max(200.0))
}

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

/// 「手动 adb 命令」用户预设的落盘位置(与 settings.json 同目录)。
fn adb_presets_path() -> PathBuf {
    config_dir().join(adbcmd::PRESET_FILE)
}

/// 一份命名的「宏草稿」(用户 2026-10-09:草稿要能长期保存、之后自如取用)。
///
/// 存的正是宏页编辑区那几样东西(即 `macro_page_*`)。刻意做成**一份命名列表**
/// 而不是给每条宏都存草稿:用户要的用法就一句"编辑区这份先留着,下次接着改",
/// 所以下拉选 → [载入] / [另存当前草稿] / [删除],与「手动 adb 命令」的用户预设
/// 完全同一套操作(同一个目录、同一个原子写),学会一个就会另一个。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
struct MacroDraft {
    name: String,
    /// 触发键;`None` = 还没定 —— 草稿允许是半成品(与"新建宏"必须齐活不同)。
    key: Option<u16>,
    fps_only: bool,
    /// 「按后延迟」(ms,用户 2026-10-10 第 2 条)。`serde(default)` 是必须的:
    /// 这一项之前的草稿文件里没有这个字段,不给默认值就会**整份草稿库读不出来**。
    #[serde(default = "default_draft_tail_delay_ms")]
    tail_delay_ms: u32,
    steps: Vec<MacroStep>,
    instructions: Vec<MacroInstruction>,
    /// 扩展宏的虚拟键位层;`None` = 普通宏。
    virtual_profile: Option<Profile>,
}

/// `MacroDraft::tail_delay_ms` 的 serde 默认值:老草稿文件里没有这一项时补齐成
/// 程序默认的「按后延迟」。默认值是 `0` = 不等(该功能自 2026-10-10 晚起为**可选**,
/// 由 `Profile::tail_delay_enabled` 总控),所以老草稿读进来不会凭空多出冷却。
fn default_draft_tail_delay_ms() -> u32 {
    crate::keymap::DEFAULT_TAIL_DELAY_MS
}

/// 「宏草稿库」的落盘位置(与 settings.json 同目录)。
fn macro_drafts_path() -> PathBuf {
    config_dir().join("macro_drafts.json")
}

/// 读宏草稿库;文件不存在按空列表处理(首次使用不算错误)。
fn load_macro_drafts(path: &Path) -> Result<Vec<MacroDraft>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("读取失败: {e}")),
    };
    serde_json::from_str(&text).map_err(|e| format!("解析失败: {e}"))
}

/// 写宏草稿库(原子写:临时文件 + 改名,与设置/配置/预设的落盘方式一致)。
fn save_macro_drafts(path: &Path, list: &[MacroDraft]) -> Result<(), String> {
    let text = serde_json::to_string_pretty(list).map_err(|e| format!("序列化失败: {e}"))?;
    write_atomic(path, &text).map_err(|e| format!("写入失败: {e}"))
}

/// settings.json 里记着的"上次选用的配置文件";没记(或记的是空白)时返回 None。
///
/// 该字段是 W0-9 才加进 `Settings` 的:在此之前,用户在 [配置] 里另选/新建的
/// 文件路径只活在内存中,重启必定回到默认的 profile.yaml。
fn remembered_profile_path(saved: &Settings) -> Option<PathBuf> {
    let s = saved.profile_path.as_deref()?.trim();
    if s.is_empty() {
        return None;
    }
    Some(PathBuf::from(s))
}

/// 外观设置的"程序自用"缓存:与键位配置同目录(便于一起备份/清理),
/// 只由程序自己读写,不提供给用户选择或编辑。
fn look_cache_path() -> PathBuf {
    config_dir().join("look.json")
}

/// 读取外观缓存;不存在或内容损坏时返回 (None, 提示),调用方退回配置里的外观。
///
/// 返回值第二项是**要上屏的提示**:读取失败的分级处理见
/// [`read_config_text_graded`] —— "文件不存在"是正常的,不该打扰用户;
/// "读不动/解析不了"必须让用户看到,否则只会觉得"我调好的外观自己变了"。
fn load_look_cache() -> (Option<theme::Look>, Vec<String>) {
    let path = look_cache_path();
    let mut notes = Vec::new();
    let text = match read_config_text_graded(&path) {
        Ok(Some(t)) => t,
        Ok(None) => return (None, notes),
        Err(e) => {
            notes.push(format!("{e},本次改回配置里保存的外观"));
            return (None, notes);
        }
    };
    match serde_json::from_str::<theme::Look>(&text) {
        Ok(l) => (Some(l), notes),
        Err(e) => {
            let mut msg = format!("{} 解析失败({e}),本次改回配置里保存的外观", path.display());
            match backup_broken_config(&path) {
                Ok(b) => msg.push_str(&format!(";原文件已备份为 {}", b.display())),
                Err(be) => msg.push_str(&format!(";备份也失败({be})")),
            }
            crate::diag_error!("look", "{msg}");
            notes.push(msg);
            (None, notes)
        }
    }
}

/// 读取键位配置(YAML);不存在或内容损坏时返回 (None, 提示),调用方用默认值。
///
/// 文件里是**多套按键组合**(见 [`keymap::ConfigFile`]):切换键要指向"哪一套",
/// 分开存反而要多维护一张名单,所以一份文件装全部。
///
/// 返回值第二项是要上屏的提示(读取错误分级 + 解析失败留档,见 W0-9):
/// 配置读不进来绝不能是静默的 —— 用户会看到"我的键位全没了"却不知道该找谁。
fn load_profile() -> (Option<ConfigFile>, Vec<String>) {
    let path = profile_path();
    let mut notes = Vec::new();
    let text = match read_config_text_graded(&path) {
        Ok(Some(t)) => t,
        Ok(None) => return (None, notes),
        Err(e) => {
            notes.push(format!("{e},本次按默认键位运行"));
            return (None, notes);
        }
    };
    match serde_norway::from_str::<ConfigFile>(&text) {
        Ok(mut doc) => {
            doc.normalize();
            (Some(doc), notes)
        }
        Err(e) => {
            let mut msg = format!("{} 解析失败({e}),本次按默认键位运行", path.display());
            match backup_broken_config(&path) {
                Ok(b) => msg.push_str(&format!(";原文件已备份为 {}", b.display())),
                Err(be) => msg.push_str(&format!(";备份也失败({be})")),
            }
            crate::diag_error!("profile", "{msg}");
            notes.push(msg);
            (None, notes)
        }
    }
}

/// 从列表里按下标删除,越界时什么都不做并返回 false(W0-10)。
///
/// 必要性:界面上的删除按钮拿到的是**渲染那一行时**的索引,而列表长度是更早
/// 一次加锁读到的。引擎线程用切换键换组合时会整份替换 `profile`(可能只有
/// 更少的键位/轮盘),`Vec::remove` 遇到越界索引直接 panic —— 一个后台按键
/// 就能把界面按崩。这里先确认下标仍有效再删。
fn remove_indexed<T>(v: &mut Vec<T>, i: usize) -> bool {
    if i < v.len() {
        v.remove(i);
        true
    } else {
        false
    }
}

/// 原子写盘:先写同目录临时文件,再 rename 覆盖目标(W0-9)。
///
/// 为什么:程序里所有配置都是"整份重写"的语义。裸 `fs::write` 写到一半时
/// 崩溃/断电/被杀进程,磁盘上就留下一份**截断的文件** —— 下次启动解析失败,
/// 用户的键位配置整份作废。临时文件 + rename 让"新内容"与"旧内容"之间
/// 只有原子的一步切换:rename 成功前目标始终是完整的旧内容,失败则原样保留。
///
/// 两个硬要求:
///   - 临时文件必须与目标**同目录**(跨卷 rename 在 Windows 上必失败);
///   - 失败时清掉临时文件,不给配置目录留垃圾。
///
/// 只读目标是错误而非静默跳过:`fs::rename` 在 Windows 上是
/// MoveFileEx(REPLACE_EXISTING),目标是只读文件时会 Access Denied ——
/// 这正是我们想要的:保存失败必须让用户看见(旧版有些落盘点把错误吞掉)。
pub(crate) fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = atomic_tmp_path(path);
    let res = (|| -> std::io::Result<()> {
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
            // 落盘后再 rename:否则断电时可能"名字换了、内容还没写下去"
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// 原子写盘用的临时文件名(与目标同目录)。单独成函数是为了让测试
/// 能在失败后检查"临时文件确实被清掉了"。
fn atomic_tmp_path(path: &Path) -> PathBuf {
    let mut name = std::ffi::OsString::from(".");
    name.push(path.file_name().unwrap_or_default());
    name.push(format!(".{}.tmp", std::process::id()));
    match path.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(dir) => dir.join(name),
        None => PathBuf::from(name),
    }
}

/// 读取配置文件文本,容忍 UTF-8 BOM,并按"是不是正常情况"给失败分级(W0-9)。
///
/// 必要性:Windows 上记事本/若干编辑器保存 UTF-8 时会加上 BOM(EF BB BF),
/// 而 BOM 会让 serde_json 直接解析失败 —— 程序于是悄悄退回默认值,
/// 并在下一次落盘时**把用户原本的内容整份覆盖掉**。
/// 用户反馈的"我设置好 scrcpy 目录后,settings.json 里根本没有位置信息"
/// 正是这一类的表现;一个字节的差别不该毁掉整份配置,所以读进来先去掉它。
///
/// 分级(旧版所有读取失败都被 `ok()?` 吞成"没有这个文件",用户只看到
/// 配置"莫名其妙回到了默认值",日志里什么都没有):
///   - 文件不存在 -> `Ok(None)`:首次运行/还没保存过,正常情况,不打日志;
///   - 其他读取错误(权限/被占用/路径是目录…) -> `Err(含路径与原因的说明)`,
///     调用方必须上屏,让用户知道"没读进来"而不是"内容丢了";
///   - 内容不是合法 UTF-8 -> 按替换字符读入(原始字节不丢,备份也照原样复制),
///     若因此解析失败,会走调用方的"解析失败"分支留下 `.broken` 备份。
pub(crate) fn read_config_text_graded(path: &Path) -> Result<Option<String>, String> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("读取 {} 失败: {e}", path.display())),
    };
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Ok(Some(match text.strip_prefix('\u{feff}') {
        Some(rest) => rest.to_string(),
        None => text,
    }))
}

/// 配置解析失败时把原文件另存一份(文件名后追加带时间戳的 `.broken`,
/// 如 `profile.yaml` -> `profile.yaml.broken-20261006-142530`)。
///
/// 为什么:解析失败后程序按默认值运行,而帧末的"内容变了就落盘"会把默认值
/// 写回同一个文件 —— 用户辛苦配的内容就此消失。先留个副本,至少还能捞回来。
///
/// 为什么带时间戳、且把结果返回给调用方(W0-9):旧版固定后缀 `profile.yaml.broken`,
/// 每次解析失败都覆盖同一份 —— 用户接连改坏两次,第一次的现场就没了;
/// 复制失败还被 `let _ =` 吞掉,于是"以为有备份、其实没有"、日志里也查不到。
/// 现在返回备份文件路径,调用方必须把结果写进日志/提示。
pub(crate) fn backup_broken_config(path: &Path) -> Result<PathBuf, String> {
    if !path.is_file() {
        return Err(format!("{} 不存在,无法备份", path.display()));
    }
    // 用 with_extension 会把原扩展名换掉(profile.yaml -> profile.broken),
    // 这里要的是"加一个后缀",因此直接在文件名上拼。
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".broken-{}", timestamp_compact()));
    // 同一秒内连续两次解析失败不能互相覆盖:必要时加序号
    let mut target = path.with_file_name(&name);
    let mut n = 2;
    while target.exists() && n < 100 {
        let mut cand = name.clone();
        cand.push(format!("-{n}"));
        target = path.with_file_name(cand);
        n += 1;
    }
    std::fs::copy(path, &target).map_err(|e| format!("备份 {} 失败: {e}", path.display()))?;
    Ok(target)
}

/// 读取并严格校验任意路径下的键位配置(YAML);返回详细中文错误便于排查
fn read_profile_at(path: &std::path::Path) -> Result<ConfigFile, String> {
    let text = match read_config_text_graded(path)? {
        Some(t) => t,
        None => return Err(format!("{} 不存在", path.display())),
    };
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

/// [已废弃 2026-10-08] `VirtualGamepad*` 分支:虚拟手柄右摇杆通道专用,不再维护。
/// 保留了"虚拟手柄模式下自动补 --mouse/--keyboard/--gamepad=disabled"的既有行为。
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
    write_atomic(path, &text).map_err(|e| format!("写入失败: {e}"))
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

/// 轮盘方向的短标签(方向行与画布标注共用一套,免得两处各写一份慢慢跑偏)。
///
/// `index` 是方向下标(0 基),显示成"上1 / 右2 / 135°3"这种,方便和画布上的
/// 手改终点标注对上号。
fn wheel_dir_label(angle_deg: f32, index: usize) -> String {
    let dir = match angle_deg.round() as i32 {
        -90 => "上".to_string(),
        0 => "右".to_string(),
        90 => "下".to_string(),
        a if a.abs() == 180 => "左".to_string(),
        a => format!("{a}°"),
    };
    format!("{}{}", dir, index + 1)
}

/// 「设置位置」落点圆圈的半径(**屏幕显示像素**)——R2,2026-10-08。
///
/// 用户要求:方向行点过「设置位置」后,那个落点在截图上要画成**圆圈**而不是圆点。
/// 圆圈大小 = **min(影响范围, 按键大小)**:
///
/// * **影响范围** = [`Wheel::push_px`](crate::keymap::Wheel::push_px)(半径 × 影响范围系数)
///   —— 方向键按下后手指实际被推出去的距离,这是圆圈"有多大意义"的本体;
/// * **按键大小** = [`crate::keymap::DEFAULT_RADIUS`],即截图上按键圈的默认大小
///   —— 作为**上限**。影响范围可以调得很大(半径 300px × 系数 4 = 1200px),
///   真按它画圆圈会把整块屏幕糊住、反而看不出落点在哪;取小者保证圆圈始终
///   "像一个手指的接触面"那么大,和截图上的键位圈是同一个视觉尺度。
///
/// **当前固定不可调**(用户拍板:现固定,未来可调)。下面这个函数**就是**那个
/// "预留的 size/半径接口":将来要做「落点圆圈大小」设置,只需在这里加一个入参
/// (或在 `WheelDirection` 上加 `end_radius: Option<f32>`),全链路
/// (浮层标注、信息卡、以及将来任何绘制落点的地方)都只改这一处 ——
/// 所以别再在别处直接写圆点/写死半径。
fn wheel_dir_end_radius_px(m: &Mapper, w: &crate::keymap::Wheel, scale: f32) -> f32 {
    let push_px = w.push_px(m) * scale;
    let key_px = m.len(crate::keymap::DEFAULT_RADIUS) * scale;
    push_px.min(key_px).max(1.0)
}

/// 可以当触发键的鼠标键位(evdev 码,与 `capture.rs` 的钩子同一套空间)。
///
/// 为什么要专门给一份名单:鼠标键**按不出来**。"按任意键"等待期间按一下鼠标,
/// 这一次点击同时就是"在截图上取点",两条路会打架。所以改成下拉里直接**选**是哪个键
/// (用户 2026-10-09 要求)。
///
/// 2026-10-10(用户第 2 条"继续补全滚轮逻辑"):四个滚轮方向**都**列出来 ——
/// 捕获层(Windows/Linux 两侧)本来就在发横向滚轮码(279/280),只是下拉里选不到、
/// 拦截位也没给它们留,于是"左滚/右滚"永远只能干瞪眼。现在补齐:
/// 与 277/278 同一套(可选、可绑、可拦)。
/// 码值对照 `capture.rs::vktable::MOUSE_VK`(272-276)与 `keymap::BTN_WHEEL_*`(277-280)。
fn mouse_key_choices() -> [(u16, &'static str); 9] {
    [
        (crate::keymap::BTN_LEFT, "鼠标左键"),
        (crate::keymap::BTN_RIGHT, "鼠标右键"),
        (274, "鼠标中键"),  // BTN_MIDDLE
        (275, "鼠标侧键1"), // BTN_SIDE (X1)
        (276, "鼠标侧键2"), // BTN_EXTRA (X2)
        (crate::keymap::BTN_WHEEL_UP, "滚轮上滚"),
        (crate::keymap::BTN_WHEEL_DOWN, "滚轮下滚"),
        (crate::keymap::BTN_WHEEL_LEFT, "滚轮左滚"),
        (crate::keymap::BTN_WHEEL_RIGHT, "滚轮右滚"),
    ]
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
        // 手改过终点的方向标出来:它的触点落点由用户在截图上说了算,
        // 不再跟影响范围走(方向行那边的「重置」才会退回基准圆)。
        let manual = w.directions.get(i).is_some_and(|x| x.manual.is_some());
        let key_label = if key.is_empty() {
            "未绑定".to_string()
        } else {
            key.label()
        };
        lines.push((
            format!(
                "{} {}{}",
                wheel_dir_label(angle, i),
                key_label,
                if manual { " · 手改" } else { "" }
            ),
            ink,
        ));
    }
    if let Some(t) = &w.temp {
        let mode = match t.mode {
            TempMode::Hold => "按住启用",
            TempMode::Toggle => "再按切换",
        };
        let key_label = if t.key.is_empty() {
            "未绑定".to_string()
        } else {
            t.key.label()
        };
        lines.push((
            format!("启用 {key_label} · {mode}"),
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
    let pts: Vec<egui::Pos2> = crate::keymap::swipe_points(path, start, end, SWIPE_SAMPLES)
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
    // 虚拟层只负责键位/轮盘坐标与「锚点」(瞄准锚点)，不继承程序级开关与视角状态。
    profile.toggle_key = KeySet::new();
    profile.cursor_toggle_key = KeySet::new();
    // R4(2026-10-08):弹窗里现在能调 [＋ 锚点],锚点必须留下来 ——
    // 否则"设置 → 保存 → 再打开"会把刚取的锚点悄悄抹掉(不可预期)。
    // 其余视角状态(开关键/灵敏度/模式/后坐力…)仍然是程序级的,照旧清空。
    let anchor = (profile.aim.anchor_x, profile.aim.anchor_y);
    profile.aim = Default::default();
    profile.aim.anchor_x = anchor.0;
    profile.aim.anchor_y = anchor.1;
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
        for (angle, keys) in wheel.active_dirs() {
            // 组合键方向:集合里每个成员都亮(点亮任一成员都能看到归属)。
            for key in keys {
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
        // R4(2026-10-08):临时摇杆的启用键也要亮 —— 它和方向键一样"归轮盘",
        // 按下去不会走普通绑定(见 engine.rs `key_owned_by_wheel`)。用 insert
        // (不是 or_insert):同一个键既是绑定又是启用键时,以启用键为准,
        // 与引擎的归属口径一致。
        if let Some(t) = &wheel.temp {
            for key in t.key {
                lights.insert(
                    key,
                    (
                        th.wheel_temp,
                        match t.mode {
                            TempMode::Hold => "虚拟轮盘启用键(按住启用)".to_string(),
                            TempMode::Toggle => "虚拟轮盘启用键(再按切换)".to_string(),
                        },
                    ),
                );
            }
        }
    }
    lights
}

/// 截图预览的**自动适应**倍率：按**容器**的可用宽高做等比内接(contain)。
///
/// 2026-10-09 改口径(用户:截图小窗"右/下大片留白,无法靠拉伸去除"):
/// 旧实现在可用宽高上各砍一刀(`avail*0.94` / `window_h*0.68`)并封顶 1.0,
/// 于是无论怎么拉窗口,右/下都固定留掉一截空白。现在:
/// - **不再留边** —— 直接按容器宽高内接,谁先顶到就按谁定,另一维的余量才是无法避免的等比余量;
/// - **不再封顶 1.0** —— 容器比图大时允许放大填满(否则必然留白);
/// - 容器尺寸取自**画布所在的容器**(`ui.available_width/height`),
///   而不是整个窗口高度 —— 小窗、弹窗、下方面板各自按自己的空间适应。
/// 手动 +/- 仍在这个倍率上乘用户倍率。
fn screenshot_fit_scale(avail_w: f32, avail_h: f32, tex_w: u32, tex_h: u32) -> f32 {
    if tex_w == 0 || tex_h == 0 {
        return 1.0;
    }
    let by_w = avail_w / tex_w as f32;
    let by_h = avail_h / tex_h as f32;
    // 容器尺寸拿不到(NaN/∞,极少数布局中间态)时按原尺寸;为 0 或负数则被下面的
    // 下限兜住 —— 不能给 1.0,那会让"没地方"的一帧画出一张溢出的大图。
    let fit = if by_w.is_finite() && by_h.is_finite() {
        by_w.min(by_h)
    } else {
        1.0
    };
    fit.clamp(0.05, 4.0)
}

/// **自动适应倍率**(用户 2026-10-10 第 3 条):竖屏按纵向、横屏按横向给出 **100%**,
/// 容器放不下时再缩到刚好放得下。
///
/// 与 [`screenshot_fit_scale`] 只差一件事:**封顶 1.0**。容器比图还大时不再放大填满
/// (那是 2026-10-09 的"不留白"口径),因为用户现在明确要"默认 100%":只有 100% 才是
/// 一像素对一像素,再大就是把图拉糊,取点时也更难对准。
///
/// 横竖屏不必各写一句:等比内接天然就是"竖屏顶到纵向、横屏顶到横向"(谁先顶到谁定),
/// 封顶 1.0 只负责"两边都放得下时就是 100%"。
fn shot_auto_base(avail_w: f32, avail_h: f32, tex_w: u32, tex_h: u32) -> f32 {
    screenshot_fit_scale(avail_w, avail_h, tex_w, tex_h).min(1.0)
}

/// 纵横比相对偏差超过它就地"未对齐"(方案 W2-2 定 2%)。
const SPACE_ASPECT_TOLERANCE: f32 = 0.02;

/// 截图空间与控制通道(注入)空间纵横比不一致的证据(W2-2 守卫,判定见
/// [`space_mismatch`])。
#[derive(Debug, Clone, Copy, PartialEq)]
struct SpaceMismatch {
    /// 截图尺寸(浮层绘制与取点所依据的空间)
    shot: (u32, u32),
    /// 控制通道尺寸(引擎注入所用坐标空间)
    inject: (u32, u32),
    /// 纵横比相对偏差(0.05 = 5%,两方向归一:谁大谁做分子)
    dev: f32,
}

impl SpaceMismatch {
    /// 一句话说明,给日志与面板提示共用(措辞即"提示明确"的验收口径)。
    fn describe(&self) -> String {
        let (sw, sh) = self.shot;
        let (iw, ih) = self.inject;
        format!(
            "截图 {sw}x{sh} 与触摸坐标空间 {iw}x{ih} 的纵横比不一致(偏差 {:.0}%)",
            self.dev * 100.0
        )
    }
}

/// **坐标空间一致性判定**:截图空间 vs 注入空间的纵横比偏差是否超阈值。
///
/// 为什么按**纵横比**而不是"尺寸必须相等":配置坐标是相对值,同比例不同分辨率
/// (截图 1080x2400 / 注入空间 720x1600)取点落点完全一致;真正会让取点落偏的是
/// **比例**变了 —— 手机转了屏、或截图还是上一次方向下拍的。返回 `None` 表示
/// "一致或无法判定"(任一侧尺寸未知),调用方按"放行"处理。
///
/// 判定用交叉相乘(`sw*ih` vs `sh*iw`),两方向对称:1080x2400 对 2400x1080
/// 的偏差 ≈ 3.94(394%),会被稳稳拦住。参照 scrcpy PositionMapper 的
/// "声明尺寸与最新帧不一致就丢事件",
/// 我们对应做"拒绝取点 + 提示"(注入侧引擎只用控制空间,内部自洽,无需守卫)。
fn space_mismatch(shot: (u32, u32), inject: (u32, u32)) -> Option<SpaceMismatch> {
    let ((sw, sh), (iw, ih)) = (shot, inject);
    if sw == 0 || sh == 0 || iw == 0 || ih == 0 {
        return None;
    }
    let r = (sw as f64 * ih as f64) / (sh as f64 * iw as f64);
    if !r.is_finite() || r <= 0.0 {
        return None;
    }
    let dev = if r >= 1.0 { r - 1.0 } else { 1.0 / r - 1.0 };
    if dev <= SPACE_ASPECT_TOLERANCE as f64 {
        return None;
    }
    Some(SpaceMismatch {
        shot,
        inject,
        dev: dev as f32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// R4(2026-10-08):扩展宏虚拟层的"净化"必须**留下锚点**。
    ///
    /// 弹窗里 [＋ 锚点] 设的锚点要能保存进宏草稿、再打开时还在
    /// (否则"设置 → 保存到宏草稿 → 再打开"会把刚取的锚点悄悄抹掉,不可预期);
    /// 其余视角状态(开关/灵敏度/模式)与程序级开关照旧不继承,
    /// 宏步骤也照旧被剔掉(宏不嵌套)。
    #[test]
    fn sanitize_virtual_profile_keeps_the_aim_anchor_only() {
        let macro_action = || {
            Action::Macro(MacroAction {
                virtual_profile: None,
                steps: Vec::new(),
                instructions: Vec::new(),
            })
        };
        let mut p = Profile::default();
        p.binds.push(KeyBind {
            key: 30,
            action: Action::Hold {
                x: 0.1,
                y: 0.2,
                radius: 0.03,
            },
            fps_only: false,
            tail_delay_ms: 0,
        });
        p.binds.push(KeyBind {
            key: 31,
            action: macro_action(),
            fps_only: false,
            tail_delay_ms: 0,
        });
        p.combos.push(KeyCombo {
            keys: vec![29, 42],
            action: macro_action(),
            fps_only: false,
            tail_delay_ms: 0,
        });
        p.aim.anchor_x = 0.25;
        p.aim.anchor_y = 0.75;
        p.aim.enabled = true;
        p.aim.sensitivity_x = 9.0;
        p.toggle_key = KeySet::single(66);
        p.cursor_toggle_key = KeySet::single(65);

        sanitize_virtual_profile(&mut p);

        assert_eq!(
            (p.aim.anchor_x, p.aim.anchor_y),
            (0.25, 0.75),
            "锚点必须留下"
        );
        assert!(p.aim.anchor_set());
        assert!(!p.aim.enabled, "视角开关是程序级的,不继承");
        assert_ne!(p.aim.sensitivity_x, 9.0, "灵敏度是程序级的,不继承");
        assert_eq!(p.toggle_key, 0, "总开关键不继承");
        assert_eq!(p.cursor_toggle_key, 0, "鼠标消隐键不继承");
        assert_eq!(p.binds.len(), 1, "宏步骤的键位要剔掉");
        assert_eq!(p.binds[0].key, 30);
        assert!(p.combos.is_empty(), "宏步骤的组合键也要剔掉");
    }

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

    /// W0-9 落盘纪律:原子写在"目标不存在""目标已存在"两种情况下
    /// 都必须产出**完整**内容,且不留临时文件。
    #[test]
    fn write_atomic_creates_and_overwrites_completely() {
        let dir = std::env::temp_dir().join(format!("scrcpy-pad-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // 父目录不存在:必须自动创建(各落盘点过去都自己 create_dir_all)
        let path = dir.join("sub").join("profile.yaml");
        write_atomic(&path, "第一版\n").expect("首次写入必须成功");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "第一版\n");
        write_atomic(&path, "第二版\n").expect("覆盖写入必须成功");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "第二版\n",
            "覆盖必须是整份替换,不能残留旧内容的尾巴"
        );
        assert!(
            !atomic_tmp_path(&path).exists(),
            "成功路径上不该留下临时文件"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// W0-9:写不进去时必须**报错**并清掉临时文件。
    /// 目标是目录(rename 必失败)用来模拟"写不进去"这种失败,跨平台稳定。
    #[test]
    fn write_atomic_failure_is_reported_and_leaves_no_temp() {
        let dir =
            std::env::temp_dir().join(format!("scrcpy-pad-atomic-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let target = dir.join("占位目录");
        std::fs::create_dir_all(&target).unwrap();
        let err = write_atomic(&target, "内容").expect_err("目标不可写时必须报错,而不是静默");
        assert!(!err.to_string().is_empty());
        assert!(
            !atomic_tmp_path(&target).exists(),
            "失败后必须清掉临时文件,不给配置目录留垃圾"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// W0-9(手工验收的自动化版):目标是只读文件时必须报错。
    ///
    /// 为什么重要:旧版有落盘点把写入错误吞掉,用户以为"保存成功",
    /// 重启才发现改动没写进去。只读文件在 Windows 上正是
    /// `fs::rename`(MoveFileEx REPLACE_EXISTING)会 Access Denied 的场景。
    #[cfg(windows)]
    #[test]
    fn write_atomic_reports_readonly_target_and_keeps_old_content() {
        let dir = std::env::temp_dir().join(format!("scrcpy-pad-ro-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("profile.yaml");
        std::fs::write(&path, "旧内容").unwrap();
        // 先存下原始权限,结束时原样还原 —— 比 set_readonly(false) 稳,
        // 后者在 Unix 语义下会把文件放开成 0o666(clippy 也会拦)。
        let orig_perms = std::fs::metadata(&path).unwrap().permissions();
        let mut perms = orig_perms.clone();
        perms.set_readonly(true);
        std::fs::set_permissions(&path, perms).unwrap();

        let err = write_atomic(&path, "新内容").expect_err("只读目标必须报错");
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "旧内容",
            "写失败时原文件必须原样保留 —— 这正是原子写的意义"
        );

        // 还原权限再清理,免得留下删不掉的只读文件
        let _ = std::fs::set_permissions(&path, orig_perms);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// W0-9 读取分级:不存在 = 正常(静默);读不动(路径是目录)= 要报给用户的错误;
    /// BOM 照旧被剥掉(不能因为分级把这条老修复弄丢)。
    #[test]
    fn read_grading_separates_missing_from_unreadable() {
        let dir = std::env::temp_dir().join(format!("scrcpy-pad-read-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        assert!(
            matches!(
                read_config_text_graded(&dir.join("没有这个文件.yaml")),
                Ok(None)
            ),
            "文件不存在是首次运行的正常情况,必须静默"
        );
        let bom = dir.join("bom.json");
        std::fs::write(&bom, "\u{feff}{\"a\":1}").unwrap();
        assert_eq!(
            read_config_text_graded(&bom).unwrap().unwrap(),
            "{\"a\":1}",
            "BOM 必须被剥掉(Windows 编辑器会加)"
        );
        assert!(
            read_config_text_graded(&dir).is_err(),
            "路径是目录时必须报错上屏,不能当成『没有配置文件』"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// W0-10:列表删除用的下标来自渲染那一行时,而列表长度是更早一次加锁读到的 ——
    /// 引擎用切换键换组合(换成更少的键位)时,`Vec::remove(i)` 会越界 panic。
    /// 删除必须先核查下标:失效就什么都不删,绝不能崩。
    #[test]
    fn remove_indexed_never_panics_on_stale_index() {
        let mut v = vec!["甲", "乙", "丙"];
        assert!(!remove_indexed(&mut v, 99), "越界下标必须安全失败");
        assert_eq!(v, vec!["甲", "乙", "丙"], "越界时不得误删别的条目");
        assert!(!remove_indexed(&mut v, 3), "正好等于长度也是越界");
        assert!(remove_indexed(&mut v, 1));
        assert_eq!(v, vec!["甲", "丙"]);
        assert!(!remove_indexed(&mut v, 2), "删到只剩两条后,原下标已失效");
        assert!(remove_indexed(&mut v, 0));
        assert_eq!(v, vec!["丙"]);
        // 空列表:任何下标都不该 panic
        v.clear();
        assert!(!remove_indexed(&mut v, 0));
    }

    /// W0-9:解析失败的备份必须带时间戳、不互相覆盖,且失败要能报出来。
    #[test]
    fn broken_backup_is_timestamped_and_never_overwrites() {
        let dir = std::env::temp_dir().join(format!("scrcpy-pad-broken-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("look.json");
        std::fs::write(&path, "{坏掉的").unwrap();

        let first = backup_broken_config(&path).expect("备份必须成功");
        let second = backup_broken_config(&path).expect("第二次备份也必须成功");
        assert_ne!(first, second, "两次备份不能互相覆盖(旧版固定后缀就会)");
        assert!(first.exists() && second.exists());
        let name = first.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("look.json.broken-"),
            "备份名应保留原名并带时间戳后缀: {name}"
        );
        assert_eq!(
            std::fs::read_to_string(&first).unwrap(),
            "{坏掉的",
            "备份必须原样"
        );

        assert!(
            backup_broken_config(&dir.join("没有这个文件")).is_err(),
            "没东西可备份时必须报错 —— 旧版 `let _ =` 会让用户以为有备份"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// W0-9:`settings.json` 里记的配置路径要能穿过"启动读回"这一步:
    /// 只有非空、且与默认位置不同的路径才触发恢复。
    #[test]
    fn remembered_profile_path_is_trimmed_and_optional() {
        let mut s = Settings::default();
        assert_eq!(remembered_profile_path(&s), None, "没记过 -> 走默认配置");
        s.profile_path = Some("   ".into());
        assert_eq!(remembered_profile_path(&s), None, "空白等于没记");
        s.profile_path = Some("  /tmp/我的.yaml  ".into());
        assert_eq!(
            remembered_profile_path(&s),
            Some(PathBuf::from("/tmp/我的.yaml")),
            "手改过的 settings.json 允许带空白,必须去掉再用"
        );
    }

    /// 宏草稿库(2026-10-09):存出去再读回来必须**逐字段一致** ——
    /// 草稿的价值全在"下次原样取回",掉一个字段就等于让人重做一遍。
    /// 顺带盯住"文件不存在 = 空列表"(首次使用不该报错)。
    #[test]
    fn macro_drafts_round_trip_and_missing_file_is_empty() {
        let dir = std::env::temp_dir().join(format!("scrcpy-pad-drafts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("macro_drafts.json");
        assert!(
            load_macro_drafts(&path).unwrap().is_empty(),
            "文件不存在时按空列表处理,不能算错误"
        );
        let list = vec![
            // 半成品草稿:还没有触发键,但有录制步骤 + 设置项(草稿允许不完整)
            MacroDraft {
                name: "半成品".into(),
                key: None,
                fps_only: false,
                tail_delay_ms: 30,
                steps: vec![MacroStep {
                    code: 65,
                    pressed: true,
                    delay_ms: 30,
                }],
                instructions: vec![MacroInstruction::Delay { ms: 120 }],
                virtual_profile: None,
            },
            // 带虚拟键位层的草稿(扩展宏那份 profile 必须一起回得来)
            MacroDraft {
                name: "带虚拟层".into(),
                key: Some(66),
                fps_only: true,
                tail_delay_ms: 0,
                steps: Vec::new(),
                instructions: Vec::new(),
                virtual_profile: Some(Profile::default()),
            },
        ];
        save_macro_drafts(&path, &list).expect("保存必须成功");
        assert_eq!(load_macro_drafts(&path).unwrap(), list);
        let _ = std::fs::remove_dir_all(&dir);
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

    /// 录制起点追加不变量(用户 2026-10-07 再次强调):**按下第一个键之前不算录制**。
    /// 点[开始录制]后哪怕发呆超过空闲时间也不许自动结束 —— 只有录到过步骤,
    /// 空闲计时才有意义。
    #[test]
    fn macro_idle_stop_waits_for_first_key() {
        let base = Instant::now();
        let idle_ago = base - Duration::from_secs(10); // 远超默认空闲 800ms
        let mut rec = MacroRecording {
            steps: Vec::new(),
            held: HashSet::new(),
            last_event: idle_ago,
            last_step_at: idle_ago,
            idle_ms: 800,
        };
        assert!(!rec.should_auto_stop(), "第一个按键之前绝不能自动停止");
        PadApp::record_macro_button(&mut rec, 18, true, base);
        assert!(!rec.should_auto_stop(), "刚按键,空闲时间未到");
        rec.last_event = base - Duration::from_millis(801);
        assert!(rec.should_auto_stop(), "有步骤 + 超时 -> 才允许停止");
    }

    /// 宏起始点(真机反馈 1):**第一个按键事件就是宏的起点**。
    ///
    /// 用户点[开始录制]之后翻键盘、想内容的空档不能算进宏里,否则回放会先愣
    /// 几秒。录制侧已经短路了首步延迟,这里锁住兜底归一化:旧配置 / 导入的 YAML /
    /// 手工编辑来的宏也必须满足同一条不变量。
    #[test]
    fn macro_first_step_delay_is_always_zeroed() {
        let step = |code: u16, pressed: bool, delay_ms: u32| MacroStep {
            code,
            pressed,
            delay_ms,
        };
        // 典型脏数据:第一步带着"开始录制→第一次按键"的空档
        let mut steps = vec![
            step(18, true, 4200),
            step(18, false, 100),
            step(19, true, 0),
        ];
        PadApp::normalize_macro_start(&mut steps);
        assert_eq!(steps[0].delay_ms, 0, "首步延迟必须归零");
        assert_eq!(
            (steps[1].delay_ms, steps[2].delay_ms),
            (100, 0),
            "后面的步骤一个都不许动"
        );
        // 已经是 0:幂等,不改变任何内容
        let before = steps.clone();
        PadApp::normalize_macro_start(&mut steps);
        assert_eq!(steps, before, "归一化必须幂等");
        // 单步宏 / 空宏都不 panic
        let mut one = vec![step(18, true, 999)];
        PadApp::normalize_macro_start(&mut one);
        assert_eq!(one[0].delay_ms, 0);
        let mut empty: Vec<MacroStep> = Vec::new();
        PadApp::normalize_macro_start(&mut empty);
        assert!(empty.is_empty());
    }

    /// 2026-10-09 改口径(用户:截图小窗"右/下大片留白,无法靠拉伸去除"):
    /// 自动适应必须是**精确等比内接** —— 谁先顶到就按谁定,不再各砍一刀留边,也不再封顶 1.0。
    #[test]
    fn screenshot_fit_scale_is_exact_contain() {
        // 竖屏:按高度受限,恰好内接(不留 0.68 那一刀)
        let portrait = screenshot_fit_scale(600.0, 900.0, 1080, 2400);
        assert!((portrait - 900.0 / 2400.0).abs() < 1e-6, "{portrait}");
        // 横屏:按宽度受限,恰好内接(旧的 0.94 缩边已去掉)
        let landscape = screenshot_fit_scale(1200.0, 900.0, 2400, 1080);
        assert!((landscape - 0.5).abs() < 1e-6, "{landscape}");
        // 容器比图大:允许放大填满 —— 封顶 1.0 就是"右下永远留白"的来源
        assert!(screenshot_fit_scale(4000.0, 3000.0, 1000, 500) > 1.0);
        // 退化输入:不 panic,也不给出"溢出的大图"(容器还没布局时给下限)
        assert!(screenshot_fit_scale(0.0, 0.0, 1080, 2400) < 0.1);
        assert_eq!(screenshot_fit_scale(100.0, 100.0, 0, 0), 1.0);
    }

    /// W2-2 验收口径:构造"截图 1080x2400 / 控制通道 2400x1080"必须被拦,
    /// 同比例不同分辨率不能误伤(相对坐标换算本来就不受影响)。
    #[test]
    fn space_guard_blocks_rotated_space_but_allows_same_aspect() {
        // 旋转对调(验收场景):拦,且提示里两个尺寸都在
        let m = space_mismatch((1080, 2400), (2400, 1080)).expect("转过屏的空间必须被拦");
        assert!(m.dev > 3.0, "旋转的偏差应远超阈值,实际 {}", m.dev);
        let d = m.describe();
        assert!(
            d.contains("1080x2400") && d.contains("2400x1080"),
            "提示: {d}"
        );
        assert!(d.contains("纵横比"), "提示: {d}");
        // 反向对调也拦
        assert!(space_mismatch((2400, 1080), (1080, 2400)).is_some());
        // 同比例不同分辨率:放行
        assert!(space_mismatch((1080, 2400), (720, 1600)).is_none());
        // 完全相同:放行
        assert!(space_mismatch((1080, 2400), (1080, 2400)).is_none());
        // 比例轻微不同(≈2.6%)也要拦;1% 以内不误伤
        assert!(space_mismatch((1080, 2400), (1080, 2340)).is_some());
        assert!(space_mismatch((1000, 1000), (1010, 1000)).is_none());
        // 某一侧尺寸未知:不判定(调用方放行)
        assert!(space_mismatch((0, 0), (1080, 2400)).is_none());
        assert!(space_mismatch((1080, 2400), (0, 0)).is_none());
    }

    /// 断线自动重连的退避节拍(2026-10-06):单调不减、首档必须够快
    /// (多数断开一秒内就能回来),末档封顶 10s(设备真掉了也别刷 adb 进程)。
    #[test]
    fn reconnect_backoff_is_monotonic_and_capped() {
        let secs: Vec<u64> = (0..PadApp::RECONNECT_MAX_ATTEMPTS + 4)
            .map(|a| PadApp::reconnect_delay(a).as_secs())
            .collect();
        assert!(
            secs.windows(2).all(|w| w[1] >= w[0]),
            "退避不得回退: {secs:?}"
        );
        assert_eq!(secs[0], 1, "第一次重连不该让玩家干等");
        assert!(secs.iter().all(|&s| s <= 10), "末档封顶 10s: {secs:?}");
        assert_eq!(*secs.last().unwrap(), 10, "超过上限后应停在 10s");
    }
}
