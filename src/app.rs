use crate::adb::{self, ControlServer};
use crate::capture::{Capture, CaptureKey};
use crate::control::ControlClient;
use crate::engine::{Shared, SharedState};
use crate::keymap::{Action, Easing, KeyBind, Profile, Swipe, SwipePath, TempMode, TempWheel, Wheel, key_name};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

const SCID: u32 = 0x1a2b3c4d;
const LOCAL_PORT: u16 = 28383;
const REPO_URL: &str = "https://github.com/Azrl-lyh/scrcpy-pad";
const AUTHOR: &str = "Azrl-lyh";

/// scrcpy 启动参数的初始(无预设)值
const BASE_SCRCPY_ARGS: &str = "--stay-awake";

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
            StartPreset::Uhd2k => "2k 高清(最长边 2560)",
            StartPreset::Uhd4k => "4k 高清(最长边 3840)",
            StartPreset::Fhd1080 => "1k 标清(最长边 1080)",
            StartPreset::Hd720 => "720p(最长边 720)",
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
    WheelDir { wheel: usize, dir: usize }, // dir: 0上 1下 2左 3右
    WheelEnable(usize),                   // 临时轮盘启用键
    Toggle,
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
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum OverlayFilter {
    All,
    Keys,
    Wheels,
    PermWheels,
    TempWheels,
}

/// 滑动曲线参数编辑的目标(已有键位或新增草稿)
#[derive(Debug, Clone, Copy, PartialEq)]
enum EasingEditTarget {
    Bind(usize),
    New,
}

impl OverlayFilter {
    fn label(&self) -> &'static str {
        match self {
            OverlayFilter::All => "全部",
            OverlayFilter::Keys => "仅键位",
            OverlayFilter::Wheels => "仅摇杆",
            OverlayFilter::PermWheels => "永久摇杆",
            OverlayFilter::TempWheels => "临时摇杆",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum DialogPurpose {
    ScrcpyExe,
    ServerJar,
    AdbExe,
    SaveLog,
    SaveProfileAs,
    /// 选用已有键位 json 作为当前配置
    ChooseProfile,
    /// 新建键位 json(路径可不存在,选择后写入全新默认配置)
    NewProfile,
}

struct DraftBind {
    key: Option<u16>,
    kind: usize, // 0点按 1长按 2滑动 3系统键
    x: i32,
    y: i32,
    /// 点按/长按的响应范围(截图像素半径)
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
            x: 540,
            y: 1200,
            radius: crate::keymap::DEFAULT_RADIUS,
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

pub struct PadApp {
    shared: SharedState,
    grab_flag: Arc<std::sync::atomic::AtomicBool>,
    _capture: Option<Capture>,
    capture_err: Option<String>,
    gui_rx: Receiver<CaptureKey>,

    devices: Vec<String>,
    selected: usize,
    scrcpy_args: String,
    /// scrcpy 可执行文件路径(空 = 使用 PATH 中的 scrcpy)
    scrcpy_path: String,
    server_path: String,
    /// adb 可执行文件路径(空 = 自动寻找:优先 scrcpy 同目录,再 PATH)
    adb_path: String,
    /// 已应用的 (scrcpy, server, adb) 三元组;用于文本改动后自动联动补齐
    suite_synced: (String, String, String),
    /// 由[测试]/[自动寻找]检测出的版本,只读显示
    server_version: String,
    test_msg: Option<(bool, String)>,

    server: Option<ControlServer>,
    connect_rx: Option<Receiver<Result<(ControlServer, ControlClient), String>>>,

    waiting_key: Option<KeySlot>,
    picking: Option<CoordSlot>,
    /// 正在修改响应范围的键位索引(None=未修改);进入后该键位圆圈显示为黄色
    resizing: Option<usize>,
    /// 正在编辑滑动曲线参数的键位(弹窗)
    easing_edit: Option<EasingEditTarget>,
    /// 新增键位草稿是否进行中(决定预览圆圈/轨迹是否显示,并允许取消)
    draft_active: bool,
    shot: Option<(egui::TextureHandle, u32, u32)>,
    shot_rx: Option<Receiver<Result<egui::ColorImage, String>>>,
    overlay_filter: OverlayFilter,

    draft: DraftBind,
    logs: VecDeque<String>,
    profile_path: PathBuf,
    grab_enabled: bool,

    about_open: bool,
    /// scrcpy 参数助手窗口状态(None=未打开;每次打开重建默认参数)
    args_helper: Option<ArgHelp>,
    dialog: Option<crate::filedialog::FileDialogHandle>,
    dialog_purpose: DialogPurpose,
    loginfo_rx: Option<Receiver<adb::DeviceInfo>>,
    pending_log: Option<String>,

    // 撤销/重做栈(键位配置快照)
    undo_stack: Vec<Profile>,
    redo_stack: Vec<Profile>,
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
        eprintln!("[font] 已加载中文字体: {path}");
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
        let (cap_tx, cap_rx) = channel::<CaptureKey>();
        let (gui_tx, gui_rx) = channel::<CaptureKey>();

        let profile = load_profile().unwrap_or_default();
        let profile_path = profile_path();

        let shared: SharedState = Arc::new(std::sync::Mutex::new(Shared {
            profile,
            enabled: false,
            control: None,
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

        // 映射引擎线程
        {
            let shared = shared.clone();
            std::thread::spawn(move || crate::engine::run(shared, cap_rx, gui_tx));
        }

        // 自动寻找 scrcpy 与 server
        let (scrcpy_path, server_path, version, found_msg) = {
            let exe = adb::find_scrcpy();
            // server 找不到就留空:后续用户选定 scrcpy.exe 后,由 sync_suite 按其同目录
            // 自动补齐(官方发行包三者同目录),避免预先填入的相对路径阻塞自动发现
            let server = adb::find_server(exe.as_deref())
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            match exe {
                Some(p) => {
                    let ps = p.display().to_string();
                    let v = adb::scrcpy_version_at(&ps).unwrap_or_default();
                    (
                        ps.clone(),
                        server,
                        v,
                        format!("已自动找到 scrcpy: {ps}"),
                    )
                }
                None => (
                    String::new(),
                    server,
                    String::new(),
                    "未找到 scrcpy,请在左栏手动指定路径".to_string(),
                ),
            }
        };

        // 启动时定位 adb:优先 scrcpy 同目录(官方 Windows 发行包含同目录 adb.exe),
        // 其次 PATH;拿到后才列设备,否则 Windows 上 scrcpy 正常但设备列表却为空
        let startup_adb = {
            let exe = if scrcpy_path.trim().is_empty() {
                None
            } else {
                Some(PathBuf::from(scrcpy_path.trim()))
            };
            adb::find_adb(exe.as_deref())
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
            _capture: capture,
            capture_err,
            gui_rx,
            devices,
            selected: 0,
            scrcpy_args: BASE_SCRCPY_ARGS.into(),
            scrcpy_path,
            server_path,
            adb_path: adb_init,
            // 置空使其在首帧自动做一次全套联动补齐
            suite_synced: (String::new(), String::new(), String::new()),
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
            shot_rx: None,
            overlay_filter: OverlayFilter::All,
            draft: DraftBind::default(),
            logs: VecDeque::new(),
            profile_path,
            grab_enabled: false,
            about_open: false,
            args_helper: None,
            dialog: None,
            dialog_purpose: DialogPurpose::ScrcpyExe,
            loginfo_rx: None,
            pending_log: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
        };
        app.log("就绪。顺序: 连接手机 -> [连接控制] -> [启动 scrcpy] -> 按总开关键开启映射");
        app.log(found_msg);
        match &startup_adb {
            Some(p) => app.log(format!("adb: {}", p.display())),
            None => app.log(
                "未找到 adb(设备列表将为空): 请将 adb.exe 所在目录加入 PATH,或在左栏手动指定",
            ),
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
                self.log(format!("scrcpy 测试通过: 版本 {v},server: {}", self.server_path));
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
                let (w, h) = adb::screen_size(&serial).map_err(|e| format!("{e:#}"))?;
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

    fn assign_key(&mut self, slot: KeySlot, code: u16) {
        self.push_undo();
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
                        match dir {
                            0 => w.up = code,
                            1 => w.down = code,
                            2 => w.left = code,
                            _ => w.right = code,
                        }
                    }
                }
                KeySlot::WheelEnable(i) => {
                    if let Some(w) = g.profile.wheels.get_mut(i) {
                        let mode = w
                            .temp
                            .as_ref()
                            .map(|t| t.mode)
                            .unwrap_or(TempMode::Hold);
                        w.temp = Some(TempWheel { key: code, mode });
                    }
                }
                KeySlot::Toggle => g.profile.toggle_key = code,
            }
        }
        self.log(format!("键位已绑定: {}", key_name(code)));
    }

    fn assign_coord(&mut self, slot: CoordSlot, x: i32, y: i32) {
        self.push_undo();
        {
            let mut g = self.shared.lock().unwrap();
            match slot {
                CoordSlot::NewBind => {
                    self.draft.x = x;
                    self.draft.y = y;
                }
                CoordSlot::Bind(i) => {
                    if let Some(b) = g.profile.binds.get_mut(i) {
                        if let Action::Tap { x: ax, y: ay, .. } | Action::Hold { x: ax, y: ay, .. } =
                            &mut b.action
                        {
                            *ax = x;
                            *ay = y;
                        }
                    }
                }
                CoordSlot::WheelCenter(i) => {
                    if let Some(w) = g.profile.wheels.get_mut(i) {
                        w.cx = x;
                        w.cy = y;
                    }
                }
                CoordSlot::SwipeStart(i) => {
                    if let Some(b) = g.profile.binds.get_mut(i) {
                        if let Action::Swipe(s) = &mut b.action {
                            s.start = (x, y);
                        }
                    }
                }
                CoordSlot::SwipeEnd(i) => {
                    if let Some(b) = g.profile.binds.get_mut(i) {
                        if let Action::Swipe(s) = &mut b.action {
                            s.end = (x, y);
                        }
                    }
                }
                CoordSlot::NewSwipeStart => self.draft.swipe_start = (x, y),
                CoordSlot::NewSwipeEnd => self.draft.swipe_end = (x, y),
                CoordSlot::CircleAngle(i) => {
                    if let Some(b) = g.profile.binds.get_mut(i) {
                        if let Action::Swipe(s) = &mut b.action {
                            set_circle_angle(&mut s.path, s.start, s.end, x, y);
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
            }
        }
        self.log(format!("坐标已设置: ({x}, {y})"));
    }

    /// 组装完整日志文本(含环境信息)
    fn build_log_content(&self, info: &adb::DeviceInfo) -> String {
        let mut s = String::new();
        s.push_str("scrcpy-pad 运行日志\n");
        s.push_str(&format!("保存时间: {}\n", fmt_timestamp(std::time::SystemTime::now())));
        s.push_str(&format!("程序版本: {}\n", env!("CARGO_PKG_VERSION")));
        s.push_str(&format!("主机环境: {}\n", info.host_os));
        s.push_str(&format!(
            "scrcpy: {} ({})\n",
            info.scrcpy,
            if self.scrcpy_path.is_empty() { "PATH" } else { &self.scrcpy_path }
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

    // ===================== 撤销 / 重做 =====================

    /// 把当前配置压入撤销栈(所有键位修改入口调用),并清空重做栈
    fn push_undo(&mut self) {
        let profile = self.shared.lock().unwrap().profile.clone();
        self.undo_stack.push(profile);
        self.redo_stack.clear();
        if self.undo_stack.len() > 50 {
            self.undo_stack.remove(0);
        }
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

    /// 保存当前键位配置到默认位置
    fn save_profile(&mut self) {
        let json = {
            let g = self.shared.lock().unwrap();
            serde_json::to_string_pretty(&g.profile)
        };
        match json {
            Ok(json) => {
                if let Some(parent) = self.profile_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match std::fs::write(&self.profile_path, json) {
                    Ok(_) => self.log(format!("已保存到 {}", self.profile_path.display())),
                    Err(e) => self.log(format!("保存失败: {e}")),
                }
            }
            Err(e) => self.log(format!("序列化失败: {e}")),
        }
    }

    /// 从当前指向的配置文件重新加载(重定向后也从新路径加载)
    fn reload_profile_from_current(&mut self) {
        match read_profile_at(&self.profile_path) {
            Ok(p) => {
                self.push_undo();
                self.shared.lock().unwrap().profile = p;
                self.log(format!("配置已重新加载(可撤销): {}", self.profile_path.display()));
            }
            Err(e) => self.log(format!("重新加载失败: {e}")),
        }
    }

    /// 切换当前配置文件后整体替换配置内容;
    /// 撤销/重做栈指向旧文件数据,与当前上下文无关,一并清空避免误操作
    fn apply_profile_switch(&mut self, p: Profile) {
        self.shared.lock().unwrap().profile = p;
        self.undo_stack.clear();
        self.redo_stack.clear();
    }

    /// 取消新增草稿:清除取点等待、等待按键与草稿圆圈/轨迹
    fn cancel_draft(&mut self) {
        self.picking = None;
        if self.waiting_key == Some(KeySlot::NewBind) {
            self.waiting_key = None;
        }
        self.draft.key = None;
        self.draft_active = false;
    }

    /// 检测【当前】配置文件是否符合格式要求(不弹文件选择框)
    fn check_current_profile(&mut self) {
        match read_profile_at(&self.profile_path) {
            Ok(p) => self.log(format!(
                "检测通过: {} 是合法配置 ({} 按键 / {} 轮盘)",
                self.profile_path.display(),
                p.binds.len(),
                p.wheels.len()
            )),
            Err(e) => self.log(format!(
                "检测不通过: {} —— {e}",
                self.profile_path.display()
            )),
        }
    }

    /// 分辨率预设:先把参数整体重置为初始“无”状态,再叠加对应 --max-size
    fn set_res_preset(&mut self, max_size: u32) {
        self.scrcpy_args = format!("{BASE_SCRCPY_ARGS} --max-size={max_size}");
        self.log(format!("启动参数已设为分辨率预设(最长边 {max_size})"));
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
                if self.scrcpy_args.split_whitespace().any(|a| a == "--no-audio") {
                    self.log("参数已含 --no-audio,无需重复");
                } else {
                    let base = self.scrcpy_args.trim();
                    self.scrcpy_args =
                        format!("{base} --no-audio").split_whitespace().collect::<Vec<_>>().join(" ");
                    self.log("已追加 --no-audio(不使用音频输出)");
                }
            }
            StartPreset::WithAudio => {
                let had = self.scrcpy_args.split_whitespace().any(|a| a == "--no-audio");
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
        self.suite_synced = (String::new(), String::new(), String::new());
        self.sync_suite();
    }

    /// 联动补齐:scrcpy.exe / scrcpy-server / adb.exe 三者任填一个,其余为空时
    /// 自动从同目录(官方 Windows 发行包三者在同一目录)推导补齐;
    /// 生效的 adb 变化时自动刷新设备列表。每帧调用,仅在三元组文本变化时执行。
    fn sync_suite(&mut self) {
        let trio = (
            self.scrcpy_path.clone(),
            self.server_path.clone(),
            self.adb_path.clone(),
        );
        if trio == self.suite_synced {
            return;
        }
        self.suite_synced = trio;

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
                None => self.log("未找到 adb(设备列表将为空): 请将 adb.exe 所在目录加入 PATH,或手动指定"),
            }
            self.refresh_devices();
        }
    }
}

impl eframe::App for PadApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;

        // 每帧联动:路径文本变化时自动补齐 scrcpy/server/adb 并刷新设备
        self.sync_suite();

        // ---- 全局快捷键: 撤销/重做/保存/另存为/刷新设备 ----
        // 键位捕获或取点进行中、正在输入文本时不拦截,保证 Ctrl+Z 等可作为待绑定键
        if self.waiting_key.is_none() && self.picking.is_none() && !ctx.egui_wants_keyboard_input() {
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
                self.dialog = Some(crate::filedialog::save_file("scrcpy-pad-profile.json"));
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
        if let Some(i) = self.resizing {
            let (zoom_in, zoom_out) = ctx.input(|input| {
                let ctrl = input.modifiers.ctrl;
                (
                    ctrl
                        && (input.key_pressed(egui::Key::Plus)
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
                if let Some(b) = g.profile.binds.get_mut(i) {
                    match &mut b.action {
                        Action::Tap { radius, .. } | Action::Hold { radius, .. } => {
                            // 乘性缩放(每步按固定比例),符合自然的缩放手感;
                            // 除浮点精度外不设上下限,仅防止缩到 0
                            let mut r = (*radius * factor).max(0.01);
                            // 精度重置:缩小损失精度后,放大回默认附近则复位默认
                            if (r - crate::keymap::DEFAULT_RADIUS).abs() < 0.5 {
                                r = crate::keymap::DEFAULT_RADIUS;
                            }
                            *radius = r;
                        }
                        _ => {}
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
                        self.server = Some(server);
                        self.shared.lock().unwrap().control = Some(client);
                        self.log("控制通道已连接");
                    }
                    Err(e) => self.log(format!("连接失败: {e}")),
                }
            }
        }
        if let Some(rx) = &self.shot_rx {
            if let Ok(r) = rx.try_recv() {
                self.shot_rx = None;
                match r {
                    Ok(img) => {
                        let (w, h) = (img.width() as u32, img.height() as u32);
                        let tex = ctx.load_texture("screenshot", img, Default::default());
                        self.shot = Some((tex, w, h));
                        self.log(format!("截图成功 {w}x{h},点击图像可取点"));
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
                        self.log(format!("已选择 scrcpy: {}", self.scrcpy_path));
                        self.resync();
                        self.test_scrcpy();
                    }
                    (DialogPurpose::ServerJar, Some(p)) => {
                        self.server_path = p.display().to_string();
                        self.log(format!("已选择 server: {}", self.server_path));
                        self.resync();
                        self.test_scrcpy();
                    }
                    (DialogPurpose::AdbExe, Some(p)) => {
                        self.adb_path = p.display().to_string();
                        self.log(format!("已选择 adb: {}", self.adb_path));
                        self.resync();
                    }
                    (DialogPurpose::SaveLog, Some(p)) => {
                        let content = self.pending_log.take().unwrap_or_default();
                        match std::fs::write(&p, content) {
                            Ok(_) => self.log(format!("日志已保存到 {}", p.display())),
                            Err(e) => self.log(format!("日志保存失败: {e}")),
                        }
                    }
                    (DialogPurpose::SaveProfileAs, Some(p)) => {
                        let json = {
                            let g = self.shared.lock().unwrap();
                            serde_json::to_string_pretty(&g.profile)
                        };
                        match json {
                            Ok(json) => match std::fs::write(&p, json) {
                                Ok(_) => self.log(format!(
                                    "配置已另存到 {}(可直接分享该文件)",
                                    p.display()
                                )),
                                Err(e) => self.log(format!("另存失败: {e}")),
                            },
                            Err(e) => self.log(format!("序列化失败: {e}")),
                        }
                    }
                    (DialogPurpose::ChooseProfile, Some(p)) => {
                        match read_profile_at(&p) {
                            Ok(prof) => {
                                self.profile_path = p.clone();
                                self.apply_profile_switch(prof);
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
                            self.apply_profile_switch(Profile::default());
                            self.log(format!("已新建空配置并切换: {}", p.display()));
                        }
                        Err(e) => self.log(format!("新建失败: {e}")),
                    },
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

        // ---- 键盘事件(绑定捕获用) ----
        while let Ok(ev) = self.gui_rx.try_recv() {
            if let Some(slot) = self.waiting_key.take() {
                self.assign_key(slot, ev.code);
            }
        }

        // ---- 同步映射开关到 grab ----
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

        // 控制通道意外断开检测
        if self.server.is_some() && !connected && self.connect_rx.is_none() {
            self.server = None;
            self.shared.lock().unwrap().control = None;
            self.log("控制通道已断开");
        }

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
                    self.resync();
                    self.refresh_devices();
                }

                ui.separator();
                ui.label("scrcpy参数:");
                if ui.small_button("...").on_hover_text("打开常用参数助手").clicked() {
                    self.args_helper = Some(ArgHelp::defaults());
                }
                ui.add(egui::TextEdit::singleline(&mut self.scrcpy_args).desired_width(160.0));
                // 启动预设:分辨率预设先重置为默认再叠加;音频类直接在现有参数上增删
                egui::ComboBox::from_id_salt("startpreset")
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
                if ui.button("启动 scrcpy").clicked() {
                    // 启动前联动一次:确保 server/adb 路径已就绪(如已手动粘贴 scrcpy 路径)
                    self.resync();
                    match adb::launch_scrcpy(&self.scrcpy_path, &self.serial(), &self.scrcpy_args)
                    {
                        Ok(_) => self.log("scrcpy 已启动"),
                        Err(e) => self.log(format!("启动失败: {e:#}")),
                    }
                }

                ui.separator();
                if connected {
                    ui.colored_label(egui::Color32::GREEN, "● 控制已连接");
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
                let color = if enabled {
                    egui::Color32::LIGHT_GREEN
                } else {
                    egui::Color32::GRAY
                };
                if ui
                    .add(egui::Button::new(txt).fill(color.gamma_multiply(0.3)))
                    .clicked()
                {
                    let mut g = self.shared.lock().unwrap();
                    g.enabled = !g.enabled;
                }

                ui.separator();
                if ui.button("关于").clicked() {
                    self.about_open = true;
                }
                });
            });
        });

        // ================= 左栏 =================
        egui::Panel::left("left").min_size(250.0).show(ui, |ui| {
            egui::CollapsingHeader::new("使用说明")
                .default_open(false)
                .show(ui, |ui| {
                    ui.label(
                        "1. 手机开 USB 调试并连接\n\
                         2. 顶栏选设备 → [连接控制]\n\
                         3. [启动 scrcpy] 出画面\n\
                         4. 右侧添加键位/轮盘,截图取点\n\
                         5. 按总开关键(默认F8)开映射\n\
                         \n\
                         临时轮盘:设置[启用键]后,\n\
                         该轮盘仅在启用期间生效,\n\
                         期间方向键的其它绑定自动让位\n\
                         \n\
                         动作:点按/长按/滑动/系统键\n\
                         点按时长=0 表示按住不松手,\n\
                         再按一次同一键才抬起\n\
                         长短按可用[转长按]按钮互切\n\
                         \n\
                         响应范围:点[修改响应范围]后\n\
                         该圆圈变黄,Ctrl++ / Ctrl+- 缩放,\n\
                         也可直接拖动圆圈;点[完成]退出\n\
                         \n\
                         滑动:取起点/取终点分别设置,\n\
                         可选曲线(加速/减速/钟形/贝塞尔)\n\
                         与轨迹(条形/方形/圆形);\n\
                         曲线参数在[设置...]中调整并预览\n\
                         \n\
                         快捷键(键位捕获/取点/打字时不生效):\n\
                         Ctrl+Z 撤销 ⟳重做用 Ctrl+Y\n\
                         Ctrl+S 保存配置\n\
                         Ctrl+Shift+S 另存为\n\
                         F5 刷新设备",
                    );
                });
            ui.separator();
            if let Some(err) = &self.capture_err {
                ui.colored_label(egui::Color32::RED, "输入捕获不可用:");
                ui.label(err);
                ui.separator();
            }

            ui.heading("配置");
            ui.horizontal(|ui| {
                if ui.button("保存配置").clicked() {
                    self.save_profile();
                }
                if ui.button("另存为...").clicked() {
                    self.dialog = Some(crate::filedialog::save_file("scrcpy-pad-profile.json"));
                    self.dialog_purpose = DialogPurpose::SaveProfileAs;
                }
                if ui.button("重新加载").clicked() {
                    self.reload_profile_from_current();
                }
            });
            // 键位文件重定向:指定任意目录/文件名为当前键位(可无文件则新建)
            ui.horizontal(|ui| {
                if ui.button("选用配置...").clicked() {
                    self.dialog = Some(crate::filedialog::pick_file());
                    self.dialog_purpose = DialogPurpose::ChooseProfile;
                }
                if ui.button("新建配置...").clicked() {
                    self.dialog = Some(crate::filedialog::save_file("scrcpy-pad-profile.json"));
                    self.dialog_purpose = DialogPurpose::NewProfile;
                }
                if ui.button("检测当前 json").clicked() {
                    self.check_current_profile();
                }
            });
            ui.small(format!(
                "当前配置文件: {}{}",
                self.profile_path.display(),
                if self.profile_path == profile_path() {
                    " (默认)"
                } else {
                    ""
                }
            ));
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

            ui.separator();
            ui.heading("scrcpy 管理");
            ui.small(
                "官方 Windows 包里 scrcpy.exe、scrcpy-server、adb.exe 三者同目录:\n填好任意一个,其余留空会自动补齐。",
            );
            ui.horizontal(|ui| {
                if ui.button("自动寻找全部").clicked() {
                    match adb::find_scrcpy() {
                        Some(p) => {
                            self.scrcpy_path = p.display().to_string();
                            self.log(format!("已找到 scrcpy: {}", self.scrcpy_path));
                            self.resync();
                            self.test_scrcpy();
                        }
                        None => self.log("未找到 scrcpy,请手动指定其所在目录"),
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
            if let Some((ok, msg)) = &self.test_msg {
                ui.colored_label(
                    if *ok { egui::Color32::GREEN } else { egui::Color32::RED },
                    msg,
                );
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

            ui.separator();
            ui.heading("日志");
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for l in self.logs.iter().rev().take(30) {
                        ui.monospace(l);
                    }
                });
        });

        // ================= 中央区 =================
        egui::CentralPanel::default().show(ui, |ui| {
            // 横纵双向滚动:键位条目较长时可用滚轮/横向滚动查看,不再被窗口裁掉
            egui::ScrollArea::both().show(ui, |ui| {
                self.ui_binds(ui);
                ui.separator();
                self.ui_wheels(ui);
                ui.separator();
                self.ui_picker(ui);
            });
        });

        // ================= 关于窗口 =================
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
                    ui.label("MIT License © 2026 Azrl");
                });
        }

        // ================= 参数助手窗口 =================
        self.ui_args_helper(ctx);

        // ================= 滑动曲线参数编辑窗口 =================
        self.ui_easing_editor(ctx);

        ctx.request_repaint_after(Duration::from_millis(120));
    }
}

impl PadApp {
    fn ui_binds(&mut self, ui: &mut egui::Ui) {
        ui.heading("按键映射");
        let mut to_delete: Option<usize> = None;
        let bind_count = self.shared.lock().unwrap().profile.binds.len();

        for i in 0..bind_count {
            ui.horizontal(|ui| {
                let (key, kind, is_swipe, is_point) = {
                    let g = self.shared.lock().unwrap();
                    let b = &g.profile.binds[i];
                    (
                        b.key,
                        b.action.kind_name(),
                        matches!(b.action, Action::Swipe(_)),
                        matches!(b.action, Action::Tap { .. } | Action::Hold { .. }),
                    )
                };
                ui.label(format!("[{kind}]"));
                let waiting = self.waiting_key == Some(KeySlot::Bind(i));
                if Self::key_button(ui, waiting, Some(key)).clicked() {
                    self.waiting_key = Some(KeySlot::Bind(i));
                }

                if is_swipe {
                    // 滑动:控件已展示起终点/时长/曲线/轨迹,不再重复 desc
                    let mut pick = self.picking;
                    let mut easing_edit = self.easing_edit;
                    let mut g = self.shared.lock().unwrap();
                    if let Some(b) = g.profile.binds.get_mut(i) {
                        if let Action::Swipe(s) = &mut b.action {
                            swipe_controls(
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
                    drop(g);
                    self.picking = pick;
                    self.easing_edit = easing_edit;
                } else if is_point {
                    // 点按/长按:坐标、时长、响应范围、取点、长短按切换
                    let (mut do_resize, mut do_convert, mut do_pick) = (false, false, false);
                    let mut g = self.shared.lock().unwrap();
                    if let Some(b) = g.profile.binds.get_mut(i) {
                        match &mut b.action {
                            Action::Tap {
                                x,
                                y,
                                duration_ms,
                                radius,
                            } => {
                                ui.label("x:");
                                ui.add(egui::DragValue::new(x).range(0..=8192));
                                ui.label("y:");
                                ui.add(egui::DragValue::new(y).range(0..=8192));
                                ui.label("时长ms:");
                                ui.add(egui::DragValue::new(duration_ms).range(0..=5000))
                                    .on_hover_text("0=按下不松手,直到再按一次");
                                ui.label("范围:");
                                ui.add(egui::DragValue::new(radius).range(0.01..=100000.0));
                            }
                            Action::Hold { x, y, radius } => {
                                ui.label("x:");
                                ui.add(egui::DragValue::new(x).range(0..=8192));
                                ui.label("y:");
                                ui.add(egui::DragValue::new(y).range(0..=8192));
                                ui.label("范围:");
                                ui.add(egui::DragValue::new(radius).range(0.01..=100000.0));
                            }
                            _ => {}
                        }
                    }
                    drop(g);
                    // 取点
                    let waiting_p = self.picking == Some(CoordSlot::Bind(i));
                    if ui
                        .button(if waiting_p { "点击截图..." } else { "取点" })
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
                        self.picking = Some(CoordSlot::Bind(i));
                    }
                    if do_convert {
                        self.push_undo();
                        let mut g = self.shared.lock().unwrap();
                        if let Some(b) = g.profile.binds.get_mut(i) {
                            b.action = match &b.action {
                                Action::Tap {
                                    x,
                                    y,
                                    radius,
                                    ..
                                } => Action::Hold {
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
                        self.push_undo();
                        self.resizing = Some(i);
                        self.log("响应范围修改中: 用 Ctrl++ / Ctrl+- 或拖动圆圈调整");
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
        }
        if let Some(i) = to_delete {
            self.push_undo();
            self.shared.lock().unwrap().profile.binds.remove(i);
            self.log("已删除绑定");
        }

        // ---- 新增绑定 ----
        ui.separator();
        ui.label("新增:");
        ui.horizontal(|ui| {
            let waiting = self.waiting_key == Some(KeySlot::NewBind);
            if Self::key_button(ui, waiting, self.draft.key).clicked() {
                self.waiting_key = Some(KeySlot::NewBind);
                self.draft_active = true;
            }
            let kind_before = self.draft.kind;
            egui::ComboBox::from_id_salt("newkind")
                .selected_text(["点按", "长按", "滑动", "系统键"][self.draft.kind])
                .show_ui(ui, |ui| {
                    for (i, n) in ["点按", "长按", "滑动", "系统键"].iter().enumerate() {
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
                    ui.add(egui::DragValue::new(&mut self.draft.x).range(0..=8192));
                    ui.label("y:");
                    ui.add(egui::DragValue::new(&mut self.draft.y).range(0..=8192));
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
                        .button(if waiting_p { "点击截图..." } else { "取点" })
                        .clicked()
                    {
                        self.picking = Some(CoordSlot::NewBind);
                        self.draft_active = true;
                    }
                }
                2 => {
                    let mut pick = self.picking;
                    let pick_before = pick;
                    let mut easing_edit = self.easing_edit;
                    let mut tmp = Swipe {
                        start: self.draft.swipe_start,
                        end: self.draft.swipe_end,
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
                    self.draft.swipe_start = tmp.start;
                    self.draft.swipe_end = tmp.end;
                    self.draft.swipe_duration_ms = tmp.duration_ms;
                    self.draft.swipe_easing = tmp.easing;
                    self.draft.swipe_path = tmp.path;
                    self.picking = pick;
                    self.easing_edit = easing_edit;
                    if pick != pick_before {
                        // 点取起点/终点/出发点即视为开始配置,显示轨迹预览
                        self.draft_active = true;
                    }
                }
                _ => {
                    ui.label("keycode(返回=4 主页=3):");
                    ui.add(egui::DragValue::new(&mut self.draft.keycode).range(0..=999));
                }
            }
            if ui.button("添加").clicked() {
                if let Some(key) = self.draft.key {
                    self.push_undo();
                    let action = match self.draft.kind {
                        0 => Action::Tap {
                            x: self.draft.x,
                            y: self.draft.y,
                            duration_ms: self.draft.tap_duration_ms,
                            radius: self.draft.radius,
                        },
                        1 => Action::Hold {
                            x: self.draft.x,
                            y: self.draft.y,
                            radius: self.draft.radius,
                        },
                        2 => Action::Swipe(Swipe {
                            start: self.draft.swipe_start,
                            end: self.draft.swipe_end,
                            duration_ms: self.draft.swipe_duration_ms,
                            easing: self.draft.swipe_easing,
                            path: self.draft.swipe_path,
                        }),
                        _ => Action::AndroidKey {
                            keycode: self.draft.keycode,
                        },
                    };
                    let is_point = matches!(action, Action::Tap { .. } | Action::Hold { .. });
                    let new_idx = {
                        let mut g = self.shared.lock().unwrap();
                        let new_idx = g.profile.binds.len();
                        g.profile.binds.push(KeyBind { key, action });
                        new_idx
                    };
                    self.draft.key = None;
                    // 已添加到正式键位列表,草稿预览结束
                    self.draft_active = false;
                    self.log("已添加绑定");
                    if is_point {
                        if self.shot.is_some() {
                            self.picking = Some(CoordSlot::Bind(new_idx));
                            self.log("请在截图上点击目标位置");
                        } else {
                            self.log("提示: 尚无截图,先[截取手机屏幕]后可自动取点");
                        }
                    }
                } else {
                    self.log("请先捕获按键");
                }
            }
        });
    }

    fn ui_wheels(&mut self, ui: &mut egui::Ui) {
        ui.heading("轮盘(虚拟摇杆)");
        ui.label("设置[启用键]后变为临时轮盘:仅在启用期间生效,期间方向键的其它绑定让位");
        let mut to_delete: Option<usize> = None;
        let wheel_count = self.shared.lock().unwrap().profile.wheels.len();
        let dir_names = ["上", "下", "左", "右"];

        for i in 0..wheel_count {
            ui.horizontal(|ui| {
                let temp_info = {
                    let g = self.shared.lock().unwrap();
                    let w = &g.profile.wheels[i];
                    (
                        w.temp.as_ref().map(|t| (t.key, t.mode)),
                        format!(
                            "轮盘{}{}",
                            i + 1,
                            if w.temp.is_some() { "(临时)" } else { "" }
                        ),
                    )
                };
                let (temp, title) = temp_info;
                ui.label(title);

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
            ui.horizontal(|ui| {
                for d in 0..4 {
                    ui.label(dir_names[d]);
                    let code = {
                        let g = self.shared.lock().unwrap();
                        let w = &g.profile.wheels[i];
                        [w.up, w.down, w.left, w.right][d]
                    };
                    let waiting = self.waiting_key == Some(KeySlot::WheelDir { wheel: i, dir: d });
                    if Self::key_button(ui, waiting, Some(code)).clicked() {
                        self.waiting_key = Some(KeySlot::WheelDir { wheel: i, dir: d });
                    }
                }
                let waiting_p = self.picking == Some(CoordSlot::WheelCenter(i));
                {
                    let mut g = self.shared.lock().unwrap();
                    let w = &mut g.profile.wheels[i];
                    ui.label("圆心x:");
                    ui.add(egui::DragValue::new(&mut w.cx).range(0..=8192));
                    ui.label("y:");
                    ui.add(egui::DragValue::new(&mut w.cy).range(0..=8192));
                    ui.label("半径:");
                    ui.add(egui::DragValue::new(&mut w.radius).range(10..=1000));
                }
                if ui
                    .button(if waiting_p { "点击截图..." } else { "取圆心" })
                    .clicked()
                {
                    self.picking = Some(CoordSlot::WheelCenter(i));
                }
                if ui.button("删除").clicked() {
                    to_delete = Some(i);
                }
            });
        }
        if let Some(i) = to_delete {
            self.push_undo();
            self.shared.lock().unwrap().profile.wheels.remove(i);
            self.log("已删除轮盘");
        }
        if ui.button("添加轮盘").clicked() {
            self.push_undo();
            self.shared.lock().unwrap().profile.wheels.push(Wheel {
                up: 17,
                down: 31,
                left: 30,
                right: 32,
                cx: 300,
                cy: 900,
                radius: 120,
                temp: None,
            });
        }
    }

    /// 在手机截图上叠加显示所有键位/轮盘的位置示意
    fn draw_overlay(&self, ui: &egui::Ui, rect: egui::Rect, scale: f32) {
        use egui::{Align2, Color32, FontId, Stroke, vec2};
        let painter = ui.painter();
        let to_screen =
            |x: i32, y: i32| rect.min + vec2(x as f32 * scale, y as f32 * scale);
        let short_name = |code: u16| key_name(code).replace("KEY_", "");

        let g = self.shared.lock().unwrap();

        // 键位(含草稿标记)仅在"全部/仅键位"时显示
        if matches!(self.overlay_filter, OverlayFilter::All | OverlayFilter::Keys) {
            for (i, b) in g.profile.binds.iter().enumerate() {
                match &b.action {
                    Action::Tap {
                        x,
                        y,
                        radius,
                        ..
                    }
                    | Action::Hold { x, y, radius } => {
                        let p = to_screen(*x, *y);
                        let r = *radius * scale;
                        // 修改响应范围中的键位显示黄色;否则点按绿、长按橙
                        let (ring, fill) = if self.resizing == Some(i) {
                            (Color32::YELLOW, Color32::from_rgba_unmultiplied(255, 230, 0, 70))
                        } else if matches!(b.action, Action::Tap { .. }) {
                            (Color32::GREEN, Color32::from_rgba_unmultiplied(0, 200, 0, 60))
                        } else {
                            (Color32::ORANGE, Color32::from_rgba_unmultiplied(255, 165, 0, 60))
                        };
                        painter.circle_filled(p, r, fill);
                        painter.circle_stroke(p, r, Stroke::new(2.0, ring));
                        painter.text(
                            p,
                            Align2::CENTER_CENTER,
                            short_name(b.key),
                            FontId::proportional(12.0),
                            Color32::WHITE,
                        );
                    }
                    Action::Swipe(s) => {
                        draw_swipe_track(painter, s, &to_screen, scale, &short_name(b.key));
                    }
                    Action::AndroidKey { .. } => {}
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
                    let ds = Swipe {
                        start: self.draft.swipe_start,
                        end: self.draft.swipe_end,
                        duration_ms: self.draft.swipe_duration_ms,
                        easing: self.draft.swipe_easing,
                        path: self.draft.swipe_path,
                    };
                    draw_swipe_track(painter, &ds, &to_screen, scale, "新增");
                }
                1 => {
                    let dp = to_screen(self.draft.x, self.draft.y);
                    let r = self.draft.radius * scale;
                    painter.circle_stroke(dp, r, Stroke::new(2.0, Color32::YELLOW));
                    painter.text(
                        dp + vec2(0.0, r + 10.0),
                        Align2::CENTER_CENTER,
                        "新增",
                        FontId::proportional(11.0),
                        Color32::YELLOW,
                    );
                }
                _ => {}
            }
        }

        // 轮盘按过滤条件显示;临时轮盘用虚线圆环区分
        if !matches!(self.overlay_filter, OverlayFilter::Keys) {
            for w in &g.profile.wheels {
                let show = match self.overlay_filter {
                    OverlayFilter::PermWheels => w.temp.is_none(),
                    OverlayFilter::TempWheels => w.temp.is_some(),
                    _ => true,
                };
                if !show {
                    continue;
                }
                let c = to_screen(w.cx, w.cy);
                let r = w.radius as f32 * scale;
                let dirs_label = format!(
                    "{}/{}/{}/{}",
                    short_name(w.up),
                    short_name(w.left),
                    short_name(w.down),
                    short_name(w.right)
                );
                if let Some(t) = &w.temp {
                    // 临时轮盘:品红虚线圆环
                    let color = Color32::from_rgb(255, 90, 220);
                    let n = 48;
                    let pts: Vec<egui::Pos2> = (0..=n)
                        .map(|i| {
                            let a = i as f32 * std::f32::consts::TAU / n as f32;
                            c + vec2(a.cos() * r, a.sin() * r)
                        })
                        .collect();
                    for shape in
                        egui::Shape::dashed_line(&pts, Stroke::new(2.0, color), 6.0, 5.0)
                    {
                        painter.add(shape);
                    }
                    painter.circle_filled(c, 5.0, color);
                    painter.circle_stroke(c, 18.0, Stroke::new(1.0, Color32::WHITE));
                    let mode = match t.mode {
                        TempMode::Hold => "按住",
                        TempMode::Toggle => "切换",
                    };
                    painter.text(
                        c - vec2(0.0, r + 14.0),
                        Align2::CENTER_CENTER,
                        format!("临时摇杆[{}·{}] {dirs_label}", short_name(t.key), mode),
                        FontId::proportional(12.0),
                        Color32::from_rgb(255, 130, 235),
                    );
                } else {
                    // 永久轮盘:青色实线圆环
                    let color = Color32::from_rgb(0, 200, 255);
                    painter.circle_stroke(c, r, Stroke::new(2.0, color));
                    painter.circle_filled(c, 5.0, color);
                    painter.circle_stroke(c, 18.0, Stroke::new(1.0, Color32::WHITE));
                    painter.text(
                        c - vec2(0.0, r + 14.0),
                        Align2::CENTER_CENTER,
                        format!("摇杆 {dirs_label}"),
                        FontId::proportional(12.0),
                        Color32::from_rgb(0, 220, 255),
                    );
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
            EasingEditTarget::New => self.draft.swipe_easing,
        };

        let mut open = true;
        let mut modified = false;
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
                        if ui
                            .add(egui::DragValue::new(power).range(0.1..=10.0))
                            .changed()
                        {
                            modified = true;
                        }
                    }
                    Easing::Bezier { x1, y1, x2, y2 } => {
                        ui.label("x1:");
                        if ui.add(egui::DragValue::new(x1).range(0.0..=1.0)).changed() {
                            modified = true;
                        }
                        ui.label("y1:");
                        if ui.add(egui::DragValue::new(y1).range(-2.0..=2.0)).changed() {
                            modified = true;
                        }
                        ui.label("x2:");
                        if ui.add(egui::DragValue::new(x2).range(0.0..=1.0)).changed() {
                            modified = true;
                        }
                        ui.label("y2:");
                        if ui.add(egui::DragValue::new(y2).range(-2.0..=2.0)).changed() {
                            modified = true;
                        }
                    }
                }
            });

        if !open {
            self.easing_edit = None;
            return;
        }
        if modified {
            match target {
                EasingEditTarget::Bind(i) => {
                    let mut g = self.shared.lock().unwrap();
                    if let Some(b) = g.profile.binds.get_mut(i) {
                        if let Action::Swipe(s) = &mut b.action {
                            s.easing = easing;
                        }
                    }
                }
                EasingEditTarget::New => self.draft.swipe_easing = easing,
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

    fn ui_picker(&mut self, ui: &mut egui::Ui) {
        ui.heading("截图取点");
        ui.horizontal(|ui| {
            let taking = self.shot_rx.is_some();
            if ui
                .button(if taking { "截图中..." } else { "截取手机屏幕" })
                .clicked()
                && !taking
            {
                self.take_screenshot();
            }
            // 浮层显示过滤
            ui.label("显示:");
            let sel = self.overlay_filter;
            egui::ComboBox::from_id_salt("overlayfilter")
                .selected_text(sel.label())
                .show_ui(ui, |ui| {
                    for f in [
                        OverlayFilter::All,
                        OverlayFilter::Keys,
                        OverlayFilter::Wheels,
                        OverlayFilter::PermWheels,
                        OverlayFilter::TempWheels,
                    ] {
                        ui.selectable_value(&mut self.overlay_filter, f, f.label());
                    }
                });
            if self.picking.is_some() {
                let draft_pick = self.picking.map(is_draft_slot).unwrap_or(false);
                ui.colored_label(
                    egui::Color32::YELLOW,
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
                    ui.colored_label(egui::Color32::YELLOW, "已取点: 请按下要绑定的按键");
                } else {
                    ui.label("新增未完成(尚未[添加])");
                }
                if ui.button("取消取点").clicked() {
                    self.cancel_draft();
                    self.log("已取消新增");
                }
            } else if self.resizing.is_some() {
                ui.colored_label(
                    egui::Color32::YELLOW,
                    "响应范围修改中: Ctrl++ / Ctrl+- 缩放,或拖动圆圈",
                );
                if ui.button("完成").clicked() {
                    self.resizing = None;
                }
            } else {
                ui.label("先点某条映射的[取点],再点击截图上的位置");
            }
        });

        let shot = self.shot.as_ref().map(|(t, w, h)| (t.id(), *w, *h));
        if let Some((tex_id, w, h)) = shot {
            let avail = ui.available_width();
            let scale = (avail / w as f32).min(1.0).min(500.0 / h as f32);
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

            // 拖动聚焦圆圈缩放响应范围
            if let Some(i) = self.resizing {
                if resp.dragged() {
                    if let Some(pos) = resp.interact_pointer_pos() {
                        let (cx, cy) = {
                            let g = self.shared.lock().unwrap();
                            match g.profile.binds.get(i).map(|b| &b.action) {
                                Some(Action::Tap { x, y, .. })
                                | Some(Action::Hold { x, y, .. }) => (*x, *y),
                                _ => (0, 0),
                            }
                        };
                        let dx = pos.x - (rect.min.x + cx as f32 * scale);
                        let dy = pos.y - (rect.min.y + cy as f32 * scale);
                        let new_r = ((dx * dx + dy * dy).sqrt() / scale).max(0.01);
                        let mut g = self.shared.lock().unwrap();
                        if let Some(b) = g.profile.binds.get_mut(i) {
                            match &mut b.action {
                                Action::Tap { radius, .. } | Action::Hold { radius, .. } => {
                                    *radius = new_r;
                                }
                                _ => {}
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
                    } else {
                        self.log(format!("截图坐标: ({px}, {py})"));
                    }
                }
            }
        }
    }
}

fn profile_path() -> PathBuf {
    directories::ProjectDirs::from("dev", "", "scrcpy-pad")
        .map(|p| p.config_dir().join("profile.json"))
        .unwrap_or_else(|| PathBuf::from("profile.json"))
}

fn load_profile() -> Option<Profile> {
    let path = profile_path();
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// 读取并严格校验任意路径下的键位 json;返回详细中文错误便于排查
fn read_profile_at(path: &std::path::Path) -> Result<Profile, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("读取失败: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("不是合法的键位 json: {e}"))
}

/// 向指定路径写入全新默认配置(父目录不存在则自动创建)
fn write_default_profile(path: &std::path::Path) -> Result<(), String> {
    let json = serde_json::to_string_pretty(&Profile::default())
        .map_err(|e| format!("序列化失败: {e}"))?;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(path, json).map_err(|e| format!("写入失败: {e}"))
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
fn circle_geometry(path: &SwipePath, start: (i32, i32), end: (i32, i32)) -> Option<(f32, f32, f32)> {
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

/// 渲染滑动键的编辑控件:取起点/取终点、时长、曲线、轨迹、圆形专用按钮、曲线设置。
/// 返回是否有任何修改(供调用方决定是否记录撤销)。
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
        .button(if w_start { "点击取起点..." } else { "取起点" })
        .clicked()
    {
        *pick = Some(slot_start);
    }
    let w_end = *pick == Some(slot_end);
    if ui
        .button(if w_end { "点击取终点..." } else { "取终点" })
        .clicked()
    {
        *pick = Some(slot_end);
    }

    ui.label("时长ms:");
    if ui
        .add(egui::DragValue::new(&mut s.duration_ms).range(10..=5000))
        .changed()
    {
        changed = true;
    }

    // 曲线选择
    ui.label("曲线:");
    let cur_ease = s.easing;
    egui::ComboBox::from_id_salt(("swipe_easing", target.id()))
        .selected_text(cur_ease.label())
        .show_ui(ui, |ui| {
            if ui.selectable_label(cur_ease == Easing::Linear, "默认(匀速)").clicked() {
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
            if ui.selectable_label(matches!(cur_path, SwipePath::Line), "默认(条形)").clicked() {
                s.path = SwipePath::Line;
                changed = true;
            }
            if ui.selectable_label(matches!(cur_path, SwipePath::Rect), "方形").clicked() {
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
            .button(if *as_diameter { "视为直径" } else { "视为圆心" })
            .clicked()
        {
            *as_diameter = !*as_diameter;
            changed = true;
        }
        let w_angle = *pick == Some(slot_angle);
        if ui
            .button(if w_angle { "点击设出发点..." } else { "设置出发点" })
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

/// 在截图上绘制滑动轨迹:边界实色、内部半透明,尽量不遮挡截图内容。
fn draw_swipe_track<F: Fn(i32, i32) -> egui::Pos2>(
    painter: &egui::Painter,
    s: &Swipe,
    to_screen: &F,
    scale: f32,
    label: &str,
) {
    use egui::{Align2, Color32, FontId, Stroke};
    let pts: Vec<egui::Pos2> = crate::keymap::swipe_points(s.path, s.start, s.end, 64)
        .iter()
        .map(|&(x, y)| to_screen(x, y))
        .collect();
    if pts.len() < 2 {
        return;
    }
    let edge = Stroke::new(2.0, Color32::LIGHT_BLUE);
    let fill = Color32::from_rgba_unmultiplied(100, 180, 255, 40);
    match s.path {
        SwipePath::Line => {
            painter.add(egui::Shape::line(pts.clone(), Stroke::new(10.0, fill)));
            painter.add(egui::Shape::line(pts.clone(), edge));
        }
        SwipePath::Rect => {
            let a = to_screen(s.start.0, s.start.1);
            let b = to_screen(s.end.0, s.end.1);
            let rect = egui::Rect::from_two_pos(a, b);
            painter.rect_filled(rect, 0.0, fill);
            painter.rect_stroke(rect, 0.0, edge, egui::StrokeKind::Inside);
        }
        SwipePath::Circle { .. } => {
            if let Some((cx, cy, r)) = circle_geometry(&s.path, s.start, s.end) {
                let c = to_screen(cx as i32, cy as i32);
                let r_screen = r * scale;
                painter.circle_filled(c, r_screen, fill);
                painter.circle_stroke(c, r_screen, edge);
            }
        }
    }
    let p0 = pts[0];
    painter.circle_stroke(p0, 12.0, edge);
    painter.text(
        p0,
        Align2::CENTER_CENTER,
        label,
        FontId::proportional(11.0),
        Color32::WHITE,
    );
}

/// 绘制速率函数(缓动曲线)预览图
fn draw_easing_preview(ui: &mut egui::Ui, e: Easing) {
    let size = egui::vec2(220.0, 100.0);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter();
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
    painter.add(egui::Shape::line(
        pts,
        egui::Stroke::new(2.0, egui::Color32::LIGHT_BLUE),
    ));
}
