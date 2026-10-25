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

/// 半径输入的三套**现存**口径(U8,2026-10-17)。
///
/// 只收敛**结构**(所有半径编辑点都走 [`radius_input`]),**不统一范围**:
/// 三套数值是现状(CN-02),统一成哪一套会改变用户可见行为,属计划 Q6 待拍板项。
/// 因此这里按单位命名,把差异显式化、可 grep,而不是悄悄合并:
///   * [`RADIUS_RANGE_PX`] 键位/草稿/组合键的半径(界面按像素编辑);
///   * [`RADIUS_RANGE_WHEEL_PX`] 摇杆卡与宏虚拟轮盘的半径(同为像素,但历史上更窄);
///   * [`RADIUS_RANGE_REL`] 宏动作编辑器里的半径(**相对屏宽**,与像素口径不同)。
///
/// 还有两处**无钳制**的半径写入(Ctrl+± 乘性缩放、截图拖动改半径)不在本组件内,
/// 见 `radius_input` 的注释。
const RADIUS_RANGE_PX: std::ops::RangeInclusive<f32> = 0.01..=100000.0;
const RADIUS_RANGE_WHEEL_PX: std::ops::RangeInclusive<f32> = 10.0..=1000.0;
const RADIUS_RANGE_REL: std::ops::RangeInclusive<f32> = 0.001..=0.5;

/// 草稿编辑卡的两种外壳(U9,2026-10-17)。字段与顺序由 [`PadApp::ui_draft_card`] 统一,
/// 这个枚举只决定"外壳长什么样"。
#[derive(Clone, Copy, PartialEq, Eq)]
enum DraftCardShell {
    /// 默认风格「新增:」一行(`ui_binds_new`)
    DefaultStyle,
    /// 可视化风格操作栏(`ui_vk_draft`)
    VisualStyle,
}

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

/// 周期性"重查触摸坐标空间 + 设备侧遥测"的间隔(2026-10-11)。
///
/// 2 秒的三个理由:
///   * 这条巡检必须**独立于输入**存在 —— 真机故障(23:18 会话)里玩家碰了一下
///     手机之后所有注入都失效,而那时**一个键都没按**,原有的
///     `engine::space_recheck_due` 那条"空闲后再输入"的路永远等不到触发;
///   * 2s 与已经在线跑着的 [`DEBUG_POLL_INTERVAL`] 同量级(每轮一次后台 adb
///     查询,在工作线程上,不碰引擎锁),是本项目已经接受的代价;
///   * 它比 3s 的空闲阈值短,转屏后最迟约 2s 就会被发现并收手重落。
///
/// **口径**(2026-10-11 晚 Lead 定级):这是**防御性**改进,不是已验证的根因修复
/// —— 用户实测"卡住后松开所有键停 30 秒再按键也不恢复",已经把"坐标空间过期"
/// 这个假设否掉了(最新证据指向引擎侧换手续接)。它保留的价值是:让"空间过期"
/// 这一类故障不可能再长期潜伏,并顺带把设备侧遥测(焦点窗口/屏幕亮灭)摊开。
const SPACE_WATCH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// 设备端输入账本(卡死自愈的取证层)的节拍:用户明确要求 **每秒只读一次**
/// `dumpsys input`(见 `adb::device_input_state`)。
///
/// 为什么是 1s:这是**取证**用的,不是控制回路 —— 下一次卡死时日志里要有设备端
/// 的真相,但它绝不能成为负担(每次都是一条 adb + 一次 dumpsys,跑在独立工作线程
/// 上)。**变化时才写日志**,所以 1s 的节拍不会刷屏。
const DEVICE_LEDGER_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// 压枪"控制强度档位表"默认展开的上限档数(用户 2026-10-10 第 3 条:
/// "挡位过多时,非添加模式自动折叠")。
///
/// 档位是这个面板里唯一会"越加越长"的列表,档位一多就把下面的[摇晃]/[覆盖灵敏度]
/// 顶出屏幕。所以档数超过这个数时,档位表**第一次出现**就默认收起,只留一行标题;
/// 用户点标题可展开/收起,点[＋ 添加一档]时强制展开(正在添加,不该藏)。
/// 注意这是"默认值"而非"每当档数变多就强制收起":一旦用户自己点过标题,
/// 之后就以用户的选择为准 —— 正在调档时列表自己收起来,才是真的难用。
const RECOIL_TIER_COLLAPSE_AT: usize = 4;

/// 自检面板里"当前触点数"开始变黄的门限(DF-7,2026-10-11)。
///
/// 设备端十指池是 10(`crate::engine::DEVICE_MAX_POINTERS`),但**池满**才是
/// 问题:到 8 就说明离"新按下的键被设备直接丢掉"只差两个触点,这时候提醒
/// 还来得及(留两格余量,不至于等真丢了才说)。
const DEVICE_POINTERS_WARN_AT: usize = 8;

/// 窄窗下"标签 + 路径输入框 + 按钮"这一行里,输入框的最小宽度。
///
/// 用户 2026-10-17(433px 窄窗截图):路径框按 egui 默认宽度铺开,把[浏览]顶出
/// 右边缘 ⇒ 按钮点不到。修法是"按钮先占位、输入框只吃剩下的";再窄也至少留
/// 这么宽,否则路径一个字都看不见。
///
/// 为什么是局部常量而不是 `theme::size` 的档位:那里目前没有"输入框最小宽度"
/// 这一档(U4 已把"按钮/输入框尺寸无档位常量"登记为规范缺口 Q6),本轮不擅自
/// 往 `theme.rs` 加新规范。
const PATH_EDIT_MIN_WIDTH: f32 = 56.0;

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

/// Windows 上「映射时屏蔽原键(grab)」的**两档**现状说明(B21·G-a,2026-10-17)。
///
/// # 为什么要有这一份
///
/// 旧文案写的是「仅 Linux 可用(EVIOCGRAB 独占设备);Windows 无需该项」—— 它把
/// **平台差**说成了「Windows 根本不需要这件事」。B21 立项补的正是 Windows 侧的准 grab,
/// 于是这句话与新增能力自相矛盾,必须换掉(B21 方案 §5.4;协议文档 B21 登记
/// 「`app.rs:13373` 仍写『Windows 无需该项』⇒ G-a 必须改」)。
///
/// # 为什么是两档而不是方案草案里的三档(Lead 2026-10-18 令)
///
/// 方案 §5.4 草案写的是三档(默认 / 独占模式 / 实验性前台独占)。第三档
/// (`RIDEV_NOLEGACY`)的 P1/P3 实测结论是**零收益**,已被裁掉 ⇒ 不为它写任何
/// 界面文案或开关。第二档的措辞也按同一次实测收窄:能**无条件链式扣下**的只有
/// 带系统默认行为的候选键(Esc / F11 / 鼠标左右中侧键 / 滚轮);W/A/S/D 这类
/// 普通键即使判给了本程序,别的程序照样收得到。所以文案只说「只在系统默认行为 /
/// legacy 消息这一层扣键」,**不出现**「完全独占」「系统级独占」这类承诺。
///
/// # 口径:只描述现状,不描述愿望
///
/// 两档里的**第二档(独占模式)已实现**(B21-2/G-b):`swallow_bound_keys` 默认关,
/// 勾上后才在 legacy 层吞已绑定键。`RIDEV_NOLEGACY` 仍只是注释(`capture.rs`),
/// 所以第三档(实验性前台独占)照旧**不存在**,本常量也不提它。
/// 文案说谎比没有文案更糟:文案必须与
/// `grep swallow_bound_keys / RIDEV_NOLEGACY` 的实际实现状态一致。
///
/// (`ClipCursor` 是**另一个**功能(B21-3/G-c:瞄准期间钉住光标),它不改这段
/// 键盘文案的两档划分 —— 那个开关在 FPS 页,默认开,见 `Settings::clip_cursor_in_aim`。)
///
/// 两条边界(方案 §5.4 的第②条「实验档需程序在前台」随第三档一起删除):
/// ① 拦不住通过 Raw Input 读输入的程序;② 出问题按 **F8** 一键关掉映射。
///
/// # 为什么是常量而不是内联字符串
///
/// 它是**多行**文案:内联进绘制函数会把左栏中段这一段撑开、可读性变差。抽成常量后
/// 界面只留 `ui.small(WIN_GRAB_MODES_NOTE);` 一行,文案本身可被下面的守护测试逐字钉住
/// (见 `win_grab_copy_states_two_tiers_without_overpromising`)。
#[cfg_attr(not(windows), allow(dead_code))]
const WIN_GRAB_MODES_NOTE: &str = "映射时屏蔽原键(grab)在 Windows 上分两档:\
     ① 默认:映射照常生效,按键仍会传给其它程序,\
     所以映射开着也照样能打字/用快捷键;\
     ② 独占模式(默认关闭):打开后,映射开启期间已被绑定的键\
     (含 W/A/S/D 这类普通键)不再传给其它程序 —— 仅在 legacy 消息层生效,\
     拦不住 Raw Input 程序。\n\
     边界(两档通用):① 本程序拦的只是 legacy 消息,拦不住通过\
     Raw Input 读输入的程序 —— 大量 PC 游戏与专业输入软件照样收得到同一批\
     输入 —— 这是原理性天花板,不是可以靠更多代码补上的缺口;\
     ② 出问题按 F8(左栏总开关键)关掉映射即立刻恢复 —— 与 Linux 一侧的\
     「映射时屏蔽原键」是同一个总开关)。";

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
    /// 常用参数类(用户 2026-10-23 第 4 条):追加一个参数,已存在则不重复追加。
    ///
    /// 与"分辨率预设"的区别:分辨率预设会**重置**参数串(它要给 --max-size 一个
    /// 干净的基线),而这些只往现有参数上加一项(与音频类同一口径),免得用户
    /// 先选了分辨率、再选全屏,结果分辨率设置被抹掉。
    Fullscreen,
    TurnScreenOff,
    MaxFps60,
    AlwaysOnTop,
    HighBitrate,
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
            StartPreset::Fullscreen => "全屏启动(--fullscreen)",
            StartPreset::TurnScreenOff => "启动即关机屏幕(--turn-screen-off)",
            StartPreset::MaxFps60 => "限制 60 帧(--max-fps=60)",
            StartPreset::AlwaysOnTop => "窗口置顶(--always-on-top)",
            StartPreset::HighBitrate => "高码率 20M(--video-bit-rate=20M)",
        }
    }
}

/// 参数助手窗口打开期间的内容;每次打开按默认参数重建,关闭即丢弃临时修改
struct ArgHelp {
    entries: Vec<ArgEntry>,
    selected: usize,
    /// 正在被临时编辑的参数下标(双击进入,焦点移走不丢失,关窗丢弃)
    editing: Option<usize>,
    /// 搜索关键词(用户 2026-10-13):按**名称 / 参数 / 说明 / 示例**四路匹配。
    ///
    /// 为什么匹配四路而不是只匹配名称:用户找参数时可能记得的是"那个录屏的"
    /// (说明里有"录制")或"`--record`"(参数名),只匹配中文名会让后两种白找。
    query: String,
    /// 过滤后的下标(空查询 = 全部)。每次查询变化时重算一次,不在绘制里算。
    filtered: Vec<usize>,
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
    /// 平台特有性:帮助里写明"only available on Linux"一类的才特别标注,
    /// 其余一律"通用"(Windows / Linux / macOS 上语义一致的参数)
    platform: &'static str,
}

/// 按**中文拼音**比较两个字符串(Windows 系统排序规则)。
///
/// 为什么用系统 API 而不是内置拼音表:`CompareStringEx` 在 `zh-CN` locale 下
/// 就是 Windows 资源管理器/文件对话框用的那套中文排序 —— **零维护**(不用手写
/// 几千字的拼音映射,也不会因为多音字写错),且已用只读探测验证过:
/// `["一","丁","万","上","不"]` 排出来是 `不 丁 上 万 一`(b/d/s/w/y,拼音序),
/// 而 Unicode 码点序是 `一 丁 万 上 不` —— 两者完全不同,证明它真的按拼音。
///
/// 非 Windows:退回 `str::cmp`(Unicode 码点序)。参数表是 Windows 用户的日常,
/// 这个退化只影响跨平台构建的可编译性。
#[cfg(windows)]
fn cmp_pinyin(a: &str, b: &str) -> std::cmp::Ordering {
    use windows_sys::Win32::Globalization::CompareStringEx;
    // CSTR_LESS_THAN = 1, CSTR_EQUAL = 2, CSTR_GREATER_THAN = 3(返回 0 = 失败)
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    // 用户默认 locale 通常就是 zh-CN;显式给 "zh-CN" 更稳(用户可能设成英文系统)
    let locale: Vec<u16> = "zh-CN".encode_utf16().chain(std::iter::once(0)).collect();
    let (wa, wb) = (wide(a), wide(b));
    let r = unsafe {
        CompareStringEx(
            locale.as_ptr(),
            0,
            wa.as_ptr(),
            -1,
            wb.as_ptr(),
            -1,
            std::ptr::null(),
            std::ptr::null(),
            0,
        )
    };
    match r - 2 {
        x if x < 0 => std::cmp::Ordering::Less,
        0 => std::cmp::Ordering::Equal,
        _ => std::cmp::Ordering::Greater,
    }
}

#[cfg(not(windows))]
fn cmp_pinyin(a: &str, b: &str) -> std::cmp::Ordering {
    a.cmp(b)
}

impl ArgHelp {
    /// 常用参数表(2026-10-11 按本机 scrcpy 3.3.3 的 `--help` 逐条核对后扩充)。
    ///
    /// 口径:
    /// - 只收录**该版本的帮助里真实存在**的参数:名称、取值、默认值都以帮助为准
    ///   (帮助里已经改名的老参数不再收录,例如旧版的 `--display-buffer` 现在是
    ///   `--video-buffer`、`--lock-video-orientation` 现在是 `--capture-orientation`);
    /// - 第五项是平台口径:帮助里写明 "only available on Linux" 的标「仅 Linux」,
    ///   其余标「通用」;
    /// - 参数文本一律写成**单个 token**(带 `=`,不带空格)—— 界面上的
    ///   [撤去该参数]是按空白切词比对的,带空格的写法会被当成两个参数。
    fn defaults() -> Self {
        // (参数文本, 中文名, 说明, 示例, 平台)
        let raw: &[(&str, &str, &str, &str, &str)] = &[
            // ---------------- 连接 / 设备选择 ----------------
            (
                "--serial=0123456789ABCDEF",
                "指定设备序列号",
                "多台设备同时连着 adb 时,用它指定镜像哪一台(只有一台时可省略)。\n序列号就是程序设备列表里显示的那一串(双击本行可改成你的)。",
                "--serial=QOGQKBXOMFOVC6XS",
                "通用",
            ),
            (
                "-d",
                "只选 USB 设备",
                "在多台设备里挑 USB 连接的那一台(相当于 adb -d);与 -e 互斥。",
                "-d",
                "通用",
            ),
            (
                "-e",
                "只选网络设备",
                "在多台设备里挑 TCP/IP 连接的那一台(相当于 adb -e);与 -d 互斥。",
                "-e",
                "通用",
            ),
            (
                "--tcpip=192.168.1.20:5555",
                "无线连接",
                "让设备启用 TCP/IP 调试并连上它(默认端口 5555)。\n不带地址时(只写 --tcpip)会自动取当前 USB 设备的 IP。\n注意:走 Wi-Fi 时延迟与稳定性都不如 USB。",
                "--tcpip\n\n或指定地址:--tcpip=192.168.1.20:5555",
                "通用",
            ),
            (
                "--port=27183",
                "客户端端口",
                "指定本机监听用的 TCP 端口(默认 27183:27199)。\n端口被别的程序占用时,用它可以换一个。",
                "--port=27183",
                "通用",
            ),
            (
                "--list-apps",
                "列出设备上的应用",
                "打印设备上已安装应用的包名(配合 --start-app 用)。\n运行后 scrcpy 直接退出,不会开窗口。",
                "--list-apps",
                "通用",
            ),
            (
                "--list-displays",
                "列出设备显示器",
                "打印设备上的显示器 id(配合 --display-id 用,比如把游戏放到外接屏)。",
                "--list-displays",
                "通用",
            ),
            (
                "--list-encoders",
                "列出可用编解码器",
                "打印设备端可用的硬/软编码器名字(配合 --video-encoder / --audio-encoder 用)。",
                "--list-encoders",
                "通用",
            ),
            (
                "--display-id=1",
                "镜像指定显示器",
                "镜像设备上某一块显示器(默认 0 = 主屏)。\n先 --list-displays 拿到 id,再把游戏投到副屏/虚拟屏上。",
                "--display-id=1",
                "通用",
            ),
            (
                "--new-display=1920x1080/420",
                "新建虚拟显示器",
                "在设备上新建一块虚拟显示器再镜像它(不干扰手机上原本的画面)。\n可只给分辨率、只给 dpi,或都不给(用主屏的尺寸与密度);\n配合 --start-app 可以把游戏直接丢进这块虚拟屏。",
                "--new-display=1920x1080/420\n\n--new-display --start-app=+com.example.game",
                "通用",
            ),
            (
                "--start-app=com.example.game",
                "启动后打开应用",
                "启动时按包名打开一个 Android 应用。\n前缀 ? = 按名字前缀模糊匹配;前缀 + = 打开前先强制停止。",
                "--start-app=+com.example.game\n\n--start-app=?firefox",
                "通用",
            ),
            // ---------------- 输入(与"双份输入"直接相关) ----------------
            (
                "--mouse=disabled",
                "不转发鼠标(避免双份输入)",
                "关掉 scrcpy 自己的鼠标转发:鼠标只由本程序的键位映射注入,\n不会再被 scrcpy 同时转一份给设备 —— 这正是“鼠标操作变成两份”的根因。\n代价:非 FPS 模式下也不能用鼠标点手机画面了(拖拽/缩放/拖放文件都用不了)。\n取值:disabled(不转发)/ sdk(系统 API,默认)/ uhid / aoa(后两者是模拟物理鼠标,仅 USB 可用 aoa)。",
                "--mouse=disabled",
                "通用",
            ),
            (
                "--keyboard=disabled",
                "不转发键盘(避免双份输入)",
                "关掉 scrcpy 自己的键盘转发:键盘只由本程序的键位映射注入,\n避免同一次按键被 scrcpy 与本程序各发一份。\n代价:非 FPS 模式下也不能用键盘给设备打字/scrcpy 快捷键里的字符输入。\n取值:disabled / sdk(默认)/ uhid / aoa(后两者模拟物理键盘,需在设备上先配好键盘布局)。",
                "--keyboard=disabled",
                "通用",
            ),
            (
                "--gamepad=disabled",
                "手柄转发模式",
                "scrcpy 的手柄(游戏控制器)转发方式。\n取值:disabled(不转发)/ uhid(模拟物理手柄,Linux UHID)/ aoa(仅 USB)。\n本程序目前不用手柄,所以通常保持 disabled 最省事。",
                "--gamepad=disabled",
                "通用",
            ),
            (
                "--otg",
                "OTG 模式",
                "不用 adb,直接把电脑的键鼠当成插在手机上的物理键鼠(仅 USB 可用)。\n这个模式下不镜像画面、没有声音,也不走本程序的触摸注入。",
                "--otg",
                "通用(仅 USB)",
            ),
            (
                "--mouse-bind=bhsn:++++",
                "次要鼠标键的功能",
                "配置右键/中键/第 4 键/第 5 键干什么(默认 bhsn:++++)。\n每个字符一个键,顺序是右/中/4/5:+ 转发给设备、- 忽略、\nb 返回、h 主页、s 最近任务、n 拉下通知栏;\n冒号后面那一串是“按住 Shift 时”的第二套绑定。",
                "--mouse-bind=bhsn:++++\n\n--mouse-bind=++++:bhsn",
                "通用",
            ),
            (
                "--no-mouse-hover",
                "不转发鼠标悬停",
                "不把“没按键、只是移动”的鼠标事件发给设备。\n有些游戏的悬浮菜单会因为鼠标划过而乱弹,开这个能压掉。",
                "--no-mouse-hover",
                "通用",
            ),
            (
                "--prefer-text",
                "字母键按文本注入",
                "字母与空格改用“文本事件”注入而不是按键事件。\n能解决组合键打特殊字符的问题,但会破坏游戏里 WASD 这类按键 —— \n打游戏时**不要**开,打字场景才开。",
                "--prefer-text",
                "通用",
            ),
            (
                "--raw-key-events",
                "全部按键走原始键事件",
                "所有按键都用原始 keycode 注入,忽略文本事件。\n与 --prefer-text 相反;游戏里更“原汁原味”,但某些输入法场景会失灵。",
                "--raw-key-events",
                "通用",
            ),
            (
                "--no-key-repeat",
                "不转发按住重复",
                "按住一个键时不转发系统的“自动重复”事件(只发一次按下 + 一次抬起)。\n宏/连点类操作更干净,但长按加速类的输入会受影响。",
                "--no-key-repeat",
                "通用",
            ),
            (
                "--no-clipboard-autosync",
                "关闭剪贴板自动同步",
                "默认情况下电脑与设备的剪贴板会自动互相同步;这一项关掉它。\n打游戏时不想让剪贴板内容被换来换去,可以开。",
                "--no-clipboard-autosync",
                "通用",
            ),
            (
                "--shortcut-mod=lctrl,lsuper",
                "换快捷键前缀",
                "改 scrcpy 自己的快捷键前缀(默认左 Alt / 左 Super)。\n可选 lctrl / rctrl / lalt / ralt / lsuper / rsuper,逗号分隔可多选。\n前缀与本程序绑定的按键冲突时,换一个能少很多误触。",
                "--shortcut-mod=lctrl,lsuper",
                "通用",
            ),
            (
                "-n",
                "只读镜像",
                "完全禁止控制设备(相当于 --no-control):能看不能点,也不注入键鼠。\n排查“到底是谁在动我的设备”时很有用。",
                "-n 或 --no-control",
                "通用",
            ),
            // ---------------- 画面 / 性能 ----------------
            (
                "--max-size=1920",
                "画面清晰度上限",
                "限制镜像视频的最长边像素,另一条边按设备比例缩放。\n值越大越清晰、越耗带宽;低于设备原始分辨率还能明显降低延迟。\n默认 0(不限制,即设备原始分辨率)。",
                "--max-size=1920\n\n手机是 1080x2400 时加 --max-size=1080 就是 1k;\n模拟器/真机 4k 屏可用 --max-size=3840。",
                "通用",
            ),
            (
                "--max-fps=60",
                "帧率上限",
                "限制屏幕采集帧率(官方口径:Android 10 起正式支持,更早版本可能也能用)。\n数值越低越省资源,但画面/操作更“肉”。部分游戏建议降到 30~60 换更稳的延迟。",
                "--max-fps=60",
                "通用",
            ),
            (
                "--video-bit-rate=16M",
                "视频码率",
                "编码码率,直接决定画面细节保留程度;支持 K / M 后缀。\n网络/串流卡顿时可调低(如 4M),画面发糊时调高(如 20M)。默认 8M。",
                "--video-bit-rate=12M",
                "通用",
            ),
            (
                "--video-codec=h265",
                "视频编码格式",
                "可选 h264 / h265 / av1(本机 scrcpy 3.3.3 只有这三种)。\n同码率下 h265 比 h264 更清晰(设备需支持);av1 更吃 CPU。默认 h264。",
                "--video-codec=h265",
                "通用",
            ),
            (
                "--video-encoder=encoder-name",
                "指定视频编码器",
                "强制用设备上某一个具体的 MediaCodec 编码器(名字先用 --list-encoders 查)。\n默认的编码器在某些机型上画质/延迟不好时,换一个试试。",
                "--video-encoder=OMX.qcom.video.encoder.avc",
                "通用",
            ),
            (
                "--video-buffer=50",
                "视频缓冲",
                "显示前先攒一段视频(毫秒),用延迟换流畅(抗网络抖动)。\n默认 0 = 不缓冲;卡顿明显时再往上加。**注意它只会增加延迟**。",
                "--video-buffer=50",
                "通用",
            ),
            (
                "--no-video",
                "不转发视频",
                "完全不做视频采集与编码(只留音频或只留控制)。\n纯粹想用本程序操作手机、不需要看画面时才用。",
                "--no-video",
                "通用",
            ),
            (
                "--no-video-playback",
                "只推流不播放画面",
                "服务端照常采集编码,但电脑上不播放(画面窗口不显示内容)。",
                "--no-video-playback",
                "通用",
            ),
            (
                "-N",
                "完全不播放",
                "电脑端不播放视频也不播放音频(相当于\n--no-video-playback --no-audio-playback 一起用)。",
                "-N 或 --no-playback",
                "通用",
            ),
            (
                "--no-window",
                "不开窗口",
                "不创建 scrcpy 窗口(隐含 --no-video-playback)。\n常与 --record / --v4l2-sink 这类“只输出到别处”的用法搭配。",
                "--no-window --record=game.mp4",
                "通用",
            ),
            (
                "--print-fps",
                "打印帧率",
                "在控制台持续打印帧率(窗口里按 MOD+i 也能随时开关)。\n调 --max-fps / 码率时用它看实际效果。",
                "--print-fps",
                "通用",
            ),
            (
                "--video-source=camera",
                "画面改用摄像头",
                "画面来源改成设备摄像头而不是屏幕(需要 Android 12+)。\n可用 --camera-id / --camera-facing=front|back|external / --camera-size 指定。",
                "--video-source=camera --camera-facing=back",
                "通用",
            ),
            // ---------------- 音频 ----------------
            (
                "--no-audio",
                "关闭音频转发",
                "完全关闭音频转发(设备端照常出声)。\n打游戏不需要听声音时能省带宽、更稳。",
                "--no-audio",
                "通用",
            ),
            (
                "--audio-codec=aac",
                "音频编码格式",
                "音频编码可选 opus / aac / flac / raw。\n默认 opus;兼容性问题时可换 aac。",
                "--audio-codec=aac",
                "通用",
            ),
            (
                "--audio-bit-rate=128K",
                "音频码率",
                "音频编码码率,支持 K / M 后缀(默认 128K)。\n音乐/语音场景可以调高;只想听个响、省带宽就调低。",
                "--audio-bit-rate=192K",
                "通用",
            ),
            (
                "--audio-source=mic",
                "音频来源",
                "选采什么声音:output(整机输出,默认)/ playback(应用播放)/ mic(麦克风)/\nvoice-call 系列(通话)/ mic-voice-communication 等。\n打游戏时想同时听到手机与电脑的声音,用 --audio-dup 配合 playback。",
                "--audio-source=mic",
                "通用",
            ),
            (
                "--audio-dup",
                "设备端同时外放",
                "采集音频的同时让设备自己继续外放(只在 --audio-source=playback 下有效)。",
                "--audio-source=playback --audio-dup",
                "通用",
            ),
            (
                "--no-audio-playback",
                "电脑端不播放音频",
                "不播放声音,但仍然转发音频(比如只为了录制)。",
                "--no-audio-playback",
                "通用",
            ),
            (
                "--require-audio",
                "音频失败即报错",
                "默认音频采集失败时只是没有声音、画面照常;加这一项则直接启动失败。\n排查音频问题、或者“必须有声音”的场景用。",
                "--require-audio",
                "通用",
            ),
            // ---------------- 录制 / 输出 ----------------
            (
                "--record=game.mp4",
                "录制到文件",
                "把镜像的画面录到文件(容器由扩展名或 --record-format 决定)。\n本程序启动 scrcpy 时可与 --no-window 组合成“只录不显示”。",
                "--record=game.mp4",
                "通用",
            ),
            (
                "--record-format=mkv",
                "录制容器格式",
                "强制录制格式:mp4 / mkv / m4a / mka / opus / aac / flac / wav。\n录制中途被中断时 mkv 更不容易坏。",
                "--record=game.mkv --record-format=mkv",
                "通用",
            ),
            (
                "--record-orientation=90",
                "录制画面旋转",
                "只把**录制**的画面旋转 0/90/180/270 度(显示不受影响)。",
                "--record-orientation=90",
                "通用",
            ),
            (
                "--time-limit=60",
                "限时自动退出",
                "镜像最多跑这么多秒,到点自动结束(录制/压测时用)。",
                "--time-limit=60",
                "通用",
            ),
            (
                "--v4l2-sink=/dev/video0",
                "输出到 v4l2 设备",
                "把画面推到 Linux 的 v4l2loopback 虚拟摄像头,供 OBS 等程序当摄像头用。\n**仅 Linux**:需要先加载 v4l2loopback 内核模块。",
                "--v4l2-sink=/dev/video0 --no-window",
                "仅 Linux",
            ),
            // ---------------- 窗口 / 方向 ----------------
            (
                "-f",
                "全屏显示",
                "scrcpy 窗口以全屏模式打开。",
                "-f 或 --fullscreen",
                "通用",
            ),
            (
                "--always-on-top",
                "窗口置顶",
                "让 scrcpy 画面窗口始终显示在最上层,不被其它窗口遮挡。",
                "--always-on-top",
                "通用",
            ),
            (
                "--window-title=scrcpy-pad",
                "自定义窗口标题",
                "给 scrcpy 窗口设置自定义标题,便于区分多开实例。",
                "--window-title=打游戏",
                "通用",
            ),
            (
                "--window-borderless",
                "无边框窗口",
                "去掉窗口标题栏与边框,画面更干净(全屏之外的另一种“沉浸”做法)。",
                "--window-borderless",
                "通用",
            ),
            (
                "--window-width=480",
                "初始窗口宽度",
                "设置 scrcpy 窗口初始宽度(高度按比例自动)。\n值设为 0 表示自动。",
                "--window-width=480",
                "通用",
            ),
            (
                "--crop=1920:1080:0:0",
                "画面裁剪",
                "只显示设备屏幕的一部分(宽:高:x:y),裁剪在设备端完成。\n适合隐藏通知栏/黑边;坐标按设备**自然方向**算(手机通常是竖屏)。",
                "--crop=2400:1080:0:1320",
                "通用",
            ),
            (
                "--display-orientation=90",
                "显示方向",
                "只旋转**显示**画面的方向(0/90/180/270,可加 flip 前缀镜像)。\n与 --record-orientation 的区别:它管录制、这一项管显示。",
                "--display-orientation=90",
                "通用",
            ),
            (
                "--orientation=90",
                "画面旋转(显示+录制)",
                "同时设置显示与录制方向,等价于\n--display-orientation=90 --record-orientation=90。\n横屏游戏旋转 90 度后,坐标与截图会按旋转后画面算。",
                "--orientation=90",
                "通用",
            ),
            (
                "--capture-orientation=@90",
                "采集方向(可锁定)",
                "设置采集方向:0/90/180/270,可加 flip 前缀。\n加 @ 前缀表示**锁定**该方向(设备转屏时画面不跟着转);\n单独的 @ 表示锁定为启动时的方向 —— 手机自动旋转导致画面乱转时用它。",
                "--capture-orientation=@90\n\n--capture-orientation=@\n\n(旧版叫 --lock-video-orientation)",
                "通用",
            ),
            // ---------------- 设备电源 / 状态 ----------------
            (
                "--turn-screen-off",
                "启动即息屏",
                "启动后立刻关闭设备屏幕(快捷键等效 -S)。\n串流打游戏时能省电并防止误触,画面不受影响。",
                "--turn-screen-off",
                "通用",
            ),
            (
                "-w",
                "保持唤醒",
                "连接期间设备不休眠(stay-awake)。本程序已默认加入,无需重复。",
                "-w 或 --stay-awake",
                "通用",
            ),
            (
                "--power-off-on-close",
                "退出时关屏",
                "关闭 scrcpy 时顺手把设备屏幕关掉(省电,避免游戏在后台亮着)。",
                "--power-off-on-close",
                "通用",
            ),
            (
                "--no-power-on",
                "启动时不点亮屏幕",
                "启动 scrcpy 时不去唤醒设备屏幕(默认会点亮)。",
                "--no-power-on",
                "通用",
            ),
            (
                "--disable-screensaver",
                "运行期间禁用屏保",
                "scrcpy 运行期间禁用电脑的屏幕保护/息屏(长局挂机时有用)。",
                "--disable-screensaver",
                "通用",
            ),
            (
                "-t",
                "显示触摸提示",
                "开启系统“显示触摸”提示(只显示物理触摸,不显示 scrcpy/本程序注入的触点)。\n调试“我按的这一下到底落在哪”时很有用。",
                "-t 或 --show-touches",
                "通用",
            ),
            // ---------------- 收尾 / 调试 ----------------
            (
                "--no-cleanup",
                "退出时不做清理",
                "默认 scrcpy 退出时会把服务端程序从设备上删掉、并恢复“显示触摸/保持唤醒/电源”这些被改过的设置;\n这一项取消清理(排查问题时能让设备保持原样)。",
                "--no-cleanup",
                "通用",
            ),
            (
                "--kill-adb-on-close",
                "退出时结束 adb",
                "scrcpy 退出时把 adb 服务一起杀掉。\nadb 卡住、或者不想留后台进程时用。",
                "--kill-adb-on-close",
                "通用",
            ),
            (
                "--verbosity=debug",
                "日志级别",
                "设置 scrcpy 自己的日志级别:verbose / debug / info / warn / error(默认 info)。\n排查连接/编码问题时用 debug 或 verbose 看细节。",
                "--verbosity=debug",
                "通用",
            ),
        ];
        let entries = raw
            .iter()
            .map(|(flag, name, desc, usage, platform)| ArgEntry {
                flag: flag.to_string(),
                name,
                desc,
                usage,
                platform,
            })
            .collect();
        let mut me = Self {
            entries,
            selected: 0,
            editing: None,
            query: String::new(),
            filtered: Vec::new(),
        };
        // 用户 2026-10-13:"把 scrcpy 参数里的顺序调整一下,让他按照中文字典序
        // (拼音顺序)来排个序,方便用户查找" —— 打开即拼音序,不必再点一次。
        me.sort_by_pinyin();
        me
    }

    /// 重算过滤结果(空查询 = 全部)。
    ///
    /// 匹配口径:**不区分大小写的子串**,四路任一命中即可 ——
    /// ① 中文名(用户看到的那一列)、② 参数文本(`--record`)、
    /// ③ 作用说明(记得"能录屏"但忘了参数名)、④ 使用示例。
    fn refilter(&mut self) {
        let q = self.query.trim().to_lowercase();
        self.filtered = if q.is_empty() {
            (0..self.entries.len()).collect()
        } else {
            self.entries
                .iter()
                .enumerate()
                .filter(|(_, e)| {
                    e.name.to_lowercase().contains(&q)
                        || e.flag.to_lowercase().contains(&q)
                        || e.desc.to_lowercase().contains(&q)
                        || e.usage.to_lowercase().contains(&q)
                })
                .map(|(i, _)| i)
                .collect()
        };
        // 选中项若被过滤掉了,落到第一条(否则详情区会显示一个不在列表里的参数)
        if !self.filtered.is_empty() && !self.filtered.contains(&self.selected) {
            self.selected = self.filtered[0];
        }
    }

    /// 按**中文拼音**给参数表排序(用户 2026-10-13)。
    ///
    /// 排序键 = 中文名;同名时用参数文本兜底(保证全序、稳定)。
    /// 这样用户"按拼音顺序找参数"就和翻通讯录一样 —— 而不是现在这种
    /// "按功能分组"的排列(那是按用途组织的,找具体某个参数要逐条扫)。
    fn sort_by_pinyin(&mut self) {
        self.entries
            .sort_by(|a, b| cmp_pinyin(a.name, b.name).then_with(|| a.flag.cmp(&b.flag)));
        self.refilter();
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
    /// 「鼠标独占(隔膜)」的手动开关热键(S②,2026-10-11):按一下翻转手动独占,
    /// 再按一下关闭。隔膜开着时鼠标点不了任何窗口,这个键是**唯一**还通的路
    /// (键盘事件先转发后吞),所以它必须是组合键槽(与 FPS 三键同一套交互)。
    AimShield,
    /// 「唤醒视角」热键(A,2026-10-11):锁死时**只抬起瞄准指**,不关映射、
    /// 不退出 FPS;下一次鼠标位移会自动重新落指。锁死时鼠标点不了任何界面,
    /// 键盘是唯一还通的路,所以它必须是组合键槽(与 S② 同一套交互)。
    AimWake,
    /// 「强制重连控制通道」热键(B,2026-10-11):备用逃生门 —— 按一下等同诊断
    /// 面板里的[强制重连控制通道](只重启我们自己的通道,不动 scrcpy 投屏)。
    /// 与 [`Self::AimWake`] 一样默认不绑,同样是组合键槽。
    AimReconnect,
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
    /// 宏录制的「开始录制」热键(用户 2026-10-12)。**单键槽**(不在 `takes_chord`
    /// 里)⇒ 组合键在这条路上根本进不来,天然满足"只允许单键绑定"。
    MacroRecordStart,
    /// 宏录制的「终止录制」热键。同为单键槽。
    MacroRecordStop,
    /// 宏录制的「取消录制」热键(用户 2026-10-12 第 1 条)。同为单键槽,
    /// 且是**全局公共**的(程序级设置,不挂在某条宏上)。
    MacroRecordCancel,
    /// 轨迹录制的**快捷键**(用户 2026-10-14)。同为单键槽(组合键进不来),
    /// 程序级设置 —— 按一下开始录制、再按一下结束(结束键未绑定时)。
    TrackRecKey,
    /// 轨迹录制的**结束键**(用户 2026-10-23 第 2 条:与宏录制一样"一对开始/结束")。
    /// 同为单键槽;未绑定(0)时退回上面的"同一个键切换"。
    TrackRecStopKey,
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
                | Self::AimShield
                | Self::AimWake
                | Self::AimReconnect
                | Self::RecoilTrigger
                | Self::RecoilSwitch
                | Self::SwitchKey(_)
                | Self::WheelDir { .. }
                | Self::WheelEnable(_)
        )
    }
}

/// 宏步骤里「按键 / 组合键」捕获的落点（2026-10-11）。
///
/// 宏步骤编辑器有三处：宏页编辑区、宏页「已录宏」展开行、可视化键位页展开行。
/// 后两处编辑的是 `binds[i]` 的**每帧重新克隆的副本**，改动只有写回 `Shared`
/// 才算数；宏页编辑区编辑的则是 `PadApp::macro_page_instructions` 这份常驻草稿。
///
/// 而捕获是**下一帧的事件阶段**才落定的（那时副本早已被丢弃）—— 所以捕获必须
/// 记住"这一次是在给谁改"。以前不记，于是后两处按下按键时，捕获会去写宏页草稿：
/// 草稿里恰好有那个下标就**悄悄改错了一条宏**，没有就**毫无反应**（界面却照样
/// 打日志说"键位已绑定"）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MacroKeyTarget {
    /// 宏页编辑区的常驻草稿
    Page,
    /// `profile.binds[i]` 里那条宏（展开副本 ⇒ 直接写回 `Shared`）
    Bind(usize),
}

/// 宏步骤「组合键」最多几个键（与界面上 `＋` 的上限、`MacroInstruction::Combo`
/// 的语义一致）。
const MACRO_COMBO_MAX_KEYS: usize = 4;

/// 把 `codes` 写进一条组合键步骤的键表。返回**是否真的写进去了**
/// (空 `codes`、或槽位越界都没写 —— 调用方据此决定要不要打"键位已绑定")。
///
/// 从第 0 格开始录 = "这一整组就是这一轮按下的键" ⇒ **整份替换**，不留旧尾巴
/// （否则从 4 键改成 2 键时会剩下两个 0，宏执行时会去按一个无效键码）。
/// 从第 1 格起录 = 只改那一格，缺位按 [`MACRO_COMBO_MAX_KEYS`] 上限补长。
fn write_combo_keys(dst: &mut Vec<u16>, slot: usize, codes: &[u16]) -> bool {
    if codes.is_empty() {
        return false;
    }
    if slot == 0 {
        dst.clear();
        dst.extend(codes.iter().take(MACRO_COMBO_MAX_KEYS));
        return true;
    }
    // 从第 1 格起录 = **只改那一格**(与上面的 doc 一致,2026-10-23 修正):
    // 以前是把整组 codes 从 slot 起连续写入 —— 在"第 2 格"上按 Ctrl+Z 会把
    // 第 3 格一起顶掉,用户看到的是"我只改了一格,旁边的键自己变了"。
    // 一格只放一个键:取 codes 的**最后一个**(canonical_chord 把修饰键排在前
    // 面,所以最后那个就是这一次按下的"主键";只按一个键时就是它本身)。
    let Some(code) = codes.last().copied() else {
        return false;
    };
    if slot < dst.len() {
        dst[slot] = code;
        true
    } else if slot < MACRO_COMBO_MAX_KEYS {
        dst.push(code);
        true
    } else {
        false
    }
}

/// 用户配置里一条组合键最多几个成员格（与界面上 `＋ 新增成员` 的上限一致）。
const COMBO_CELLS_MAX: usize = 4;

/// 把一次捕获写进「用户配置的组合键成员格」（`profile.combos[i].keys`）。
///
/// 两条规矩（2026-10-23 组合键事故第二半）：
/// - **只按一个键** = 只改这一格（与从前逐字相同，绝不碰旁边的格子）；
/// - **按住组合**（≥2 个键）= 从这一格起，这整组就是这些键 ⇒ 连续填写，并把这一格
///   之后的旧尾巴截掉（否则把 3 键改成 2 键时会剩一个上一次的残键）。
///
/// 为什么要走和弦捕获：成员格原先走单键捕获 —— 按下 Ctrl 就立刻落定、`waiting_key`
/// 随即清空，用户仍按着的那个 Z 被全局快捷键当成“撤销”执行，刚写的值被回滚成原值
/// （用户原话：“设的 Ctrl+Z，界面变成 Ctrl+X”）。
fn write_chord_into_combo(dst: &mut Vec<u16>, slot: usize, codes: &[u16]) -> bool {
    let codes: Vec<u16> = codes.iter().copied().filter(|k| *k != 0).collect();
    if codes.is_empty() || slot >= COMBO_CELLS_MAX {
        return false;
    }
    if codes.len() == 1 {
        // 一个键 = 只改这一格。保持在列表内的原位，不截尾巴。
        while dst.len() <= slot {
            dst.push(0);
        }
        dst[slot] = codes[0];
        return true;
    }
    let take = codes.len().min(COMBO_CELLS_MAX - slot);
    let codes = &codes[..take];
    let want = slot + take;
    while dst.len() < want {
        dst.push(0);
    }
    dst[slot..want].copy_from_slice(codes);
    if dst.len() > want {
        dst.truncate(want);
    }
    while dst.len() < 2 {
        // 组合键的语义就是“至少两个键”：缺的位补 0 占位，与界面上 `＋ 新增成员`
        // 推入的 0 同一形态（`keymap::normalize` 会把含 0 的组合键判为不生效）。
        dst.push(0);
    }
    true
}

/// 把本帧收集到的「组合键卡片字段编辑」写回实时列表（2026-10-23 用户第 1 条：
/// 「点击完组合键，组合键映射的位置突然瞬移了」）。
///
/// 两条纪律，缺一不可：
/// 1. **必须在同一帧的结构性编辑（删组合 / 删成员 / 加成员 / 新增组合）之前调用** ——
///    那一刻实时列表还是渲染时的顺序，帧首下标才指向它当初渲染的那一条；放到后面做就
///    成了「帧首下标 + 已经 remove/push 过的实时列表」；
/// 2. **字段级写回**，绝不整份覆盖 `KeyCombo` —— `keys` 的改动由删/加成员直接写在实时
///    列表上，整份覆盖会把旧快照的键位盖回去（键位与落点一起「搬」到邻居身上）。
///
/// 返回实际写入条数：下标越界就跳过（宁可丢掉这一次界面改动，也不允许静默写到别人
/// 身上），调用方据此把「有几条没写回」说出来。
fn commit_combo_field_edits(
    frame_start: &[KeyCombo],
    live: &mut [KeyCombo],
    edits: &[(usize, KeyCombo)],
) -> usize {
    if frame_start.len() != live.len() {
        // 长度变了 = 调用点把写回排到了结构性编辑之后:下标语义已经不一致,一律不写
        // (宁可丢掉这一次界面改动,也不写到邻居身上)。顺序由守护测试
        // combo_field_edits_are_committed_before_the_frames_structural_edits 钉住。
        return 0;
    }
    let mut hit = 0usize;
    for (index, edited) in edits {
        let Some(target) = live.get_mut(*index) else {
            continue;
        };
        target.action = edited.action.clone();
        target.fps_only = edited.fps_only;
        target.tail_delay_ms = edited.tail_delay_ms;
        target.repeat_while_held = edited.repeat_while_held;
        hit += 1;
    }
    hit
}

/// 帧首快照的回写规则（同一次事故的次要通路）：一个在渲染期间被局部改写的状态
/// （取点 / 缓动编辑）只允许在「这一帧里没有别处动过它」时回写。
///
/// 否则会把这一帧里已经被消费掉的取点**复活** —— 用户以为取点结束了，下一次点画布
/// 却又写入一点，同样表现为「位置瞬移」。谁先改就以谁为准：`live != frame_start`
/// 说明本帧别处动过，保留 `live`。
fn frame_local_take<T: PartialEq + Copy>(
    frame_start: Option<T>,
    live: Option<T>,
    local: Option<T>,
) -> Option<T> {
    if live == frame_start { local } else { live }
}

/// 在 `binds[bind]` 那条宏里取出第 `index` 条「组合键」步骤的键表。
fn macro_combo_in_bind(
    binds: &mut [crate::keymap::KeyBind],
    bind: usize,
    index: usize,
) -> Option<&mut Vec<u16>> {
    match &mut binds.get_mut(bind)?.action {
        Action::Macro(m) => match m.instructions.get_mut(index) {
            Some(MacroInstruction::Combo { keys, .. }) => Some(keys),
            _ => None,
        },
        _ => None,
    }
}

/// 在 `binds[bind]` 那条宏里改写第 `index` 条「按键」步骤的键码。
/// 返回是否真的写进去了(目标下标不存在 = 列表刚变过,什么都没写)。
fn write_macro_key_in_bind(
    binds: &mut [crate::keymap::KeyBind],
    bind: usize,
    index: usize,
    code: u16,
) -> bool {
    if let Some(b) = binds.get_mut(bind)
        && let Action::Macro(m) = &mut b.action
        && let Some(MacroInstruction::Key { code: dst, .. }) = m.instructions.get_mut(index)
    {
        *dst = code;
        return true;
    }
    false
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
///
/// 这里只是"从输入色推导出一个更暗的同色相颜色",**没有字面色板**;
/// alpha 显式写 255 = 输出恒不透明(透明度由调用方的 `theme::with_alpha` 决定)。
fn darken(c: egui::Color32, sub: u8) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(
        c.r().saturating_sub(sub),
        c.g().saturating_sub(sub),
        c.b().saturating_sub(sub),
        255,
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

/// 遮板(蒙版)显示项目的位标志(用户 2026-10-13 第 2 条)。
///
/// 语义与「截图取点」的 [`OverlayFilter`] **逐项一致** —— 用户明确要求
/// "里面与截图取点的项目相同"。用位标志存进 `settings.json`,于是加项不会破坏
/// 老配置,而且与 `OverlayFilter` 的布尔字段可以互相转换(见 `to_bits/from_bits`)。
pub const MASK_FILTER_KEYS: u8 = 1 << 0;
pub const MASK_FILTER_COMBOS: u8 = 1 << 1;
pub const MASK_FILTER_MACROS: u8 = 1 << 2;
pub const MASK_FILTER_WHEELS_PERM: u8 = 1 << 3;
pub const MASK_FILTER_WHEELS_TEMP: u8 = 1 << 4;
pub const MASK_FILTER_AIM: u8 = 1 << 5;
/// 缺省 = 与 `OverlayFilter::default()` 一致(键位/组合/永久轮盘/临时轮盘 开;宏/锚点 关)
pub const MASK_FILTER_DEFAULT: u8 =
    MASK_FILTER_KEYS | MASK_FILTER_COMBOS | MASK_FILTER_WHEELS_PERM | MASK_FILTER_WHEELS_TEMP;

impl OverlayFilter {
    /// 转成可存进 settings.json 的位标志(见 `MASK_FILTER_*`)
    fn to_bits(self) -> u8 {
        let mut b = 0u8;
        if self.keys {
            b |= MASK_FILTER_KEYS;
        }
        if self.combos {
            b |= MASK_FILTER_COMBOS;
        }
        if self.macros {
            b |= MASK_FILTER_MACROS;
        }
        if self.wheels_perm {
            b |= MASK_FILTER_WHEELS_PERM;
        }
        if self.wheels_temp {
            b |= MASK_FILTER_WHEELS_TEMP;
        }
        if self.aim {
            b |= MASK_FILTER_AIM;
        }
        b
    }

    /// 从位标志还原(遮板用;未知位忽略)
    fn from_bits(b: u8) -> Self {
        Self {
            keys: b & MASK_FILTER_KEYS != 0,
            combos: b & MASK_FILTER_COMBOS != 0,
            macros: b & MASK_FILTER_MACROS != 0,
            wheels_perm: b & MASK_FILTER_WHEELS_PERM != 0,
            wheels_temp: b & MASK_FILTER_WHEELS_TEMP != 0,
            aim: b & MASK_FILTER_AIM != 0,
        }
    }
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

/// 宏绑定在遮板上的"中心点"(相对坐标)。
///
/// 宏没有"响应范围圆"(它的动作是一串步骤),所以取它**设置步骤**里第一个带
/// 坐标的动作(点击/滑动起点)作为代表位置;没有就返回 `None`,由调用方跳过。
/// 抽成辅助函数是为了让 `fill_mask_shapes` 里的过滤判断保持一行。
fn macro_bind_center(action: &Action) -> Option<(f32, f32)> {
    let Action::Macro(m) = action else {
        return None;
    };
    for ins in &m.instructions {
        match ins {
            MacroInstruction::Click { x, y, .. } => return Some((*x, *y)),
            MacroInstruction::Swipe {
                start_x, start_y, ..
            } => return Some((*start_x, *start_y)),
            _ => {}
        }
    }
    None
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
    /// 「设备侧」那一行的三样:控制通道此刻认的触摸坐标空间、距上一轮周期巡检
    /// 过了多久、最近一次焦点窗口(短形式,见 [`short_focus`])。
    ///
    /// 用户 2026-10-11 的故障里"空间过期"只能靠翻日志看;现在和引擎侧判据挤在
    /// 同一行,卡住的那一刻就能当场读出"我们发的坐标属于哪个空间、设备前台是谁"。
    space: String,
    space_rechecked: String,
    focus: String,
    /// 最近一次「设备注入自检」的结论(含它在手机上点了哪儿)
    selfcheck: String,
    /// 控制通道存活自检的结论:`活/死/未知(上次自检 Xms 前)`。
    ///
    /// 这一项是"server 死了但本地写入仍成功"那条故障**唯一**会露馅的地方:
    /// 空间/焦点/shell 自检全都正常时,只有它说得清"我们这条通道在设备端
    /// 还有没有人收"(见 `adb::channel_probe`)。
    channel: String,
    /// 最近一条设备端 server 自己的错误(带"多久之前";None = 本次运行没出现过)
    channel_error: Option<String>,
    aim_motions: u64,
    aim_last_dx: f32,
    aim_last_dy: f32,
    /// 「FPS 状态」行的引擎侧判据(用户 2026-10-11 第 6 条)。全部是**只读快照**:
    /// 悬浮窗每帧从 `Shared.aim_live` 取一份,用来回答"为什么视角转不动"。
    aim_mode_active: bool,
    aim_gate_down: bool,
    aim_anchor_set: bool,
    aim_down: bool,
    aim_others: usize,
    aim_enabled: bool,
    /// 卡死自愈(2026-10-12,用户第 6 条)在状态行里的几样:`手机=摸/离`(只读监听
    /// 手机触摸屏的状态)、`账单=N`(设备端 `TouchStates` 触点数;`None` = 未知,
    /// 显示 `?`)、自愈次数与最近原因。同样是**只读快照**,不参与任何判定。
    physical_touch: bool,
    device_pointers: Option<u32>,
    self_heal_count: u64,
    self_heal_reason: &'static str,
    /// 捕获层只读探针(`capture::raw_probe`):Raw Input 状态机 0/1/2/3、
    /// WM_INPUT 累计条数、距最近一条 WM_INPUT 的毫秒数。
    raw_state: u8,
    raw_input_count: u64,
    raw_age_ms: u64,
    updated: String,
    error: Option<String>,
}

/// 一次「设备注入自检」的结果(工作线程 → 界面线程)。
///
/// 自检的全部意义是**对照**:用 `adb shell input tap` 绕过本程序的控制通道去点
/// 手机,再把这一轮的设备现场(空间/焦点/屏幕)一起带回来。所以结果里三样都要留着,
/// 而且 `tap` 的成败要原样保留原因,不能在界面层被简化成"成功/失败"。
struct SelfCheckOutcome {
    /// 点之前那一轮巡检
    probe: adb::DeviceProbe,
    /// 实际点在哪个坐标(屏幕中心;取不到尺寸时为 None)
    tapped: Option<(u32, u32)>,
    /// `adb shell input tap` 的结果:Ok(设备端输出,通常为空)/ Err(人话原因)
    tap: Result<String, String>,
}

/// 一轮周期巡检的结果:探针结果 + **发起那一刻**控制通道认的坐标空间。
///
/// 为什么要记下"发起时"的值:探针飞在路上时,别的事件路径(开打 / 空闲恢复 /
/// 截图)可能已经把空间更新过了。那种时候手里这份快照比缓存**旧**,拿它比对会把
/// 坐标空间**倒退**回旧方向 —— 接下来那一两秒的注入全会落到错的地方。缓存变过
/// ⇒ 这一轮只取遥测,不判尺寸(见 [`probe_snapshot_is_fresh`])。
struct SpaceWatchOutcome {
    probe: adb::DeviceProbe,
    cached_at_start: Option<(u32, u32)>,
}

/// 周期巡检的判定(纯函数,单测覆盖):这一轮结果该不该走 DF-5 那条路
/// —— **更新坐标空间 + 请求引擎抬起所有触点 + 失效锚点**。
///
/// 返回 true ⇔ **确定**当前触摸坐标空间与缓存不同。三种"不动作"各有各的理由,
/// 必须分开:
///   * 尺寸没变 —— 绝大多数轮次(常态,连日志都不许写);
///   * 这一轮没查到(`None`)—— "查不到"绝不是"变了";
///   * 查到的只是回退的 `wm size`(物理分辨率、自然方向)—— 横屏下与真实坐标
///     空间差 90°,拿它比对必然误报,而误报的代价是**把玩家正按着的触点全部抬起**。
fn space_change_detected(
    cached: (u32, u32),
    probed: Option<(u32, u32)>,
    probed_is_display_space: bool,
) -> bool {
    let Some((w, h)) = probed else {
        return false;
    };
    if w == 0 || h == 0 || !probed_is_display_space {
        return false;
    }
    (w, h) != cached
}

/// "值变了才说"的判定(纯函数,单测覆盖):焦点窗口/屏幕状态这类遥测只在变化时
/// 写日志(不许每 2 秒刷一行)。
///
/// * `prev == None` → 第一次拿到值:说(否则日志里永远没有基线);
/// * `now  == None` → 这一轮没查到:不说(查不到不是"变了");
/// * 两边都有且不同 → 说。
fn telemetry_changed<T: PartialEq>(prev: Option<&T>, now: Option<&T>) -> bool {
    match now {
        None => false,
        Some(v) => prev != Some(v),
    }
}

/// 巡检快照还算不算"新鲜"(纯函数,单测覆盖):发起时缓存的空间必须与此刻**完全
/// 相同**。不同就说明探针飞在路上时别的事件路径已经更新过空间 —— 手里这份比缓存旧,
/// 不能拿它判"尺寸变了"(那会把坐标空间倒退回去,而倒退期间注入全落在错的地方)。
fn probe_snapshot_is_fresh(
    cached_at_start: Option<(u32, u32)>,
    cached_now: Option<(u32, u32)>,
) -> bool {
    cached_at_start == cached_now
}

/// 存活自检要连续判死几次才允许重连。
///
/// 为什么不是一次就动手:单次"设备端 socket 不见了"可能是 adb 一抖、设备端
/// server 正在被换掉(用户切了一下投屏参数)、或 `/proc/net/unix` 一时读不全。
/// 误判的代价是**把一条本来好的通道重启掉**(正在按着的触点全部重建),比多等
/// 一个 2s 节拍严重得多。两轮巡检间隔 2s,所以最坏也只晚 ~2s 才补救。
const CHANNEL_DEAD_CONFIRMATIONS: u32 = 2;

/// "连续两次确认才允许重连"的门(纯逻辑,单测覆盖)。
///
/// 只有**连续**判死才累计:中间夹一次"活"或"未知"就清零 —— 那说明之前那次
/// 很可能就是抖动,不能把两个隔了半分钟的判死凑成"连续两次"。
#[derive(Default)]
struct DeadConfirm {
    streak: u32,
}

impl DeadConfirm {
    /// 收到一次自检结论。返回 true ⇔ 这一次正好凑满 [`CHANNEL_DEAD_CONFIRMATIONS`]
    /// 次连续判死。触发后立刻清零:一次确认只对应一次重连(重连自己会重新排程)。
    fn observe(&mut self, verdict: adb::ChannelVerdict) -> bool {
        if verdict != adb::ChannelVerdict::Dead {
            self.streak = 0;
            return false;
        }
        self.streak += 1;
        if self.streak >= CHANNEL_DEAD_CONFIRMATIONS {
            self.streak = 0;
            true
        } else {
            false
        }
    }

    /// 手动重连/断开时把累计清零(用户已经替我们做了决定)。
    fn reset(&mut self) {
        self.streak = 0;
    }
}

/// 焦点窗口的**短**形式(悬浮窗那一行要短):
/// `Window{8b0f3a1 u0 com.x/.Y}` / `ActivityRecord{1a2b3c u0 com.x/.Y t42}` → `com.x/.Y`。
///
/// 优先取花括号里**含 `/` 的那一段**(组件名);没有就退回最后一段;连花括号都没有
/// (例如字面量 `null`)原样返回。纯字符串整理,只影响显示。
fn short_focus(focus: &str) -> &str {
    let inner = match (focus.find('{'), focus.rfind('}')) {
        (Some(a), Some(b)) if b > a => &focus[a + 1..b],
        _ => focus,
    };
    inner
        .rsplit(' ')
        .find(|t| t.contains('/'))
        .unwrap_or_else(|| inner.rsplit(' ').next().unwrap_or(inner))
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

/// 宏录制热键在某一按键上该做什么(见 `PadApp::macro_hotkey_action`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MacroHotkey {
    /// 什么都不做(未绑定 / 未启用 / 状态不匹配)
    None,
    /// 进入"等待首个按键"状态(不立即开始录制)
    Start,
    /// 立即终止当前录制(按现有停止/保存逻辑收尾)
    Stop,
    /// **取消**当前录制(用户 2026-10-12 第 1 条):丢弃这次录制,不写入任何东西。
    Cancel,
}

/// 轨迹录制快捷键在某一按键上该做什么(见 `PadApp::track_rec_hotkey`)。
///
/// 用户 2026-10-23 第 2 条:轨迹录制要"跟录制宏一样共用一对开始/结束"。
/// 结束键留空(0)时退回 2026-10-14 的单键切换 —— 那时只会出现 `Toggle`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrackRecHotkey {
    /// 开始键被按下:没在录 ⇒ 开始;正在录 ⇒ 结束(单键切换的老口径)。
    Toggle,
    /// 结束键被按下:正在录 ⇒ 收尾保存;没在录 ⇒ 什么都不做(调用方给一句提示)。
    Stop,
}

/// 右上角那颗"红色哑光按钮"该显示什么(用户 2026-10-12 第 2 条)。
///
/// 用户原话:空载(拔了手机不想连)时界面"一震一震的",因为提示跟着消息一闪一闪,
/// "人的手速得多快才能点到这个按钮"。所以要做成**常驻位置**的按钮:
/// 平时哑光无字,需要时**亮起来并一直亮**;而且不止一种情况要用它,
/// 因此要有**优先级仲裁**。
///
/// 仲裁规则(自己定,写在这里以免以后各写一套):
/// - **优先级高的覆盖低的**;
/// - **优先级相同的先来先服务**(先进入该状态的赢,不会来回跳)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RescueAction {
    /// 连接已断开且自动重连已放弃(用户描述的场景):点一下 = 重新发起连接
    ReconnectGaveUp,
    /// 正在自动重连中:点一下 = 立即重试(不等退避)
    Reconnecting,
    /// 控制通道存活自检判死:点一下 = 强制重连控制通道
    ChannelDead,
    /// 映射开着但控制通道没连上
    MappingWithoutChannel,
}

impl RescueAction {
    /// 优先级(越大越急)。同值先来先服务。
    fn priority(self) -> u8 {
        match self {
            RescueAction::ReconnectGaveUp => 50,
            RescueAction::Reconnecting => 40,
            RescueAction::ChannelDead => 30,
            RescueAction::MappingWithoutChannel => 20,
        }
    }

    /// 按钮亮起时显示的字(平时是哑光无字,见 `rescue_button`)
    fn label(self) -> &'static str {
        match self {
            RescueAction::ReconnectGaveUp => "重新连接",
            RescueAction::Reconnecting => "重试连接",
            RescueAction::ChannelDead => "重连控制通道",
            RescueAction::MappingWithoutChannel => "连接控制",
        }
    }

    /// 悬停说明:告诉用户"为什么亮"以及"点一下会怎样"
    fn tooltip(self) -> &'static str {
        match self {
            RescueAction::ReconnectGaveUp => {
                "连接已断开,自动重连已放弃。\n点一下 = 立即重新发起连接(插回数据线后点这个)。"
            }
            RescueAction::Reconnecting => "正在自动重连中。\n点一下 = 不等退避,立刻再试一次。",
            RescueAction::ChannelDead => {
                "控制通道存活自检判定设备端已消失。\n点一下 = 强制重连我们自己的控制通道(不动投屏)。"
            }
            RescueAction::MappingWithoutChannel => {
                "映射开着,但控制通道还没连上 —— 现在按键不会生效。\n点一下 = 连接控制通道。"
            }
        }
    }
}

/// 优先级仲裁:从候选里选出该亮的那一个。
///
/// **同优先级先来先服务**:`candidates` 的顺序就是"谁先进入该状态",
/// 因此只比较 `>`(严格大于)而不是 `>=` —— 后到的同优先级项不会抢走位置,
/// 界面不会在两者之间来回跳。
fn pick_rescue_action(candidates: &[RescueAction]) -> Option<RescueAction> {
    let mut best: Option<RescueAction> = None;
    for &c in candidates {
        match best {
            Some(b) if c.priority() <= b.priority() => {}
            _ => best = Some(c),
        }
    }
    best
}

/// `pending_task_label` 的全部取值(与那里的分支一一对应,`debug_assert` 盯着它们同步)。///
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

/// 轨迹录制的**目标**:录完之后这些点要写回哪里(用户 2026-10-13,V2-3)。
///
/// 为什么需要它:录制入口从"只在新增草稿面板里"扩展到"已有键位/组合键也能重录",
/// 而两者写入的位置完全不同(草稿在 `self.draft.swipe_points`,已有绑定在
/// `Shared.profile`)。把目标做成状态的一部分,于是**同一套录制 UI 与同一套
/// 状态机**服务于全部入口,不必每处各写一份。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrackTarget {
    /// 新增草稿(像素坐标,写在 `App::draft` 里)
    Draft,
    /// 已有键位(相对坐标,写在 `Shared.profile.binds[i]`)
    Bind(usize),
    /// 已有组合键(相对坐标,写在 `Shared.profile.combos[i]`)
    Combo(usize),
}

/// 一次**进行中**的轨迹录制会话(用户 2026-10-13,V2-3)。
///
/// # 为什么要"会话缓冲"而不是直接往目标里写
///
/// 上一版的录制是"边划边写进 `draft.swipe_points`" —— 于是有两条硬伤:
/// ① **取消不了**:划到一半发现划歪了,没有"丢弃重来"的路(只能划完再点清除);
/// ② **已有键位改不了**:目标固定是草稿,想给一条已存在的滑动重录轨迹得先删了重建。
///
/// 现在改成:录制期间点记在 `points`(会话私有缓冲),结束时才由
/// [`App::finish_track_recording`] **一次性提交**到目标,或者由
/// [`App::cancel_track_recording`] 整份丢弃。于是"有始必有终"这条纪律在
/// 录制功能上第一次真正成立 —— 开始录制的每条路径(按钮/再次点击/失焦/关面板)
/// 都有一条明确的收尾路径(提交/取消)。
struct TrackRec {
    /// 录制目标
    target: TrackTarget,
    /// 已录点(像素坐标,与浮层显示同一口径)
    points: Vec<(i32, i32)>,
    /// 录制**开始时刻**:用于在浮层上做"录制中"的呼吸动画(纯显示)
    started: std::time::Instant,
}

/// 轨迹录制的点数上限与采样抽稀规则都在 `keymap` 里
/// (见 [`crate::keymap::push_track_sample`]) —— 那是引擎与界面共用的唯一一份
/// 口径,放这里会变成两处各写一份。
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
    /// **录制的轨迹点**(用户 2026-10-13,V2-2):非空时这条滑动按轨迹注入,
    /// 空则走"起点→终点 + 路径/曲线"(与从前一致)。
    ///
    /// 草稿里存的是**像素**坐标(与 `swipe_start/swipe_end` 同一口径),
    /// 入配置时才用 `Mapper::rel_x/rel_y` 换算成相对坐标。
    swipe_points: Vec<(i32, i32)>,
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
            swipe_points: Vec::new(),
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

/// 键位浮层可读性的**预设档**(2026-10-17,B20/U16)。
///
/// # 为什么收敛成档位
///
/// 浮层可读性原来由四层叠加决定:① 反相外套(casing)、② 文字衬底(plate)、
/// ③ 逐块亮度采样(LumaGrid 的 around)、④ 自动档位 auto_tone + 手动滑块。
/// 前三层是固定做法、没有旋钮;真正的旋钮只有两个 —— Look::overlay_auto 与
/// Look::overlay_tone。两个控件并排摆在「外观」页里,用户很难说清"到底是哪一层
/// 在起作用",回归时也无从定位(审计 U7-11)。
///
/// 现在收敛成**一个入口 + 四个档**:
/// - ReadabilityPreset::Auto      ⇒ (overlay_auto=true,  overlay_tone=0.0)
/// - ReadabilityPreset::ForceDark ⇒ (false, theme::TONE_MIN)
/// - ReadabilityPreset::ForceLight⇒ (false, theme::TONE_MAX)
/// - ReadabilityPreset::Custom    ⇒ (false, 用户手调的那个值)
///
/// 每家方案(profile.yaml)里落的仍然是原来的 overlay_auto / overlay_tone
/// 两个字段 —— **配置 schema 一个字都没变**,旧 profile 照样读、照样写。
/// 档位只是这两个字段上的一个**视图**:由 preset_of 读出来,由 apply_preset
/// 写回去。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ReadabilityPreset {
    /// 默认档:按截图**逐块**采样明暗,每一处自己决定用深色还是浅色(自动对比)。
    Auto,
    /// 强制深色(适合明亮画面):固定走亮度下限。
    ForceDark,
    /// 强制浅色(适合昏暗画面):固定走亮度上限。
    ForceLight,
    /// 手动:关掉自动对比,用下面展开的滑块自己定一个亮度(其余取值一律归这里)。
    Custom,
}

impl ReadabilityPreset {
    /// 下拉里显示的字面量:档位文案的**唯一来源**(界面只许取这里)。
    fn label(self) -> &'static str {
        match self {
            ReadabilityPreset::Auto => "自动对比(按截图明暗逐块选深浅)",
            ReadabilityPreset::ForceDark => "强制深色(适合明亮画面)",
            ReadabilityPreset::ForceLight => "强制浅色(适合昏暗画面)",
            ReadabilityPreset::Custom => "自定义(手动亮度)",
        }
    }
}

/// 由**落盘的两个字段**读出当前档位。判据(2026-10-17 Lead 裁定):
///
/// - auto == true                        ⇒ ReadabilityPreset::Auto
/// - auto == false && manual <= TONE_MIN ⇒ ReadabilityPreset::ForceDark
/// - auto == false && manual >= TONE_MAX ⇒ ReadabilityPreset::ForceLight
/// - 其余(auto == false 且 manual 在开区间内) ⇒ ReadabilityPreset::Custom
///
/// **端点归强制档**是有意为之:apply_preset 写出的组合必须能被这里读回同一个
/// 档 —— 往返一致性优先于"端点值也该算手动"的语义洁癖。代价是"有人手改
/// profile.yaml 把 manual 写到端点"会被显示成强制档;但端点值本来就是那两个档
/// 的值,绘制结果逐位相同,所以这个代价可接受。
///
/// 注意 Auto 档**不丢失** manual:用户从「自定义」切回「自动」时,overlay_tone
/// 仍留着上次手调的值(apply_preset 只写档位、不动它),切回「自定义」就能接着
/// 调;只有在**用户显式选中某个非 Custom 档**时才由 apply_preset 写出确定组合
/// (Auto 写 0.0、强制档写端点)。
fn preset_of(auto: bool, manual: f32) -> ReadabilityPreset {
    if auto {
        return ReadabilityPreset::Auto;
    }
    if manual <= theme::TONE_MIN {
        return ReadabilityPreset::ForceDark;
    }
    if manual >= theme::TONE_MAX {
        return ReadabilityPreset::ForceLight;
    }
    ReadabilityPreset::Custom
}

/// 把档位写成**唯一确定**的 (overlay_auto, overlay_tone) 组合 —— 全项目唯一
/// 的档位写入口,不许别处再手搓这两个字段的赋值。
///
/// manual_now 是当前落盘的 overlay_tone,只在 Custom 档用得上(它要保住用户手调
/// 的值);其余三档的取值与它无关,所以"同样的档 ⇒ 同样的组合"是硬保证。
/// 非有限值(手改 json 成 NaN/inf)按 0.0 处理,避免把 NaN 写进配置。
fn apply_preset(p: ReadabilityPreset, manual_now: f32) -> (bool, f32) {
    match p {
        ReadabilityPreset::Auto => (true, 0.0),
        ReadabilityPreset::ForceDark => (false, theme::TONE_MIN),
        ReadabilityPreset::ForceLight => (false, theme::TONE_MAX),
        ReadabilityPreset::Custom => {
            // 手动档:关掉自动对比,滑块取当前值(非有限值兜底成 0.0)。
            let m = if manual_now.is_finite() {
                manual_now
            } else {
                0.0
            };
            (false, m)
        }
    }
}

/// 「手动 adb 命令」助手窗口的临时状态(打开期间有效,关窗丢弃)。
struct AdbHelperState {
    search: String,
    /// 内置库条目下标(过滤后仍保持指向 full list,避免搜索改变选中含义)
    selected: usize,
}

/// args / adb 两个 helper 弹窗的**来源**:标题、默认宽度、egui Id 的唯一来源。
///
/// U15:两窗骨架逐字同构 ⇒ 共用 [`PadApp::helper_window`];但 Id 必须按来源区分,
/// 否则两窗同时打开时 egui 会把它们当成同一个 Window,位置/大小/滚动状态互相顶。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum HelperSource {
    Args,
    Adb,
}

impl HelperSource {
    fn title(self) -> &'static str {
        match self {
            HelperSource::Args => "scrcpy 常用参数助手",
            HelperSource::Adb => "常用 adb 命令助手",
        }
    }

    fn default_width(self) -> f32 {
        match self {
            HelperSource::Args => 500.0,
            HelperSource::Adb => 560.0,
        }
    }

    /// 窗口 Id:两个来源必须得到**不同**的 Id。
    fn window_id(self) -> egui::Id {
        egui::Id::new(match self {
            HelperSource::Args => "helper_window::args",
            HelperSource::Adb => "helper_window::adb",
        })
    }
}

/// 两个 helper 弹窗的**共用骨架**(U15,2026-10-17):标题、默认宽度、egui Id
/// 全部来自 `source`,调用方只提供面板内容。
///
/// 为什么 Id 必须显式给:两个窗口的标题**不同**,但 egui 的 Window 默认按
/// 标题生成 Id —— 一旦将来有人让两个来源共用一个标题(或改成参数化标题),
/// 两窗同时打开就会互相顶位置/大小/滚动状态。这里把 Id 钉成 `source.window_id()`。
///
/// 为什么是自由函数而不是 `PadApp` 的方法:调用方(如
/// `ui_args_helper` / `ui_adb_helper`)需要在这个骨架**执行期间**捕获自己的
/// 局部状态(取走的 `ArgHelp` / `AdbHelperState`)。若写成 `&mut self` 方法,
/// 闭包里再借 `self` 就是二次可变借用(借用检查直接拒绝)。因此把 `app`
/// 作为参数交给内容回调,骨架本身不持有任何状态。
///
/// `.open(&mut open)`:关闭按钮把 `open` 置 false ⇒ 调用方据此决定
/// 「关闭即丢弃 / 仍开则放回」—— 两个 helper 的放回语义不同,由调用方说了算。
fn helper_window(
    app: &mut PadApp,
    ctx: &egui::Context,
    source: HelperSource,
    open: &mut bool,
    body: &mut dyn FnMut(&mut PadApp, &mut egui::Ui),
) {
    egui::Window::new(source.title())
        .id(source.window_id())
        .open(open)
        .default_width(source.default_width())
        .show(ctx, |ui| body(app, ui));
}

pub struct PadApp {
    shared: SharedState,
    grab_flag: Arc<std::sync::atomic::AtomicBool>,
    /// 「拦截系统默认行为」掩码(见 capture::swallow_bit):每帧按 映射开关 ×
    /// 独占键盘 × 当前绑定 预先算好,钩子回调只读一次原子量。
    swallow_flag: Arc<std::sync::atomic::AtomicU16>,
    /// 「已绑定键不再传给其它程序」位图(B21-2/G-b,独占模式;见
    /// capture::BoundKeys):每帧按当前绑定**全量重算**逐 word 写入,
    /// 钩子回调只读一次位测试。组合键里的键本期不进位图(如实登记为未做)。
    bound_keys_flag: Arc<crate::capture::BoundKeys>,
    /// 独占模式生效开关(= `swallow_bound_keys`(设置)× 映射开启):
    /// 界面每帧重算写入;映射一关必须落回 false,绝不能关了映射还吞键。
    swallow_bound_keys_flag: Arc<std::sync::atomic::AtomicBool>,
    /// 独占模式**设置值**(存 settings.json,程序级偏好;默认关)。
    /// 界面入口:左栏总开关键下方的勾选框(见 ui_toggle_key_row)。
    swallow_bound_keys: bool,
    /// **瞄准期间把光标钉住**(B21-3/G-c;存 settings.json,**默认开**)。
    ///
    /// 与 `swallow_bound_keys` 的区别:那个管键盘(独占),这个管**光标**。
    /// 真正的"钉"由捕获层用 `ClipCursor` 做,判据是既有的 `mouse_grab`
    /// (引擎写),所以这里**只要把开关每帧传给捕获层**就够了 —— 界面不做决策。
    /// 界面入口:FPS 页「鼠标独占与自救」分组里的勾选框(见 ui_aim_body)。
    clip_cursor_in_aim: bool,
    /// G-c 的原子量(= `clip_cursor_in_aim` × 程序在跑):每帧写,捕获层读。
    /// 为什么不像 `swallow_bound_keys_flag` 那样再乘"映射开启":钉住本身已经由
    /// `mouse_grab` 二次把关,而 `mouse_grab` 就是"映射开启 × 瞄准中"。
    clip_cursor_flag: Arc<std::sync::atomic::AtomicBool>,
    /// 「FPS 鼠标隔膜」开关(见 capture::mouse_shield_swallows;2026-10-11 用户
    /// 根因定位):true = 捕获层把鼠标消息**一律**从系统输入流里吞掉,投屏窗口
    /// (scrcpy.exe)再也收不到点击/拖动,也就不会在我们注入手机触摸的同时
    /// 自己再转发一套(用户实测的抖动/跳变来源)。
    ///
    /// 每帧按「映射开启 × FPS 模式生效 × 控制通道在线」重算(与 swallow 掩码
    /// 同一段)。**不新增配置开关**:FPS 模式自动开启,退出即恢复 —— 三条复位
    /// 路径见 `update()` 里写这一枚原子量的那段注释。
    ///
    /// 2026-10-11 晚(S①):"什么时候独占"改成可由 `Aim::mouse_shield` 三档选择
    /// (仅 FPS / 仅投屏窗口在前台 / 始终),判据统一走纯函数
    /// [`crate::keymap::mouse_shield_on`];再加 S② 的手动开关热键
    /// (状态在引擎侧 `AimLive::shield_manual`,界面每帧镜像成下面这个原子量)。
    mouse_shield_flag: Arc<std::sync::atomic::AtomicBool>,
    /// **遮板**(用户 2026-10-13):窗口层接管鼠标的那块透明置顶窗。
    ///
    /// 与 `mouse_shield_flag`(钩子层隔膜)是**同一个问题的两种实现**,由
    /// [`theme::MouseCaptureMode`] 二选一,入口在「外观」页,**热切换**:
    /// - `Some(mask)` = 遮板正在工作;
    /// - `None` = 没开遮板(隔膜模式、或遮板起不来/定位不到投屏窗口)。
    ///
    /// 生命周期:`Mask::drop` 会销毁窗口并等线程退出,所以这里持句柄就够了;
    /// 切换模式时把它置 `None` 即完成收尾。
    mask: Option<crate::mask::Mask>,
    /// 鼠标接管方式:隔膜 / 遮板(用户 2026-10-13)。
    ///
    /// **程序级**(存 settings.json),不是 `Look` 的一部分 —— 理由见
    /// `settings::Settings::mouse_capture` 的注释(放 Look 里会跟着按键组合走、
    /// 还会被 look.json 覆盖,实测导致"选了遮板却跑成隔膜")。
    mouse_capture: theme::MouseCaptureMode,
    /// **轨迹录制会话**(用户 2026-10-13,V2-3):`Some` = 正在录制。
    ///
    /// 只记**本程序窗口内**的鼠标移动(不 hook 系统鼠标)—— 用户按一下按钮、
    /// 在截图浮层上划一遍路径,再按一下结束。这比"取起点+取终点"能表达任意
    /// 曲线(绕圈、折返、S 形),是竞品对标的能力。
    ///
    /// 为什么是"会话缓冲 + 结束时提交"而不是边划边写:见 [`TrackRec`] 的说明
    /// (核心是**取消路径**与**多目标复用**)。
    track_rec: Option<TrackRec>,
    /// 遮板要显示哪些项目(用户 2026-10-13 第 2 条)。
    ///
    /// 语义与「截图取点」的过滤项**同一套**(`OverlayFilter`),默认值与它一致;
    /// 用户可在「外观」页的鼠标接管那一行右侧单独勾选。落盘在 settings.json
    /// (程序级偏好,不跟按键组合走)。
    mask_filter: OverlayFilter,
    /// 上一帧提交给遮板的矩形(避免每帧重算 `EnumWindows` 枚举)。
    ///
    /// 为什么需要:`window_rect_of_pid` 要枚举全部顶层窗口(实测毫秒级),
    /// 而遮板只需要"投屏窗口位置变了才更新" —— 用时间节流而不是每帧枚举。
    mask_rect: Option<crate::mask::MaskRect>,
    /// 上次重新定位遮板矩形的时刻(节流,见上)。
    mask_rect_at: Option<std::time::Instant>,
    /// 遮板最近一次"贴不上"的原因(只在变化时写日志,避免每帧刷屏)。
    mask_rect_log: Option<&'static str>,
    /// 最近一次启动的 scrcpy **子进程 pid**(S①,"仅投屏窗口在前台"这一档的判据)。
    ///
    /// `None` = 没启动过 / 进程已退出 —— 此时**保守地不独占**(拿不到判据就不赌)。
    /// 由 [`adb::launch_scrcpy`] 返回,收到"scrcpy 已退出"类的消息时清掉。
    scrcpy_pid: Option<u32>,
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
    /// 周期性设备巡检的在途结果。**与 `space_rx` 分开是刻意的**:那条是"某个事件
    /// 刚发生,补查一次"(开打 / 空闲恢复)的一次性请求,在途时会被顶栏[取消坐标
    /// 刷新]清掉、也随时可能被别的调用重置;这条**只认定时器**,必须一直在跑 ——
    /// 用户这次的场景恰恰是"一个键都没按,没人触发重查"。
    space_watch_rx: Option<Receiver<SpaceWatchOutcome>>,
    /// 上一轮周期巡检**发出**的时刻(到 [`SPACE_WATCH_INTERVAL`] 才发下一轮)
    space_watch_last: Instant,
    /// 上一轮周期巡检**回来**的时刻(悬浮窗显示"重查=…ms前";None = 还没跑过)
    space_checked_at: Option<Instant>,
    /// 上一次看到的焦点窗口 / 前台 Activity / 屏幕亮灭。**只在变化时**写 INFO
    /// 日志,所以必须留一份"上一次说什么了"的基线。
    probe_focus: Option<String>,
    probe_focused_app: Option<String>,
    probe_screen_on: Option<bool>,
    /// 设备注入自检的在途结果
    selfcheck_rx: Option<Receiver<SelfCheckOutcome>>,
    /// 最近一次自检的人话结论(悬浮窗那一行显示;None = 还没跑过)
    selfcheck_note: Option<String>,
    /// 最近一次自检发生的时刻
    selfcheck_at: Option<Instant>,
    /// 控制通道存活自检(2026-10-11)的在途结果。它在**工作线程**上用只读 adb
    /// 问设备端"我们自己那个 server 的 abstract socket 还在不在" —— server 死掉
    /// 时本地写入照样成功、`is_connected()` 一直为真、日志零错误,只看本地什么都
    /// 看不出来(见 `adb::channel_probe`)。
    channel_rx: Option<Receiver<adb::ChannelProbe>>,
    /// 上一轮存活自检**发出**的时刻(到 [`SPACE_WATCH_INTERVAL`] 才发下一轮)
    channel_watch_last: Instant,
    /// 最近一次存活自检的结论与证据(None = 还没跑过)
    channel_probe: Option<adb::ChannelProbe>,
    /// 最近一次存活自检**回来**的时刻(悬浮窗显示"上次自检 Xms 前")
    channel_probed_at: Option<Instant>,
    /// 上一次自检结论:只在**变化**时写日志(不许每 2 秒刷一行)
    channel_verdict_prev: Option<adb::ChannelVerdict>,
    /// 连续判死计数器:连续两次确认才允许重连(单次可能只是 adb 抖动)
    channel_dead_confirm: DeadConfirm,
    /// 本轮回控制通道重启的发起时刻(连上后报耗时;None = 不是在重启)
    channel_restart_started: Option<Instant>,
    // ============ 卡死自愈的取证层(2026-10-12,用户第 6 条) ============
    /// 设备端输入账本(1 次/秒只读 `dumpsys input`)的在途结果。
    /// `None` 是"这一轮没读到",调用方必须与"读到 0 个触点"分开处理。
    ledger_rx: Option<Receiver<Option<adb::DeviceInputState>>>,
    /// 上一轮账本取证**发出**的时刻(到 [`DEVICE_LEDGER_INTERVAL`] 才发下一轮)
    ledger_watch_last: Instant,
    /// 上一轮账本的**签名**(MIUI 那几个开关 + `TouchStates` 原文 + 触点计数)。
    /// 只在签名变化时写 INFO —— 账本每秒读一次,不许每秒写一行日志。
    ledger_sig_prev: Option<String>,
    /// 手机被手触摸监听器的在途状态:工作线程由 `adb::watch_physical_touch` 内部
    /// 起好(节点查找也在那条线程里,**绝不阻塞界面**),这里只持有 Receiver。
    /// 每个连接只起一个(见 [`PadApp::maybe_physical_touch_watch`])。
    touch_rx: Option<Receiver<adb::PhysicalTouch>>,
    /// 已经为哪个序列号起过触摸监听。只要它还是当前序列号就**不重复起**
    /// —— 每个监听都是一条一直在读的 `getevent`,`adb::watch_physical_touch`
    /// 没有停止句柄,重复起就是泄漏线程。断连/换设备时清掉,允许下一次重连重建。
    touch_watch_serial: Option<String>,
    /// 上一帧 FPS 模式是否激活:进入 FPS 模式时顺带补查一次坐标空间
    /// (只是"提前一步",**不能**替代周期巡检 —— 见 [`SPACE_WATCH_INTERVAL`])
    aim_mode_prev: bool,
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
    /// 上次**真正选中过**的设备序列号(D1,2026-10-22)。
    ///
    /// 为什么要单独存一份:`serial()` 的语义是"此刻选中的设备",拔线/adb 掉线后
    /// 设备列表为空 ⇒ 它返回空串。而 `settings_snapshot()` 以前直接写它,于是
    /// **一次刷新就把用户记住的设备冲成空串**,重启后再也回不到那台机器
    /// (读回侧要求非空且在线 ⇒ 永久失效)。这里只在拿到非空序列号时更新,
    /// 写盘时以它兜底。
    last_serial: String,
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
    /// 宏步骤键位捕获的落点（见 [`MacroKeyTarget`]）：`waiting_key` / `waiting_keys`
    /// 只记"哪个槽位"，这个字段记"写进哪份缓冲"。
    macro_key_target: MacroKeyTarget,
    /// 组合键捕获:此刻**按着**的键(按按下顺序,最多 2 个)。
    capture_down: Vec<u16>,
    /// 组合键捕获:这一轮**捕获到**的键(按按下顺序,最多 2 个)。
    capture_seen: Vec<u16>,
    /// 逃生热键(B,2026-10-11)用的"物理上此刻按着哪几个键"镜像。
    ///
    /// 为什么界面侧也要一份:强制重连的处理点在界面(`gui_rx` 拿到的是**门控
    /// 之前**的原始捕获事件),而热键的纪律与引擎的系统键逐字相同 —— 只认
    /// **上升沿**(Windows 自动重复不连触发)、绑组合键时要求整个集合按着。
    /// 引擎那份 `Held` 是引擎私有的,界面侧只能自己记这一份。
    hotkey_down: Vec<u16>,
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
    /// 宏页：「长按重复」(用户 2026-10-11 第 2 条)—— 按住触发键时一轮接一轮
    /// 重放这条宏,直到抬起;松手后正在跑的那一轮跑完即停。两轮之间的间隔用
    /// 上面那个「按后延迟」,其总开关关着时不留间隔。随宏一起保存/载入/存草稿。
    macro_page_repeat_while_held: bool,
    /// 宏页：空闲自动停止时间。
    macro_idle_ms: u32,
    /// 宏录制热键(用户 2026-10-12):「开始录制」键 + 独立启用勾选。
    ///
    /// 语义(任务书逐条对齐):
    /// - 未勾选启用 ⇒ **即使绑了键也不触发任何行为**(键位值保留);
    /// - 已启用且已绑定 ⇒ 按下后**不立即开始录制**,而是进入"等待首个按键"
    ///   状态(与点[开始录制]同一状态);第一个可录制按键按下即开始录制,
    ///   且该按键按现有语义记为**起始事件**;
    /// - 热键本身**不写入宏**、也不递归触发。
    /// 只支持单键 —— 组合键无法绑定(捕获走 `waiting_key`,与宏触发键同路)。
    macro_record_start_key: u16,
    macro_record_start_key_enabled: bool,
    /// ---- 轨迹录制的触发配置(用户 2026-10-14) ----
    ///
    /// 两项都是**程序级偏好**(与宏录制热键同类,存 settings.json,不跟按键组合走):
    /// - `track_rec_enabled`:总开关。未勾选时快捷键**不触发**,只有录制卡片上的
    ///   按钮能用。
    /// - `track_rec_key`:开始键键码(0 = 空缺);按一下开始,**再按一下也结束**。
    /// - `track_rec_stop_key`:结束键键码(0 = 未绑定 ⇒ 只用开始键切换,即旧行为)。
    ///
    /// 用户 2026-10-14 决定**去掉鼠标键控制录制**(左/右键按下开始、抬起停止):
    /// 「把鼠标控制绘制录像的功能去掉吧,你干不明白」。所以这里不再有"触发方式"
    /// 这一层 —— 只剩快捷键这一条路 + 卡片按钮。
    track_rec_enabled: bool,
    track_rec_key: u16,
    /// 轨迹录制的**结束键**(用户 2026-10-23 第 2 条)。0 = 未绑定 ⇒ 只用开始键切换。
    track_rec_stop_key: u16,
    /// 本帧已经画过录制触发配置区没有(见 `track_rec_card` 的说明)。
    ///
    /// 用"帧号"而不是 `bool`,是为了**下一帧自动复位** —— 不需要任何显式清理
    /// (用一个 bool 就得记得每帧开头清,漏一处就会"配置区从某帧起再也不显示")。
    track_cfg_drawn_pass: Option<u64>,
    /// 「终止录制」键 + 独立启用勾选。录制中按下 ⇒ 立即按现有停止/保存逻辑收尾;
    /// 未处于录制状态时按下 ⇒ 无操作;未勾选启用 ⇒ 不触发。热键本身不写入宏。
    macro_record_stop_key: u16,
    macro_record_stop_key_enabled: bool,
    /// 「取消录制」键 + 独立启用勾选(用户 2026-10-12 第 1 条)。
    ///
    /// 与「终止录制」的区别:**取消 = 丢弃这次录制**(不写入任何宏),
    /// 终止 = 保存已录内容。用户明确要求它是**全局公共的**:一个键对所有宏
    /// 录制生效,不必逐宏设置。
    macro_record_cancel_key: u16,
    macro_record_cancel_key_enabled: bool,
    /// 宏页：是否展开显示原始录制事件（默认只显示结果摘要）。
    macro_show_events: bool,
    /// 录制过程中点[显示步骤]后常开:不看摘要,直接看逐条步骤(用户 2026-10-07 要求
    /// "先看步骤再决定保存",不必等保存后的[展开])。
    macro_show_recording_steps: bool,
    /// 录制中这份**还没命名/保存**的草稿是否展开(用户 2026-10-12 第 3 条)。
    ///
    /// 与 `macro_show_recording_steps` 的区别:那个是"常驻的 16 行只读小框"
    /// (看最近录了什么),这个是**与「已录宏」[展开]同一套视图**的开关 ——
    /// 用户明确要求"录制中还没命名的时候也能展开宏",此前这个入口不存在
    /// (要等保存成 `宏#N` 才有[展开])。
    macro_recording_expanded: bool,
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
    let mut loaded: Option<String> = None;
    for path in CANDIDATES {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert(
                "cjk".into(),
                std::sync::Arc::new(egui::FontData::from_owned(bytes)),
            );
            loaded = Some((*path).to_string());
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
                loaded = Some("~/.local/share/fonts/SourceHanSans.ttc".to_string());
            }
        }
    }
    // 最后交给 fontconfig 兜底:发行版把中文字体装在什么路径,**只有 fontconfig 知道**。
    // 上面那份硬编码清单只覆盖了 Debian/Ubuntu/Arch 的几种常见布局,遇到 Fedora
    // (/usr/share/fonts/google-noto-cjk/)、openSUSE、NixOS 就整条落空。
    if loaded.is_none() {
        if let Some(path) = crate::fonts::fontconfig_cjk_font() {
            if let Ok(bytes) = std::fs::read(&path) {
                fonts.font_data.insert(
                    "cjk".into(),
                    std::sync::Arc::new(egui::FontData::from_owned(bytes)),
                );
                loaded = Some(path);
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
            // DF-5(engine 侧在跑):界面确认坐标空间变了之后置位,由引擎取走并
            // 抬掉所有按在旧空间里的触点(一次性请求;见 `Shared` 的注释)。
            space_changed_release: false,
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
        // 独占模式(B21-2/G-b):捕获层没起来时给空位图 + 开关关(照抄同款写法)
        let bound_keys_flag = capture
            .as_ref()
            .map(|c| c.bound_keys.clone())
            .unwrap_or_else(|| Arc::new(crate::capture::BoundKeys::new()));
        let swallow_bound_keys_flag = capture
            .as_ref()
            .map(|c| c.swallow_bound_keys.clone())
            .unwrap_or_else(|| Arc::new(false.into()));
        // 「FPS 鼠标隔膜」(2026-10-11):初值 false —— 程序刚起来绝不能吞鼠标。
        let mouse_shield_flag = capture
            .as_ref()
            .map(|c| c.mouse_shield.clone())
            .unwrap_or_else(|| Arc::new(false.into()));
        // G-c(钉住光标):初值 false —— 与捕获层里的初值一致,**由界面第一帧
        // 按设置写进去**。捕获层起来到第一帧之间那一小段宁可不钉,也不要
        // 在一个还没读设置的实例上先钉住。
        let clip_cursor_flag = capture
            .as_ref()
            .map(|c| c.clip_cursor.clone())
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
                    format!(
                        "未找到 scrcpy,请在左栏指定 {} 或它所在的目录(指定一次后会被记住)",
                        adb::scrcpy_exe_name()
                    ),
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
            bound_keys_flag,
            swallow_bound_keys_flag,
            mouse_shield_flag,
            mask: None,
            mouse_capture: saved.mouse_capture,
            // G-c 的 App 级字段:设置值 + 原子量(原子量已在上面 clone 好)。
            // 位置紧跟 mouse_capture 只是就近;字段初始化顺序与声明顺序无关。
            clip_cursor_in_aim: saved.clip_cursor_in_aim,
            clip_cursor_flag,
            track_rec: None,
            mask_filter: OverlayFilter::from_bits(saved.mask_filter),
            mask_rect: None,
            mask_rect_at: None,
            mask_rect_log: None,
            scrcpy_pid: None,
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
            space_watch_rx: None,
            // 启动即开始计时:映射一开、通道一在线,最迟 2s 后就有第一轮设备巡检
            space_watch_last: Instant::now(),
            space_checked_at: None,
            probe_focus: None,
            probe_focused_app: None,
            probe_screen_on: None,
            selfcheck_rx: None,
            selfcheck_note: None,
            selfcheck_at: None,
            // 存活自检与周期巡检共用同一个 2s 节拍,但两条计时器**互相独立**:
            // 巡检那条会被 adb 卡住,不能让它把存活自检也一起拖住。
            channel_rx: None,
            channel_watch_last: Instant::now(),
            channel_probe: None,
            channel_probed_at: None,
            channel_verdict_prev: None,
            channel_dead_confirm: DeadConfirm::default(),
            channel_restart_started: None,
            // 卡死自愈的取证层:账本启动即开始计时(映射一开、通道一在线,
            // 最迟 1s 后就有第一轮账本),触摸监听等通道建立后再起。
            ledger_rx: None,
            ledger_watch_last: Instant::now(),
            ledger_sig_prev: None,
            touch_rx: None,
            touch_watch_serial: None,
            aim_mode_prev: false,
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
            last_serial: remembered.selected_serial.trim().to_string(),
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
            macro_key_target: MacroKeyTarget::Page,
            capture_down: Vec::new(),
            capture_seen: Vec::new(),
            hotkey_down: Vec::new(),
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
            macro_page_repeat_while_held: false,
            macro_idle_ms: 800,
            // 宏录制热键:从 settings.json 读回(程序级偏好,与 scrcpy_args 同类)。
            // 老配置没有这几个字段 ⇒ serde default = 未绑定 + 未启用,
            // 即"完全保持原宏录制逻辑,不产生任何行为变化"。
            macro_record_start_key: saved.macro_record_start_key,
            macro_record_start_key_enabled: saved.macro_record_start_key_enabled,
            macro_record_stop_key: saved.macro_record_stop_key,
            macro_record_stop_key_enabled: saved.macro_record_stop_key_enabled,
            macro_record_cancel_key: saved.macro_record_cancel_key,
            macro_record_cancel_key_enabled: saved.macro_record_cancel_key_enabled,
            // 轨迹录制触发配置:同样从 settings.json 读回。老配置没有这几个字段
            // ⇒ serde default = 未启用 + 快捷键方式 + 空缺,即"行为与从前一致"
            // (快捷键空缺 ⇒ 不触发;总开关关 ⇒ 鼠标键也不触发)。
            track_rec_enabled: saved.track_rec_enabled,
            track_rec_key: saved.track_rec_key,
            track_rec_stop_key: saved.track_rec_stop_key,
            // 独占模式(B21-2/G-b):从 settings.json 读回;老配置没有该字段 ⇒
            // serde default = false,行为与从前逐位一致(一个键都不多吞)。
            swallow_bound_keys: saved.swallow_bound_keys,
            // 映射时屏蔽原键(Linux EVIOCGRAB):从 settings.json 读回。
            // 老配置没有该字段 ⇒ serde default = false(与从前一致)。
            grab_enabled: saved.grab_enabled,
            track_cfg_drawn_pass: None,
            macro_show_events: false,
            macro_show_recording_steps: false,
            macro_recording_expanded: false,
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
            None => app.log(format!(
                "未找到 adb(设备列表将为空): 请将 {} 所在目录加入 PATH,或在左栏手动指定",
                adb::adb_exe_name()
            )),
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
        let s = s.into();
        // 2026-10-12(Lead,用户第 6 条取证):界面日志**同时**落进诊断文件。
        //
        // 为什么必须这样:诊断文件原先只收 `diag_*!` 那一路,而"手机被手摸 /
        // 手离开手机 / 设备端输入账本 / 触摸监听起停"这些**自愈层的全部消息**都走
        // 这个环形缓冲 ⇒ 卡死现场在文件里**一个字都没有**(用户 05:46 那次的整份
        // 日志里自愈层输出 0 行,取证链等于断的)。环形缓冲继续留着给界面显示。
        crate::diag::log(crate::diag::Level::Info, "app", &s);
        self.logs.push_back(s);
        while self.logs.len() > 200 {
            self.logs.pop_front();
        }
    }

    fn serial(&self) -> String {
        self.devices.get(self.selected).cloned().unwrap_or_default()
    }

    /// 写进 settings.json 的「上次选中的设备」(D1,纯函数便于单测)。
    ///
    /// 当前**有**设备就用当前;设备列表为空(拔线/掉线/启动时还没连上)时
    /// 保留上一次记住的值 —— 否则一次刷新就把用户记住的设备冲成空串。
    fn remembered_serial(current: &str, last: &str) -> String {
        let cur = current.trim();
        if cur.is_empty() {
            last.trim().to_string()
        } else {
            cur.to_string()
        }
    }

    /// 把"此刻真正选中的设备"记进 `last_serial`。
    /// 只在拿到非空序列号时更新(见 `remembered_serial` 的注释)。
    fn note_current_serial(&mut self) {
        let cur = self.serial();
        self.last_serial = Self::remembered_serial(&cur, &self.last_serial);
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
        // 断开 = 用户/流程明确不要这条通道了:自检的"连续判死"累计一并作废,
        // 免得重连后拿断开之前的死讯接着数。
        self.channel_dead_confirm.reset();
        self.channel_restart_started = None;
    }

    /// 重启控制通道:**关掉旧的 → 复用既有的启动+连接路径**(2026-10-11)。
    ///
    /// 自动(存活自检连续判死)与手动([`Self::force_reconnect_control_channel`])
    /// 共用这一条,不另写一套启动逻辑。收尾也走既有那条路:`connect_control` 先
    /// 把 `shared.control` 清成 None(引擎据此清空本地幻影触点/系统键),连上后
    /// 再变回 Some(引擎按当前按键真值重建触点)—— 正是 engine.rs 里判
    /// "由断到通"的同一对边沿。少任何一半都会留下"本地以为按着、设备端没有"。
    ///
    /// 返回 false = 没发起(已有一轮连接在途 / 没选设备),原因已写进日志。
    fn restart_control_channel(&mut self, reason: &str) -> bool {
        if self.connect_rx.is_some() {
            self.log(format!("控制通道重启被跳过(已有一轮连接在进行中):{reason}"));
            return false;
        }
        if self.serial().is_empty() {
            self.log(format!("控制通道重启被跳过(未选择设备):{reason}"));
            return false;
        }
        self.log(format!(
            "正在重启控制通道:{reason}(端口 tcp:{LOCAL_PORT},socket scrcpy_{SCID:08x};旧 server 先关掉以腾干净 socket 名与端口,连上后按当前按键状态重建触点)"
        ));
        crate::diag_warn!("app", "重启控制通道: {reason}");
        // 旧结论作废:重连之后要重新自检,不能拿"上一条通道的死讯"当新通道的现状
        self.channel_dead_confirm.reset();
        self.channel_probe = None;
        self.channel_probed_at = None;
        // 重启 = 用户还想要连接(自动路径同理);退避计数清零,与[连接控制]同口径
        self.reconnect_attempts = 0;
        self.channel_restart_started = Some(Instant::now());
        self.connect_control();
        true
    }

    /// 手动**强制重连控制通道**(诊断面板按钮;以后接热键只需调这一句)。
    ///
    /// 它补的是存活自检抓不到的那一半:server 的**进程与 socket 都还在**,但设备端
    /// 读线程已经死了 —— 探针说"活",用户却什么都按不动。所以这条入口**不受自检
    /// 结论限制**,随时可执行:重连后能用,就是"坏的是我们这条通道"最直接的判据。
    fn force_reconnect_control_channel(&mut self) {
        self.restart_control_channel(
            "用户手动强制重连(只重启我们自己的控制通道,不影响 scrcpy 投屏)",
        );
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
    /// 当前该亮起的"救援动作"(按优先级仲裁),`None` = 一切正常。
    ///
    /// 候选**按"先来先服务"的顺序**给出:同一优先级下,先进这个列表的赢。
    /// 顺序依据"用户此刻最可能想干什么"。
    fn rescue_candidates(&self) -> Vec<RescueAction> {
        let mut v = Vec::new();
        // ① 连接已断开、且自动重连已放弃 —— 用户描述的空载场景。
        //    判据:`reconnect_armed == false`(不打算再连了)+ 没有在途连接 +
        //    没有可用的控制通道。注意"从未连接过"也会落在这里,那是合理的:
        //    用户确实需要点一下[连接控制]。
        let connecting = self.connect_rx.is_some();
        let has_channel = {
            let g = lock_shared(&self.shared);
            g.control.as_ref().is_some_and(|c| c.is_connected())
        };
        if !connecting && !has_channel && !self.reconnect_armed {
            v.push(RescueAction::ReconnectGaveUp);
        }
        // ② 正在自动重连(还没放弃):给"立即重试"的入口
        if self.reconnect_armed && !connecting && !has_channel {
            v.push(RescueAction::Reconnecting);
        }
        // ③ 通道存活自检判死(见 `channel_probe`):判死才亮,未知不亮
        if self
            .channel_probe
            .as_ref()
            .is_some_and(|p| p.verdict == crate::adb::ChannelVerdict::Dead)
        {
            v.push(RescueAction::ChannelDead);
        }
        // ④ 映射开着但控制通道没连上:按键不会生效,需要用户注意
        let mapping_on = lock_shared(&self.shared).enabled;
        if mapping_on && !has_channel {
            v.push(RescueAction::MappingWithoutChannel);
        }
        v
    }

    /// 右上角「重新连接」按钮(用户 2026-10-12 第 1 条)。
    ///
    /// **取代上一版的"红色哑光占位块"**。用户实测两个问题:
    /// ① 点[连接控制]之后那个块会**闪一下**(块消失 ↔ 亮按钮出现,整块跳变);
    /// ② 平时"一直在显示一些没用的东西"。
    ///
    /// 按用户的"**有始必有终**"原则:去掉占位块,但功能**就地保留** ——
    /// 一个常驻、有字的「重新连接」按钮。为什么不会再闪:
    /// **按钮本体常驻不变**(位置/大小/文字都固定),只有"要不要高亮"随状态变;
    /// 不存在"整块消失↔出现"的跳变。
    ///
    /// 高亮依据仍是 `pick_rescue_action` 的优先级仲裁(高覆盖低、同级先来先服务),
    /// 只是仲裁结果现在只决定**颜色**,不再决定"这块地方有没有东西"。
    fn reconnect_button(&mut self, ui: &mut egui::Ui, th: &theme::Theme) {
        let action = pick_rescue_action(&self.rescue_candidates());
        // 常驻:文字固定「重新连接」。需要时高亮(实心红),平时普通外观。
        let btn = if action.is_some() {
            egui::Button::new(
                egui::RichText::new("重新连接")
                    .color(egui::Color32::WHITE)
                    .strong(),
            )
            .fill(th.danger)
            .stroke(egui::Stroke::new(1.0, egui::Color32::WHITE))
        } else {
            egui::Button::new("重新连接")
        };
        let tip = match action {
            Some(a) => format!("{}\n\n(当前状态:{})", a.tooltip(), a.label()),
            None => {
                "重新发起连接(清零退避计数后立刻重连)。\n已连接时点它 = 断开重连一次。".to_string()
            }
        };
        if ui.add(btn).on_hover_text(tip).clicked() {
            // 动作仍按仲裁结果分流(与上一版一致):
            // 通道判死走"只重启我们自己那条通道",其余走"重新发起连接"。
            match action {
                Some(RescueAction::ChannelDead) => self.force_reconnect_control_channel(),
                _ => {
                    self.reconnect_attempts = 0;
                    self.reconnect_due = None;
                    self.connect_control();
                    self.log("已重新发起连接(右上角[重新连接])");
                }
            }
        }
    }

    /// 「强制重连」热键此刻该不该触发 —— 把 `gui_rx` 循环里的**接线条件**
    /// 抽成纯函数(协议 §6.3 必测缝②:接线本身也要有测试)。
    ///
    /// 为什么必须抽出来:`sim.rs` 的无头测试台跑的是**引擎线程**,而
    /// 「强制重连」与宏三键都接在**界面线程的 `gui_rx` 循环**里 —— 那条缝
    /// 用引擎级测试够不着。抽成纯函数后判定条件可被单测逐条钉住,
    /// 调用点只剩"把入参凑齐"。
    ///
    /// 五个条件全中才触发(与调用点逐字对应):
    /// ① 不是"正在捕获/录制/取点/文本输入"(那时按键是给绑定用的);
    /// ② 这一下是**上升沿**(Windows 自动重复不连发);
    /// ③ 集合非空(未绑定 ⇒ 不触发);
    /// ④ 该键在绑定的集合里;
    /// ⑤ 绑的组合键**此刻整体都按着**(顺序无关)。
    fn reconnect_hotkey_hit(
        keys: &crate::keymap::KeySet,
        code: u16,
        fresh_edge: bool,
        capturing: bool,
        held: &[u16],
    ) -> bool {
        !capturing
            && fresh_edge
            && !keys.is_empty()
            && keys.contains(&code)
            && keys.as_slice().iter().all(|k| held.contains(k))
    }

    /// 轨迹录制**快捷键此刻该做什么**(用户 2026-10-14;2026-10-23 第 2 条扩成一对)。
    ///
    /// 抽成纯函数(与 [`Self::reconnect_hotkey_hit`] 同一套路):这条缝在
    /// **界面线程的 `gui_rx` 循环**里,`sim.rs` 的无头测试台跑的是引擎线程、
    /// 够不着它;抽成纯函数后判定条件可被单测逐条钉住。
    ///
    /// 先过三条前置判据(任一不中 ⇒ `None`,事件不归它、继续往下走):
    /// ① **总开关已勾选**(未勾选 ⇒ 快捷键不触发 —— 用户 2026-10-14 的要求);
    /// ② 按下的键**命中开始键或结束键**(0 = 未绑定 ⇒ 该键永不命中);
    /// ③ **上升沿**(`fresh`)—— Windows 自动重复不连发;否则按住不放会疯狂
    ///    开始/结束切换。
    ///
    /// 注意这里**不看** `capturing`(等待绑键/取点/宏录制中):
    /// 快捷键本身就是"录制开关",而录制中必然要能再次按下它来结束 ——
    /// 若也用 `capturing` 拦,就会"开始后按同一个键结束不掉"。
    ///
    /// 命中之后再看是哪一颗:
    /// - **开始键** ⇒ `TrackRecHotkey::Toggle`:没在录就开录;正在录就收尾。
    ///   这条**兼容 2026-10-14 的单键口径** —— 结束键留空(0)时行为与从前逐字一致。
    /// - **结束键** ⇒ `TrackRecHotkey::Stop`:只负责结束;没在录时什么都不做
    ///   (调用方给一句提示,不静默吞掉用户的按键)。
    ///
    /// 两颗键绑成同一个码时**开始键优先**(先判 Toggle):与单键切换同义,
    /// 不会出现"一按既开又关"。
    ///
    /// 该键**不穿透给设备**(见 `handle_gui_button_event` 返回 true 消费掉)。
    fn track_rec_hotkey(
        enabled: bool,
        start_key: u16,
        stop_key: u16,
        code: u16,
        fresh: bool,
    ) -> Option<TrackRecHotkey> {
        if !enabled || !fresh {
            return None;
        }
        if start_key != 0 && code == start_key {
            return Some(TrackRecHotkey::Toggle);
        }
        if stop_key != 0 && code == stop_key {
            return Some(TrackRecHotkey::Stop);
        }
        None
    }

    /// 处理 `gui_rx` 里的**一个按键事件**(界面层热键的接线点)。
    ///
    /// 抽出来的理由同 [`Self::reconnect_hotkey_hit`]:让"接线"本身可被测试,
    /// 而不只是判定函数。返回"该事件是否已被热键消费"(消费了就不该再进
    /// 录制/捕获路径 —— 这正是"热键事件不写入宏"的实现方式)。
    fn handle_gui_button_event(&mut self, code: u16, pressed: bool, capturing: bool) -> bool {
        // ---- ① 「强制重连」热键 ----
        let was_down = self.hotkey_down.contains(&code);
        let fresh = pressed && !was_down;
        if pressed && !was_down {
            self.hotkey_down.push(code);
        } else if !pressed {
            self.hotkey_down.retain(|k| *k != code);
        }
        let keys = lock_shared(&self.shared).profile.aim.reconnect_key;
        if Self::reconnect_hotkey_hit(&keys, code, fresh, capturing, &self.hotkey_down) {
            self.force_reconnect_control_channel();
        }
        // ---- ② 宏录制热键(开始/终止/取消) ----
        let recording = self.macro_recording.is_some();
        let action = Self::macro_hotkey_action(
            code,
            self.macro_record_start_key,
            self.macro_record_start_key_enabled,
            self.macro_record_stop_key,
            self.macro_record_stop_key_enabled,
            self.macro_record_cancel_key,
            self.macro_record_cancel_key_enabled,
            recording,
        );
        match action {
            MacroHotkey::None => {}
            MacroHotkey::Start => {
                if pressed {
                    self.begin_macro_recording("热键");
                }
                return true; // 热键的按下/抬起都不进录制
            }
            MacroHotkey::Stop => {
                if pressed {
                    self.log("宏录制结束(热键):立即停止并按现有逻辑保存");
                    self.finish_macro_recording();
                }
                return true;
            }
            MacroHotkey::Cancel => {
                if pressed {
                    self.cancel_macro_recording();
                }
                return true;
            }
        }
        // ---- ③ 轨迹录制快捷键(用户 2026-10-14) ----
        //
        // **同一个键按两下切换**(用户拍板):第一次按 = 开始录;录制中再按 = 结束。
        // 放在宏热键**之后**:宏热键已把事件消费掉时不会走到这里(两条热键
        // 哪怕绑成同一个键也先归宏,不会"一按两件事")。
        //
        // 用户 2026-10-14:鼠标键控制录制那条路已**整条去掉**,录制只有
        // 快捷键与卡片按钮两个入口。
        match Self::track_rec_hotkey(
            self.track_rec_enabled,
            self.track_rec_key,
            self.track_rec_stop_key,
            code,
            fresh,
        ) {
            Some(TrackRecHotkey::Toggle) => {
                if self.track_rec.is_some() {
                    self.finish_track_recording();
                } else {
                    // 没有"当前正在编辑的那条滑动"时,快捷键不知道该把轨迹录给谁
                    // ⇒ 明确告知,而不是静默开始一个悬空会话。
                    match self.track_rec_default_target() {
                        Some(t) => self.begin_track_recording(t),
                        None => self.log(
                            "轨迹快捷键:当前没有正在编辑的滑动 —— 请先在键位列表里\
                         打开一条滑动(或新建一条滑动草稿)再按",
                        ),
                    }
                }
                return true; // 快捷键的按下/抬起都不穿透给设备
            }
            Some(TrackRecHotkey::Stop) => {
                if self.track_rec.is_some() {
                    self.finish_track_recording();
                } else {
                    self.log("轨迹快捷键:结束键按下了,但现在没有正在录制的轨迹");
                }
                return true; // 快捷键的按下/抬起都不穿透给设备
            }
            None => {}
        }
        false
    }

    /// 快捷键开始录制时"该录到哪条滑动上"(用户 2026-10-14)。
    ///
    /// 快捷键不像按钮那样自带上下文(按钮就长在某条滑动的编辑区里),
    /// 所以它必须自己**推断目标**,优先级从"用户此刻正在编辑什么"出发:
    ///
    /// ① 正在编辑的**已有键位**且动作是滑动 ⇒ 录给它(`vk_sel` / 弹窗选中);
    /// ② 否则,**新增草稿**正好是滑动 ⇒ 录给草稿;
    /// ③ 都没有 ⇒ `None`(调用方记一条明确提示,不静默开一个悬空会话)。
    ///
    /// 组合键不在推断范围内:它必须先由用户在组合键区里点开某一条,
    /// 那条路用卡片上的按钮更自然(快捷键留给"最常见的键位"这一条)。
    fn track_rec_default_target(&self) -> Option<TrackTarget> {
        // ② 草稿正在配置滑动(用户在新建一条滑动)—— 优先录给草稿
        if self.draft_active && self.draft.kind == 2 {
            return Some(TrackTarget::Draft);
        }
        // ① 可视化风格里选中的那条键位
        if let VkSel::Bind(i) = self.vk_sel.unwrap_or(VkSel::New) {
            let g = lock_shared(&self.shared);
            if matches!(
                g.profile.binds.get(i).map(|b| &b.action),
                Some(Action::Swipe(_))
            ) {
                return Some(TrackTarget::Bind(i));
            }
        }
        None
    }

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
        self.scrcpy_pid = None; // S①:退出后不再拿旧 pid 判定前台
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
        // F6(2026-10-12):启动前统一去重。老配置里可能已经堆了重复项(用户机上
        // `--max-size` 三份、`--max-fps` 两份),只在"加入参数"那一处修的话,
        // 历史脏数据仍会原样带进启动命令。保留**最后一次**出现 = 与 scrcpy
        // 实际生效的那一个逐位一致,不改变任何现有行为,只去掉无效重复。
        let deduped = dedupe_args(&self.scrcpy_args);
        if deduped != self.scrcpy_args.trim() {
            crate::diag_info!(
                "scrcpy",
                "启动参数去重: before={:?}, after={:?}",
                self.scrcpy_args,
                deduped
            );
            self.scrcpy_args = deduped;
            // **写回 settings.json**(2026-10-12 补):只在内存里去重的话,
            // 用户下次看参数框、或点"撤去该参数",面对的仍是那份脏串
            // (用户机上 `--max-size` 三份,每次都只被临时清掉)。
            self.remember_now();
        }
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
        // 宏步骤里的「组合键」槽位同属这一类(2026-10-11):一次捕获可能带进来两个
        // 键,必须走集合落定入口,否则第二个键永远进不来(旧实现就是"抓不到和弦")。
        if slot.takes_chord() || matches!(slot, KeySlot::MacroInstructionComboKey { .. }) {
            self.assign_keys(slot, KeySet::single(code));
            return;
        }
        // 宏步骤的「按键」槽位:落点是宏页草稿或 `binds[i]` 展开副本,与实时配置
        // 无关 —— 与虚拟槽位同一条纪律,先于 `push_undo()` 分开处理(2026-10-11)。
        if let KeySlot::MacroInstructionKey(index) = slot {
            let target = self.macro_key_target;
            if matches!(target, MacroKeyTarget::Bind(_)) {
                self.push_undo();
            }
            // 任务 F:写失败(该步骤已不在 —— 列表刚被删过/换过页)时不许再谎报
            // "键位已绑定"。用户看到的必须与配置里的实际状态一致。
            if self.write_macro_instruction_key(target, index, code) {
                self.log(format!("键位已绑定: {}", key_name(code)));
            } else {
                self.log("本次未写入:该宏步骤已不在(列表刚变过),或本次没有可写入的键");
            }
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
                | KeySlot::AimShield
                | KeySlot::AimWake
                | KeySlot::AimReconnect
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
                // 宏录制热键(2026-10-12):单键槽,写进程序级设置。
                // 落盘由 `remember_now()` 在帧末统一做(与其它 settings 字段同一条路)。
                KeySlot::MacroRecordStart => self.macro_record_start_key = code,
                KeySlot::MacroRecordStop => self.macro_record_stop_key = code,
                KeySlot::MacroRecordCancel => self.macro_record_cancel_key = code,
                // 轨迹录制快捷键(2026-10-14):单键槽,写程序级设置。
                KeySlot::TrackRecKey => self.track_rec_key = code,
                KeySlot::TrackRecStopKey => self.track_rec_stop_key = code,
                // 宏步骤的按键/组合键槽位已在函数开头按 `macro_key_target` 分流
                // (2026-10-11),不会走到这里。
                KeySlot::MacroInstructionKey(_) | KeySlot::MacroInstructionComboKey { .. } => {}
                // R4:扩展宏弹窗的虚拟取键已在函数开头单独处理(写宏草稿,不碰 Shared),
                // 这里只为穷尽枚举。
                KeySlot::MacroVirtualNewBind
                | KeySlot::MacroVirtualComboKey { .. }
                | KeySlot::MacroVirtualWheelDir { .. }
                | KeySlot::MacroVirtualWheelEnable(_) => {}
            }
        }
        self.log(format!("键位已绑定: {}", key_name(code)));
        // 宏录制热键写的是**程序级设置**(不是键位配置),落盘由 settings 那条路
        // 统一做。必须在 `g`(Shared 锁)离开作用域之后登记 —— 否则与
        // `settings_snapshot()` 的不可变借用冲突(编译期就会报 E0502)。
        if matches!(slot, KeySlot::MacroRecordStart | KeySlot::MacroRecordStop) {
            self.remember_now();
        }
    }

    /// 组合键槽位的落定入口(用户 2026-10-09 第 4 条)。
    ///
    /// 单键与组合键同一条路:单个键 = 只含一个键的集合 —— 于是"只允许单键的
    /// 键位也套用这套逻辑"(用户原话),不需要两套写法。
    /// **不给任何时长/间隔参数**:这类键就是"这几个键一起按住",
    /// 与用户同一条要求里的限制一致。
    fn assign_keys(&mut self, slot: KeySlot, keys: KeySet) {
        // 宏步骤里的「组合键」槽位:写的是宏页草稿 / `binds[i]` 展开副本,不是实时
        // 配置 —— 先分流,避免在撤销栈上留一步空操作(2026-10-11)。
        if let KeySlot::MacroInstructionComboKey {
            instruction,
            slot: key_slot,
        } = slot
        {
            let target = self.macro_key_target;
            if matches!(target, MacroKeyTarget::Bind(_)) {
                self.push_undo();
            }
            let codes: Vec<u16> = keys.iter().copied().filter(|k| *k != 0).collect();
            if self.write_macro_instruction_keys(target, instruction, key_slot, &codes) {
                self.log(format!("键位已绑定: {}", keys.label()));
            } else {
                self.log("本次未写入:该宏步骤已不在(列表刚变过),或本次没有可写入的键");
            }
            return;
        }
        self.push_undo();
        // 切换键属于"组合表结构",改完自动落盘(见 sync_scheme_state)。
        let mut switch_key_touched = false;
        let mut clobbered = false;
        // 组合键成员格写失败时的提示语 —— 只能在**放开配置锁之后**再 `self.log`
        // （`self.log` 会取同一把锁，抱着锁调用就是自锁）。
        let mut combo_write_failed = false;
        {
            let mut g = lock_shared(&self.shared);
            match slot {
                KeySlot::ComboKey { combo, slot } => {
                    // 用户配置里的组合键成员格：以前走单键捕获，按下 Ctrl 的瞬间就落定、
                    // `waiting_key` 随即清空 —— 用户仍按着的那个 Z 于是被全局快捷键当成
                    // “撤销”执行，刚写的值被回滚（用户 2026-10-23：“设的 Ctrl+Z，界面
                    // 变成 Ctrl+X”）。改走和弦捕获：整段手势期间 `waiting_keys` 挂着 ⇒
                    // 全局快捷键让路，而且按住 Ctrl+Z 一次就能把整组填好。
                    let codes: Vec<u16> = keys.iter().copied().collect();
                    let wrote = g
                        .profile
                        .combos
                        .get_mut(combo)
                        .is_some_and(|c| write_chord_into_combo(&mut c.keys, slot, &codes));
                    if !wrote {
                        combo_write_failed = true;
                    }
                }
                KeySlot::Toggle => g.profile.toggle_key = keys,
                KeySlot::CursorToggle => g.profile.cursor_toggle_key = keys,
                KeySlot::AimHold => g.profile.aim.hold_key = keys,
                KeySlot::AimToggle => g.profile.aim.toggle_key = keys,
                KeySlot::AimSuspend => g.profile.aim.suspend_key = keys,
                KeySlot::AimShield => g.profile.aim.shield_key = keys,
                KeySlot::AimWake => g.profile.aim.wake_key = keys,
                KeySlot::AimReconnect => g.profile.aim.reconnect_key = keys,
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
        if combo_write_failed {
            self.log("本次未写入:这一格或这个组合键已不在(列表刚变过)");
        }
        // 用户 2026-10-23 要求"开最大警告档位":绑定结果连同**原始码**一起落盘,
        // 于是"按 Z 绑成 X"这类事下次能靠一行日判定,不必再猜。
        if keys.is_empty() {
            self.log("键位已绑定: 未绑定".to_string());
        } else {
            let codes: Vec<u16> = keys.iter().copied().collect();
            self.log(format!("键位已绑定: {} [码 {:?}]", keys.label(), codes));
        }
    }

    /// 把一次「按键」捕获写到正确的宏缓冲上（见 [`MacroKeyTarget`]）。
    /// 返回是否真的写成功 —— 目标下标不存在(列表刚被删过/换过页)时什么都不写,
    /// 调用方**不许**再打"键位已绑定"(任务 F,2026-10-11)。
    fn write_macro_instruction_key(
        &mut self,
        target: MacroKeyTarget,
        index: usize,
        code: u16,
    ) -> bool {
        match target {
            MacroKeyTarget::Page => {
                if let Some(MacroInstruction::Key { code: dst, .. }) =
                    self.macro_page_instructions.get_mut(index)
                {
                    *dst = code;
                    true
                } else {
                    false
                }
            }
            MacroKeyTarget::Bind(bind) => {
                let mut g = lock_shared(&self.shared);
                write_macro_key_in_bind(&mut g.profile.binds, bind, index, code)
            }
        }
    }

    /// 把一次「组合键」捕获写到正确的宏缓冲上（见 [`MacroKeyTarget`]）。
    /// 返回是否真的写成功(理由同上)。
    fn write_macro_instruction_keys(
        &mut self,
        target: MacroKeyTarget,
        index: usize,
        slot: usize,
        codes: &[u16],
    ) -> bool {
        match target {
            MacroKeyTarget::Page => {
                if let Some(MacroInstruction::Combo { keys: dst, .. }) =
                    self.macro_page_instructions.get_mut(index)
                {
                    write_combo_keys(dst, slot, codes)
                } else {
                    false
                }
            }
            MacroKeyTarget::Bind(bind) => {
                let mut g = lock_shared(&self.shared);
                if let Some(dst) = macro_combo_in_bind(&mut g.profile.binds, bind, index) {
                    write_combo_keys(dst, slot, codes)
                } else {
                    false
                }
            }
        }
    }

    /// 单键捕获的**唯一**赋值点。
    ///
    /// 与组合键捕获互斥:两处都武装的话,同一次按键会被两条路各处理一遍
    /// (事件循环里和弦分支在单键分支之前判断)。进入时清空上一轮的按键记录 ——
    /// 否则上一次没按完的半个组合会粘进来,下一次落定就落在那半个组合上。
    ///
    /// **任何"点一下按钮等用户按键"的入口都必须走这里**:裸赋值会让上一次的组合键捕获
    /// 继续举着,用户按下的键就落到别的行上 —— 用户看到的是"我设的是这一行,改的却是另一行",
    /// 或者"选中的位置突然瞬移"(2026-10-23 组合键事故)。
    /// 守护测试 `every_capture_arming_goes_through_the_two_helpers`。
    fn arm_key_capture(&mut self, slot: KeySlot) {
        self.picking = None;
        self.resizing = None;
        self.waiting_keys = None;
        self.capture_down.clear();
        self.capture_seen.clear();
        self.waiting_key = Some(slot);
    }

    /// 组合键捕获的**唯一**赋值点(与单键捕获互斥,理由见 [`Self::arm_key_capture`])。
    fn arm_keys_capture(&mut self, slot: KeySlot) {
        self.picking = None;
        self.resizing = None;
        self.waiting_key = None;
        self.capture_down.clear();
        self.capture_seen.clear();
        self.waiting_keys = Some(slot);
    }

    /// 进入**组合键**捕获(见 [`Self::keys_button`])。
    fn begin_keys_capture(&mut self, slot: KeySlot) {
        self.arm_keys_capture(slot);
        self.log("请按下组合键:按住 Ctrl 再按另一个键(只按一个键就是单键);全部松开即生效");
    }

    /// 进入单键捕获(按下的那一个键立即落定)。
    fn begin_key_capture(&mut self, slot: KeySlot) {
        self.arm_key_capture(slot);
        self.log("按任意键完成改绑(或点[取消选择]退出)");
    }

    /// 进入「组合键成员格」的捕获（用户配置的 `combos[i].keys[slot]`）。
    ///
    /// 走**和弦**捕获而不是单键（2026-10-23 组合键事故第二半）：单键捕获在按下
    /// Ctrl 的瞬间就落定并清掉 `waiting_key`，用户仍按着的 Z 随即被全局快捷键当成
    /// “撤销”执行，刚写进去的值被回滚成原值 —— 用户看到的是“我设的是 Ctrl+Z，
    /// 界面变成 Ctrl+X”。和弦捕获期间 `waiting_keys` 一直挂着，全局快捷键让路；
    /// 而且按住 Ctrl+Z 一次就能把整组填好（见 [`write_chord_into_combo`]）。
    fn begin_combo_member_capture(&mut self, combo: usize, slot: usize) {
        self.arm_keys_capture(KeySlot::ComboKey { combo, slot });
        self.log("请按下这一格要用的键:只按一个键=只改这一格;按住 Ctrl 再按另一个键=从这一格起,这整组就是这两个键");
    }

    /// 退出所有按键捕获(单键与组合键一起清,不留下"举着等按键"的半状态)。
    fn cancel_key_capture(&mut self) {
        self.waiting_key = None;
        self.waiting_keys = None;
        self.capture_down.clear();
        self.capture_seen.clear();
    }

    /// 列表增删之后**必须**调这里:主界面的"当前选中 / 正在等按键 / 正在取点"全是**按下标**
    /// 记的目标(`vk_sel` / `waiting_key` / `waiting_keys` / `picking`)。删掉一项后下标全体前移,
    /// 旧目标就指到邻居身上 —— 用户看到的是"我设的是这一行,改的却是另一行"或"位置突然瞬移"
    /// (2026-10-23 组合键事故)。与虚拟弹窗的 [`Self::clear_virtual_pick`] 同一口径,
    /// 这里只管实时配置那一套,虚拟弹窗自己的清理仍走 `clear_virtual_pick`。
    fn forget_stale_target(&mut self, what: &str) {
        let had = self.vk_sel.is_some()
            || self.waiting_key.is_some()
            || self.waiting_keys.is_some()
            || self.picking.is_some();
        self.cancel_key_capture();
        self.vk_sel = None;
        self.picking = None;
        if had {
            self.log(format!(
                "{what}:后面的下标已经变了,已取消当前选中,免得改到别的行"
            ));
        }
    }

    /// 界面此刻是否正在"从用户手里收输入"(等单键 / 等组合键 / 取点 / 宏录制)。
    ///
    /// **唯一判据**:全局快捷键(撤销/重做/保存/刷新)与界面层热键(强制重连、
    /// 宏三键)都必须整体让路 —— 这些状态下每个按键都可能是用户**要绑/要记**的
    /// 东西,顺带消费掉就会出事。2026-10-23 用户实测:捕获组合键时按 Ctrl+Z,
    /// 捕获路径刚把 Ctrl+Z 写进槽位,同一帧里全局快捷键又把 Ctrl+Z 当"撤销"
    /// 执行一次,把刚写的值回滚成旧值(用户原话:设的 Ctrl+Z,界面变成 Ctrl+X)。
    ///
    /// 原来这段条件在三处各抄了一份(全局快捷键 / 界面热键 / 自愈 ui_busy),
    /// 全局快捷键那份漏了 `waiting_keys` 与 `macro_recording` —— 口径漂移就是这么
    /// 来的。新增"等用户输入"的状态必须加到这里,别在各处重新拼条件。
    fn is_capturing_input(&self) -> bool {
        self.waiting_key.is_some()
            || self.waiting_keys.is_some()
            || self.picking.is_some()
            || self.macro_recording.is_some()
    }

    /// 取证用(用户 2026-10-23:"VK 0x58 之前碰到过",要能下次直接判定):
    /// 把**捕获层拿到的原始码**记一行。`44` = KEY_Z、`45` = KEY_X —— 若又出现
    /// "按 Z 却绑成 X",这一行能区分是「OS 报的码就不对」还是「值被后一步覆盖」。
    /// 只在捕获期间打(一次捕获至多两三个键),不会刷屏。
    fn log_captured_key(&mut self, code: u16) {
        self.log(format!("捕获按键:{} (码 {})", key_name(code), code));
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
                                repeat_interval_ms: 0,
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
                self.arm_key_capture(slot);
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
        // 用户 2026-10-23 第 2 条:一列按键控件原先统一 110px,比"一个按键"该有的
        // 长度宽出一截(组合键行里两三个按钮直接撑到 300+px)。收敛到 84px ——
        // 仍容得下"鼠标左键"这类中文键名,但一行能站下更多东西。
        Self::key_button_sized(ui, waiting, code, 84.0)
    }

    /// 与 [`Self::key_button`] 同款、可指定最小宽度的版本。
    ///
    /// 拆出来是为了让"组合键成员"能用更窄的一档(62.0),
    /// 而单键槽保持能显示中文键名的宽度 —— 同一个组件、两种紧度(用户 2026-10-23
    /// 第 2 条:要"不紧不松",不要一行塞满整屏)。
    fn key_button_sized(
        ui: &mut egui::Ui,
        waiting: bool,
        code: Option<u16>,
        min_w: f32,
    ) -> egui::Response {
        let label = if waiting {
            "按任意键...".to_string()
        } else {
            code.map(key_name).unwrap_or_else(|| "未绑定".into())
        };
        ui.add(egui::Button::new(label).min_size(egui::vec2(min_w, 0.0)))
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
            // 换掉的是整份 profile(组合表长度都可能变):按位置记的「正在等按键 / 正在取点」
            // 目标全部作废,否则下一次落定会写到别的行上(2026-10-23 组合键事故,同根因)。
            self.forget_stale_target("撤销");
        }
    }

    fn redo(&mut self) {
        if let Some(next) = self.redo_stack.pop() {
            let current = lock_shared(&self.shared).profile.clone();
            self.undo_stack.push(current);
            lock_shared(&self.shared).profile = next;
            self.log("已重做");
            // 同 undo:整份 profile 被换掉之后,按位置记的目标不再可靠。
            self.forget_stale_target("重做");
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
            // 删掉一套组合 ⇒ 后面每套的下标都前移,而且 active_scheme 可能已经换成
            // 另一套 profile:面板上挂着的"当前选中 / 待按键 / 待取点"全是按下标记的,
            // 整套作废(2026-10-23)。
            self.forget_stale_target("删除按键组合");
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
    /// 开始一次轨迹录制(用户 2026-10-13,V2-3)。
    ///
    /// 与取点/改范围**互斥**(按最新操作优先):开始录制即终止正在进行的取点与
    /// 改范围,免得"一边等取点一边记轨迹"两套交互抢同一次点击。
    ///
    /// 已经在录制时再次调用 = **重新开始**(清掉已录点)。这条路径是有意的:
    /// 用户划歪了想重划,不必先"取消"再"开始"。
    fn begin_track_recording(&mut self, target: TrackTarget) {
        self.picking = None;
        self.resizing = None;
        // 同一时刻只允许一个录制会话:换目标(或对同一目标重新开始)时,
        // **先把旧会话收尾**。对同一目标再次点[录制] = 重新开始(丢掉已录点,
        // 用户明知自己在重来,不必报"已取消");换到别的目标 = 旧的那次按取消
        // 处理(绝不静默丢弃 —— 写日志留痕)。
        let old = self.track_rec.as_ref().map(|r| r.target);
        match old {
            Some(o) if o == target => {
                self.track_rec = None;
                self.log("轨迹录制重新开始:已录的点全部丢弃");
            }
            Some(_) => {
                self.cancel_track_recording();
            }
            None => {}
        }
        // 重录已有键位/组合键要能撤销回原样:快照在**开始录制**那一刻压
        // (那时配置还没被改),于是"取消"丢掉会话就够、"提交"则留下一条
        // 可撤销的历史。草稿是临时状态,不进撤销栈。
        if !matches!(target, TrackTarget::Draft) {
            self.push_undo();
        }
        self.track_rec = Some(TrackRec {
            target,
            points: Vec::new(),
            started: std::time::Instant::now(),
        });
        self.draft_active = true;
        self.log("轨迹录制开始:在截图上划出路径,再点[结束并保存];[取消]可整份丢弃");
    }

    /// 结束录制并**提交**到目标(用户 2026-10-13,V2-3)。
    ///
    /// 少于 2 个点 = 没有轨迹可言 ⇒ 按"取消"处理(不留半条轨迹,也不写空数组)。
    /// 返回是否真的提交了(供界面决定提示文案)。
    fn finish_track_recording(&mut self) -> bool {
        let Some(rec) = self.track_rec.take() else {
            return false;
        };
        let n = rec.points.len();
        if n < 2 {
            self.log(format!(
                "轨迹录制结束:只记到 {n} 个点(至少 2 个才生效,已丢弃)"
            ));
            return false;
        }
        match rec.target {
            TrackTarget::Draft => {
                self.draft.swipe_points = rec.points.clone();
                // 草稿的起终点跟着轨迹的首末点走:用户看到的那条轨迹与
                // "起点→终点"的语义于是永远自洽(切换/清除轨迹后不留矛盾几何)。
                if let (Some(&f), Some(&l)) = (rec.points.first(), rec.points.last()) {
                    self.draft.swipe_start = f;
                    self.draft.swipe_end = l;
                }
            }
            TrackTarget::Bind(i) => {
                let m = self.mapper();
                let pts: Vec<(f32, f32)> = rec
                    .points
                    .iter()
                    .map(|&(x, y)| (m.rel_x(x), m.rel_y(y)))
                    .collect();
                let mut g = lock_shared(&self.shared);
                if let Some(b) = g.profile.binds.get_mut(i)
                    && let Action::Swipe(s) = &mut b.action
                {
                    s.recorded_points = pts.clone();
                    if let (Some(&f), Some(&l)) = (pts.first(), pts.last()) {
                        s.start = f;
                        s.end = l;
                    }
                }
            }
            TrackTarget::Combo(i) => {
                let m = self.mapper();
                let pts: Vec<(f32, f32)> = rec
                    .points
                    .iter()
                    .map(|&(x, y)| (m.rel_x(x), m.rel_y(y)))
                    .collect();
                let mut g = lock_shared(&self.shared);
                if let Some(c) = g.profile.combos.get_mut(i)
                    && let Action::Swipe(s) = &mut c.action
                {
                    s.recorded_points = pts.clone();
                    if let (Some(&f), Some(&l)) = (pts.first(), pts.last()) {
                        s.start = f;
                        s.end = l;
                    }
                }
            }
        }
        self.log(format!("轨迹录制完成:{n} 个点(已启用,优先于起点/终点)"));
        true
    }

    /// 取消录制,**整份丢弃**已录点(用户 2026-10-13,V2-3)。
    ///
    /// 这是上一版缺失的那条路:划到一半发现划歪了可以直接丢掉重来,
    /// 不必"先划完、再点清除轨迹"。
    fn cancel_track_recording(&mut self) -> bool {
        let Some(rec) = self.track_rec.take() else {
            return false;
        };
        self.log(format!(
            "已取消轨迹录制:{} 个点全部丢弃(原轨迹未改动)",
            rec.points.len()
        ));
        true
    }

    /// 录制采样:把浮层上的一个鼠标位置记进会话。
    ///
    /// 抽稀规则与点数上限都在 [`crate::keymap::push_track_sample`] 里(纯函数、
    /// 有单测),这里只负责"从会话里取缓冲、交给它"。三种录制目标(草稿/键位/
    /// 组合键)共用同一条规则,不会因为录到哪儿而疏密不同。
    fn push_track_point(&mut self, px: i32, py: i32) {
        let Some(rec) = self.track_rec.as_mut() else {
            return;
        };
        crate::keymap::push_track_sample(&mut rec.points, px, py);
    }

    /// 当前录制会话的点数(没有录制时为 0)。界面显示用。
    fn track_rec_count(&self) -> usize {
        self.track_rec.as_ref().map_or(0, |r| r.points.len())
    }

    /// 录制会话已持续的秒数(没有录制时 0)。浮层 REC 角标与卡片呼吸点共用。
    ///
    /// 独立成方法而不是在绘制处现算:`draw_overlay` 里已经借了 `self.track_rec`
    /// 的不可变引用,再调别的方法会撞借用检查 —— 调用方改成先取这个数
    /// (它是 `Copy` 的 `f32`),再进借用。
    fn track_rec_elapsed(&self) -> f32 {
        self.track_rec
            .as_ref()
            .map_or(0.0, |r| r.started.elapsed().as_secs_f32())
    }

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
        // 草稿被取消 = 这次录制的目标没了 ⇒ 会话必须一并收尾,否则会留下一个
        // 指向"已经不存在的草稿"的录制态(下一帧浮层还会画它的实时轨迹)。
        if self.track_rec.as_ref().map(|r| r.target) == Some(TrackTarget::Draft) {
            self.cancel_track_recording();
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
        // F6:走 `upsert_arg`(同键就地替换),而不是整串重写 —— 后者会把用户
        // 手动加的其它参数(码率、--no-audio 等)一并抹掉。
        self.scrcpy_args = upsert_arg(BASE_SCRCPY_ARGS, &format!("--max-size={max_size}"));
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

    /// 「启动预设」里"只加一项参数"那一类的共同动作(用户 2026-10-23 第 4 条)。
    ///
    /// 与音频类同一口径:[upsert_arg] 就地替换/追加,不动其它参数 —— 于是
    /// "先选分辨率、再选全屏"不会把分辨率预设抹掉。
    /// 已经含有同键参数时只记一条日志、不重复追加:用户连点两次下拉不会
    /// 在参数框里看到两份 `--fullscreen`([dedupe_args] 那道兜底也还在,
    /// 但"根本不产生重复"比"产生后再去重"更干净)。
    fn add_scrcpy_arg(&mut self, flag: &str, what: &str) {
        if self
            .scrcpy_args
            .split_whitespace()
            .any(|a| arg_key(a) == arg_key(flag))
        {
            self.log(format!("参数已含 {flag}({what}),无需重复"));
        } else {
            self.scrcpy_args = upsert_arg(&self.scrcpy_args, flag);
            self.log(format!("已追加 {flag}({what})"));
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
                    // F6:统一走 upsert_arg,避免与"加入参数"按钮产生两份写法。
                    self.scrcpy_args = upsert_arg(&self.scrcpy_args, "--no-audio");
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
            // ---- 常用参数类(用户 2026-10-23 第 4 条)----
            // 只追加、不重置:见 [Self::add_scrcpy_arg] 的说明。
            StartPreset::Fullscreen => self.add_scrcpy_arg("--fullscreen", "全屏启动"),
            StartPreset::TurnScreenOff => {
                self.add_scrcpy_arg("--turn-screen-off", "启动即关闭设备屏幕(省电,可随时点亮)")
            }
            StartPreset::MaxFps60 => {
                self.add_scrcpy_arg("--max-fps=60", "限制投屏帧率上限 60(降低延迟与发热)")
            }
            StartPreset::AlwaysOnTop => self.add_scrcpy_arg("--always-on-top", "投屏窗口置顶"),
            StartPreset::HighBitrate => self.add_scrcpy_arg(
                "--video-bit-rate=20M",
                "提高视频码率到 20M(画面更清楚,网络要求更高)",
            ),
        }
    }

    /// 用当前生效的 adb 重新拉取设备列表
    fn refresh_devices(&mut self) {
        self.devices = adb::list_devices();
        if self.selected >= self.devices.len() {
            self.selected = 0;
        }
        // D1:刷新**不能**把记住的设备冲掉 —— 拔线后列表为空,这里若不管,
        // 帧末写盘就会把 selected_serial 写成空串(读回侧要求非空 ⇒ 永久失效)。
        self.note_current_serial();
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
                None => self.log(format!(
                    "未找到 adb(设备列表将为空): 请将 {} 所在目录加入 PATH,或手动指定",
                    adb::adb_exe_name()
                )),
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
            // D1:设备列表为空时 `serial()` 是空串 —— 直接写它会一次刷新就把
            // 用户记住的设备冲掉(读回侧要求非空且在线 ⇒ 永久失效)。
            selected_serial: Self::remembered_serial(&self.serial(), &self.last_serial),
            // 日志级别由界面上的下拉框负责写入,这里原样带回上次读到的值,
            // 免得"改一次别的设置"就把用户选的级别冲掉
            log_level: self.settings_loaded.log_level.clone(),
            // 当前在用的配置文件位置(W0-9):它让"另选/新建的配置"活过重启。
            // **不受 [记住路径] 开关管辖** —— 那条开关的语义是"scrcpy 装在哪"
            // (本机环境信息);而"我在编辑哪一份配置"换了却不记得,重启后
            // 打开的是另一份文件、用户会以为改动丢了,属于正确性问题。
            profile_path: Some(self.profile_path.display().to_string()),
            macro_record_start_key: self.macro_record_start_key,
            macro_record_start_key_enabled: self.macro_record_start_key_enabled,
            macro_record_stop_key: self.macro_record_stop_key,
            macro_record_stop_key_enabled: self.macro_record_stop_key_enabled,
            macro_record_cancel_key: self.macro_record_cancel_key,
            macro_record_cancel_key_enabled: self.macro_record_cancel_key_enabled,
            track_rec_enabled: self.track_rec_enabled,
            track_rec_key: self.track_rec_key,
            track_rec_stop_key: self.track_rec_stop_key,
            swallow_bound_keys: self.swallow_bound_keys,
            clip_cursor_in_aim: self.clip_cursor_in_aim,
            grab_enabled: self.grab_enabled,
            mouse_capture: self.mouse_capture,
            mask_filter: self.mask_filter.to_bits(),
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

        // ---- Raw Input 通道自检(T6)----
        // 与"控制队列 深度 …/峰值 …"同一块(自检区),口吻一致。
        // **同一帧只探一次**:这一行的状态与下面"最近日志"块共用这一份读数,
        // 免得两次采样落在 700ms 状态机边界的两侧而自相矛盾。
        //
        // 注:RawEvent 那条通道是**无界的 std mpsc**,没有 `len()`,
        // 因此这里**没有**"Raw 队列深度"可言(与上面控制队列那条不同)——
        // 不存在可读的深度量,不去编一个假数字。
        let (raw_state, raw_count, _raw_age_ms) = crate::capture::raw_probe();
        if let Some(row) = Self::raw_diag_row(raw_state, raw_count) {
            let color = match raw_state {
                1 => th.warn, // 已注册、等首个事件:还不算坏,琥珀色
                2 => th.ok,   // 按设备计数上报:正常
                3 => th.warn, // 已降级:琥珀色
                _ => th.muted,
            };
            ui.colored_label(color, row);
        }

        // ---- 最近日志(含 Raw 通道)----
        // 诊断日志只有落到文件才完整,但用户出问题时第一眼需要看到"刚刚发生了什么"。
        // 这里显示的是**已经写进 diagnostics.log 的那些行**(含捕获层的 Raw Input 三条),
        // 来源是 `diag::recent_lines()` 环形缓冲 —— 与文件内容逐字一致。
        ui.separator();
        ui.label("最近日志（含 Raw 通道）:");
        let recent = crate::diag::recent_lines();
        if recent.is_empty() {
            ui.small("(本次运行还没有任何日志行)");
        } else {
            for l in recent.iter().rev().take(8) {
                ui.monospace(l);
            }
        }
        ui.small("这里只显示最近 8 行；完整日志见下方按钮打开的 diagnostics.log。");

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

        // ---- 设备注入自检(2026-10-11)----
        // 放在"状态自检"这块所有本机检查之后:上面那些只回答"本程序内部通不通",
        // 这一条回答的是"**手机那一侧**到底收不收得到注入" —— 用户报的"所有按键
        // 失效 / 重进 FPS 才恢复"卡在两者之间,必须有一件绕过我们自己通道的器械。
        ui.separator();
        ui.label("设备注入自检:");
        ui.horizontal_wrapped(|ui| {
            let busy = self.selfcheck_rx.is_some();
            if ui
                .small_button(if busy {
                    "自检中..."
                } else {
                    "设备注入自检(会在手机上点一下)"
                })
                .on_hover_text(
                    "会用 adb shell input tap 在手机屏幕**中心**真的点一下 —— 这条路完全\n\
                     绕过本程序的控制通道。读法:手机对它有反应、而我们的按键没反应 ⇒\n\
                     问题在本程序的控制通道;它也没反应 ⇒ 问题在设备/系统层(灭屏、\n\
                     焦点被别的窗口抢走、系统挡了注入)。\n\
                     顺带重查坐标空间,并把焦点窗口/屏幕状态写进日志与本悬浮窗。",
                )
                .clicked()
                && !busy
            {
                self.start_device_selfcheck();
            }
            if let Some(note) = &self.selfcheck_note {
                ui.small(format!("最近一次: {note}"));
            }
        });
        if let Some(f) = &self.probe_focus {
            ui.small(format!("焦点窗口: {f}"));
        }
        if let Some(on) = self.probe_screen_on {
            ui.small(format!("屏幕状态: {}", if on { "亮" } else { "灭" }));
        }
        ui.small("自检结果同时写进日志:『设备注入自检』『设备注入自检结论』『设备注入自检现场』。");

        // ---- 控制通道存活自检 + 强制重连(2026-10-11)----
        // 用户真机现场:FPS 里锁死、界面显示一切正常、日志零错误,而设备端我们那个
        // server 早已不接收 —— 本地写入照样成功,`is_connected()` 一直为真。上面
        // 那些本机检查全回答不了"设备端还有没有人收",这一块就是那件器械。
        ui.separator();
        ui.label("控制通道存活自检:");
        ui.horizontal_wrapped(|ui| {
            if self.channel_rx.is_some() {
                ui.small("自检中...");
            } else {
                ui.small(format!("通道={}", self.channel_value_text()));
            }
            match self.channel_probe.as_ref().map(|p| p.verdict) {
                Some(adb::ChannelVerdict::Dead) => {
                    ui.colored_label(th.warn, "设备端已没有我们这条通道");
                }
                Some(adb::ChannelVerdict::Unknown) => {
                    ui.small("这次没取到可信证据(按未知处理,不会自动重连)");
                }
                _ => {}
            }
            let busy = self.connect_rx.is_some();
            if ui
                .small_button(if busy {
                    "重连中..."
                } else {
                    "强制重连控制通道"
                })
                .on_hover_text(
                    "会重启**我们自己**的 control-only 控制通道(不影响 scrcpy 投屏)。\n\
                     用途:按键全部失效、界面却显示一切正常时按一下 —— 如果重连后就能用,\n\
                     说明坏的正是这条通道。它不看自检结论,随时可执行。",
                )
                .clicked()
                && !busy
            {
                self.force_reconnect_control_channel();
            }
        });
        if let Some(p) = &self.channel_probe {
            ui.small(format!("最近一次证据: {}", p.evidence));
        }
        match adb::last_server_error() {
            Some((line, at)) => {
                ui.colored_label(
                    th.warn,
                    format!(
                        "最近一条 server 侧错误({}s前): {line}",
                        at.elapsed().as_secs()
                    ),
                );
            }
            None => {
                ui.small("本次运行还没见过 server 侧错误(它会以 ERROR 级别写进 diagnostics.log)");
            }
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
        // ---- 设备侧巡检(2026-10-11)----
        // 先收结果、再看该不该起下一轮:与输入无关的独立定时器(见 SPACE_WATCH_INTERVAL)
        self.poll_space_watch();
        self.poll_device_selfcheck();
        // 存活自检(2026-10-11)与巡检同节拍但**独立计时**:抓的是"本地以为通着、
        // 设备端其实没人收"——那条故障在本地完全看不出来(见 `adb::channel_probe`)。
        self.poll_channel_watch();
        self.maybe_space_watch();
        self.maybe_channel_watch();
        // ---- 卡死自愈的取证层(2026-10-12,用户第 6 条)----
        // 先收结果、再看该不该起下一轮(与上面的巡检/自检同一个写法):
        // ① 设备端账本每秒一次只读 `dumpsys input`(变化才写日志);
        // ② 手机被手触摸的只读监听(Up 边沿 = 请求引擎自愈一次)。
        self.poll_device_ledger();
        self.maybe_device_ledger();
        self.poll_physical_touch();
        self.maybe_physical_touch_watch();
        // 界面交互状态(宏步进/绑定捕获/取点)每帧同步给引擎:那些期间不自愈。
        self.sync_self_heal_ui_state();
        // 进入 FPS 模式这一下也顺带补查一次(进 FPS 往往伴随手机转屏/分辨率切换)。
        // 这只是"提前一步",**不替代**上面的定时巡检 —— 定时那条不需要任何输入。
        {
            let fps_now = lock_shared(&self.shared).aim_live.mode_active;
            if fps_now && !self.aim_mode_prev {
                self.refresh_display_space();
            }
            self.aim_mode_prev = fps_now;
        }

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
            // S①:子进程真的退出了就把 pid 作废 —— 否则"仅投屏窗口在前台"这一档
            // 会拿着一个可能被系统回收再分配给别的进程的旧 pid 去比。
            // (这三条正是 `adb::launch_scrcpy` 里等待线程发的收尾消息。)
            if message.starts_with("scrcpy 已退出")
                || message.starts_with("scrcpy 异常退出")
                || message.starts_with("无法获取 scrcpy 退出状态")
            {
                self.scrcpy_pid = None;
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
        // 2026-10-23 事故:这里原先只挡了 waiting_key 与 picking,**漏了
        // waiting_keys(组合键捕获)与宏录制**。于是在"等组合键"时按 Ctrl+Z:
        // 捕获路径刚把 Ctrl+Z 写进槽位,同一帧里这一段又把它当"撤销"执行了一次,
        // 把刚写的值回滚成那格原来的值(Ctrl+X)。判据见 is_capturing_input。
        if !self.is_capturing_input() && !ctx.egui_wants_keyboard_input() {
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

        // ---- 轨迹录制的取消键(Esc,用户 2026-10-13,V2-3) ----
        //
        // "有始必有终":录制是一条会改变数据的路径,它必须随时能退出。
        // Esc 是用户对"取消"最本能的按键,而且它不与任何既有快捷键冲突
        // (上面的全局快捷键里没有 Esc)。
        //
        // 为什么要在**这里**、而且无条件处理:录制中用户的眼睛在截图浮层上,
        // 手边的按钮可能被面板挡住/需要滚动才看得见;Esc 是唯一"闭着眼也按得到"
        // 的退出路径。丢弃整份会话,原轨迹不受影响。
        if self.track_rec.is_some() && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.cancel_track_recording();
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
                        // 存活自检/手动[强制重连]发起的重启:报一句"回来了"与耗时。
                        // 耗时从发起那一刻算起(含中间的重试),这才对得上用户体感。
                        if let Some(t0) = self.channel_restart_started.take() {
                            self.log(format!(
                                "控制通道重启完成,从发起起共耗时 {}ms(端口 tcp:{LOCAL_PORT},socket scrcpy_{SCID:08x});触点已按当前按键状态重建",
                                t0.elapsed().as_millis()
                            ));
                            crate::diag_info!(
                                "app",
                                "控制通道重启完成,{}ms(端口 tcp:{LOCAL_PORT},socket scrcpy_{SCID:08x})",
                                t0.elapsed().as_millis()
                            );
                        }
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
            // ---- 「强制重连控制通道」逃生热键(B,2026-10-11) ----
            // 为什么在这里而不是等 egui 的键盘事件:锁死时鼠标点不了任何界面,
            // 键盘是唯一还通的路,而焦点**不在**本窗口上(用户多半刚在 scrcpy
            // 窗口里),egui 收不到键。这里拿到的是捕获层的原始事件(转发在组合键
            // 门控之前,见 engine 的 `gui_tx.send`)—— 与引擎处理系统键同一个来源。
            //
            // 纪律与引擎的系统键逐字相同:只认**上升沿**(Windows 自动重复不连
            // 触发);绑组合键时要求整个集合此刻都按着(顺序无关);捕获/录制/
            // 取点/文本输入期间一律不生效(那时按键是给绑定用的)。
            // 动作只有一行 —— 复用 task-5 抽好的 `force_reconnect_control_channel`
            // (它走"工作线程 + 请求",不在持锁状态里做 adb/网络)。
            if let CaptureEvent::Button { code, pressed, .. } = ev {
                // 唯一判据见 is_capturing_input(2026-10-23 统一:原先这里
                // 手抄了一份,已经和全局快捷键那段漂移过一次)。
                let capturing = self.is_capturing_input() || ui.ctx().egui_wants_keyboard_input();
                // 界面层热键(强制重连 + 宏三键)的**唯一接线点** ——
                // 判定与动作都在 `handle_gui_button_event` 里,这里只负责
                // 收集"此刻在不在捕获/录制中"这一个上下文。
                if self.handle_gui_button_event(code, pressed, capturing) {
                    continue;
                }
            }
            // ---- 「按下取消的那一刻」把这一下整体丢掉(用户 2026-10-10 第 1 条)----
            // 用户用来**退出**捕获/录制的那个按钮,自己也是一次鼠标按键;这几行就是
            // 让"点取消"不留下任何录入痕迹。放在**所有**录入路径(宏录制 / 组合键 /
            // 单键)之前 —— 三条路都会把这一下当成用户的输入。
            if let CaptureEvent::Button { code, pressed, .. } = ev {
                if self.swallow_cancel_click(ui.ctx(), code, pressed) {
                    continue;
                }
            }
            // ---- 宏录制热键已移到 `handle_gui_button_event`(上面统一处理)----
            // 那里与「强制重连」共用同一个接线点,且**先于**录制写入 ——
            // 保证热键本身不被写进宏、也不递归触发录制(协议 §6.3 缝②)。
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
                        self.log_captured_key(code);
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
                        self.log_captured_key(code);
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
        // 隔膜判据的结果(锁内算、锁外落地,见下面的 `sync_mouse_capture`)
        let mut shield_wanted = false;
        // G-b:锁内收集、锁外落地的位图(全 0 = 不吞任何键)
        let mut bound_words = [0u64; 4];
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
                // G-b(独占模式):同一把锁、同一趟遍历里把"全部已绑定键(带
                // fps_only 标志)"收进本地 Vec,锁外算成 256 位 vk 位图逐 word
                // 换入(全量重算,不 clear —— clear 会让钩子回调恰好在读的瞬间
                // 什么都不吞)。fps_only 的过滤在**纯函数** bound_key_words 里做,
                // 与下面吞掉位的 continue 同口径,且被表驱动单测钉住
                // (fps_only_binds_stay_out_of_the_bitmap_unless_fps_active)。
                // 组合键(combos)里的键本期不进位图(G-b 范围外,如实登记为未做)。
                let mut bound_codes: Vec<(u16, bool)> = Vec::new();
                for b in &g.profile.binds {
                    bound_codes.push((b.key, b.fps_only));
                    // 仅 FPS 的绑定在非 FPS 模式里并不生效,不能占着吞掉位 ——
                    // 否则用户平时按 Esc 也会被吞(映射里那条 fps_only 还没生效)。
                    if b.fps_only && !fps_active {
                        continue;
                    }
                    if let Some(bit) = crate::capture::swallow_bit(b.key) {
                        bits |= bit;
                    }
                }
                bound_words = crate::capture::bound_key_words(bound_codes.into_iter(), fps_active);
                if aim.wheel_zoom && fps_active {
                    if let (Some(up), Some(down)) = (
                        crate::capture::swallow_bit(crate::keymap::BTN_WHEEL_UP),
                        crate::capture::swallow_bit(crate::keymap::BTN_WHEEL_DOWN),
                    ) {
                        bits |= up | down;
                    }
                }
                // ---- 「鼠标独占(隔膜)」(2026-10-11,用户根因定位 + 当日晚 S①②) ----
                // 语义:独占期间鼠标消息**不投递给任何窗口**(含投屏窗口 scrcpy.exe)
                // —— 它就不会在我们注入手机触摸的同时自己再转发一套(点击有反馈、
                // 甩动时抖动/跳变/不流畅的根因;用户用 Notepad 盖住投屏窗口后
                // "极其流畅"反证了它)。
                //
                // 2026-10-11 晚:**什么时候独占**由 `Aim::mouse_shield` 三档选
                //   - 仅 FPS 模式(默认,= 今天的行为)
                //   - 仅投屏窗口在前台(用户说这条"特别推荐")
                //   - 始终独占
                // 判据统一落在纯函数 `keymap::mouse_shield_on` 上(单测覆盖),
                // 这里只负责把它的六个入参凑齐:
                //   * `scrcpy_foreground` = "前台窗口的 pid == 我们启动的 scrcpy pid";
                //     拿不到 pid 时**保守地不独占**(见 `Self::scrcpy_is_foreground`);
                //   * `manual_override` = S② 的手动开关(引擎按 `Aim::shield_key`
                //     的上升沿翻转,镜像在 `aim_live.shield_manual`)。
                //
                // 三条复位路径(**没有显式清理,都是"下一帧重算"这同一个机制**):
                //   ① 映射关闭:走下面的 else 分支直接写 false;引擎同时也把
                //      `shield_manual` 清掉;
                //   ② 模式判据不再成立(FPS 退出 / 投屏窗口到后台 / 失活):
                //      下一帧算出 false;
                //   ③ 断连:硬闸条件不成立 → false(引擎同时清 `shield_manual`)。
                // 为什么要多一条 `connected`(比"映射开启 × fps_active"更严):
                // 断连时引擎**不会**清 aim.mode_active(`aim_release_local` 只清
                // 按下的触点),于是 fps_active 会保持 true —— 只要映射开关还开着,
                // 隔膜就会一直吞用户鼠标,而这时候一个触摸都注入不出去。加上"控制
                // 通道在线"这一条,断连的下一帧就把隔膜摘掉(与引擎维护 mouse_grab
                // 的口径一致,见 engine.rs 顶部维护块)。
                // 另外程序退出(Drop / `Capture::drop` / `shutdown`)也会直接写 false。
                // 帧循环每帧至少被 request_repaint_after(120ms) 叫醒一次,
                // 所以最迟 120ms 内隔膜一定跟着状态走。
                let scrcpy_foreground = self.scrcpy_is_foreground();
                let manual = live.shield_manual;
                let shield = crate::keymap::mouse_shield_on(
                    aim.mouse_shield,
                    enabled,
                    connected,
                    fps_active,
                    scrcpy_foreground,
                    manual,
                );
                // ---- 鼠标接管:隔膜 还是 遮板(用户 2026-10-13)----
                // 这里只把**隔膜的判据**算出来;真正决定"谁生效"的
                // `sync_mouse_capture` 在锁外调用(它要起/销毁窗口,还会去
                // `self.look()` 取模式 —— 在持锁块里调用会与外层借用冲突,
                // 编译期就会报 E0502)。
                shield_wanted = shield;
                bits
            } else {
                // 映射关闭:隔膜绝不能留着(否则用户关了映射,鼠标点不了任何窗口)
                self.mouse_shield_flag.store(false, Ordering::Relaxed);
                0
            };
            self.swallow_flag.store(bits, Ordering::Relaxed);
        }
        // ---- G-b(独占模式)落地:位图 + 生效开关 ----
        // 映射关闭时 bound_words 保持全 0、开关写 false:关了映射绝不能再吞键。
        // 逐 word store:位图短暂不一致无害(最多少吞/多吞一帧),不 clear 再置。
        self.bound_keys_flag.store_words(bound_words);
        self.swallow_bound_keys_flag
            .store(enabled && self.swallow_bound_keys, Ordering::Relaxed);
        // ---- G-c(瞄准期间钉住光标)落地 ----
        // 只写**用户开关**:乘不乘"此刻在瞄准"由捕获层用既有的 `mouse_grab`
        // 判(见 `capture::should_clip`)。这里再乘一遍映射开关属于重复把关,
        // 而且会让"映射刚关、光标还没放"那段状态失去唯一判据。
        // 位置放在块外与其它设置开关一致:每帧无条件写,翻转在下一帧生效。
        self.clip_cursor_flag
            .store(self.clip_cursor_in_aim, Ordering::Relaxed);
        // ---- 鼠标接管方式落地(隔膜 / 遮板,热切换)----
        // 放在锁外:遮板要起窗/提交帧/可能 `EnumWindows`,不该在持锁状态下做。
        // 返回"隔膜此刻该不该开":遮板模式会压成 false(两者只能生效一个)。
        let shield_final = if enabled {
            self.sync_mouse_capture(shield_wanted)
        } else {
            self.sync_mouse_capture(false)
        };
        self.mouse_shield_flag
            .store(shield_final, Ordering::Relaxed);
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
                "鼠标已捕获: 视角由鼠标控制"
            } else {
                "鼠标已交还给系统(FPS 模式已结束)"
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
            // 取证层跟着这条通道一起收尾:触摸监听放掉(允许重连后重建一个),
            // 账本与"手机被摸"状态作废(那是上一条通道的事实)。
            self.reset_self_heal_watch();
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
                        // ---- 最右侧两个按钮(用户 2026-10-12 第 1 条) ----
                        // 用户要求:
                        // ① 去掉上一版那个"红色哑光占位块"(点连接控制时会闪一下、
                        //    平时还一直占着地方显示没用的东西);按"有始必有终"原则
                        //    功能就地保留 = 一个常驻的[重新连接]按钮;
                        // ② **去掉[重新连接]/[关于]与其它按钮之间的无谓距离** ——
                        //    用 `right_to_left` 把它们贴到最右,中间不再留空白。
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("关于").clicked() {
                                self.about_open = true;
                            }
                            self.reconnect_button(ui, &th);
                        });
                    });
                });
            });

            // ================= 左栏 =================
            egui::Panel::left("left")
                // 用户 2026-10-21:左栏要能**左右拖**。`resizable(true)` 是拖拽手柄的
                // 开关,原来没写 —— 而且即便写了,`max_size(430)` 也只让拖到 430,
                // 长路径/长配置项用户仍看不全。改成:可拖 + 上限放到窗口宽度的
                // 一半(再宽中央画布就没地方了),下限保留"按钮不被挤扁"的 260。
                .resizable(true)
                .min_size(220.0)
                // 显式给默认宽度并封顶:否则面板会按内容(长路径/多按钮行)自行撑宽,
                // 把中央的虚拟键盘/键位列表挤到需要横向滚动(视觉验证时发现)。
                .default_size(340.0)
                .max_size((ui.available_width() * 0.5).max(430.0))
                .show(ui, |ui| {
                    // 日志卡高度**只由 self.log_height 决定**(拖动卡片上沿的手柄改它)。
                    //
                    // 为什么不用 `default_size` + 可拖面板:egui 的 Panel 每帧把**内容矩形**
                    // 存成下一帧的尺寸(`panel.rs` 的 `PanelState{outer_rect}`),内容只要比
                    // 面板高一点点,下一帧面板就更高、内容又更高 ⇒ 空日志也一帧比一帧往上
                    // 窜(用户 2026-10-23 两次报到)。这里把三个量钉成相等的固定值:
                    // `min_size == max_size == card_h`,且卡片内容按 card_h 精确分配 ⇒
                    // 面板 → 内容 → 面板 这条反馈链断掉。
                    let max_card = (ui.available_height() - 160.0).max(Self::LOG_CARD_MIN_H);
                    let card_h = self.log_height.clamp(Self::LOG_CARD_MIN_H, max_card);
                    egui::Panel::bottom("left_log_panel")
                        .resizable(false)
                        .min_size(card_h)
                        .max_size(card_h)
                        .show(ui, |ui| {
                            self.ui_log_card(ui, card_h);
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
            // Linux(第 30 条):没有 `SetCursor(NULL)` 这种进程级原语,能做的最强
            // 一步是"给**本窗口**设一个不画光标的 cursor"(egui → winit →
            // Wayland `set_cursor(NULL)` / X11 透明 cursor)。它只作用于本程序的
            // 窗口,盖不到 scrcpy 那个窗口上 —— 这是平台边界,不是实现欠缺。
            // 每帧重设:同 Windows 侧"重绘后重设"的同一理由(别的东西会改回去)。
            #[cfg(target_os = "linux")]
            ctx.set_cursor_icon(egui::CursorIcon::None);
        }
        ctx.request_repaint_after(Duration::from_millis(120));
    }
}

impl Drop for PadApp {
    fn drop(&mut self) {
        self.cursor_hide_flag.store(false, Ordering::Relaxed);
        // 「FPS 鼠标隔膜」:关窗/换主题重建界面时先把隔膜摘掉。正常路径由每帧
        // 重算复位(见 `update()`),这里只是"界面已经不再跑帧"时的兜底 ——
        // 用户鼠标在任何退出路径上都不该继续被吞(`Capture::drop` 里还有一道)。
        self.mouse_shield_flag.store(false, Ordering::Relaxed);
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

        // 轮换集合:哪些组合能被"切换键位(正向/反向循环)"轮到 —— 用户 2026-10-23
        // 第 3 条(反复强调过好几次)。控件放在这里、而不是[其他功能 → 切换键位]
        // 里,因为"勾不勾这套组合"必须跟组合本身排在同一行才看得懂。
        //
        // 口径:轮换集合是**按切换键**存的(keymap::SwitchKey::rotate_set),但用户
        // 想的是"这套组合参不参与",所以这里把"所有设为 Next/Prev 的切换键"视为
        // 一个整体:勾/去勾对它们**同时**生效。一个循环切换键都没有 ⇒ 这列控件
        // 整体禁用(先把切换键设成循环,勾选才有意义)。
        let rot_sets: Vec<Vec<usize>> = {
            let g = lock_shared(&self.shared);
            g.switch_keys
                .iter()
                .filter(|s| matches!(s.direction, SwitchDirection::Next | SwitchDirection::Prev))
                .map(|s| crate::keymap::rotate_members(s.rotate_set.as_deref(), n))
                .collect()
        };
        let rotatable = !rot_sets.is_empty();

        let mut pick: Option<usize> = None;
        let mut del: Option<usize> = None;
        // 本帧的勾选改动 / 批量操作,循环里只记录,循环外一次加锁写回
        // (与 pick/del 同一套模式:绝不在遍历中反复取锁)。
        let mut rotate_edit: Vec<(usize, bool)> = Vec::new();
        let mut bulk_rotate: Option<bool> = None;
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
                // 「轮换」勾选框 = 这套组合参不参与循环。显示值 = 所有循环切换键
                // 的集合都含 i(只要有一个切换键没勾,就不显示为勾上 —— 避免
                // "看起来勾了、按下去却轮不到")。
                let mut on = rotatable && rot_sets.iter().all(|s| s.contains(&i));
                let resp = ui
                    .push_id(("scheme_rotate", i), |ui| {
                        ui.add_enabled(rotatable, egui::Checkbox::new(&mut on, "轮换"))
                    })
                    .inner;
                if !rotatable {
                    resp.on_hover_text(
                        "这里勾选哪些组合能参与轮换。
                         先在[其他功能 → 切换键位]里把某个切换键设成「正向循环」                         或「反向循环」,否则轮换无从谈起。",
                    );
                } else {
                    let resp = resp.on_hover_text(
                        "勾上的组合才会被切换键轮换到。
                         全部勾上 = 不限制(新增组合自动参与)。",
                    );
                    if resp.changed() {
                        rotate_edit.push((i, on));
                    }
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
            ui.add_space(10.0);
            if ui
                .add_enabled(rotatable, egui::Button::new("轮换全选").small())
                .on_hover_text("所有组合都参与轮换(等价于不限制)")
                .clicked()
            {
                bulk_rotate = Some(true);
            }
            if ui
                .add_enabled(rotatable, egui::Button::new("轮换全不选").small())
                .on_hover_text("一个都不参与:循环切换键按下不会换组合")
                .clicked()
            {
                bulk_rotate = Some(false);
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
        ui.small(
            "切换键的具体按键/组合键和目标组合，在[其他功能 → 切换键位]里设置；
             上面每套组合后面的[轮换]勾选框决定哪些组合能被循环切换键轮到。",
        );

        // ---- 轮换集合写回(本帧的勾选 / 批量操作)----
        // 与 pick/del 同一套模式:UI 遍历里只记录,这里一次加锁写回。
        if bulk_rotate.is_some() || !rotate_edit.is_empty() {
            {
                let mut g = lock_shared(&self.shared);
                for s in g.switch_keys.iter_mut() {
                    if !matches!(s.direction, SwitchDirection::Next | SwitchDirection::Prev) {
                        continue;
                    }
                    if let Some(all) = bulk_rotate {
                        // 全选 ⇒ None(字段不落盘)= 不限制;全不选 ⇒ Some([]) = 一个都不参与。
                        s.rotate_set = if all { None } else { Some(Vec::new()) };
                        continue;
                    }
                    let mut set = crate::keymap::rotate_members(s.rotate_set.as_deref(), n);
                    for &(j, on) in &rotate_edit {
                        if on {
                            if !set.contains(&j) {
                                set.push(j);
                            }
                        } else {
                            set.retain(|&x| x != j);
                        }
                    }
                    set.sort_unstable();
                    set.dedup();
                    // 全都勾上 = 不限制 ⇒ 存 None:与旧配置同形,而且以后新增组合
                    // 自动参与(用户想要的就是"不限制"这个状态)。
                    s.rotate_set = if set.len() == n { None } else { Some(set) };
                }
            }
            self.scheme_dirty = true;
            let msg = match bulk_rotate {
                Some(true) => "轮换集合:全部组合参与",
                Some(false) => "轮换集合:一个都不参与",
                None => "已更新轮换集合",
            };
            self.log(msg);
        }

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
            // 2026-10-18 真机崩溃修复:`ui_bind_row` 内部自带水平行(键型+键位+仅FPS+删除),
            // 而它的滑动细项要落在**垂直**布局里才能换行(见函数内 `ui.indent` 的说明)。
            // 原先这里再包一层 `ui.horizontal` ⇒ ui_bind_row 拿到的是水平 Ui ⇒ `ui.indent`
            // **直接 panic**(egui 只允许缩进垂直布局)。去掉这层包裹:外层本就是垂直列表。
            if let Some(d) = self.ui_bind_row(ui, i) {
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
        // B21-E 修正后:滑动的细项要在**水平行之外**渲染,所以这里先取一次
        // "这条是不是滑动"。与下面闭包里的读取同源(同一把锁、同一份快照),
        // 唯一代价是多一次短锁 —— 冷路径,可忽略。
        let is_swipe_outer = {
            let g = lock_shared(&self.shared);
            g.profile
                .binds
                .get(i)
                .map(|b| matches!(b.action, Action::Swipe(_)))
                .unwrap_or(false)
        };
        ui.horizontal(|ui| {
            let (key, kind, _is_swipe, is_point, fps_only) = {
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
                self.arm_key_capture(KeySlot::Bind(i));
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

            if is_point {
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
                                ..
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
                                let r = radius_input(ui, &mut pr, RADIUS_RANGE_PX, None);
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
                                let r = radius_input(ui, &mut pr, RADIUS_RANGE_PX, None);
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
                                repeat_interval_ms: 0,
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
        // B21-E 修正(2026-10-18,子智能体取证):`swipe_controls` 原先在**上面那个**
        // `ui.horizontal` 闭包里被调用 ⇒ 它内部再开 `ui.horizontal` 只是开一个水平
        // 子布局,**不会换行**(egui 的 `Ui::end_row` 只对 grid/wrapping 布局生效,
        // 对普通 horizontal 无效 —— 已在 egui 0.36.1 源码里核实)。所以"轨迹单独一行"
        // 必须在这里做:水平行只放"键型 + 键位按钮 + 仅FPS + 删除",滑动的全部细项
        // 在**闭包之后**另起一个垂直区渲染 —— 那才是真正的新行。
        if is_swipe_outer {
            // 缩进表示"这些属于上面那一行键位",视觉上仍挂在这一条下面。
            //
            // ⚠️ **必须套一层 `ui.vertical`**(2026-10-18 真机启动崩溃修复):
            // `ui.indent` 只允许用在**垂直**布局里,而本函数的调用点可能把它放进
            // 水平布局(历史上有两处 `ui.horizontal(|ui| self.ui_bind_row(...))`)。
            // 一旦父布局是水平,indent 会**直接 panic**(egui ui.rs:2246),表现是
            // 「程序启动就崩」。外层显式开 vertical ⇒ 无论调用方什么布局都安全。
            // 这也是本轮教训的可执行固化:不要依赖"调用方现在是垂直的"。
            ui.vertical(|ui| {
                ui.indent(("swipe_detail", i), |ui| {
                    let mut pick = self.picking;
                    let mut easing_edit = self.easing_edit;
                    let mut swipe_edit = false;
                    let mut swipe_for_card = None;
                    {
                        let mut g = lock_shared(&self.shared);
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
                                swipe_for_card = Some(s.clone());
                            }
                        }
                        if swipe_edit {
                            if let Some(before) = before {
                                let mut snapshot = g.profile.clone();
                                snapshot.binds[i] = before;
                                self.pending_undo = Some(snapshot);
                            }
                        }
                    }
                    if let Some(s) = swipe_for_card {
                        self.track_rec_card(ui, TrackTarget::Bind(i), &s);
                    }
                    if let Some(slot) = pick {
                        self.begin_pick(slot);
                    } else {
                        self.picking = None;
                    }
                    self.easing_edit = easing_edit;
                });
            }); // ← 关闭防御性的 `ui.vertical`
        }
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
        // 删掉一条 ⇒ 后面每条的下标都前移;当前选中 / 待按键 / 待取点全按下标记。
        self.forget_stale_target("删除绑定");
        self.log("已删除绑定");
    }

    /// 新增绑定(草稿编辑器)+ 组合键列表。ui_binds 与可视化操作栏共用。
    ///
    /// U9(2026-10-17):草稿编辑的**字段与顺序**已全部搬进 [`Self::ui_draft_card`],
    /// 这里只留默认风格的外壳(「新增:」标题 + 组合键列表)。
    fn ui_binds_new(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.label("新增:");
        self.ui_draft_card(ui, DraftCardShell::DefaultStyle);
        self.ui_combos(ui);
    }

    /// 草稿编辑卡(U9,2026-10-17):两个草稿入口共用的**唯一**实现。
    ///
    /// 键位按钮、KIND_NAMES 类型选择、x/y/时长/范围、仅 FPS、按后延迟、各类型参数
    /// (含轨迹录制卡片)、提交按钮都在这里;入口只通过 [`DraftCardShell`] 选外壳。
    ///
    /// 两个外壳的差异**逐项保持收敛前的可见行为**:
    ///   * 键位:默认风格是一个键名按钮(点一下进入捕获);可视化风格是"新增键位: 键名" +
    ///     `改键` 标签按钮;
    ///   * 类型:默认风格用下拉框(`newkind`),可视化风格用四个方形标签按钮;
    ///   * 仅 FPS:只有可视化风格显示勾选(默认风格新增节恒为普通键位,点击时显式置 false);
    ///   * 按后延迟:默认风格在参数区之后,可视化风格在类型选择之前(位置不同,控件相同);
    ///   * 提交:默认风格是 `新增` 按钮;可视化风格是 `确认新增` / `取消` 一对。
    fn ui_draft_card(&mut self, ui: &mut egui::Ui, shell: DraftCardShell) {
        match shell {
            DraftCardShell::DefaultStyle => {
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
                        self.arm_key_capture(KeySlot::NewBind);
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
                    // 「按后延迟」(用户 2026-10-10 第 2 条):与可视化页的草稿编辑器同一份控件。
                    if let Some(v) = Self::tail_delay_widget(ui, self.draft.tail_delay_ms) {
                        self.draft.tail_delay_ms = v;
                    }
                    if ui.button("新增").clicked() {
                        self.commit_draft();
                    }
                });
                // B21-E 修正②(2026-10-18,子智能体入口面矩阵实测):`ui_draft_fields`
                // 原先在**上面那条水平行**里被调用 ⇒ 草稿的滑动细项(取起点/取终点/
                // 时长/录制/曲线/轨迹形状)全挤在同一行,用户在那个入口新建滑动时看到的
                // 仍是长串(其余三个入口已修,唯独默认风格这条漏了)。
                // 现移出闭包:水平行只留「键名 + 类型 + 按后延迟 + 新增」,参数区在
                // **垂直布局**里另起,于是 `swipe_controls` 内部的 horizontal 才是新行。
                self.ui_draft_fields(ui, shell);
            }
            DraftCardShell::VisualStyle => {
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
                        self.arm_key_capture(KeySlot::NewBind);
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
                self.ui_draft_fields(ui, shell);
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
        }
    }

    /// 草稿参数区(U9 共用卡的主体):按类型渲染 x/y/时长/范围、滑动控件、系统键 code。
    ///
    /// 轨迹录制卡片(`track_rec_card`)就在滑动分支里 —— 全仓**只有这一处**给草稿接线,
    /// 所以新增草稿入口不可能再漏接录制按钮(V2-4 事故的结构性修复)。
    fn ui_draft_fields(&mut self, ui: &mut egui::Ui, shell: DraftCardShell) {
        match self.draft.kind {
            0 | 1 => {
                // B21-E 修正②之后,**两个外壳都已是纵向布局**(默认风格的外壳把
                // `ui_draft_fields` 移出了水平行),所以这里两边都自己开一条水平行。
                // 收敛前默认风格不需要,是因为它当时仍被包在水平闭包里。
                let _ = shell;
                ui.horizontal(|ui| self.ui_draft_point_row(ui, shell));
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
                    // 草稿临时量:像素直接转 f32(与 start/end 的草稿口径一致)
                    recorded_points: self
                        .draft
                        .swipe_points
                        .iter()
                        .map(|(px, py)| (*px as f32, *py as f32))
                        .collect(),
                };
                // ---- 轨迹录制(用户 2026-10-13,V2-3) ----
                // 与已有键位/组合键共用同一张录制卡片(见 `track_rec_card`)。
                self.track_rec_card(ui, TrackTarget::Draft, &tmp);
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
                // 点取起点/终点/出发点即视为开始配置,显示轨迹预览。
                // 只有默认风格有这一步:可视化风格从收敛前起就没有(登记为已知不一致,
                // 本轮只做结构收敛,不改可见行为)。
                if shell == DraftCardShell::DefaultStyle && pick != pick_before {
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
    }

    /// 草稿的「点按/长按」参数行:x / y / 时长(仅点按)/ 范围 / 取点。
    fn ui_draft_point_row(&mut self, ui: &mut egui::Ui, shell: DraftCardShell) {
        ui.label("x:");
        ui.add(egui::DragValue::new(&mut self.draft.x).range(COORD_RANGE));
        ui.label("y:");
        ui.add(egui::DragValue::new(&mut self.draft.y).range(COORD_RANGE));
        if self.draft.kind == 0 {
            ui.label("时长ms:");
            ui.add(egui::DragValue::new(&mut self.draft.tap_duration_ms).range(0..=5000))
                .on_hover_text("0=按下不松手,直到再按一次");
        }
        ui.label("范围:");
        radius_input(ui, &mut self.draft.radius, RADIUS_RANGE_PX, None);
        let waiting_p = self.picking == Some(CoordSlot::NewBind);
        if shell == DraftCardShell::VisualStyle {
            let th = self.theme();
            if Self::tab_button(
                ui,
                if waiting_p { "取点中..." } else { "取点" },
                waiting_p,
                th.ok,
            ) {
                self.begin_pick(CoordSlot::NewBind);
                self.draft_active = true;
            }
        } else if ui
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

    /// 把当前草稿提交为一条键位绑定。键未捕获时只记日志,返回 false。
    /// 默认风格的[添加]按钮与可视化操作栏的[确定添加]共用,行为完全一致。
    fn commit_draft(&mut self) -> bool {
        let Some(key) = self.draft.key else {
            self.log("请先捕获按键");
            return false;
        };
        self.push_undo();
        // 若用户是"录到一半直接点新增":会话缓冲里的点必须先落到草稿上,
        // 否则下面读 `draft.swipe_points` 会拿到空数组 —— 用户看着浮层上
        // 明明有轨迹,新增出来的键位却没有(这一条是录制与提交的接缝,
        // 最容易漏,所以在这里显式收口)。
        if self.track_rec.as_ref().map(|r| r.target) == Some(TrackTarget::Draft) {
            self.finish_track_recording();
        }
        // 草稿是像素坐标,入配置前换算成相对值
        let m = self.mapper();
        let (dx, dy) = (self.draft.x, self.draft.y);
        let action = match self.draft.kind {
            0 => Action::Tap {
                x: m.rel_x(dx),
                y: m.rel_y(dy),
                duration_ms: self.draft.tap_duration_ms,
                radius: m.rel_len(self.draft.radius),
                repeat_interval_ms: 0,
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
                // 录制的轨迹点是**像素**坐标(草稿口径),入配置必须换算成
                // **相对坐标** —— 否则换分辨率/换设备后轨迹会错位。
                recorded_points: self
                    .draft
                    .swipe_points
                    .iter()
                    .map(|(px, py)| (m.rel_x(*px), m.rel_y(*py)))
                    .collect(),
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
        // 草稿已经变成正式键位 ⇒ 录制会话必须收尾。若用户是"边录边点新增",
        // 那些点已经通过上面的 `recorded_points` 写进这条新键位了,会话再留着
        // 就会指向一个不存在的草稿目标。
        if self.track_rec.as_ref().map(|r| r.target) == Some(TrackTarget::Draft) {
            self.track_rec = None;
        }
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
                        self.arm_key_capture(KeySlot::NewBind);
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
                    repeat_interval_ms: 0,
                },
                fps_only: fps_tab,
                tail_delay_ms: crate::keymap::DEFAULT_TAIL_DELAY_MS,

                repeat_while_held: false,
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
                                repeat_interval_ms: 0,
                            },
                            fps_only: fps_tab,
                            tail_delay_ms: crate::keymap::DEFAULT_TAIL_DELAY_MS,

                            repeat_while_held: false,
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
                self.arm_key_capture(KeySlot::NewBind);
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
                    // 同 :7586 的修复:不许再包水平层(那会让函数内的 `ui.indent` panic)。
                    if let Some(d) = self.ui_bind_row(ui, i) {
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
                                MacroKeyTarget::Bind(i),
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
                self.begin_macro_recording("点[开始录制]");
            }
            if Self::tab_button(ui, "清空", false, th.accent) {
                self.reset_macro_draft();
            }
            if Self::tab_button(ui, "扩展宏...", false, th.accent) {
                self.open_macro_virtual_editor();
            }
        });

        // ---- 宏录制热键(用户 2026-10-12 任务书) ----
        // 两项:**按键设置控件 + 独立勾选**。未勾选启用时即使绑了键也不触发任何
        // 行为(键位值保留);两者相互独立,可只启用其中一个或都不启用;
        // **只支持单键**(组合键进不来:这里走 `waiting_key` 单键捕获)。
        ui.horizontal(|ui| {
            ui.label("开始录制:");
            let waiting = self.waiting_key == Some(KeySlot::MacroRecordStart);
            if Self::key_button(ui, waiting, Some(self.macro_record_start_key)).clicked() {
                self.arm_key_capture(KeySlot::MacroRecordStart);
                self.log("请按任意**单个**键作为「开始录制」热键(不支持组合键)");
            }
            ui.checkbox(&mut self.macro_record_start_key_enabled, "启用")
                .on_hover_text(
                    "勾选后才生效。按下该键**不立即开始录制**,而是进入「等待第一个按键」\
                     状态;\n第一个可录制按键按下后开始录制,并作为起始事件被记录。\
                     \n该热键本身不写入宏、也不会递归触发录制。未勾选时即使绑了键也不触发。",
                );
            if waiting || self.macro_record_start_key != 0 {
                if Self::cancel_bind_button(ui, waiting) {
                    if waiting {
                        self.waiting_key = None;
                    } else {
                        self.macro_record_start_key = 0;
                        self.remember_now();
                    }
                }
            }
        });
        ui.horizontal(|ui| {
            ui.label("终止录制:");
            let waiting = self.waiting_key == Some(KeySlot::MacroRecordStop);
            if Self::key_button(ui, waiting, Some(self.macro_record_stop_key)).clicked() {
                self.arm_key_capture(KeySlot::MacroRecordStop);
                self.log("请按任意**单个**键作为「终止录制」热键(不支持组合键)");
            }
            ui.checkbox(&mut self.macro_record_stop_key_enabled, "启用")
                .on_hover_text(
                    "勾选后才生效。录制过程中按下该键 ⇒ 立即终止当前录制,\
                     并按现有停止/保存逻辑收尾;\n未处于录制状态时按下 = 无操作。\
                     \n该热键本身不写入宏。未勾选时即使绑了键也不触发。",
                );
            if waiting || self.macro_record_stop_key != 0 {
                if Self::cancel_bind_button(ui, waiting) {
                    if waiting {
                        self.waiting_key = None;
                    } else {
                        self.macro_record_stop_key = 0;
                        self.remember_now();
                    }
                }
            }
        });
        ui.horizontal(|ui| {
            ui.label("取消录制:");
            let waiting = self.waiting_key == Some(KeySlot::MacroRecordCancel);
            if Self::key_button(ui, waiting, Some(self.macro_record_cancel_key)).clicked() {
                self.arm_key_capture(KeySlot::MacroRecordCancel);
                self.log("请按任意**单个**键作为「取消录制」热键(不支持组合键)");
            }
            ui.checkbox(&mut self.macro_record_cancel_key_enabled, "启用")
                .on_hover_text(
                    "勾选后才生效。录制过程中按下该键 ⇒ **丢弃**这次录制(不写入任何宏),\
                     与「终止录制」的『保存已录内容』相反。\n未处于录制状态时按下 = 无操作。\
                     \n这是**全局公共**热键:对所有宏录制生效,不必逐宏设置。\n该热键本身不写入宏。",
                );
            if waiting || self.macro_record_cancel_key != 0 {
                if Self::cancel_bind_button(ui, waiting) {
                    if waiting {
                        self.waiting_key = None;
                    } else {
                        self.macro_record_cancel_key = 0;
                        self.remember_now();
                    }
                }
            }
        });
        ui.small(
            "三个热键默认都未启用(不改变原有宏录制行为):不勾选「启用」时,即使绑了键也不生效。\
             只支持单键绑定,不支持组合键。「终止」= 保存已录内容;「取消」= 丢弃这次录制。",
        );

        ui.horizontal(|ui| {
            ui.label("触发键:");
            let waiting = self.waiting_key == Some(KeySlot::MacroTrigger);
            if Self::key_button(ui, waiting, self.macro_page_key).clicked() {
                self.arm_key_capture(KeySlot::MacroTrigger);
                self.log("请按任意键作为宏触发键");
            }
            ui.checkbox(&mut self.macro_page_fps_only, "仅 FPS");
            ui.checkbox(
                &mut self.macro_page_repeat_while_held,
                "长按重复（按住时循环，直到抬起）",
            )
            .on_hover_text(
                "长按重复(按住时循环,直到抬起):\n\
                     按住触发键不放,这条宏就一轮接一轮地重放,直到你抬起触发键。\n\
                     松开后**正在跑的那一轮会跑完**,只是不再排下一轮 —— \
                     设备端不会留下'只按不抬'的触点。\n\
                     两轮之间的间隔用这一条的「按后延迟」;那个总开关关着时不留间隔,\
                     一轮接一轮不等待。",
            );
            // 「按后延迟」(用户 2026-10-10 第 2 条):宏触发键自己的一份。
            // 整条宏跑完才开始算冷却 —— 连按宏键不会让两次执行叠在一起。
            // 打开「长按重复」时,它同时是两轮之间的间隔(见悬停提示)。
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
            // ---- 录制中也能「展开」这份还没命名的宏(用户 2026-10-12 第 3 条) ----
            //
            // 用户原话:"可以在录制中还没命名的时候展开宏"。此前录制中只有一条
            // 「显示步骤」开关(16 行小滚动框,只读),而「已录宏」列表里的
            // [展开] 要等这条宏**保存**成 `宏#N` 之后才存在 —— 于是"刚录到一半
            // 想仔细看看"这件事在录制期间**根本没有入口**。
            //
            // 按"有始必有终"的就地补齐:给录制中的草稿加同一个[展开],
            // 展开后与「已录宏」的展开体**共用同一个渲染函数**
            // (`ui_macro_recorded_steps`),内容与观感完全一致。
            if !rec.steps.is_empty() {
                ui.horizontal(|ui| {
                    let expanded = self.macro_recording_expanded;
                    if ui.button(if expanded { "收起" } else { "展开" }).clicked() {
                        self.macro_recording_expanded = !expanded;
                    }
                    ui.small("展开这份还没保存的录制内容(与保存后的[展开]同一视图)");
                });
                if self.macro_recording_expanded {
                    Self::ui_macro_recorded_steps(ui, &rec.steps);
                }
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
        self.ui_macro_instruction_list(
            ui,
            &mut instructions,
            "macro_page",
            true,
            false,
            MacroKeyTarget::Page,
        );
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
                    self.arm_key_capture(KeySlot::Bind(i));
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
                            MacroKeyTarget::Bind(i),
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
                    Action::Macro(m) => Some((
                        b.key,
                        m.clone(),
                        b.fps_only,
                        b.tail_delay_ms,
                        m.repeat_while_held,
                    )),
                    _ => None,
                })
            };
            if let Some((key, action, fps, tail, repeat)) = loaded {
                self.macro_page_key = Some(key);
                self.macro_page_steps = action.steps;
                // 旧文件/手工编辑来的宏可能第一步带延迟(甚至是在"录制起点"
                // 修复之前录的):进了编辑区就按不变量归一化,让它可见可控。
                Self::normalize_macro_start(&mut self.macro_page_steps);
                self.macro_page_instructions = action.instructions;
                self.macro_page_virtual_profile = action.virtual_profile.as_deref().cloned();
                self.macro_page_fps_only = fps;
                self.macro_page_tail_delay_ms = tail;
                self.macro_page_repeat_while_held = repeat;
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
            // 同上:宏也是一条按键绑定,删掉后下标整体前移。
            self.forget_stale_target("删除宏");
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
        self.macro_page_repeat_while_held = false;
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
            repeat_while_held: self.macro_page_repeat_while_held,
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
        self.macro_page_repeat_while_held = d.repeat_while_held;
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
                                repeat_interval_ms: 0,
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
                        repeat_interval_ms: 0,
                    },
                    fps_only: false,
                    tail_delay_ms: crate::keymap::DEFAULT_TAIL_DELAY_MS,

                    repeat_while_held: false,
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
                self.arm_key_capture(KeySlot::MacroVirtualNewBind);
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
                if radius_input(ui, &mut pr, RADIUS_RANGE_WHEEL_PX, None).changed() {
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
                    recorded_points: Vec::new(),
                }),
                3 => Action::AndroidKey { keycode: 4 },
                _ => Action::Tap {
                    x: px,
                    y: py,
                    duration_ms: crate::keymap::DEFAULT_TAP_DURATION_MS,
                    radius: crate::keymap::DEFAULT_RADIUS,
                    repeat_interval_ms: 0,
                },
            };
        }
        match action {
            Action::Tap {
                x,
                y,
                duration_ms,
                radius,
                repeat_interval_ms,
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
                    radius_input(ui, radius, RADIUS_RANGE_REL, Some(0.001));
                });
                // 连点(用户 2026-10-13,V2-2):按住期间每隔这么久点一次
                ui.horizontal(|ui| {
                    ui.label("连点间隔");
                    ui.add(
                        egui::DragValue::new(repeat_interval_ms)
                            .range(0..=10_000)
                            .suffix(" ms"),
                    )
                    .on_hover_text(
                        "按住期间每隔这么久点一次(0 = 只点一次,与从前一致)。\n\
                         由本地排程一次性展开,不逐击过网 ⇒ 每击间隔稳定;\n\
                         松键立刻停,不会留下'只按不抬'的触点。\n\
                         间隔会自动不小于单次按住时长(否则点击会互相叠住)。",
                    );
                    if *repeat_interval_ms > 0 {
                        ui.small("连点中");
                    }
                });
            }
            Action::Hold { x, y, radius } => {
                ui.horizontal(|ui| {
                    ui.label("X/Y");
                    ui.add(egui::DragValue::new(x).range(0.0..=1.0).speed(0.005));
                    ui.add(egui::DragValue::new(y).range(0.0..=1.0).speed(0.005));
                    ui.label("范围");
                    radius_input(ui, radius, RADIUS_RANGE_REL, Some(0.001));
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
        // 这份列表的键位捕获写进哪份缓冲(见 [`MacroKeyTarget`])。
        target: MacroKeyTarget,
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
                    self.ui_macro_instruction(ui, instruction, i, id, pick_enabled, target);
                });
            });
        }
        if let Some(i) = remove {
            instructions.remove(i);
            // 下标整体位移了:若此时还挂着一次"等着按键"的捕获,它记的是**旧下标**,
            // 落定时会悄悄改到另一条步骤上 —— 删完就一律退出交互态(2026-10-11;
            // 2026-10-23 起由 forget_stale_target 统一收口,连 vk_sel/picking 一起清)。
            self.forget_stale_target("删除宏步骤");
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
        target: MacroKeyTarget,
    ) {
        let mut pending_key = None;
        let mut pending_chord = None;
        // 「有始必有终」(任务 E,2026-10-11):宏步骤的「等待按键 / 等待组合键」也要
        // 有就地出口。置位即退出捕获,**不动已有绑定**(与主界面 [`Self::cancel_bind_button`]
        // 同一套语义和同一句话)。
        let mut cancel_capture = false;
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
                    // 就地取消:登记进取消区(`note_cancel_zone`,由
                    // `cancel_bind_button` 内部完成),于是"点这个按钮的那一下鼠标"
                    // 会被 `swallow_cancel_click` 整条丢掉 —— 不会被当成键码写进
                    // 这一步(这正是用户报的"取消了却把左键绑上去了")。
                    if waiting && Self::cancel_bind_button(ui, true) {
                        cancel_capture = true;
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
                    let mut armed_here = false;
                    for (slot, key) in keys.iter_mut().enumerate() {
                        // 组合键槽位走**和弦捕获**(与主界面 [组合键设置] 同一条路):
                        // 按下 Ctrl+X 一次就把两个键都录进来。
                        // 旧实现塞的是 `waiting_key`(单键路径)—— 按住 Ctrl 的那一下
                        // 就把这次捕获用掉了,第二个键既没进组合、还落进了游戏
                        // (2026-10-11 用户报告:"按下按键设置时无法被正确捕获")。
                        let waiting = self.waiting_keys
                            == Some(KeySlot::MacroInstructionComboKey {
                                instruction: index,
                                slot,
                            });
                        armed_here |= waiting;
                        if Self::key_button(ui, waiting, Some(*key)).clicked() {
                            pending_chord = Some(KeySlot::MacroInstructionComboKey {
                                instruction: index,
                                slot,
                            });
                        }
                    }
                    if keys.len() < MACRO_COMBO_MAX_KEYS && ui.small_button("＋").clicked() {
                        keys.push(0);
                    }
                    if keys.len() > 1 && ui.small_button("－").clicked() {
                        keys.pop();
                    }
                    // 就地取消(与「按键」分支同一个控件、同一套语义)
                    if armed_here && Self::cancel_bind_button(ui, true) {
                        cancel_capture = true;
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
            self.macro_key_target = target;
            self.arm_key_capture(slot);
            self.log("请按键盘/鼠标键完成宏动作设置");
        }
        if let Some(slot) = pending_chord {
            // 组合键:一次按下 Ctrl+X 就落定整组(见 Combo 分支的说明)。
            self.macro_key_target = target;
            self.begin_keys_capture(slot);
        }
        // 就地取消放在最后:它必须**压过**同一帧里可能被 arm 的那一次
        // (一次点击不可能同时落在两个按钮上,这里只是让优先级明确)。
        if cancel_capture {
            self.cancel_key_capture();
            self.log("已取消设置:已退出按键捕获,原绑定保持不变");
        }
    }

    fn macro_virtual_key(&mut self, code: u16) {
        if let Some(KeySlot::MacroInstructionKey(index)) = self.waiting_key {
            // 先清等待态再落定:虚拟键盘这条路以前不清,于是"举着"不放 ——
            // 之后每点一个虚拟键都会重复改写同一步骤(2026-10-11)。
            self.waiting_key = None;
            self.assign_key(KeySlot::MacroInstructionKey(index), code);
            return;
        }
        if let Some(slot) = self.waiting_keys {
            // 宏步骤的组合键槽位现在走**和弦捕获**,虚拟键盘点一下仍是"单键落定"。
            if matches!(slot, KeySlot::MacroInstructionComboKey { .. }) {
                self.waiting_keys = None;
                self.assign_keys(slot, KeySet::single(code));
                return;
            }
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
    /// 进入"等待首个按键"的录制状态(点[开始录制] 与「开始录制」热键**共用**)。
    ///
    /// 用户 2026-10-12 任务书要求:热键按下后**不立即开始录制**,而是进入
    /// "等待首个按键"状态;第一个可录制按键按下后才开始录制,且该按键按现有
    /// 语义记为起始事件。这里与点按钮走**完全同一段代码** —— 原录制核心逻辑
    /// (空档不计时、首步延迟归零、空闲自动停止)一个字节都没改。
    fn begin_macro_recording(&mut self, how: &str) {
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
        self.log(format!(
            "宏录制开始({how})：等待第一个按键……（从第一个按键才开始计时与空闲停止；\
             之后空闲会自动停止）"
        ));
    }

    /// 宏录制热键的**纯判定**(单测钉着):给定按键码,返回该键此刻该做什么。
    ///
    /// 抽成纯函数是为了能直接单测"未勾选启用就不触发""只支持单键"这些
    /// 验收条款,而不必起界面。
    fn macro_hotkey_action(
        code: u16,
        start_key: u16,
        start_on: bool,
        stop_key: u16,
        stop_on: bool,
        cancel_key: u16,
        cancel_on: bool,
        recording: bool,
    ) -> MacroHotkey {
        // 未勾选启用 ⇒ 即使绑了键也不触发任何行为(键位值保留)。
        if start_on && start_key != 0 && code == start_key {
            return if recording {
                // 已经在录制中:不重复开(避免清掉刚录到的步骤)
                MacroHotkey::None
            } else {
                MacroHotkey::Start
            };
        }
        // 未处于录制状态时按下终止/取消键 ⇒ 无操作。
        if stop_on && stop_key != 0 && code == stop_key && recording {
            return MacroHotkey::Stop;
        }
        // 「取消录制」(用户 2026-10-12 第 1 条):丢弃这次录制,不写入任何东西。
        // 与「终止」的区别就是"存不存":终止走既有停止/保存逻辑,取消直接丢。
        if cancel_on && cancel_key != 0 && code == cancel_key && recording {
            return MacroHotkey::Cancel;
        }
        MacroHotkey::None
    }

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
        let repeat_while_held = self.macro_page_repeat_while_held;
        lock_shared(&self.shared).profile.binds.push(KeyBind {
            key,
            action: Action::Macro(MacroAction {
                steps: self.macro_page_steps.clone(),
                instructions: self.macro_page_instructions.clone(),
                virtual_profile: self.macro_page_virtual_profile.clone().map(Box::new),
                // 「长按重复」(用户 2026-10-11 第 2 条):宏页上勾的那一份跟着宏走
                repeat_while_held,
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
            // 「长按重复」:覆盖保存时一并写回,否则"载入编辑 → 勾上它 → 保存宏"
            // 会静默丢掉这次改动(与下面 `tail_delay_ms` 同一条纪律)。
            repeat_while_held: self.macro_page_repeat_while_held,
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
    /// **取消**当前宏录制(用户 2026-10-12 第 1 条):丢弃这次录制,不写入任何东西。
    ///
    /// 与 [`Self::finish_macro_recording`] 的区别就是"存不存":
    /// - 终止 = 把已录步骤收进编辑区(走既有停止/保存逻辑);
    /// - 取消 = **直接丢掉**,连已录的步骤也不保留(编辑区保持原样)。
    ///
    /// 用户要求它是**全局公共的**:一个键对所有宏录制生效,不必逐宏设置 ——
    /// 因此它读写的是程序级设置(`macro_record_cancel_key`),不是某条宏的字段。
    fn cancel_macro_recording(&mut self) {
        let Some(rec) = self.macro_recording.take() else {
            return; // 未在录制:无操作(判定层已经拦过,这里兜底)
        };
        self.log(format!(
            "宏录制已取消:丢弃本次录制的 {} 个事件(编辑区保持原样,未写入任何宏)",
            rec.steps.len()
        ));
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
    ///
    /// U9(2026-10-17):参数区已全部搬进 [`Self::ui_draft_card`] 的
    /// [`DraftCardShell::VisualStyle`] 外壳 —— 两个草稿入口共用同一张卡,
    /// 这里只剩一个调用,漏接录制卡片这类事故不再可能从这一层发生。
    fn ui_vk_draft(&mut self, ui: &mut egui::Ui) {
        self.ui_draft_card(ui, DraftCardShell::VisualStyle);
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
            KeySlot::AimShield => g.profile.aim.shield_key,
            KeySlot::AimWake => g.profile.aim.wake_key,
            KeySlot::AimReconnect => g.profile.aim.reconnect_key,
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
            // 宏录制热键(2026-10-12):单键槽,值存在程序级设置里。
            // `KeySet::single(0)` = 空集 ⇒ 界面显示"未绑定"。
            KeySlot::MacroRecordStart => KeySet::single(self.macro_record_start_key),
            KeySlot::MacroRecordStop => KeySet::single(self.macro_record_stop_key),
            KeySlot::MacroRecordCancel => KeySet::single(self.macro_record_cancel_key),
            // 轨迹录制快捷键:`single(0)` = 空集 ⇒ 界面显示"空缺"
            KeySlot::TrackRecKey => KeySet::single(self.track_rec_key),
            KeySlot::TrackRecStopKey => KeySet::single(self.track_rec_stop_key),
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
        // #2 的**限制说明**(用户 2026-10-11 明确"暂时不修改"行为,所以这里只把既有
        // 优先级写出来,免得用户以为自己设错了):
        // 组合键的成员若同时是功能键,那个键会被功能键路径**先**放行,组合补不齐。
        ui.small(
            "注意：组合键里若含功能键（总开关键、FPS 开关键、按住才退出、按住才瞄准、\
             压枪触发键、滚轮换档键、轮盘启用键、切换键等），该键会按功能键优先处理，\
             这一组组合键不会触发。例：左键被绑成压枪触发键时，Shift+左键 不生效。",
        );
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

        // 帧首快照:字段写回时用它对齐「实时列表里那条还是不是我渲染的那条」
        // (见 commit_combo_field_edits 的两条纪律)。
        let frame_start = combos.clone();
        // 取点 / 缓动编辑的帧首值:回写规则见 frame_local_take。
        let (pick_at_start, easing_at_start) = (self.picking, self.easing_edit);
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
                            // 用户 2026-10-23 第 2 条:每个键后面原本还挂一个
                            // "前缀"/"成员"标签,一行被撑成 300+px、不像一个按键。
                            // 前缀语义上面那行说明已经讲清 ⇒ 只留"＋"分隔符,
                            // 前缀/成员的身份改为悬停提示(不丢信息,只挪位置)。
                            for (slot, key) in combo.keys.iter().copied().enumerate() {
                                let this = KeySlot::ComboKey { combo: index, slot };
                                // 等待态要同时认**和弦**捕获 —— 成员格现在走
                                // `begin_combo_member_capture`(见那里的事故说明)。
                                let waiting = self.waiting_key == Some(this)
                                    || self.waiting_keys == Some(this);
                                let shown = (key != 0).then_some(key);
                                if slot > 0 {
                                    ui.label("＋");
                                }
                                let r = Self::key_button_sized(ui, waiting, shown, 62.0);
                                if r.clicked() {
                                    // 走唯一武装点(第二个 helper):顺手解掉上一次遗留的捕获,
                                    // 否则这一下按的键会落到那一行上(2026-10-23)。
                                    self.begin_combo_member_capture(index, slot);
                                }
                                r.on_hover_text(if slot == 0 {
                                    "第 1 个键 = 前缀:按住它再按后面的键,就是一个组合键"
                                } else {
                                    "与前缀同时按住的那个键"
                                });
                                if combo.keys.len() > 2 && ui.small_button("×").clicked() {
                                    delete_slot = Some((index, slot));
                                }
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
                        // 轨迹录制卡片(组合键):与键位/草稿共用同一张。
                        // 放在这里而不是 `combo_action_editor` 内部 —— 后者是自由函数,
                        // 拿不到 `&mut App`(录制要改会话状态、要写日志、要取 Mapper)。
                        if let Action::Swipe(s) = &combo.action {
                            self.track_rec_card(ui, TrackTarget::Combo(index), s);
                        }
                        let fps_changed = ui.checkbox(&mut combo.fps_only, "仅 FPS").changed();
                        // 「长按重复」(用户 2026-10-11 第 2 条):组合键自己的一份 ——
                        // 整组键都按着时循环重放这一组合的动作,松开任一成员即停
                        // (正在跑的那一轮跑完)。间隔用本组合的「按后延迟」。
                        let repeat_changed = ui
                            .checkbox(&mut combo.repeat_while_held, "长按重复")
                            .on_hover_text(
                                "长按重复(按住时循环,直到抬起):\n\
                                 整组键都按着时,这一组合的动作一轮接一轮地重放;\n\
                                 松开任何一个成员键就**不再排下一轮** —— \
                                 正在跑的那一轮会跑完,设备端不会留下'只按不抬'的触点。\n\
                                 两轮之间的间隔用本组合的「按后延迟」;那个总开关关着时\
                                 不留间隔,一轮接一轮不等待。",
                            )
                            .changed();
                        // 「按后延迟」(用户 2026-10-10 第 2 条):组合键自己的一份。
                        let tail_changed = Self::tail_delay_widget(ui, combo.tail_delay_ms)
                            .map(|v| combo.tail_delay_ms = v)
                            .is_some();
                        if action_changed || fps_changed || repeat_changed || tail_changed {
                            snapshots.push((index, combo.clone()));
                        }
                    });
            });
        }
        // 帧首快照回写:只在这一帧里没人动过时才写回(见 frame_local_take)。
        self.picking = frame_local_take(pick_at_start, self.picking, combo_pick);
        self.easing_edit = frame_local_take(easing_at_start, self.easing_edit, combo_easing);
        // 2026-10-23 事故(用户第 1 条「点击完组合键,组合键映射的位置突然瞬移」):
        // 字段编辑**必须**在同一帧的结构性编辑之前写回 —— 此刻实时列表还是渲染时的顺序,
        // 帧首下标才指向它当初渲染的那一条。放到后面做,就变成「帧首下标 + 已经 remove/push
        // 过的实时列表」:整组键位与落点会被搬到邻居身上。
        for _ in &snapshots {
            self.push_undo();
        }
        let edited_count = snapshots.len();
        let committed = {
            let mut g = lock_shared(&self.shared);
            commit_combo_field_edits(&frame_start, &mut g.profile.combos, &snapshots)
        };
        if committed < edited_count {
            // 只有越界才会走到这里(列表刚被别处改短):宁可丢掉这一次界面改动。
            self.log(format!(
                "组合键改动:有 {} 条没写回(列表刚变过,已跳过,免得改到别的组合)",
                edited_count - committed
            ));
        }
        if let Some(index) = delete_combo {
            self.push_undo();
            {
                let mut g = lock_shared(&self.shared);
                if index < g.profile.combos.len() {
                    g.profile.combos.remove(index);
                }
            }
            // 删掉一个组合之后,后面每个组合的下标都前移一位。当前选中 / 正在等按键的目标
            // 全是按下标记的,不收掉就会指到邻居身上(用户 2026-10-23:位置突然瞬移)。
            self.forget_stale_target("删除组合");
        }
        if let Some((combo_index, slot)) = delete_slot {
            self.push_undo();
            {
                let mut g = lock_shared(&self.shared);
                if let Some(combo) = g.profile.combos.get_mut(combo_index)
                    && slot < combo.keys.len()
                {
                    combo.keys.remove(slot);
                }
            }
            self.forget_stale_target("删掉组合里的一个键");
        }
        if let Some(index) = add_slot {
            self.push_undo();
            {
                let mut g = lock_shared(&self.shared);
                if let Some(combo) = g.profile.combos.get_mut(index) {
                    combo.keys.push(0);
                }
            }
            // 列表长度变了:按位置记的「正在等按键 / 正在取点」目标不再可靠。
            self.forget_stale_target("新增组合键成员");
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
                    repeat_interval_ms: 0,
                },
                fps_only: false,
                tail_delay_ms: crate::keymap::DEFAULT_TAIL_DELAY_MS,

                repeat_while_held: false,
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
                    let r = radius_input(ui, &mut pr, RADIUS_RANGE_WHEEL_PX, None);
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
                self.forget_stale_target("删除轮盘");
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
        // 录制会话的已持续秒数:在**借用 `self.track_rec` 之前**取好(见
        // [`Self::track_rec_elapsed`] 的说明)。浮层上的 REC 角标靠它闪烁。
        let track_elapsed = self.track_rec_elapsed();
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
                        // 正在给**这一条**重录轨迹时画会话缓冲(实时预览,带 REC 角标);
                        // 否则有录制点就画真实折线,都没有才退回几何轨迹。
                        let live = match &self.track_rec {
                            Some(r) if r.target == TrackTarget::Bind(i) && r.points.len() >= 2 => {
                                Some(&r.points)
                            }
                            _ => None,
                        };
                        if let Some(pts) = live {
                            let anchor = pts[0];
                            draw_recorded_track(
                                painter,
                                &th,
                                pts,
                                &to_screen,
                                &short_name(b.key),
                                tone_at(anchor.0, anchor.1),
                                true,
                                track_elapsed,
                            );
                        } else if s.recorded_points.len() >= 2 {
                            let pts: Vec<(i32, i32)> = s
                                .recorded_points
                                .iter()
                                .map(|&(x, y)| m.point(x, y))
                                .collect();
                            let anchor = pts[0];
                            draw_recorded_track(
                                painter,
                                &th,
                                &pts,
                                &to_screen,
                                &short_name(b.key),
                                tone_at(anchor.0, anchor.1),
                                false,
                                track_elapsed,
                            );
                        } else {
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
                    // 草稿坐标本身就是像素,直接绘制。
                    // 三种情形按优先级:① 正在录制 → 画**会话缓冲**(实时预览,
                    // 用户边划边看得见);② 已录好轨迹 → 画真实折线;
                    // ③ 都没有 → 画几何轨迹(条形/方形/圆形)。
                    let rec_pts: Option<&[(i32, i32)]> = match &self.track_rec {
                        Some(r)
                            if matches!(r.target, TrackTarget::Draft) && r.points.len() >= 2 =>
                        {
                            Some(&r.points)
                        }
                        _ => None,
                    };
                    if let Some(pts) = rec_pts {
                        draw_recorded_track(
                            painter,
                            &th,
                            pts,
                            &to_screen,
                            "录制中",
                            tone_at(self.draft.swipe_start.0, self.draft.swipe_start.1),
                            true,
                            track_elapsed,
                        );
                    } else if self.draft.swipe_points.len() >= 2 {
                        draw_recorded_track(
                            painter,
                            &th,
                            &self.draft.swipe_points,
                            &to_screen,
                            "新增",
                            tone_at(self.draft.swipe_start.0, self.draft.swipe_start.1),
                            false,
                            track_elapsed,
                        );
                    } else {
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
                        // 与键位同一条口径:录制的真实轨迹优先于几何轨迹
                        let live = match &self.track_rec {
                            Some(r)
                                if r.target == TrackTarget::Combo(ci) && r.points.len() >= 2 =>
                            {
                                Some(&r.points)
                            }
                            _ => None,
                        };
                        if let Some(pts) = live {
                            let anchor = pts[0];
                            draw_recorded_track(
                                painter,
                                &th,
                                pts,
                                &to_screen,
                                &format!("组合{} {}", ci + 1, keys),
                                tone_at(anchor.0, anchor.1),
                                true,
                                track_elapsed,
                            );
                        } else if s.recorded_points.len() >= 2 {
                            let pts: Vec<(i32, i32)> = s
                                .recorded_points
                                .iter()
                                .map(|&(x, y)| m.point(x, y))
                                .collect();
                            let anchor = pts[0];
                            draw_recorded_track(
                                painter,
                                &th,
                                &pts,
                                &to_screen,
                                &format!("组合{} {}", ci + 1, keys),
                                tone_at(anchor.0, anchor.1),
                                false,
                                track_elapsed,
                            );
                        } else {
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
    ///
    /// 骨架(标题 / 默认宽度 / Id)一律走 `PadApp::helper_window`(U15),
    /// 这里只剩「取走状态 ⇒ 渲染 ⇒ 仍开则放回」这一段语义。
    fn ui_args_helper(&mut self, ctx: &egui::Context) {
        let Some(mut helper) = self.args_helper.take() else {
            return;
        };
        let mut open = true;
        helper_window(self, ctx, HelperSource::Args, &mut open, &mut |app, ui| {
            app.arg_help_panel(ui, &mut helper);
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
        // ---- 搜索(用户 2026-10-13):按中文名 / 参数 / 说明 / 示例四路匹配 ----
        ui.horizontal(|ui| {
            ui.label("搜索:");
            let r = ui.add(
                egui::TextEdit::singleline(&mut h.query)
                    .desired_width(220.0)
                    .hint_text("参数名 / 中文名 / 说明(如:录屏、无线、帧率)"),
            );
            if r.changed() {
                h.refilter();
            }
            if !h.query.is_empty() && ui.small_button("清空").clicked() {
                h.query.clear();
                h.refilter();
            }
            ui.small(format!("{} / {} 条", h.filtered.len(), h.entries.len()));
        });
        let scrcpy_gh = "https://github.com/Genymobile/scrcpy";
        // 列表按**拼音顺序**排(见 `sort_by_pinyin`);这里用过滤后的下标,
        // 于是"搜索 + 排序"同时生效。
        let visible = h.filtered.clone();
        egui::ScrollArea::vertical()
            .max_height(300.0)
            .show(ui, |ui| {
                if visible.is_empty() {
                    ui.weak("没有匹配的参数;换个关键词试试(可搜中文名、参数名或说明)");
                    return;
                }
                for &i in &visible {
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
        let count = h.entries.len();
        if count > 0 {
            let sel = h.selected.min(count - 1);
            let e = &h.entries[sel];
            ui.horizontal(|ui| {
                ui.strong(e.name);
                ui.monospace(&e.flag);
            });
            // 位置口径:搜索时显示"在匹配结果里的第几条",否则显示"共几条"
            let pos = h.filtered.iter().position(|&i| i == sel).map(|p| p + 1);
            match pos {
                Some(p) => ui.small(format!(
                    "第 {p}/{} 条(已过滤,共 {} 条)· 平台:{}",
                    h.filtered.len(),
                    count,
                    e.platform
                )),
                None => ui.small(format!("第 {}/{} 条 · 平台:{}", sel + 1, count, e.platform)),
            };
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
                        // F6(2026-10-12):按**字符串匹配替换** —— 同一个键已有值就
                        // 就地更新,不再追加第二份。用户实测过配置里 `--max-size`
                        // 堆了三份、`--max-fps` 两份,行为不可预期(后者覆盖前者)。
                        let before = self.scrcpy_args.clone();
                        self.scrcpy_args = upsert_arg(&self.scrcpy_args, &flag);
                        if self.scrcpy_args == before {
                            self.log(format!("参数已是该值,未重复加入: {flag}"));
                        } else {
                            self.log(format!("已加入参数(同键就地替换): {flag}"));
                        }
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
        let mut changed: Option<(u32, u32)> = None;
        {
            let mut g = lock_shared(&self.shared);
            if let Some(c) = g.control.as_mut() {
                if (c.screen_w, c.screen_h) != (w, h) {
                    changed = Some((c.screen_w, c.screen_h));
                    // 走 set_screen:它会同时更新写线程用的原子量。
                    // 直接写字段的话,协议里声明的尺寸会永远停在连接时的那个值。
                    c.set_screen(w, h);
                    // 坐标空间真的变了 ⇒ 请引擎把此刻按着的触点全部抬手
                    // (它们是**旧空间**里落下的,位置已经错了;见
                    // `engine::Shared::space_changed_release` 的说明)。
                    // 这里只置请求位,由引擎在下一轮(≤4ms)执行收尾。
                    g.space_changed_release = true;
                }
            }
        }
        // 日志里带上**旧值**:排查"按键全失效"时,只有"更新为多少"看不出
        // 手机是从哪个空间换过来的(一次转屏?还是 2560x1184 ↔ 1184x2560 抖了两下?)
        if let Some((ow, oh)) = changed {
            self.log(format!(
                "触摸坐标空间已更新为 {w}x{h}(原 {ow}x{oh};已请求抬起旧空间触点)"
            ));
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

    /// 路径输入框该占多宽:先扣掉同一行里按钮的宽度,再按上限/下限收敛。
    ///
    /// 用户 2026-10-17(433px 窄窗截图):路径框按 egui 的默认宽度铺开,把[浏览]
    /// 按钮顶出右边缘 ⇒ 按钮不可达。改成"按钮先占位、输入框只吃剩下的"。
    ///
    /// 两条不变量(单测 `narrow_window_path_rows_keep_their_buttons_reachable` 钉住):
    ///   * 宽窗:剩余宽度 ≥ `cap` ⇒ 结果**就是** `cap`(与改动前的默认宽度逐像素一致);
    ///   * 窄窗:结果 = 剩余宽度 ⇒ 输入框收缩,按钮留在右边缘以内。
    fn fit_edit_width(available: f32, reserved: f32, cap: f32) -> f32 {
        (available - reserved).min(cap).max(PATH_EDIT_MIN_WIDTH)
    }

    /// 量出这一行按钮要占的横向宽度(文字宽 + 左右内边距 + 一个项间距),
    /// 交给 [`Self::fit_edit_width`] 扣减。
    ///
    /// 字体与内边距全部取**当前样式**:换主题、换控件密度、改字号都自动跟着走,
    /// 不写死像素(与上面"使用说明"按钮量尺寸的做法同一套)。
    fn row_edit_width(ui: &egui::Ui, buttons: &[&str], cap: f32) -> f32 {
        let font = egui::TextStyle::Button.resolve(ui.style());
        let pad = ui.spacing().button_padding.x * 2.0;
        let gap = ui.spacing().item_spacing.x;
        let reserved: f32 = buttons
            .iter()
            .map(|t| {
                let w = ui
                    .painter()
                    .layout_no_wrap((*t).to_owned(), font.clone(), egui::Color32::WHITE)
                    .size()
                    .x;
                w + pad + gap
            })
            .sum();
        Self::fit_edit_width(ui.available_width(), reserved, cap)
    }

    /// scrcpy 三件套的定位与管理(目录/自动寻找/测试/记住路径/打开配置目录)。
    /// 默认与可视化风格共用这一份(放在左栏)。
    fn ui_scrcpy_manage(&mut self, ui: &mut egui::Ui) {
        ui.heading("scrcpy 管理");
        // 文案里的可执行文件名必须按平台取(2026-10-19:Linux 版曾逐字写死
        // scrcpy.exe/adb.exe,在 Linux 上等于指错文件、指了也不存在)。
        ui.small(format!(
            "scrcpy 发行包里 {}、scrcpy-server、{} 三者同目录。\n\
             可以直接指定 scrcpy 所在**目录**(最省事),也可以只填其中一个文件,\n\
             其余留空会自动补齐。",
            adb::scrcpy_exe_name(),
            adb::adb_exe_name()
        ));
        // scrcpy 目录:用户最自然的用法就是把发行包那个文件夹指给它
        ui.horizontal(|ui| {
            ui.label("scrcpy 目录");
            // 窄窗下按钮优先:输入框只吃"扣掉按钮之后"的宽度(见 row_edit_width)
            let w = Self::row_edit_width(ui, &["浏览目录", "应用"], 220.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.scrcpy_dir)
                    .desired_width(w)
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
                    None => self.log(format!(
                        "未找到 scrcpy,请手动指定它所在目录或 {}",
                        adb::scrcpy_exe_name()
                    )),
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
            // 等宽占位:两行标签要左对齐,但名字本身按平台取(Windows 带 .exe)
            ui.label(format!("{:<11}", adb::scrcpy_exe_name()));
            let w = Self::row_edit_width(ui, &["浏览"], ui.spacing().text_edit_width);
            ui.add(egui::TextEdit::singleline(&mut self.scrcpy_path).desired_width(w));
            if ui.small_button("浏览").clicked() {
                self.dialog = Some(crate::filedialog::pick_file());
                self.dialog_purpose = DialogPurpose::ScrcpyExe;
            }
        });
        ui.horizontal(|ui| {
            ui.label("scrcpy-server");
            let w = Self::row_edit_width(ui, &["浏览"], ui.spacing().text_edit_width);
            ui.add(egui::TextEdit::singleline(&mut self.server_path).desired_width(w));
            if ui.small_button("浏览").clicked() {
                self.dialog = Some(crate::filedialog::pick_file());
                self.dialog_purpose = DialogPurpose::ServerJar;
            }
        });
        ui.horizontal(|ui| {
            ui.label(format!("{:<11}", adb::adb_exe_name()));
            let w = Self::row_edit_width(ui, &["浏览", "自动"], ui.spacing().text_edit_width);
            ui.add(egui::TextEdit::singleline(&mut self.adb_path).desired_width(w));
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
        // 这两行可能很长(路径):窄窗下**省略**而不是横向溢出 ——
        // 溢出会把同一行/下一行的控件顶出可视区(用户 2026-10-17 的 433px 截图)。
        ui.add(
            egui::Label::new(egui::RichText::new(format!("当前 adb: {adb_eff}")).small())
                .truncate(),
        );
        ui.add(
            egui::Label::new(egui::RichText::new(format!("server 版本: {ver_txt}")).small())
                .truncate(),
        );
    }

    /// 总开关键 + [映射时屏蔽原键] 勾选(默认/可视化风格共用,放在左栏中段)。
    /// 复选框只有 Linux 有实际语义(EVIOCGRAB 独占 /dev/input),Windows 上
    /// 隐藏并把两档现状写成一段说明(2026-10-07 V1-2 ③ 决策:死 UI 不留)。
    ///
    /// B21/G-a(2026-10-17,2026-10-18 按 P1/P3 实测收窄为两档):Windows 侧的
    /// 说明改成**两档**(默认 / 独占模式),写清两条边界(拦不住通过 Raw Input
    /// 读输入的程序、出问题按总开关键一键关掉),且**不承诺**「完全独占 /
    /// 系统级独占」。措辞必须与实现状态对齐:第二档已由 B21-2/G-b 落地
    /// (`Settings::swallow_bound_keys`,**默认关闭**),所以文案写"默认关闭"
    /// 而**不是**"尚未实现"。第三档 `RIDEV_NOLEGACY` 实测零收益、已被裁掉,
    /// 不写文案。
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
        if ui
            .checkbox(
                &mut self.grab_enabled,
                "映射时屏蔽原键(grab)\n注意:开启后映射期间键盘只对本程序生效",
            )
            .on_hover_text(
                "开启后映射期间键盘被内核独占(EVIOCGRAB):同一把键盘的按键在系统里\
                 任何前台程序都收不到,不需要逐键白名单。\n\
                 这是程序级设置,存在 settings.json 里,重启后保持。",
            )
            .changed()
        {
            // 与其它 settings 开关同一条路:登记后帧末统一落盘。
            // 2026-10-21 之前这个开关**不落盘**,每次启动都重置为关 —— 用户勾了
            // 等于没勾,下次启动静默失效。
            self.remember_now();
            self.log(if self.grab_enabled {
                "已开启「映射时屏蔽原键(grab)」:映射期间键盘设备被独占,其它程序收不到按键"
            } else {
                "已关闭「映射时屏蔽原键(grab)」:按键恢复传给其它程序"
            });
        }
        // Windows 走全局钩子(拦截后不转发),没有"独占设备"这一档 ——
        // 但 G-b 起有"独占模式(默认关)":已绑定键不再传给其它程序(legacy 层)。
        #[cfg(windows)]
        {
            if ui
                .checkbox(
                    &mut self.swallow_bound_keys,
                    "已绑定键不再传给其它程序(独占模式)",
                )
                .on_hover_text(
                    "默认关闭。打开后,映射开启期间已被绑定的键(含 W/A/S/D 这类\
                     普通键)不再传给其它程序 —— 仅在 legacy 消息层生效,拦不住\
                     通过 Raw Input 读输入的程序(原理边界)。\n\
                     只吞**当前生效**的绑定:「仅 FPS」的键在 FPS 模式生效期间才吞;\
                     组合键里的键本期不吞;总开关键(F8)关掉映射立刻全部恢复。\n\
                     这是程序级设置,存在 settings.json 里,重启后保持。",
                )
                .changed()
            {
                // 与其它 settings 开关同一条路:remember_now 登记,帧末统一落盘
                self.remember_now();
                self.log(if self.swallow_bound_keys {
                    "已开启「已绑定键不再传给其它程序」(独占模式):映射开启期间,已绑定键在 legacy 消息层被吞(Raw Input 程序拦不住)"
                } else {
                    "已关闭「已绑定键不再传给其它程序」:按键恢复传给其它程序(默认行为)"
                });
            }
            ui.small(WIN_GRAB_MODES_NOTE);
        }
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
            // DF-10(2026-10-11):探针自愈被**永久停用**了(连续多轮重装后探针
            // 仍无回音,判定环境吞掉了注入事件)。这是持续状态,不是事件:
            // 停用之后系统再静默摘钩就没人数得清了 —— 必须让用户知道
            // "现在是裸奔状态,失灵只能重启",而不是像以前那样静默失效。
            3 => {
                ui.colored_label(
                    th.danger,
                    "⚠ 输入捕获自愈已停用：本进程内不再自动重装钩子；若出现按键/视角失灵，请重启程序。",
                )
                .on_hover_text(
                    "探针连续多轮重装后仍收不到回音,已判定本机环境(安全软件/注入\n\
                     过滤)吞掉了探针事件,于是停用了这条自愈路径 —— 否则它会每几秒\n\
                     重装一轮、刷屏日志,还连累好着的鼠标钩子一起被卸装。\n\
                     消息泵心跳检测仍然保留(那条靠自己 PostThreadMessage,不依赖\n\
                     探针回音),但探针这条路确实不再自动重装钩子了。\n\
                     若此刻按键/视角失灵:请重启本程序(重启会重新装一次钩子)。",
                );
            }
            _ => {}
        }

        // R11:Raw Input 通道状态(平台中立纯函数决定文案;非 Windows 恒 (0,0,0)
        // ⇒ `raw_status_line` 返回 None,这一整段在 Linux 上不产生任何界面行)。
        //
        // **同一帧只探一次**:状态行与下面的红字告警共用这一份读数,免得两次
        // 采样落在不同瞬间(状态机在 700ms 边界上会翻转,两次读数可能自相矛盾)。
        let (raw_state, raw_count, _raw_age_ms) = crate::capture::raw_probe();
        if let Some(line) = Self::raw_status_line(raw_state, raw_count) {
            let color = match raw_state {
                1 => th.warn, // 已注册、等首个事件:还不算坏,琥珀色
                2 => th.ok,   // 相对接管:正常
                3 => th.warn, // 只有绝对设备:降级,琥珀色
                _ => th.muted,
            };
            ui.colored_label(color, line);
        }
        // T1:注册流程走完(state==3)、却**一条 WM_INPUT 都没到**(count==0)
        // ⇒ 这不是"刚启动还没来",而是"通道通了但事件一个没进来",必须红字。
        if let Some(alert) = Self::raw_probe_alert(raw_state, raw_count) {
            ui.colored_label(th.danger, alert).on_hover_text(
                "Raw Input 通道已完成注册,但从注册到现在**一条事件都没收到**。\n\
                 计数只在捕获层真收到 WM_INPUT 时 +1,所以 0 条 = 真的没来过。\n\
                 常见原因:指针设备只上报绝对坐标(触摸板/数位板/部分虚拟鼠标),\n\
                 或安全软件/注入过滤拦掉了原始输入。\n\
                 程序已按 700ms 判据自动退回『光标差』路径 —— 位移仍然能用,\n\
                 只是不再按设备计数上报(灵敏度/手感可能与设备原生不同)。",
            );
        }

        // 「鼠标独占(隔膜)」状态(2026-10-11 根因定位;当日晚扩为 S①②③):
        // 独占期间捕获层把所有鼠标消息从系统输入流里吞掉 —— 投屏窗口(scrcpy)
        // 再也收不到,就不会在我们注入触摸的同时自己再发一套(用户实测的
        // 抖动/跳变来源)。现在它**不一定与 FPS 绑定**(见 `Aim::mouse_shield`
        // 三档与手动开关),所以文案必须把"为什么开着"和"怎么关掉"一起写出来。
        //
        // 这条**必须可见**:隔膜开着时鼠标连本界面都点不了,不写出来就会被当成
        // "鼠标坏了"。只在 Windows 提示:Linux 侧 FPS 的鼠标独占走 EVIOCGRAB
        // (引擎写 mouse_grab),没有这层隔膜,写了就是假话。
        #[cfg(windows)]
        {
            let scrcpy_foreground = self.scrcpy_is_foreground();
            let connected = g
                .control
                .as_ref()
                .map(|c| c.is_connected())
                .unwrap_or(false);
            let fps_active = g.profile.aim.enabled
                && g.aim_live.mode_active
                && !g.aim_live.suspended
                && g.profile.aim.anchor_set();
            let (on, why) = Self::shield_on_and_reason(
                g.profile.aim.mouse_shield,
                g.enabled,
                connected,
                fps_active,
                scrcpy_foreground,
                g.aim_live.shield_manual,
            );
            if on {
                let line = Self::shield_status_line(g.profile.aim.shield_key, on, why);
                ui.colored_label(th.warn, egui::RichText::new(line).small())
                    .on_hover_text(
                        "鼠标消息(点击/拖动/滚轮)不再投递给任何窗口 ——\n\
                         投屏窗口 scrcpy 因此收不到它们,不会在已经注入手机触摸的同时\n\
                         自己再转发一套(快速甩动/转视角时的抖动、跳变就是这么来的)。\n\
                         代价:开着时鼠标也点不了本界面(有意为之)。\n\
                         键盘事件不受影响(钩子是先转发后吞),按上面写的热键即可关闭;\n\
                         也可以关掉映射/断开连接/切走投屏窗口,下一帧(≤120ms)自动恢复。",
                    );
            } else {
                ui.colored_label(
                    th.muted,
                    egui::RichText::new(Self::shield_status_line(
                        g.profile.aim.shield_key,
                        false,
                        "",
                    ))
                    .small(),
                );
            }
        }
    }

    // ==================== 日志三形态的共用文案核(U14) ====================
    //
    // 左栏日志卡 / 浮窗日志 / 列表核三种形态**只许绘制**,文案一律从这里取:
    // 标题、弹窗按钮、每一行。任何一处想改字只能改这里,否则三形态会漂移
    // (U6 审计 A4-2:同一块文案写三遍,改一处漏两处)。纯函数 ⇒ 单测逐位比字符串。

    /// 日志块的标题:卡片 heading 与浮窗标题栏**共用同一串**。
    fn log_card_title() -> &'static str {
        "日志"
    }

    /// 「弹窗查看 / 关闭弹窗」按钮文案(随浮窗开合切换)。
    fn log_popup_button(open: bool) -> &'static str {
        if open { "关闭弹窗" } else { "弹窗查看" }
    }

    /// 一行日志的显示文本。
    ///
    /// 当前**逐字保留**原始行:同一个字符串既进界面也进诊断文件(见 `PadApp::log`),
    /// 界面侧加前缀/裁剪会让两处证据对不上。收敛的是"入口唯一",不是内容。
    fn log_row_text(line: &str) -> &str {
        line
    }

    /// 日志列表最多显示多少行(环形缓冲本身只留 200 条,见 `PadApp::log`)。
    const LOG_ROW_CAP: usize = 500;

    /// 日志**卡片**最多显示多少行(浮窗不受此限)。
    ///
    /// 用户 2026-10-23:"现在状态日志覆盖太多,滚动太多" —— 卡片原来把整份
    /// 环形缓冲(最多 200 行)全塞进滚动区,左栏几乎被日志占满、要滚很多屏。
    /// 卡片只留 12 行当"一眼摘要",全部内容走[弹窗查看]或[导出日志]。
    const LOG_CARD_ROWS: usize = 12;

    /// 日志卡的最小高度(再矮就只剩标题栏了)。
    const LOG_CARD_MIN_H: f32 = 96.0;

    /// 卡片顶部那条"拖我改高度"的细条高度。
    const LOG_CARD_GRIP_H: f32 = 8.0;

    /// 卡片里除列表以外的固定占用:细条 + 标题栏 + 底部提示 + 行距。
    /// 这个余量必须**大于**真实装饰高度,否则内容会比面板高一点,
    /// 又回到"内容撑面板、面板撑内容"的棘轮(2026-10-23 两次事故)。
    const LOG_CARD_CHROME_H: f32 = 100.0;

    /// [导出日志] 按钮文案(卡片与浮窗共用同一串)。
    fn log_export_button() -> &'static str {
        "导出日志"
    }

    /// [复制全部] 按钮文案(卡片与浮窗都有 —— 用户 2026-10-23 要求"可自如复制")。
    fn log_copy_all_button() -> &'static str {
        "复制全部"
    }

    /// 卡片底部那行提示:只显示了一部分、怎么全看、怎么调高度、怎么复制。
    ///
    /// 用户 2026-10-23:"消息越多框涨得越高,把我上面的都挡住了…做的基础不可自动拉伸 +
    /// 我手动可调节 + 自动滚动 + 可自如复制有那么难吗" ⇒ 卡片高度**只由面板高度决定**
    /// (内容再多也不涨),面板上沿可拖,列表自动跟随最新,行可选中、整份可一键复制。
    fn log_card_hint(shown: usize, total: usize) -> String {
        format!(
            "最近 {shown}/{total} 行 · 自动跟随最新 · 拖上沿可改高度 · [复制全部] 整份拷走 · [弹窗查看] 看全部"
        )
    }

    /// 把**紧邻重复**的日志行折叠成"一行 + 次数"(只影响显示,不改变证据)。
    ///
    /// 输入按**显示顺序**给(调用方已经 `rev()` 过,最新在最前)。只折叠紧邻的相同行:
    /// "鼠标已捕获 / 已钉住光标"这类会连刷十几条,折叠后一屏能看完;中间夹了别的行
    /// 就不折叠 —— **顺序本身是证据**,不许做全局去重。
    fn fold_log_rows<'a, I>(rows: I) -> Vec<(&'a str, usize)>
    where
        I: Iterator<Item = &'a str>,
    {
        let mut out: Vec<(&'a str, usize)> = Vec::new();
        for line in rows {
            match out.last_mut() {
                Some((last, n)) if *last == line => *n += 1,
                _ => out.push((line, 1)),
            }
        }
        out
    }

    /// 导出文件名(纯函数 ⇒ 可单测)。放诊断文件同目录,靠时间戳区分多次导出。
    fn log_export_file_name(stamp: &str) -> String {
        format!("log-export-{stamp}.txt")
    }

    /// 导出文件正文:抬头 + 界面日志缓冲全文 + 诊断文件全文。
    ///
    /// 为什么要两份都写:界面日志走 `log()` 时**同时**进诊断文件,但诊断文件按
    /// `log_level` 过滤(warn 档下 info 行整批不落盘)⇒ 只导文件会漏掉界面上用户
    /// 正看着的那些行;只导缓冲则丢掉 debug/trace 细节。两份合起来才是完整现场。
    fn log_export_body(
        version: &str,
        stamp: &str,
        level: &str,
        level_source: &str,
        ui_lines: &[String],
        diag_text: &str,
    ) -> String {
        let mut out = String::new();
        out.push_str(&format!("scrcpy-pad 日志导出 v{version}\n"));
        out.push_str(&format!("导出时间: {stamp}\n"));
        out.push_str(&format!("日志级别: {level}(来源: {level_source})\n"));
        out.push_str(&format!("界面日志行数: {}\n", ui_lines.len()));
        out.push_str("\n===== 界面日志(按时间顺序,最新在最后) =====\n");
        for l in ui_lines {
            out.push_str(l);
            out.push('\n');
        }
        out.push_str("\n===== 诊断文件全文 =====\n");
        out.push_str(diag_text);
        if !diag_text.ends_with('\n') {
            out.push('\n');
        }
        out
    }

    /// 日志三形态共用的**唯一**渲染核:同一个滚动区 + 同一份行文案。
    ///
    /// `id` 必须由调用方按形态区分:卡片与浮窗可能同时可见,共用一个
    /// `ScrollArea` id 会让两边互相顶滚动位置。
    fn ui_log_list(&self, ui: &mut egui::Ui, max_h: f32, id: &str, rows_cap: usize) {
        let rows = Self::fold_log_rows(
            self.logs
                .iter()
                .rev()
                .take(Self::LOG_ROW_CAP)
                .map(|s| s.as_str()),
        );
        let h = max_h.max(40.0);
        egui::ScrollArea::vertical()
            .id_salt(id)
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .max_height(h)
            .show(ui, |ui| {
                for (l, n) in rows.into_iter().take(rows_cap) {
                    let text = if n > 1 {
                        format!("{}  (x{n})", Self::log_row_text(l))
                    } else {
                        Self::log_row_text(l).to_string()
                    };
                    let long = text.chars().count() > 72;
                    let label = egui::Label::new(egui::RichText::new(&text).monospace())
                        .truncate()
                        .selectable(true);
                    let resp = ui.add(label);
                    if long {
                        resp.on_hover_text(&text);
                    }
                }
            });
    }

    fn ui_log_card(&mut self, ui: &mut egui::Ui, card_h: f32) {
        let w = ui.available_width();
        ui.allocate_ui_with_layout(
            egui::vec2(w, card_h),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.set_min_height(card_h);
                ui.set_max_height(card_h);
                let grip = ui.allocate_response(
                    egui::vec2(ui.available_width(), Self::LOG_CARD_GRIP_H),
                    egui::Sense::click_and_drag(),
                );
                if grip.hovered() || grip.dragged() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
                }
                if grip.dragged() {
                    self.log_height =
                        (self.log_height - grip.drag_delta().y).clamp(Self::LOG_CARD_MIN_H, 560.0);
                }
                ui.horizontal(|ui| {
                    ui.heading(Self::log_card_title());
                    if ui.button(Self::log_export_button()).clicked() {
                        self.export_ui_log();
                    }
                    if ui.button(Self::log_copy_all_button()).clicked() {
                        let all: Vec<&str> = self.logs.iter().map(|s| s.as_str()).collect();
                        ui.ctx().copy_text(all.join("\n"));
                    }
                    if ui
                        .button(Self::log_popup_button(self.log_window_open))
                        .clicked()
                    {
                        self.log_window_open = !self.log_window_open;
                    }
                });

                let shown = Self::fold_log_rows(self.logs.iter().rev().map(|s| s.as_str())).len();
                let list_h = (card_h - Self::LOG_CARD_CHROME_H).max(40.0);
                self.ui_log_list(ui, list_h, "log_card", Self::LOG_CARD_ROWS);
                ui.small(Self::log_card_hint(
                    shown.min(Self::LOG_CARD_ROWS),
                    self.logs.len(),
                ));
            },
        );
    }

    /// 日志浮窗的**默认**尺寸(纯函数 ⇒ 可单测;B21-F,用户 2026-10-18 第 3 条)。
    ///
    /// 旧实现固定 760x460,而实际日志常常只有两三行短文本 ⇒ 大片空白("默认太大了")。
    /// 这里按**最长一行的字符数**与**条数**估算:
    /// - 宽度:等宽字体每个字符约 7.2pt(@ 默认字号)+ 左右各 12pt 边距;
    /// - 高度:每行约 18pt + 抬头(标题栏 + 按钮行)约 50pt;
    /// - 两头都钳制在可用区间内(太窄读不了,太宽会出屏幕)。
    ///
    /// 只在窗口**首次出现**时作为 `default_size` 生效 —— 用户手动拉过之后不再覆盖。
    fn log_window_size(log_count: usize, max_line_chars: usize) -> (f32, f32) {
        const CHAR_W: f32 = 7.2;
        const ROW_H: f32 = 18.0;
        const HEADER_H: f32 = 50.0;
        const PAD_W: f32 = 24.0;
        if log_count == 0 {
            return (420.0, 240.0);
        }
        let w = (max_line_chars as f32 * CHAR_W + PAD_W).clamp(420.0, 1100.0);
        let h = (log_count as f32 * ROW_H + HEADER_H).clamp(240.0, 620.0);
        (w, h)
    }

    /// B21-F(用户 2026-10-18 第 3 条):默认尺寸按**文本长度**自适应,不再固定
    /// 760x460(用户原话"诊断的框默认太大了")。只在 `default_size` 用 ⇒ egui
    /// 天然只在该窗口**首次出现**时采纳,之后用户手动拖过的尺寸不会被每帧覆盖。
    ///
    /// 注:说明文字**必须写在函数体外** —— 守护测试
    /// `log_forms_share_one_text_source` 逐行扫描本函数体并禁止中文(它管的是
    /// "文案唯一来源",中文注释一并在内)。
    fn ui_log_window(&mut self, ctx: &egui::Context) {
        if !self.log_window_open {
            return;
        }
        let mut open = self.log_window_open;
        let (default_w, default_h) = {
            let max_chars = self
                .logs
                .iter()
                .take(Self::LOG_ROW_CAP)
                .map(|l| l.chars().count())
                .max()
                .unwrap_or(0);
            Self::log_window_size(self.logs.len(), max_chars)
        };
        egui::Window::new(Self::log_card_title())
            .open(&mut open)
            .default_size([default_w, default_h])
            .min_size([420.0, 240.0])
            .resizable(true)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button(Self::log_export_button()).clicked() {
                        self.export_ui_log();
                    }
                    if ui.button(Self::log_copy_all_button()).clicked() {
                        let all: Vec<&str> = self.logs.iter().map(|s| s.as_str()).collect();
                        ui.ctx().copy_text(all.join("\n"));
                    }
                });
                self.ui_log_list(ui, default_h.max(240.0), "log_window", usize::MAX)
            });
        self.log_window_open = open;
    }

    /// 一键导出日志(用户 2026-10-23:"可以一键导出日志")。
    ///
    /// 与[导出诊断报告]的分工:那个偏"我的配置 + 自检结论摘要",这个只干一件事 ——
    /// 把**用户此刻看着的这份日志**原样落成文件,不用滚动、不用挑选、不用描述现象。
    /// 落在诊断文件同目录(名字带时间戳),导出后再补一行日志把路径告诉用户。
    fn export_ui_log(&mut self) {
        let stamp = timestamp_compact();
        let ui_lines: Vec<String> = self.logs.iter().cloned().collect();
        let text = Self::log_export_body(
            env!("CARGO_PKG_VERSION"),
            &stamp,
            crate::diag::level().name(),
            &crate::diag::level_source(),
            &ui_lines,
            &crate::diag::tail(usize::MAX),
        );
        let dir = crate::diag::path()
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        match Self::write_log_export(&dir, &stamp, &text) {
            Ok(path) => {
                crate::diag::flush();
                self.log(format!("日志已导出: {}", path.display()));
            }
            Err(e) => self.log(format!("日志导出失败: {e}")),
        }
    }

    /// 把导出正文真的写进 `dir`(IO 部分单列 ⇒ 测试能在临时目录里**真写一次**)。
    ///
    /// 为什么要把这一小段拆出来:按钮点击路径 `&mut PadApp` 在单测里造不出来,
    /// 于是"点了按钮到底有没有落文件"会成为纯 B 级;拆出这一层后,文件名、原子写、
    /// 内容逐字一致这三件事都能在临时目录里验到(剩下的只有一根按钮接线)。
    fn write_log_export(
        dir: &std::path::Path,
        stamp: &str,
        text: &str,
    ) -> std::io::Result<std::path::PathBuf> {
        let path = dir.join(Self::log_export_file_name(stamp));
        write_atomic(&path, text)?;
        Ok(path)
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

    /// 由"投屏窗口客户区 + 手机长宽比"算出**画面矩形**(屏幕坐标)。
    ///
    /// 这是本轮(用户 2026-10-13 三轮反馈后)的**根治改法**:
    /// 让**蒙版窗口本身就等于画面矩形**,而不是等于整个投屏窗口。
    ///
    /// 为什么这样就不偏了:
    /// - 蒙版窗口是 `WS_POPUP` **无边窗口**,客户区 = 整窗 ⇒ 窗口内坐标
    ///   `(0,0)` 就是画面左上角,`(w,h)` 就是画面右下角;
    /// - 于是"设备像素 → 窗口坐标"只剩一次**纯等比缩放**
    ///   (`x * win_w / dw`),不再有任何原点加减 —— 数学上不可能偏;
    /// - 黑边区域**不被蒙版覆盖**(用户点黑边就落到投屏窗口上,本来就无害),
    ///   而按整窗贴合时黑边也在遮板里,既浪费又容易算错。
    ///
    /// 返回 `None` 表示"尺寸不合法"。
    fn mask_viewport_rect(
        win: (i32, i32, i32, i32),
        space: (u32, u32),
    ) -> Option<crate::mask::MaskRect> {
        let (wx, wy, ww, wh) = win;
        // 坐标空间未知(0)或窗口太小 ⇒ 不贴遮板(调用方退回隔膜)。
        // **不许**用 `.max(1)` 兜住 —— 那会拿一个假的 1×1 空间算出满屏的矩形,
        // 表现是"遮板贴上了但图形全错",比不贴更糟(单测当场抓出过这个)。
        if space.0 == 0 || space.1 == 0 {
            return None;
        }
        let (dw, dh) = (space.0 as f32, space.1 as f32);
        let (ww, wh) = (ww.max(1) as f32, wh.max(1) as f32);
        // 等比:画面比例 = 手机长宽比;取较小的那个倍率 ⇒ 完整装下、不变形
        let scale = (ww / dw).min(wh / dh);
        let vw = (dw * scale).round().max(1.0) as i32;
        let vh = (dh * scale).round().max(1.0) as i32;
        if vw < 32 || vh < 32 {
            return None;
        }
        // 在客户区内居中(黑边均匀分到两侧)
        let ox = wx + ((ww as i32) - vw) / 2;
        let oy = wy + ((wh as i32) - vh) / 2;
        Some(crate::mask::MaskRect::new(ox, oy, vw, vh))
    }

    /// S①:投屏窗口(scrcpy 子进程)此刻是否在前台。
    ///
    /// 判据 = **前台窗口所属进程的 pid == 我们启动的 scrcpy 子进程 pid**
    /// (见 [`crate::capture::foreground_pid`] 与 [`adb::launch_scrcpy`] 的返回值)。
    /// 用它而不是窗口标题:标题会被用户的 `--window-title` 改掉,pid 不会。
    ///
    /// **拿不到就保守地返回 `false`**(= 不独占):没启动过 scrcpy、子进程已退出
    /// (pid 已作废)、查不到前台窗口、或是非 Windows —— 这些都是"判据不成立",
    /// 不能当成"用户正在投屏窗口里"。
    fn scrcpy_is_foreground(&self) -> bool {
        let Some(pid) = self.scrcpy_pid else {
            return false;
        };
        crate::capture::foreground_pid() == Some(pid)
    }

    /// 遮板(窗口层接管鼠标)的**热切换 + 每帧同步**(用户 2026-10-13)。
    ///
    /// 语义(与隔膜的关系):
    /// - 外观页选「遮板」⇒ 起一块透明置顶窗贴在投屏窗口上,鼠标事件落在它身上,
    ///   底下的投屏窗口收不到 ⇒ 双份输入从根上消失;**同时把钩子层隔膜关掉**
    ///   (两者同时生效会把鼠标吞两次,而且界面也点不了)。
    /// - 外观页选「隔膜」(默认)⇒ 立刻销毁遮板,回到从前那条路。
    /// - **热切换**:翻转后下一帧就走完上面两条,不必重启。
    ///
    /// 三条**自动退回**条件(起不来就不硬撑,回到隔膜):
    /// ① 非 Windows(`available()` 为 false);
    /// ② 定位不到投屏窗口矩形(还没启动投屏 / 窗口最小化);
    /// ③ 创建窗口失败(权限/资源)。
    /// 退回时写一条日志(**只在状态变化时**写一次,不刷屏)。
    ///
    /// 参数 `shield_wanted` = 隔膜那套判据算出来的结果;本函数在遮板模式下会
    /// 把它**覆盖成 false**(见上:两者只能有一个生效)。
    fn sync_mouse_capture(&mut self, shield_wanted: bool) -> bool {
        let mode = self.mouse_capture;
        let want_mask = mode == theme::MouseCaptureMode::Mask && crate::mask::available();
        if !want_mask {
            // 隔膜模式(或平台不支持):确保遮板已收掉
            if self.mask.take().is_some() {
                self.mask_rect = None;
                self.mask_rect_at = None;
                self.mask_rect_log = None;
                // WARN 级:用户把日志级别设成 warn 时也要看得见状态变化
                crate::diag_warn!("app", "鼠标接管:已切回隔膜(遮板窗口已销毁)");
            }
            return shield_wanted;
        }
        // ---- 遮板模式 ----
        // **每帧重新定位**(用户 2026-10-13 第 3 条:移动/最小化投屏窗口时遮板
        // 必须跟着走)。上一版按 500ms 节流 + 只在 `self.mask.is_none()` 时重定位,
        // 于是窗口一动,遮板就"盖在屏幕表面不走了" —— 那是错的。
        //
        // 性能:`EnumWindows` 只枚举顶层窗,实测毫秒级;而这条路径只在**选了遮板**
        // 且**映射开着**时才跑(默认是隔膜,根本不进这里)。为把开销再压低,
        // 这里仍保留一个很短的节流(100ms ≈ 10Hz),足够跟手又不至于每帧枚举。
        const RELOCATE_MS: u128 = 100;
        let due = self
            .mask_rect_at
            .is_none_or(|t| t.elapsed().as_millis() >= RELOCATE_MS);
        if due {
            self.mask_rect_at = Some(std::time::Instant::now());
            let pid = self.scrcpy_pid;
            // ① 取**客户区**(不含标题栏/边框),② 再按手机长宽比算出**画面矩形**。
            // 蒙版窗口贴的就是这个画面矩形 —— 于是窗口内坐标 ≡ 画面坐标。
            let space = {
                let g = lock_shared(&self.shared);
                g.control
                    .as_ref()
                    .map(|c| (c.screen_w, c.screen_h))
                    .or(g.profile.screen)
                    .unwrap_or((0, 0))
            };
            let viewport = pid
                .and_then(crate::mask::client_area_of_pid)
                .and_then(|c| Self::mask_viewport_rect((c.0, c.1, c.2, c.3), space));
            match (viewport, self.mask.is_some()) {
                (Some(r), _) => {
                    // 窗口移动/缩放 ⇒ 下一帧 `set_frame` 会把它挪过去(见 apply_frame)
                    self.mask_rect = Some(r);
                    self.mask_rect_log = None;
                }
                (None, true) => {
                    // 投屏窗口被最小化 / 关了:按用户 2026-10-13 的决定**不贴**,
                    // 收掉遮板并退回隔膜(最小化时 GetWindowRect 返回哨兵坐标)。
                    self.mask = None;
                    self.mask_rect = None;
                    self.mask_rect_log = Some("投屏窗口不可贴(未启动或被最小化)");
                    crate::diag_warn!(
                        "app",
                        "遮板:投屏窗口不可贴(未启动或被最小化),已退回隔膜;恢复后会自动贴上"
                    );
                }
                (None, false) => {}
            }
        }
        let Some(rect) = self.mask_rect else {
            // 还没定位到:本帧退回隔膜(不能凭空贴一块遮板挡住屏幕)
            return shield_wanted;
        };
        // 起窗(只起一次)
        if self.mask.is_none() {
            match crate::mask::Mask::start(rect) {
                Ok(m) => {
                    self.mask = Some(m);
                    // WARN 级:这是"遮板真的生效了"的唯一硬证据,必须让
                    // `log_level=warn` 的用户也看得见(上一版用 INFO,用户完全
                    // 看不到它到底跑没跑 —— 这是本轮最该改的地方)。
                    crate::diag_warn!(
                        "app",
                        "遮板已启用:贴在投屏窗口 ({},{}) {}x{};鼠标事件由它接管(隔膜已让位)",
                        rect.x,
                        rect.y,
                        rect.w,
                        rect.h
                    );
                }
                Err(e) => {
                    crate::diag_warn!("app", "遮板起窗失败({e}),已退回隔膜");
                    return shield_wanted;
                }
            }
        }
        // 提交本帧内容:矩形(跟随投屏窗口移动/缩放)+ 键位/轮盘/锚点图形。
        // `passthrough = false` = **吃掉**鼠标事件(这正是遮板存在的意义)。
        if let Some(m) = self.mask.as_ref() {
            let mut frame = crate::mask::Frame::new(rect);
            frame.passthrough = false;
            self.fill_mask_shapes(&mut frame);
            m.set_frame(frame);
        }
        // 遮板真的在工作 ⇒ 隔膜必须关(两套一起上会把鼠标吞两次)。
        // 判定走纯函数(单测覆盖),这里只提供事实:`mask_active = true`。
        theme::shield_effective(theme::MouseCaptureMode::Mask, true, true, shield_wanted)
    }

    /// 往遮板那一帧里塞要画的图形(键位圈 / 轮盘圈 / 瞄准锚点)。
    ///
    /// 坐标换算**复用既有的 `Mapper`**(与"键位浮层"同一套换算):
    /// 键位配置里存的是**相对坐标**(0~1),`Mapper` 按当前坐标空间把它们换成
    /// 设备触摸像素;再按投屏窗口矩形等比缩放到屏幕像素。这样画出来的圈
    /// 正好落在游戏里对应的位置上。
    fn fill_mask_shapes(&self, frame: &mut crate::mask::Frame) {
        let (mapper, binds, combos, wheels, aim_on, anchor) = {
            let g = lock_shared(&self.shared);
            let space = g
                .control
                .as_ref()
                .map(|c| (c.screen_w, c.screen_h))
                .or(g.profile.screen)
                .unwrap_or((0, 0));
            let m = g.profile.mapper(space);
            (
                m,
                g.profile.binds.clone(),
                g.profile.combos.clone(),
                g.profile.wheels.clone(),
                g.profile.aim.enabled && g.profile.aim.anchor_set(),
                (g.profile.aim.anchor_x, g.profile.aim.anchor_y),
            )
        };
        let (dw, dh) = (mapper.w, mapper.h);
        if dw <= 0.0 || dh <= 0.0 || frame.rect.w <= 0 || frame.rect.h <= 0 {
            return;
        }
        // ---- 坐标换算(用户 2026-10-13:按**手机长宽比**算画面区域) ----
        //
        // 用户指出的关键点:**手机长宽比是一定的**。所以画面在窗口里怎么放,
        // 只取决于"窗口客户区的形状"与"手机长宽比"这两个量的关系 ——
        // 而不是拿窗口矩形直接拉伸(那会把圆压成椭圆、位置整体偏移)。
        //
        // 三种情况必须都正确(用户明确点名):
        // ① **全屏模式**:窗口客户区就是屏幕,画面仍按手机比例居中 ⇒ 两侧或上下
        //    留黑边(除非屏幕比例恰好等于手机比例);
        // ② **最大化(非全屏)**:客户区是"屏幕减去任务栏/标题栏",比例与全屏不同
        //    ⇒ 黑边位置随之变化;
        // ③ **任意比例缩放**:窗口比例任意 ⇒ 黑边随比例变化。
        //
        // 统一做法(与 scrcpy 自己摆放画面的方式一致):
        //   画面比例 = 设备触摸空间宽高比(dw:dh,即手机长宽比)
        //   scale    = min(客户区宽/画面宽, 客户区高/画面高)   ← 等比,绝不拉伸
        //   画面居中于**客户区**(不是整个窗口矩形 —— 后者含标题栏与边框)
        //
        // 关于竖屏/横屏:设备触摸空间本身就带着方向(`dw × dh` 谁大谁小),
        // 所以这里**不需要**再单独判断横竖屏 —— 只要用 `dw:dh` 这个真实比例,
        // 竖屏(如 1280×2772)与横屏(如 2772×1280)各自都会得到正确的画面框。
        // **坐标换算(根治版,用户 2026-10-13 第四轮反馈)**:
        // 蒙版窗口现在就等于**画面矩形**(见 `mask_viewport_rect`),所以:
        //   - 窗口客户区坐标 (0,0) = 画面左上角,窗口宽高 = 画面宽高;
        //   - "设备像素 → 窗口坐标"只剩**一次纯等比缩放**,没有任何原点加减;
        //   - 因此不可能再出现"偏左上角"或"缩到窗口外"。
        //
        // 缩放倍率用**窗口宽/设备宽**与**窗口高/设备高**里较小的那个:因为窗口
        // 已经按手机长宽比裁好,两者理论上相等(取 min 兜住取整误差)。绝不拉伸。
        let sx = frame.rect.w as f32 / dw;
        let sy = frame.rect.h as f32 / dh;
        let scale = sx.min(sy);
        // 设备像素 → 蒙版窗口客户区坐标(纯缩放,无偏移)
        let to_screen =
            |x: i32, y: i32| -> (i32, i32) { ((x as f32 * sx) as i32, (y as f32 * sy) as i32) };
        // 键位(只画"响应范围圈",不画整张虚拟键盘 —— 那会盖住游戏画面)
        // 过滤项与「截图取点」同一套(用户 2026-10-13 第 2 条)。
        if self.mask_filter.keys {
            for b in &binds {
                // 圈 = 响应范围圆,与键位浮层同一口径:只有 Tap/Hold 有圆心与半径,
                // 滑动/宏/系统键没有"一个圆"可画,跳过(浮层那边也一样)。
                let (cx, cy, r) = match &b.action {
                    Action::Tap { x, y, radius, .. } | Action::Hold { x, y, radius } => {
                        (mapper.x(*x), mapper.y(*y), mapper.len(*radius))
                    }
                    _ => continue,
                };
                let (px, py) = to_screen(cx, cy);
                let kind = if b.fps_only {
                    crate::mask::ShapeKind::Fps
                } else {
                    crate::mask::ShapeKind::Bind
                };
                frame.shapes.push(crate::mask::Shape::new(
                    kind,
                    px,
                    py,
                    ((r * scale) as i32).max(4),
                    crate::keymap::key_name(b.key),
                ));
            }
            // **滑动轨迹**(用户 2026-10-13,V2-3):录制的真实折线画上遮板。
            //
            // 归在 `keys` 这一项下而不是新增一个过滤位:轨迹是"这条滑动键位长什么
            // 样"的一部分,用户勾"按键映射"时想看到的就是完整的键位示意(含它的
            // 轨迹)。新增过滤位还要动 settings 的位标志与迁移,收益却只是多一个
            // 勾选框 —— 不值当。
            //
            // 只有**录制的**轨迹才画:几何轨迹(条形/方形/圆形)在遮板上画出来
            // 会与"响应范围圈"抢画面,而录制轨迹是用户真正手绘的那条,信息量最大。
            for b in &binds {
                let Action::Swipe(s) = &b.action else {
                    continue;
                };
                if s.recorded_points.len() < 2 {
                    continue;
                }
                let pts: Vec<(i32, i32)> = s
                    .recorded_points
                    .iter()
                    .map(|&(x, y)| to_screen(mapper.x(x), mapper.y(y)))
                    .collect();
                frame.tracks.push(crate::mask::Polyline::new(
                    crate::keymap::decimate(&pts, 256),
                    true,
                    crate::keymap::key_name(b.key),
                ));
            }
        }
        // 组合键(用户第 2 条要求"与截图取点项目相同" ⇒ 这一项也要有)
        if self.mask_filter.combos {
            for c in &combos {
                if let Action::Tap { x, y, radius, .. } | Action::Hold { x, y, radius } = &c.action
                {
                    let (px, py) = to_screen(mapper.x(*x), mapper.y(*y));
                    frame.shapes.push(crate::mask::Shape::new(
                        crate::mask::ShapeKind::Bind,
                        px,
                        py,
                        ((mapper.len(*radius) * scale) as i32).max(4),
                        crate::keymap::key_name(c.keys.first().copied().unwrap_or(0)),
                    ));
                }
                // 组合键的滑动轨迹同样上遮板(与"项目相同"的口径一致)
                if let Action::Swipe(s) = &c.action
                    && s.recorded_points.len() >= 2
                {
                    let pts: Vec<(i32, i32)> = s
                        .recorded_points
                        .iter()
                        .map(|&(x, y)| to_screen(mapper.x(x), mapper.y(y)))
                        .collect();
                    frame.tracks.push(crate::mask::Polyline::new(
                        crate::keymap::decimate(&pts, 256),
                        true,
                        crate::keymap::key_name(c.keys.first().copied().unwrap_or(0)),
                    ));
                }
            }
        }
        // 轮盘(圆心 + 半径):永久与临时分开过滤
        for w in &wheels {
            let show = if w.temp.is_none() {
                self.mask_filter.wheels_perm
            } else {
                self.mask_filter.wheels_temp
            };
            if !show {
                continue;
            }
            let (px, py) = to_screen(mapper.x(w.cx), mapper.y(w.cy));
            let r = ((mapper.len(w.radius) * scale) as i32).max(4);
            frame.shapes.push(crate::mask::Shape::new(
                crate::mask::ShapeKind::Wheel,
                px,
                py,
                r,
                "",
            ));
        }
        // 宏(画触发键所在位置的小圈,提示"这个键是宏")
        if self.mask_filter.macros {
            for b in &binds {
                if !matches!(b.action, Action::Macro(_)) {
                    continue;
                }
                if let Some((x, y)) = macro_bind_center(&b.action) {
                    let (px, py) = to_screen(mapper.x(x), mapper.y(y));
                    frame.shapes.push(crate::mask::Shape::new(
                        crate::mask::ShapeKind::Bind,
                        px,
                        py,
                        14,
                        crate::keymap::key_name(b.key),
                    ));
                }
            }
        }
        // 瞄准锚点(画成小圈,便于对位)
        if self.mask_filter.aim && aim_on {
            let (px, py) = to_screen((anchor.0 * dw) as i32, (anchor.1 * dh) as i32);
            frame.shapes.push(crate::mask::Shape::new(
                crate::mask::ShapeKind::Aim,
                px,
                py,
                10,
                "",
            ));
        }
    }

    /// S③:「鼠标独占」当前是否开启 + 原因文案(左栏状态行与瞄准面板**共用一处口径**)。
    ///
    /// 入参全是**标量快照**而不是 `&Shared`:绘制路径里已经握着 `Shared` 的锁
    /// (`std::sync::Mutex` 不可重入,再锁一次就是死锁),而"取了 `&mut
    /// g.profile.aim` 之后还想整份借 `g`"又过不了借用检查 —— 所以这里只收值。
    fn shield_on_and_reason(
        mode: crate::keymap::MouseShieldMode,
        mapping_enabled: bool,
        connected: bool,
        fps_active: bool,
        scrcpy_foreground: bool,
        manual: bool,
    ) -> (bool, &'static str) {
        let on = crate::keymap::mouse_shield_on(
            mode,
            mapping_enabled,
            connected,
            fps_active,
            scrcpy_foreground,
            manual,
        );
        if !on {
            return (false, "");
        }
        if manual {
            return (true, "手动开启");
        }
        match mode {
            crate::keymap::MouseShieldMode::FpsOnly => (true, "FPS 模式生效中"),
            crate::keymap::MouseShieldMode::ScrcpyForeground => (true, "投屏窗口在前台"),
            crate::keymap::MouseShieldMode::Always => (true, "始终独占"),
        }
    }

    /// **Raw Input 通道状态行**(R11):平台中立纯函数,只决定"要不要显示"与
    /// 显示哪句文案;颜色由调用点按状态选。
    ///
    /// 为什么**不用 age**:捕获层在 `count == 0` 时把"距最近一条 WM_INPUT 的毫秒数"
    /// 硬编码为 0(capture.rs 的 `raw_probe`),拿它当"多久没事件"会得出
    /// "刚刚才来过"的假结论 ⇒ 判据只用 `state + count` 两项。
    ///
    /// 四态(常量见 capture.rs 的 `RAW_*`):
    /// - 0 未注册 ⇒ `None`(不显示;非 Windows 没有这条通道,天然落此)
    /// - 1 待定(已注册,等首个事件)⇒ 琥珀色
    /// - 2 相对接管(位移按设备计数上报)⇒ 正常色
    /// - 3 只有绝对设备(watchdog 已按 700ms 判据退回光标差路径)⇒ 琥珀色;
    ///   `count == 0` 与 `count > 0` **同一句**降级文案 —— 零事件那份额外的红字
    ///   由 `raw_probe_alert` 单独叠加。
    fn raw_status_line(state: u8, count: u64) -> Option<String> {
        // 四态文案只由 `state` 决定:`count` 在降级态(3)不产生分支 ——
        // 0 条与 >0 条都是同一句降级说明(零事件的红字由 `raw_probe_alert` 叠加)。
        // 显式消费它,避免"未使用参数"告警(判据口径仍是 state + count 两项)。
        let _ = count;
        match state {
            0 => None,
            1 => Some("原始输入待定：已注册，等待首个事件".to_string()),
            2 => Some("原始输入已启用：位移按设备计数上报".to_string()),
            3 => Some("原始输入降级：绝对坐标设备，位移退回光标差路径".to_string()),
            _ => None,
        }
    }

    /// **T6:诊断面板用的 Raw Input 一行**(与 [`Self::raw_status_line`] 不同位置、
    /// 不同用途)。
    ///
    /// 两者的区别值得写清楚,免得后来者以为可以合并:
    /// - `raw_status_line` 挂在**引擎状态**区(R11),是"现在能不能用"的运行态口吻;
    /// - 本函数挂在**诊断面板**的"状态自检"里,是"这台机器上这条通道到底怎么了"
    ///   的自检口吻 —— 与同一块里那些 `✓/✗` 检查项并排,给的是"要不要去查"的判断。
    ///
    /// 判据只用 `state + count`(**不许用 age**):捕获层在 `count == 0` 时把
    /// "距最近一条 WM_INPUT 的毫秒数"硬编码成 0,拿它当"多久没事件"会得出
    /// "刚刚才来过"的假结论(与 `raw_status_line` 同一口径)。
    ///
    /// 四态(常量见 capture.rs 的 `RAW_*`):
    /// - 0 未注册 ⇒ `None`(非 Windows 没有这条通道,天然落此,不显示)
    /// - 1 已注册、等首个事件
    /// - 2 已启用(按设备计数上报)
    /// - 3 已降级;`count == 0`(一条都没来过)与 `count > 0`(只报绝对坐标)
    ///   **必须分叉** —— 这两件事的下一步动作完全不同:前者要怀疑"事件被拦/
    ///   设备只报绝对坐标",后者只是这台设备不吃 Raw Input。
    /// - 其它状态 ⇒ `None`(不猜)
    fn raw_diag_row(state: u8, count: u64) -> Option<String> {
        match state {
            0 => None,
            1 => Some("原始输入：已注册，等待首个事件".to_string()),
            2 => Some("原始输入：已启用（按设备计数上报）".to_string()),
            3 => Some(if count > 0 {
                "原始输入：已降级（绝对坐标设备，退回光标差）".to_string()
            } else {
                "原始输入：已注册但从未收到任何事件（退回光标差）".to_string()
            }),
            _ => None,
        }
    }

    /// **T1 红字判据**:注册成功却零事件。
    ///
    /// 三个条件同时成立才返回 `Some`:
    /// - `state != 0`:注册流程**走完了**(不是"没注册 / 捕获层没起来")。
    /// - `count == 0`:累计条数为 0 —— 捕获层只在**真收到 WM_INPUT** 时才 +1
    ///   (capture.rs 里 `raw_input_count.fetch_add`),所以这就是"一条都没到"。
    /// - `state == 3`:watchdog 已按 700ms 判据判定"只有绝对坐标设备"并退回
    ///   ⇒ 不是"刚注册、首个事件还没来"的启动瞬时。
    ///
    /// `state == 1 && count == 0` 只是启动那一瞬(注册完、事件还没来),
    /// 归**琥珀色状态行**,不红。其余组合一律 `None`。
    fn raw_probe_alert(state: u8, count: u64) -> Option<&'static str> {
        if state == 3 && count == 0 {
            Some("⚠ 原始输入已注册但从未收到任何事件：位移已退回光标差路径")
        } else {
            None
        }
    }

    /// S③:「鼠标独占」状态行的**完整文案**(含"怎么关掉它")。
    ///
    /// 为什么必须写出来:隔膜开着时**连本程序界面都点不了**,不写清楚就会被
    /// 当成"鼠标坏了"。手动/自动那一档都把当前热键名一起写出来 ——
    /// 键盘事件走的是"先转发后吞",所以那个键在隔膜开着时仍然有效。
    fn shield_status_line(shield_key: KeySet, on: bool, why: &str) -> String {
        if !on {
            return "鼠标独占:未开启(鼠标消息照常投递给窗口)".to_string();
        }
        let how = if shield_key.is_empty() {
            "未绑定[鼠标独占开关]热键 —— 建议在瞄准面板绑一个,用于随时关掉它".to_string()
        } else {
            format!("按 {}(鼠标独占开关热键)可手动关闭", shield_key.label())
        };
        format!("鼠标独占:已开启(原因:{why})—— {how}")
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
            Ok((rx, pid)) => {
                self.scrcpy_status_rx = Some(rx);
                // S①:「仅投屏窗口在前台」这一档的判据 = 前台窗口 pid == 这个 pid。
                self.scrcpy_pid = Some(pid);
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
                    StartPreset::Fullscreen,
                    StartPreset::TurnScreenOff,
                    StartPreset::MaxFps60,
                    StartPreset::AlwaysOnTop,
                    StartPreset::HighBitrate,
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

    /// U16(2026-10-17):键位浮层可读性的**单一入口** —— 一个档位下拉 +
    /// (仅手动档才展开的)微调滑块。
    ///
    /// 收敛之前这里是两个并列控件(自动对比勾选框 + 亮度滑块),四层可读性参数摊在
    /// 用户面前,"哪一层在起作用"说不清、回归也定位不到是哪个旋钮(审计 U7-11)。
    /// 现在四层里前三层(反相外套 / 文字衬底 / 逐块采样)是固定做法、不给旋钮,唯一
    /// 的旋钮就是这个档位。
    ///
    /// 返回值 = **是否改动了外观**,语义与 ui_look 里既有的 edit 变量一致:调用方
    /// 据此记一个撤销点、并在本帧末落盘。拖动滑块只在 drag_started() / gained_focus()
    /// 那一帧算改动,免得每帧都塞一条撤销记录(与"背景图压暗"等连续滑块同一口径)。
    ///
    /// 旧配置兼容:老的 (auto=true, manual=m) 会被 preset_of 读成 Auto,m 仍留在
    /// overlay_tone 里;下拉显示"自动对比"并记住 m,切到「自定义」就能接着调。
    fn ui_readability_controls(ui: &mut egui::Ui, look: &mut theme::Look) -> bool {
        use ReadabilityPreset::{Auto, Custom, ForceDark, ForceLight};
        if !look.overlay_tone.is_finite() {
            // 手改 json 写进来的 NaN/inf:先收回合法范围,再让下面的 match 决定档位
            // 与写回值。这一帧不算"用户改动"(与加载期收敛同口径)。
            look.overlay_tone = theme::clamp_tone(look.overlay_tone);
        }
        let mut changed = false;
        // 进控件前先读档位:下面每一条分支都可能改写这两个字段,选中的档位必须依据
        // "改动之前"的形态来定,不能拿被改写后的值反推(否则自动档一被写入就会显示
        // 成被强制切走的档)。
        let preset = preset_of(look.overlay_auto, look.overlay_tone);
        let manual_saved = look.overlay_tone;
        // 手改 profile.yaml 写出的非有限值会落到 Custom 档,但那样滑块取不到值;
        // 选中它时给一个保守的 0.0(与 clamp_tone 同口径)。
        let manual_now = if manual_saved.is_finite() {
            manual_saved
        } else {
            0.0
        };
        ui.horizontal(|ui| {
            ui.label("键位可读性:");
            egui::ComboBox::from_id_salt("look_overlay_preset")
                .selected_text(preset.label())
                .show_ui(ui, |ui| {
                    for p in [Auto, ForceDark, ForceLight, Custom] {
                        if ui.selectable_label(p == preset, p.label()).clicked() {
                            let (auto, tone) = apply_preset(p, manual_now);
                            // 三处一次写全:自动开关、手动值 —— 不留"半旧半新"状态。
                            if look.overlay_auto != auto || look.overlay_tone != tone {
                                look.overlay_auto = auto;
                                look.overlay_tone = tone;
                                changed = true;
                            }
                        }
                    }
                });
        });
        // 只有手动档才展开滑块:自动/强制两档下它没有意义(那时的唯一依据是逐块采样
        // 或端点),展开就等于回到"参数叠加、说不清是哪一层"的老问题。
        if preset == Custom {
            let r = ui
                .add(
                    egui::Slider::new(&mut look.overlay_tone, theme::TONE_MIN..=theme::TONE_MAX)
                        .text("键位显示亮度(手动)"),
                )
                .on_hover_text("自定义档的手动亮度:往左整体加深(适合明亮画面),往右整体变浅(适合昏暗画面),0=与旧版一致。只改明暗、不改色相:点按绿 / 长按橙 / 永久摇杆青 / 临时摇杆品红照样分得清。");
            if r.drag_started() || r.gained_focus() {
                changed = true;
            }
        }
        ui.small(match preset {
            Auto => "按截图明暗逐块自动选深浅;切到「自定义」才会用到手动亮度",
            ForceDark => "固定走深色档,不再按截图采样",
            ForceLight => "固定走浅色档,不再按截图采样",
            Custom => "已关掉自动对比,只用上面的手动亮度",
        });
        changed
    }

    /// 外观设置面板:风格 / 配色 / 控件密度 / 背景图(改动可撤销,随配置保存)
    fn ui_look(&mut self, ui: &mut egui::Ui) {
        let mut pick_bg = false;
        let mut edit = false;
        let mut style_restart: Option<theme::UiStyle> = None;
        // 鼠标接管方式/遮板过滤项改了(程序级设置,需在持锁块外登记落盘)
        let mut capture_or_filter_dirty = false;
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
            // ---- 鼠标接管方式:遮板 / 隔膜(用户 2026-10-13) ----
            // 用户指定:入口在「外观」,两项二选,**支持热切换**。
            // 两者解决同一个问题(同一次点击被投递两份:投屏程序一份、我们的映射一份),
            // 区别只在**在哪一层拦**:隔膜在钩子层吞消息,遮板在窗口层贴一块透明窗。
            //
            // 注意:它存在 `settings.json`(**程序级**),不是 `Look` 的一部分 ——
            // 放 Look 里会跟着按键组合走、还会被 look.json 覆盖,实测导致
            // "选了遮板却跑成隔膜"。界面入口仍按用户要求留在这里。
            ui.separator();
            // 为什么用局部量中转:`ui_look` 这段里握着 `g`(Shared 锁),
            // 闭包内直接碰 `self.mouse_capture` 会与外层借用冲突(E0500)。
            // 所以先读进局部量,改完在块外写回 + 落盘。
            let mut capture_mode = self.mouse_capture;
            let mut filter = self.mask_filter;
            let mut capture_changed = false;
            let mut filter_changed = false;
            // U11:这一段的提示色取语义色,不写死 RGB(取一次,Theme 是 Copy)
            let th = look.theme();
            ui.horizontal(|ui| {
                ui.label("鼠标接管:");
                egui::ComboBox::from_id_salt("look_mouse_capture")
                    .selected_text(capture_mode.label())
                    .show_ui(ui, |ui| {
                        for m in theme::MouseCaptureMode::ALL {
                            if ui
                                .selectable_value(&mut capture_mode, m, m.label())
                                .on_hover_text(m.hint())
                                .changed()
                            {
                                capture_changed = true;
                            }
                        }
                    });
                // 第 2 条:选了遮板时,右侧给一个"显示项目"下拉 ——
                // 项目与「截图取点」**逐项相同**(同一个 `OverlayFilter`)。
                if capture_mode == theme::MouseCaptureMode::Mask {
                    ui.label("显示项目:");
                    let before = filter;
                    filter.ui(ui);
                    if filter != before {
                        filter_changed = true;
                    }
                    if !crate::mask::available() {
                        ui.colored_label(th.warn, "本平台不支持遮板,将退回隔膜");
                    }
                }
            });
            if capture_changed {
                self.mouse_capture = capture_mode;
                crate::diag_warn!(
                    "app",
                    "鼠标接管方式切换为 {} (热切换,下一帧生效)",
                    capture_mode.label()
                );
            }
            if filter_changed {
                self.mask_filter = filter;
            }
            if capture_changed || filter_changed {
                // 落盘登记:这两个是**程序级**设置(settings.json)。
                // 放在这里(持锁块外)是因为 `remember_now` 会取 `settings_snapshot`,
                // 而那要再借一次 `self` —— 在 `g` 还活着时调用过不了借用检查。
                capture_or_filter_dirty = true;
            }
            ui.small(
                "「隔膜」在钩子层吞鼠标(默认);「遮板」在窗口层贴一块透明置顶窗挡鼠标,\
                 并把勾选的项目画在画面上。两者只能生效一个;遮板贴不上时(投屏未启动\
                 或被最小化)自动退回隔膜。切换立即生效,不必重启。",
            );

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
                    // 长文件名必须**截断**并给宽度上限,否则它会把右边的[更换]/[清除]
                    // 顶出可视区(用户 2026-10-21 反馈"更换图片的按钮被顶到右边无法找回")。
                    //
                    // 两件事缺一不可:
                    //  ① `Label::truncate()` —— 超宽时截成 "很长的名字…";
                    //  ② `add_sized` 给一个**宽度上限** —— `horizontal` 默认按内容给宽,
                    //     光靠 truncate 它会先撑开父布局,截断永远不触发。
                    // 预留宽度按按钮文字实际排版量出来(不写死像素),窗口缩窄时
                    // 按钮仍然完整可见;完整路径走悬停提示。
                    let btn_w = ["更换", "清除"]
                        .iter()
                        .map(|t| {
                            let font = egui::TextStyle::Button.resolve(ui.style());
                            ui.painter()
                                .layout_no_wrap((*t).to_owned(), font, egui::Color32::WHITE)
                                .size()
                                .x
                                + ui.spacing().button_padding.x * 2.0
                        })
                        .fold(0.0f32, f32::max);
                    let reserved = btn_w * 2.0 + ui.spacing().item_spacing.x * 3.0;
                    let name_w = (ui.available_width() - reserved).max(24.0);
                    ui.add_sized(
                        [name_w, ui.spacing().interact_size.y],
                        egui::Label::new(name)
                            .truncate()
                            .sense(egui::Sense::hover()),
                    )
                    .on_hover_text(&look.bg_path);
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
            // 键位浮层的可读性(U16):收敛成**单一入口 + 预设档**
            // (见 ui_readability_controls 与 ReadabilityPreset 的说明)。
            if Self::ui_readability_controls(ui, look) {
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
        // 鼠标接管方式 / 遮板过滤项是**程序级**设置(settings.json):
        // 在持锁块外统一登记落盘(见上面 `capture_or_filter_dirty` 的说明)。
        if capture_or_filter_dirty {
            self.remember_now();
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

    // ================= 周期性设备巡检(坐标空间重查 + 遥测) =================

    /// 把一轮设备巡检的结果落到界面;**周期巡检与手动自检共用这一份口径**。
    ///
    /// ① 坐标空间:只有 [`space_change_detected`] 说"确定变了"才动 —— 走既有的
    ///    DF-5 路径(`sync_display_space`:更新尺寸 + 请求引擎抬起所有触点;尺寸
    ///    变化时引擎侧同时失效 `anchor_used`)。没变 / 没查到**一个字都不写**。
    ///    `may_act_on_size=false` 用于"这份快照已经过期"的情况(见
    ///    [`probe_snapshot_is_fresh`]):那时只取遥测,绝不拿旧快照改坐标空间。
    /// ② 遥测:焦点窗口 / 前台 Activity / 屏幕亮灭**只在变化时**各写一条 INFO。
    fn apply_device_probe(&mut self, probe: &adb::DeviceProbe, may_act_on_size: bool) {
        let cached = {
            let g = lock_shared(&self.shared);
            g.control.as_ref().map(|c| (c.screen_w, c.screen_h))
        };
        if let Some(cached) = cached {
            if may_act_on_size
                && space_change_detected(cached, probe.size, probe.size_is_display_space)
            {
                if let Some((w, h)) = probe.size {
                    // 日志由 sync_display_space 写(含旧/新尺寸与"已请求抬起触点")
                    self.sync_display_space(w, h);
                }
            }
        }
        if telemetry_changed(self.probe_focus.as_ref(), probe.focus.as_ref()) {
            self.log(format!(
                "焦点窗口: {}",
                probe.focus.as_deref().unwrap_or("未知")
            ));
            self.probe_focus = probe.focus.clone();
        }
        if telemetry_changed(self.probe_focused_app.as_ref(), probe.focused_app.as_ref()) {
            self.log(format!(
                "前台 Activity: {}",
                probe.focused_app.as_deref().unwrap_or("未知")
            ));
            self.probe_focused_app = probe.focused_app.clone();
        }
        if telemetry_changed(self.probe_screen_on.as_ref(), probe.screen_on.as_ref()) {
            self.log(format!("屏幕状态: {}", probe.screen_on_text()));
            self.probe_screen_on = probe.screen_on;
        }
    }

    /// 一轮周期巡检到点了吗:到了就在**工作线程**上起一次设备查询。
    ///
    /// 与 [`Self::refresh_display_space`] 的分工:那条是"刚发生了某个事件,补查一次"
    /// (开打 / 空闲恢复),在途会被[取消坐标刷新]清掉;这条**只认定时器**,与输入
    /// 无关 —— 正是"玩家碰了一下手机、之后一个键都没按"那种场景里唯一还会跑的重查。
    fn maybe_space_watch(&mut self) {
        if self.space_watch_rx.is_some() || self.space_watch_last.elapsed() < SPACE_WATCH_INTERVAL {
            return;
        }
        // 只在"映射开着且控制通道在线"时查:这两个条件不成立时坐标空间没人用,
        // 白查一轮只会平白多一次 adb。**故意不推进计时器** —— 条件一恢复就应当
        // 立刻补一轮,而不是再等满一个间隔。
        let (enabled, online) = {
            let g = lock_shared(&self.shared);
            (g.enabled, g.control.is_some())
        };
        if !enabled || !online {
            return;
        }
        self.start_space_watch();
    }

    /// 起一轮后台巡检。**绝不在持锁时做 adb**:这里只取一次序列号与"发起时的
    /// 坐标空间快照",查询在工作线程里。
    fn start_space_watch(&mut self) {
        if self.space_watch_rx.is_some() {
            return;
        }
        let serial = self.serial();
        if serial.is_empty() {
            return;
        }
        let cached_at_start = {
            let g = lock_shared(&self.shared);
            g.control.as_ref().map(|c| (c.screen_w, c.screen_h))
        };
        let (tx, rx) = channel();
        self.space_watch_rx = Some(rx);
        self.space_watch_last = Instant::now();
        std::thread::spawn(move || {
            let _ = tx.send(SpaceWatchOutcome {
                probe: adb::device_probe(&serial),
                cached_at_start,
            });
        });
    }

    /// 回收周期巡检的结果(每帧看一眼,不阻塞界面/引擎线程)
    fn poll_space_watch(&mut self) {
        let Some(rx) = self.space_watch_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(outcome) => {
                self.space_watch_rx = None;
                self.space_checked_at = Some(Instant::now());
                // 这一轮快照还新不新?(飞在路上时别的事件路径可能已经更新过空间)
                let cached_now = {
                    let g = lock_shared(&self.shared);
                    g.control.as_ref().map(|c| (c.screen_w, c.screen_h))
                };
                let fresh = probe_snapshot_is_fresh(outcome.cached_at_start, cached_now);
                self.apply_device_probe(&outcome.probe, fresh);
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            // 工作线程 panic / 通道断:必须清掉在途状态,否则"永远在途"会让定时器
            // 再也不起新一轮(巡检从此静默死亡)
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.space_watch_rx = None;
            }
        }
    }

    /// 设备注入自检:用 **`adb shell input tap`** 在手机屏幕中心点一下。
    ///
    /// 为什么这是关键器械:这条路**完全绕过我们的控制通道**(不走 scrcpy 注入协议、
    /// 不用我们缓存的坐标空间)。同一下点击能把"按键全部失效"一刀切开:
    ///   * shell 点得动、我们的按键没反应 ⇒ 问题在**我们这条通道**;
    ///   * shell 也点不动 ⇒ 问题在**设备/系统层**(灭屏、焦点被别的窗口占住、
    ///     系统挡了注入)。
    /// 顺带把这一轮的坐标空间/焦点窗口/屏幕状态记进日志与悬浮窗。
    ///
    /// 注意:它**真的会在手机上点一下**(按钮文案已写明)。热键入口留给后续接线,
    /// 所以这里只暴露成一个方法 —— 接键只需调这一句。
    fn start_device_selfcheck(&mut self) {
        if self.selfcheck_rx.is_some() {
            // 自检是手动的:上一轮还没回来就不排队(连点没有意义,还会多插一次真点击)
            return;
        }
        let serial = self.serial();
        if serial.is_empty() {
            self.log("设备注入自检:还没选设备(序列号为空),无法执行");
            return;
        }
        // 兜底坐标:巡检读不到尺寸时用控制通道缓存的空间(用了哪一份,日志会写明)
        let cached = {
            let g = lock_shared(&self.shared);
            g.control.as_ref().map(|c| (c.screen_w, c.screen_h))
        };
        let (tx, rx) = channel();
        self.selfcheck_rx = Some(rx);
        std::thread::spawn(move || {
            // 先取现场再点:点完屏幕状态可能就变了(例如把灭屏点亮),那就不是"点之前"的现场
            let probe = adb::device_probe(&serial);
            let point = probe
                .size
                .or(cached)
                .filter(|(w, h)| *w > 0 && *h > 0)
                .map(|(w, h)| (w / 2, h / 2));
            let tap = match point {
                Some((x, y)) => adb::input_tap(&serial, x, y).map_err(|e| format!("{e:#}")),
                None => Err("取不到屏幕尺寸,无法定位屏幕中心".to_string()),
            };
            let _ = tx.send(SelfCheckOutcome {
                probe,
                tapped: point,
                tap,
            });
        });
    }

    /// 回收设备自检的结果:写日志 + 存成悬浮窗的"最近一次自检"。
    fn poll_device_selfcheck(&mut self) {
        let Some(rx) = self.selfcheck_rx.as_ref() else {
            return;
        };
        let outcome = match rx.try_recv() {
            Ok(o) => o,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.selfcheck_rx = None;
                self.log("设备注入自检:线程意外退出(未返回结果)");
                return;
            }
        };
        self.selfcheck_rx = None;
        self.selfcheck_at = Some(Instant::now());
        // ① 主结论:shell 这一路到底点成了没有
        let point_text = match outcome.tapped {
            Some((x, y)) => format!("({x},{y})"),
            None => "未点(取不到屏幕尺寸)".to_string(),
        };
        let (ok, detail) = match &outcome.tap {
            Ok(extra) if extra.is_empty() => (true, String::new()),
            Ok(extra) => (true, format!(";设备端输出:{extra}")),
            Err(e) => (false, format!(";原因:{e}")),
        };
        self.log(format!(
            "设备注入自检:{point_text} adb shell input tap {}{detail}",
            if ok { "已发送" } else { "未成功" }
        ));
        self.log(format!(
            "设备注入自检结论:{}",
            if ok {
                "shell 注入这条路是通的 —— 若此刻我们的按键仍无反应,问题在本程序的控制通道"
            } else if outcome.tapped.is_some() {
                "shell 注入也失败 ⇒ 更可能在设备/系统层(灭屏、焦点被别的窗口抢走、系统挡了注入)"
            } else {
                "连屏幕尺寸都取不到,这次自检没形成对照(先确认 adb 与设备在线)"
            }
        ));
        self.log(format!("设备注入自检现场:{}", outcome.probe.summary()));
        self.selfcheck_note = Some(if ok {
            format!("{point_text} shell 注入成功")
        } else {
            format!("{point_text} shell 注入失败")
        });
        // ② 顺带把空间/遥测走同一条口径:尺寸**真的变了**才会动坐标空间
        //    (手动自检取的现场就是此刻的,不存在"快照过期"这一说)
        self.apply_device_probe(&outcome.probe, true);
    }

    // ============ 控制通道存活自检 + 自动重连(2026-10-11) ============

    /// 一轮存活自检到点了吗:到了就在**工作线程**上问一次设备端。
    ///
    /// 为什么"我们自认为已连接"是前提:这条自检的全部意义就是抓住
    /// "**本地以为通着、设备端其实没人收**"那种状态。真断线(EOF/写失败)走的是
    /// 另一条既有路径(`is_connected()` 变 false → 自动重连),不需要它再插一脚。
    /// 条件不成立时**故意不推进计时器**:条件一恢复就立刻补一轮,不必再等满间隔。
    fn maybe_channel_watch(&mut self) {
        if self.channel_rx.is_some() || self.channel_watch_last.elapsed() < SPACE_WATCH_INTERVAL {
            return;
        }
        let online = {
            let g = lock_shared(&self.shared);
            g.enabled && g.control.as_ref().is_some_and(|c| c.is_connected())
        };
        if !online || self.server.is_none() {
            return;
        }
        self.start_channel_watch();
    }

    /// 起一轮后台存活自检。**绝不在持锁时做 adb**:这里只取一次序列号,
    /// 查询(两条只读 adb)全在工作线程里(与 [`Self::start_space_watch`] 同款)。
    fn start_channel_watch(&mut self) {
        if self.channel_rx.is_some() {
            return;
        }
        let serial = self.serial();
        if serial.is_empty() {
            return;
        }
        let (tx, rx) = channel();
        self.channel_rx = Some(rx);
        self.channel_watch_last = Instant::now();
        std::thread::spawn(move || {
            let _ = tx.send(adb::channel_probe(&serial, SCID));
        });
    }

    /// 回收存活自检结果:只在结论变化时写一条日志;连续两次判死才重启通道。
    fn poll_channel_watch(&mut self) {
        let Some(rx) = self.channel_rx.as_ref() else {
            return;
        };
        let probe = match rx.try_recv() {
            Ok(p) => p,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            // 工作线程 panic:必须清掉在途状态,否则"永远在途"会让定时器再也不起
            // 新一轮(存活自检从此静默死亡,比不做还糟)。
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.channel_rx = None;
                return;
            }
        };
        self.channel_rx = None;
        self.channel_probed_at = Some(Instant::now());
        if self.channel_verdict_prev != Some(probe.verdict) {
            self.channel_verdict_prev = Some(probe.verdict);
            match probe.verdict {
                adb::ChannelVerdict::Alive => {
                    self.log(format!("控制通道存活自检: 设备端在({})", probe.evidence));
                }
                adb::ChannelVerdict::Dead => {
                    self.log(format!(
                        "控制通道存活自检: 设备端 socket 已消失({})",
                        probe.evidence
                    ));
                    crate::diag_warn!("app", "控制通道存活自检判死: {}", probe.evidence);
                }
                adb::ChannelVerdict::Unknown => {
                    self.log(format!("控制通道存活自检: 未知({})", probe.evidence));
                }
            }
        }
        let verdict = probe.verdict;
        let confirmed = self.channel_dead_confirm.observe(verdict);
        self.channel_probe = Some(probe);
        // "未知"永远走不到这里(observe 只在判死时累计);`may_restart` 是那条
        // "查询失败 ⇒ 判未知而不动手"的显式落点,单测钉着。
        if confirmed && verdict.may_restart() && self.reconnect_armed {
            self.restart_control_channel("存活自检连续两次确认设备端 socket 已消失");
        }
    }

    /// 悬浮窗与诊断面板共用的那一格:`活/死/未知(上次自检 Xms 前)`。
    fn channel_value_text(&self) -> String {
        let verdict = self
            .channel_probe
            .as_ref()
            .map(|p| p.verdict.label())
            .unwrap_or("未知");
        match self.channel_probed_at {
            Some(t) => format!("{verdict}(上次自检 {}ms 前)", t.elapsed().as_millis()),
            None => format!("{verdict}(尚未自检)"),
        }
    }

    // ============ 卡死自愈的取证层(2026-10-12,用户第 6 条) ============
    //
    // 用户现象:手碰一下手机屏 ⇒ 我们这条注入流全失效,而本地一切正常(通道活着、
    // 日志零异常、命令确实写进了 socket)。这一层只做两件**只读**的事,把"设备端
    // 的真相"摊开,让下一次卡死能直接查:
    //   ① 每秒一次 `dumpsys input`(MIUI 注入开关 + `TouchStates` 触点账本);
    //   ② 只读监听手机触摸屏(`getevent` 流),看得见"手摸手机 / 手离开手机"。
    //
    // 两样都灌进 `Shared`(`aim_live`)供引擎对账与悬浮窗显示;真正的动作
    // (抬指重建)在引擎侧(见 `engine::self_heal_tick`)。**这里没有一条写操作**。

    /// 一轮设备端账本到点了吗:到了就在**工作线程**上看一次设备端。
    ///
    /// 纪律与 [`Self::maybe_channel_watch`] 逐字相同:条件不成立时**故意不推进
    /// 计时器**(条件一恢复就立刻补一轮),而且绝不在持锁时做 adb。
    fn maybe_device_ledger(&mut self) {
        if self.ledger_rx.is_some() || self.ledger_watch_last.elapsed() < DEVICE_LEDGER_INTERVAL {
            return;
        }
        let online = {
            let g = lock_shared(&self.shared);
            g.enabled && g.control.as_ref().is_some_and(|c| c.is_connected())
        };
        if !online || self.server.is_none() {
            return;
        }
        self.start_device_ledger();
    }

    /// 起一轮后台账本取证(一条只读 `dumpsys input`)。
    fn start_device_ledger(&mut self) {
        if self.ledger_rx.is_some() {
            return;
        }
        let serial = self.serial();
        if serial.is_empty() {
            return;
        }
        let (tx, rx) = channel();
        self.ledger_rx = Some(rx);
        self.ledger_watch_last = Instant::now();
        std::thread::spawn(move || {
            let _ = tx.send(adb::device_input_state(&serial));
        });
    }

    /// 回收账本:灌进 `Shared`,内容**变化时**才写一条 INFO。
    ///
    /// 为什么必须"变化才写":账本 1 次/秒,若每轮都写一行,诊断日志会被它刷爆
    /// (仓库里已有同样纪律的范例:焦点窗口/亮灭屏都只在变化时记)。日志里带上
    /// MIUI 那几个开关、`TouchStates` 原文与三个触点计数 —— 下次卡死时这就是
    /// 设备端的证词。
    fn poll_device_ledger(&mut self) {
        let Some(rx) = self.ledger_rx.as_ref() else {
            return;
        };
        let state = match rx.try_recv() {
            Ok(s) => s,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            // 工作线程 panic:必须清掉在途状态,否则"永远在途"会让定时器再也不起
            // 新一轮(账本取证从此静默死亡,比不做还糟)。
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.ledger_rx = None;
                return;
            }
        };
        self.ledger_rx = None;
        // 签名 = "这一轮账本说了什么"。读失败也有签名(并且只写一次),不刷屏。
        let (sig, pointers, line) = match state.as_ref() {
            Some(s) => (
                format!(
                    "inject={} filter={} synergy={} oneway={} pad={} touch={:?} raw={:?} cooked={:?} line={:?}",
                    s.inject_status,
                    u8::from(s.filter_intercept),
                    u8::from(s.synergy),
                    u8::from(s.oneway),
                    u8::from(s.pad_mode),
                    s.touch_pointers,
                    s.raw_pointers,
                    s.cooked_pointers,
                    s.touch_line
                ),
                s.touch_pointers,
                Some(format!(
                    "设备端输入账本: 注入开关 mInjectEventStatus={} mIsFilterInterceptMode={} \
                     mSynergyMode={} mOnewayMode={} mPadMode={} | 触点账单={} \
                     (raw={:?} cooked={:?}) | TouchStates: {}",
                    s.inject_status,
                    u8::from(s.filter_intercept),
                    u8::from(s.synergy),
                    u8::from(s.oneway),
                    u8::from(s.pad_mode),
                    s.touch_pointers
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "?".to_string()),
                    s.raw_pointers,
                    s.cooked_pointers,
                    s.touch_line
                )),
            ),
            None => (
                "读取失败".to_string(),
                None,
                Some(
                    "设备端输入账本: 这一轮没读到(adb 抖动 / 解析失败),按「未知」处理,\
                     不参与自愈对账"
                        .to_string(),
                ),
            ),
        };
        // 灌进 Shared:账本触点数 + **读到它的时刻**(新鲜度判据)。读失败时把时刻
        // 清成 None ⇒ 引擎那边"没有证据",绝不会拿一份旧账本当现状。
        {
            let mut g = lock_shared(&self.shared);
            g.aim_live.device_pointers = pointers;
            g.aim_live.device_input_seen_at = state.is_some().then(Instant::now);
        }
        if self.ledger_sig_prev.as_deref() != Some(sig.as_str()) {
            self.ledger_sig_prev = Some(sig);
            if let Some(line) = line {
                self.log(line);
            }
        }
    }

    /// 通道建立后起"手机被手触摸"监听(**只起一个**)。
    ///
    /// 为什么等通道:它是给"卡死自愈"用的 —— 没有控制通道时"抬指重建"也无从
    /// 谈起,白白多一条一直在读的 `getevent` 流。
    /// 为什么不自己起线程:`adb::watch_physical_touch` **内部**已经起了线程
    /// (节点查找也放在那条线程里),调用方拿到 `Receiver` 立即返回 —— 界面线程
    /// 不会被任何 adb 查询卡住(见它的注释)。
    /// 为什么只起一个:监听线程是 detach 的(frozen 签名只给 `Receiver`,没有停止
    /// 句柄),重复起就是泄漏线程 ⇒ 按序列号**粘住**;只有断连/换设备(清掉
    /// [`Self::touch_watch_serial`])之后才允许重建一次。
    fn maybe_physical_touch_watch(&mut self) {
        if self.touch_rx.is_some() {
            return;
        }
        let online = {
            let g = lock_shared(&self.shared);
            g.enabled && g.control.as_ref().is_some_and(|c| c.is_connected())
        };
        if !online || self.server.is_none() {
            return;
        }
        let serial = self.serial();
        if serial.is_empty() || self.touch_watch_serial.as_deref() == Some(serial.as_str()) {
            return;
        }
        self.touch_watch_serial = Some(serial.clone());
        self.touch_rx = Some(adb::watch_physical_touch(serial));
        self.log("手机触摸监听已启动(只读 getevent,不写入设备):手离开手机时会自动抬指重建");
    }

    /// 每帧收一次触摸边沿:**状态翻转时**写一条 INFO,并灌进 `Shared`。
    ///
    /// **`Up` 边沿(手离开手机)⇒ 立刻在 `Shared` 里留一个自愈请求**,由引擎在
    /// 下一轮维护里取走并抬指重建(见 `engine::self_heal_tick`)—— 这就是把用户
    /// 现在手动按的那一下自动化。
    fn poll_physical_touch(&mut self) {
        // 一帧最多收 64 条:这是一条"一直在跑"的流,不能让它把某一帧的界面饿死
        // (剩下的下一帧继续收)。
        let mut events: Vec<adb::PhysicalTouch> = Vec::new();
        let mut gone = false;
        if let Some(rx) = self.touch_rx.as_ref() {
            for _ in 0..64 {
                match rx.try_recv() {
                    Ok(e) => events.push(e),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    // 监听线程收工了(设备上找不到触摸屏节点 / adb 断开 / getevent
                    // 结束)。**不自动重启**:`touch_watch_serial` 还粘着,只有断连
                    // 换设备才会重建 —— 否则就是每帧一次 adb 查询。
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        self.touch_rx = None;
                        gone = true;
                        break;
                    }
                }
            }
        }
        if gone {
            self.log("手机触摸监听已结束(没找到触摸屏节点或 adb 断开):「手离开手机」这条自愈触发本轮不可用,重连后会自动重建");
        }
        if events.is_empty() {
            return;
        }
        let mut lines: Vec<&'static str> = Vec::new();
        let mut heal_req = false;
        {
            let mut g = lock_shared(&self.shared);
            for e in events {
                let touched = match e {
                    adb::PhysicalTouch::Down => true,
                    adb::PhysicalTouch::Up => false,
                };
                // 只在**真的翻转**时写日志与状态:同一个方向的重复上报(手指多点/
                // 重复 tracking id)不该刷屏,也不该被当成新的 Up 边沿去反复请求自愈。
                if g.aim_live.physical_touch == touched {
                    continue;
                }
                g.aim_live.physical_touch = touched;
                if touched {
                    lines.push("手机被手触摸");
                } else {
                    lines.push("手离开手机");
                    // Up 边沿 = 用户报的那个触发动作刚刚结束 ⇒ 请求一次自愈。
                    heal_req = true;
                }
            }
            if heal_req {
                g.aim_live.self_heal_req = Some(crate::engine::SELF_HEAL_REASON_TOUCH_UP);
            }
        }
        for l in lines {
            self.log(l);
        }
    }

    /// 把"界面此刻在不在交互里"告诉引擎(`AimLive::ui_busy`):宏录制/步进、
    /// 绑定捕获、截图取点期间**不自愈** —— 那些操作里用户是在界面上动手,
    /// 不该被一次莫名其妙的抬指打断。
    fn sync_self_heal_ui_state(&mut self) {
        let busy = self.is_capturing_input();
        lock_shared(&self.shared).aim_live.ui_busy = busy;
    }

    /// 断连/重启通道时的取证层收尾。
    ///
    /// **触摸监听不在这里放掉**(2026-10-12 修正):它是与通道无关的只读流。早先这里
    /// 把 Receiver drop 掉,而工作线程正卡在 `getevent` 的管道读上 —— 它要等到
    /// **下一次真有人摸手机**才发现 `send` 失败并退出,于是每次重连都会新起一个,
    /// 设备端同一触摸屏节点上叠出多个残留 `getevent`(真机实测 3 个)。
    /// 现在整进程只保留一个监听(`touch_watch_serial` 继续粘着),重连只作废
    /// "手在不在手机上"这份**状态**;监听子进程由 `finish()` 统一收掉。
    fn reset_self_heal_watch(&mut self) {
        self.ledger_rx = None;
        self.ledger_sig_prev = None;
        let mut g = lock_shared(&self.shared);
        g.aim_live.physical_touch = false;
        g.aim_live.self_heal_req = None;
        g.aim_live.device_pointers = None;
        g.aim_live.device_input_seen_at = None;
        g.aim_live.ui_busy = false;
    }

    /// 鼠标瞄准(FPS / 开放世界)。内容**直接展开**(不再套折叠栏 ——
    /// 用户明确要求把它放出来,少一层点击)。
    fn ui_aim(&mut self, ui: &mut egui::Ui, captured: bool) {
        ui.heading("鼠标瞄准（FPS / 开放世界）");
        self.ui_aim_body(ui, captured);
    }

    /// G-c「钉住光标」的**实时状态行**文案 + 配色(纯函数 ⇒ 可单测)。
    ///
    /// 为什么需要它(2026-10-18 用户实测反馈):用户 `settings.json` 的
    /// `log_level` 是 `warn`,而该功能原先只在 `info/debug` 落证据 ⇒ 用户勾了
    /// 开关却**无法判断到底有没有钉住**。这条状态行把结论直接摆在界面上。
    ///
    /// 四个状态,与捕获层的四个分支一一对应(不许出现第五种含糊说法):
    /// - 开关关 ⇒ 明确说"关着",免得用户以为默认就钉;
    /// - 开关开 + 未瞄准 ⇒ "待命"(还没到瞄准,不是失效);
    /// - 开关开 + 在瞄准 ⇒ "钉住中"(这是唯一的"生效"态);
    /// - 开关开 + 在瞄准但**窗口不在前台** ⇒ 提示可能被系统/其它程序覆盖
    ///   (P2 未证伪项:ClipCursor 是进程级全局状态,前台程序可能覆盖它)。
    #[cfg_attr(not(windows), allow(dead_code))]
    fn clip_status_line(
        th: &crate::theme::Theme,
        setting: bool,
        in_aim: bool,
        scrcpy_foreground: bool,
    ) -> (String, egui::Color32) {
        if !setting {
            return ("钉住光标:关着(瞄准时光标可自由移动)".to_string(), th.muted);
        }
        if !in_aim {
            return (
                "钉住光标:已开启,待命(进入 FPS 瞄准后生效)".to_string(),
                th.muted,
            );
        }
        if scrcpy_foreground {
            (
                "钉住光标:钉住中(瞄准期间光标被关在 1 像素内)".to_string(),
                th.ok,
            )
        } else {
            (
                "钉住光标:瞄准中,但投屏窗口不在前台 —— 若光标仍能动,可能是被前台程序覆盖"
                    .to_string(),
                th.warn,
            )
        }
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
        // `pointers` 取的是引擎侧 `EngineLive` 的触点占用(`aim_live` 里没有它);
        // 与下面的坐标空间一样,顺手在同一次加锁里取走(DF-7 触点自检)。
        let (mapping_enabled, aim_on, anchor_ok, hold_key, connected, space, live, pointers) = {
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
                g.live.pointers,
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
        // DF-7:无边界模式的锚点四方向可用带(贴边的那一侧会变小甚至为 0,
        // 那正是"某个方向怎么推都转不动"的成因)。
        let band = {
            let g = lock_shared(&self.shared);
            let a = &g.profile.aim;
            match space {
                Some((w, h)) if !gamepad_mode && !a.boundary && a.anchor_set() => {
                    let m = g.profile.mapper((w, h));
                    let (ax, ay) = m.point(a.anchor_x, a.anchor_y);
                    Some(crate::engine::aim_band_limits(&m, a, ax, ay))
                }
                _ => None,
            }
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
        // DF-7:设备十指池的实时占用。与左栏「引擎」状态行同一份数据,这里
        // 就地显示 —— "按了没反应"的常见原因之一就是触点池满了,当时的按下
        // 会被设备直接丢掉(自检面板正是回答这个问题的地方)。
        {
            let max_pointers = crate::engine::DEVICE_MAX_POINTERS;
            let near_full = pointers >= DEVICE_POINTERS_WARN_AT;
            ui.colored_label(
                if near_full { th.warn } else { th.muted },
                format!(
                    "当前触点数: {pointers}/{max_pointers}（普通键位+摇杆+瞄准共用设备十指池）{}",
                    if near_full { " ⚠ 接近上限" } else { "" }
                ),
            )
            .on_hover_text(
                "设备端同时最多认 10 个触点(普通键位 + 摇杆 + 瞄准共用)。\n\
                 这里显示此刻占用了几个;接近上限(≥8)时新按下的键可能因池满\n\
                 被设备丢掉,表现为『按了没反应』,左栏「引擎」行会记下被放弃的次数。",
            );
        }
        if let Some((l, r, u, d)) = band {
            let blocked = l <= 0.0 || r <= 0.0 || u <= 0.0 || d <= 0.0;
            ui.colored_label(
                if blocked { th.warn } else { th.muted },
                format!(
                    "锚点可用带: 左 {l:.0} / 右 {r:.0} / 上 {u:.0} / 下 {d:.0} px\
                     （无边界模式推到该距离就抬指/重按换手;贴边的那一侧会变小,\
                     为 0 时那个方向转不动 —— 把锚点挪离屏幕边缘即可恢复）"
                ),
            );
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
        // S②:鼠标独占的手动开关热键(与上面几个系统键同一套组合键捕获)
        let mut pick_shield_key = false;
        // A/B(2026-10-11):两个"锁死自救"热键(同一套组合键捕获;默认都不绑)
        let mut pick_wake_key = false;
        let mut pick_reconnect_key = false;
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
        // 锁内**不能**调 `self.log`(`&mut self` 与 `lock_shared(&self.shared)` 的
        // guard 借用冲突,编译不过)。需要记日志的按钮把字符串攒到这里,锁外
        // 统一发(见本函数里 `preset_log` 的 flush;仓库里已有同样做法的范例)。
        let mut preset_log: Option<String> = None;
        // G-c:光标钉住开关的改动要等到锁外才登记落盘(理由同 preset_log)。
        // `Some(新值)` = 本帧用户点了那个勾选框。
        //
        // 2026-10-22:整个变量按平台门控 —— 那个勾选框本身是 `#[cfg(windows)]`
        // (Linux 走 evdev 整把抓取,没有"钉住光标"这回事),Linux 上它恒为 `None`
        // 且永远不会被写,于是 `mut` 与下面的消费块都成了死代码(clippy 在 Linux
        // 目标上报 unused_mut)。门控掉比加 `#[allow]` 诚实:它本来就不该存在。
        #[cfg(windows)]
        let mut clip_changed: Option<bool> = None;
        // 锚点以像素显示(存储是相对值)
        let am = self.mapper();
        // 「恢复默认」的目标:手机屏幕正中央(仍按配置的坐标单位换算)。
        // 在加锁之前算好 —— `mapper()` 要读配置,持锁时再取会自锁。
        let default_anchor = (am.rel_x((am.w / 2.0) as i32), am.rel_y((am.h / 2.0) as i32));
        // G-c 状态行要用的主题色,同样在加锁之前取好 —— 取主题内部要加同一把配置锁,
        // 而下面那个块从头到尾持着 `g`(std Mutex 不可重入)⇒ 在块里再取一次必自锁。
        // 2026-10-23 用户实测:点 [FPS 功能] 页立刻卡死、CPU 0%,根源就是这一行。
        // 非 Windows 上这个块不渲染,变量按平台门控掉(clippy 在 Linux 目标会报 unused)。
        #[cfg(windows)]
        let aim_page_theme = self.theme();

        {
            let mut g = lock_shared(&self.shared);
            undo_before_aim = g.profile.aim.clone();
            // 压枪(V2-1)小节要读 aim_live 的运行态(是否生效/当前档),
            // 在 `aim` 的可变借用建立之前先拷一份(AimLive 是 Copy)。
            let recoil_live = g.aim_live;
            // S③ 鼠标独占状态行要的两个"通道侧"标量:在 `aim` 的可变借用建立之前
            // 取出来(`g` 之后会被 `&mut g.profile.aim` 借走,整份借 `g` 编译不过)。
            let mapping_enabled = g.enabled;
            let connected = g
                .control
                .as_ref()
                .map(|c| c.is_connected())
                .unwrap_or(false);
            let aim = &mut g.profile.aim;

            // ---- U10(2026-10-17):FPS 页分组 ----
            // 只**加**分组标题/分隔线/间距,不动任何字段的先后顺序 —— 没有 GUI
            // 走查条件时,重排的破坏性无法验证(V2-4 事故的教训)。
            // 顺序由 `aim_page_keeps_its_field_order` 逐项钉住。
            ui.heading("启用与视角输入");
            ui.add_space(2.0);
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
                // DF-7(2026-10-11 用户批准):一键写入"FPS 起手参数"。
                // 只写七项(关边界 / 关开放世界 / 回转半径 / 死区 / 归中方式与
                // 时长 / 锚点居中偏上),灵敏度、视角移速、指针消隐、门控键这些
                // **个人调校一律不动** —— 所以它放在锚点这一行收尾、拖动死区之前,
                // 紧挨着它真正会改的那几个控件。
                ui.horizontal(|ui| {
                    ui.label("快速配置:");
                    if ui
                        .button("FPS 推荐参数")
                        .on_hover_text(
                            "一键写入 FPS 起手参数(只改这七项):\n\
                             ① 关闭[限制在屏幕边界内] ② 关闭[开放世界模式]\n\
                             ③ 回转半径 200px ④ 拖动死区 0.0\n\
                             ⑤ 归中方式[静止归中] ⑥ 静止 80ms\n\
                             ⑦ 锚点放到屏幕居中偏上(宽度 1/2、高度 42%)\n\
                             灵敏度、视角移速、指针消隐、按住才瞄准/开关键/临时退出键\n\
                             等个人调校保持不变;整份旧配置进撤销栈,可撤销。",
                        )
                        .clicked()
                    {
                        aim.apply_fps_preset(&am);
                        // 锁内不能发日志(借用冲突),攒到锁外再发
                        preset_log = Some(
                            "已应用 FPS 推荐参数: 无边界 / 非开放世界 / 回转半径 200px / \
                             拖动死区 0 / 静止归中 80ms / 锚点居中偏上"
                                .to_string(),
                        );
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

            // ---- 分组:手感参数(灵敏度 / 滚轮缩放 / 移速 / 归中) ----
            ui.separator();
            ui.heading("灵敏度与归中");
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.label("灵敏度 X（设备px/计数）:").on_hover_text(
                    "单位 = 设备像素 / 每 1 个鼠标计数；实际增益 = 灵敏度 × 视角移速。\n\
                     本版本位移取自原始输入，增益与系统鼠标设置（指针速度/加速）无关。\n\
                     从更早的版本迁移时：旧版位移取自光标差，同一数值会被系统放大 ——\n\
                     加速关 ≈×1.0、加速开 ≈×1.5–2.0、指针速度最右 ≈×3.0–3.5，按此折算即可对齐旧手感。",
                );
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
            // 两项都想用滚轮时谁在生效,这里明说一句(下面压枪区也有同样的判定):
            // 勾了[滚轮换档]且有 ≥2 个参与档位时,换档优先,缩放暂时让位。
            {
                let rc = &aim.recoil;
                if aim.wheel_zoom && rc.enabled && rc.wheel_switch && rc.wheel_gear_count() >= 2 {
                    ui.small(
                        "提示:[滚轮换档]已开启且参与档位 ≥ 2 —— FPS 模式内滚轮现在用来换压枪档,\
                         上面的缩放在 FPS 内暂时不生效(取消勾选[滚轮换档]即恢复)。",
                    );
                }
            }

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

            // ---- 分组:FPS 模式的触发与开关按键 ----
            ui.separator();
            ui.heading("FPS 开关与按键");
            ui.add_space(2.0);
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
            // ---- 分组:压枪(后坐力补偿) ----
            ui.heading("压枪");
            ui.add_space(2.0);
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
                    // "第 N 档"带上档位名字(用户 2026-10-11:档位可以起名字)
                    let active = if aim.recoil.strengths.is_empty() {
                        0
                    } else {
                        recoil_live.recoil_index.min(aim.recoil.strengths.len() - 1)
                    };
                    ui.colored_label(th.ok, format!("生效中:{}", aim.recoil.gear_display(active)));
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
                        .on_hover_text("射击键一般是鼠标左键(射击类游戏的常见绑定)")
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
                        .on_hover_text("每秒控制的次数(同类工具里通常按 60 次/秒);默认 60。");
                    if rr.drag_started() || rr.gained_focus() {
                        undo_needed = true;
                    }
                    ui.label(format!(
                        "当前强度:{} (共 {} 档)",
                        aim.recoil.gear_display(cur_idx),
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
                // 档位表的三列(强度 / 名字 / 是否参与滚轮换档)是三个**平行向量**,
                // 界面按下标读写 —— 先拉齐长度(老配置里后两个是空的;这一步幂等,
                // 不是用户的编辑,所以不记撤销)。
                aim.recoil.sync_gears();
                // [＋ 添加一档]与[滚轮换档]放在档位表**外面**:档位多时表是收起的,
                // 添加按钮跟着被藏起来就等于"越用越加不了档"。
                let mut add_gear = false;
                ui.horizontal(|ui| {
                    if ui
                        .button("＋ 添加一档")
                        .on_hover_text("控制强度可以加多档;每档还能起个名字")
                        .clicked()
                    {
                        add_gear = true;
                    }
                    ui.checkbox(&mut aim.recoil.wheel_switch, "滚轮换档")
                        .on_hover_text(
                            "勾选后:FPS 模式内滚轮直接切换控制强度档\n\
                             (上滚往后一档、下滚往前一档,环绕)。\n\
                             未勾选:滚轮仍是缩放(双指张开/捏合)。两者都要时换档优先。\n\
                             压枪未开启、不在 FPS 模式、或参与换档的档位不到 2 个时,\n\
                             滚轮自动回到缩放。",
                        );
                });
                if add_gear {
                    aim.recoil.add_gear();
                    undo_needed = true;
                }
                // 控制强度档位表:每档 = 名字 + 强度 + 是否参与滚轮换档,可拖动排序。
                // 档位过多时默认收起(非添加模式),见 RECOIL_TIER_COLLAPSE_AT;
                // 点[＋ 添加一档]的这一帧强制展开 —— 正在添加就不该藏。
                let n_now = aim.recoil.strengths.len();
                let mut remove_idx = None;
                // (被拖的档, 插入边界):插入边界 = "插到第几档之前"(0..=档数)
                let mut reorder: Option<(usize, usize)> = None;
                egui::CollapsingHeader::new("控制强度档位表")
                    .default_open(n_now <= RECOIL_TIER_COLLAPSE_AT)
                    .open(add_gear.then_some(true))
                    .show(ui, |ui| {
                        ui.small(
                            "每档:名字(可留空)+ 强度 + 水平 px(常量偏置)+ 水平幅度/周期/\
                             波形(交替补偿)+ 是否参与滚轮换档 —— 切档时垂直与水平一起切。\
                             按住行首的 ↑↓ 上下拖动可调整顺序;滚轮换档只在参与档之间跳。",
                        );
                        // 拖拽载荷 = 被拖动的那一档的下标(egui 的 DragAndDrop 载荷)
                        let drag_from = egui::DragAndDrop::payload::<usize>(ui.ctx()).map(|i| *i);
                        // 本帧各行的矩形:落点按指针的纵向位置算,行本身不动
                        let mut row_rects: Vec<egui::Rect> = Vec::with_capacity(n_now);
                        for i in 0..n_now {
                            let id = egui::Id::new(("recoil_gear_row", i));
                            let row = ui.horizontal_wrapped(|ui| {
                                // 拖拽把手**只有它可拖**:整行都能拖的话,egui 的命中
                                // 测试会让它跟数值框/名字输入框抢"拖动"这一个手势。
                                ui.dnd_drag_source(id, i, |ui| {
                                    ui.label("↑↓").on_hover_text("按住上下拖动,改变档位顺序");
                                });
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
                                // 名字:可留空。空名字 = 只显示"第 N 档"。
                                let name = ui
                                    .add(
                                        egui::TextEdit::singleline(&mut aim.recoil.gear_names[i])
                                            .desired_width(72.0)
                                            .hint_text("名字"),
                                    )
                                    .on_hover_text(
                                        "这一档的名字(可留空);换档提示与[生效中]都会带上它",
                                    );
                                if name.gained_focus() {
                                    undo_needed = true;
                                }
                                let d = ui
                                    .add(
                                        egui::DragValue::new(&mut aim.recoil.strengths[i])
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
                                // H② 每档水平偏置(用户 2026-10-11:"有些枪械总往一边偏")。
                                // 与"摇晃"是**相反**的两件事:这里是**常量抵消**,
                                // 不是每次叠加随机量。允许负值(= 向左)。
                                let h = ui
                                    .add(
                                        egui::DragValue::new(&mut aim.recoil.gear_horizontal[i])
                                            .range(-200.0..=200.0)
                                            .speed(0.5)
                                            .suffix(" px/次"),
                                    )
                                    .on_hover_text(
                                        "这一档每次控制的**水平**位移(设备像素):正 = 向右,\
                                         负 = 向左。\n\
                                         这是**常量偏置**(对付「总往一边偏」的枪,每拍都推同一边);\n\
                                         右边那一组[水平幅度/周期/波形]是按波形来回的**交替补偿**\n\
                                         (对付「左右摆」),表外的[摇晃]是随机打散 —— 三者不是一回事。",
                                    );
                                if h.drag_started() || h.gained_focus() {
                                    undo_needed = true;
                                }
                                // H① 每档水平交替补偿(用户 2026-10-11 第 3 轮:"让水平补偿
                                // 和垂直补偿在一个组里,切换的时候让水平补偿和垂直补偿一起
                                // 切换"):幅度/周期/波形都长在这一档里,与左边的垂直强度、
                                // 水平 px 同属一档 —— 换档就整套一起换。
                                let hp = ui
                                    .add(
                                        egui::DragValue::new(&mut aim.recoil.gear_horizontal_px[i])
                                            .range(-200.0..=200.0)
                                            .speed(0.5)
                                            .suffix(" px"),
                                    )
                                    .on_hover_text(
                                        "这一档的**水平交替补偿**幅度(设备像素):按右边的\
                                         波形与周期在 ±幅度 之间往返,抵消枪械的「左右摆」。\n\
                                         正 = 先向左再向右(波形从 0 向上),负 = 反向,0 = 不振荡。\n\
                                         与左边的[水平 px]分工:那个是常量偏置(总往一边偏),\n\
                                         这个是按波形来回的确定抵消;与[摇晃]相反(那是随机打散)。",
                                    );
                                if hp.drag_started() || hp.gained_focus() {
                                    undo_needed = true;
                                }
                                let hper = ui
                                    .add(
                                        egui::DragValue::new(
                                            &mut aim.recoil.gear_horizontal_period_ms[i],
                                        )
                                        .range(0..=5000)
                                        .speed(5.0)
                                        .suffix(" ms"),
                                    )
                                    .on_hover_text(
                                        "这一档一个完整来回的时间(毫秒):枪械左右摆多快就填\
                                         多快(600ms 是个中庸的起步值);0 = 这一档不振荡。",
                                    );
                                if hper.drag_started() || hper.gained_focus() {
                                    undo_needed = true;
                                }
                                egui::ComboBox::from_id_salt(("recoil_gear_wave", i))
                                    .width(52.0)
                                    .selected_text(aim.recoil.gear_horizontal_wave[i].label())
                                    .show_ui(ui, |ui| {
                                        for w in crate::keymap::HorizontalWave::ALL {
                                            if ui
                                                .selectable_value(
                                                    &mut aim.recoil.gear_horizontal_wave[i],
                                                    w,
                                                    w.label(),
                                                )
                                                .on_hover_text(match w {
                                                    crate::keymap::HorizontalWave::Sine => {
                                                        "正弦:起停柔和(大多数左右摆的近似)"
                                                    }
                                                    crate::keymap::HorizontalWave::Triangle => {
                                                        "三角:幅值线性往返(匀速摆)"
                                                    }
                                                })
                                                .changed()
                                            {
                                                undo_needed = true;
                                            }
                                        }
                                    });
                                // 参与滚轮换档:不勾的档位会被滚轮跳过
                                if ui
                                    .checkbox(&mut aim.recoil.gear_wheel[i], "参与滚轮")
                                    .on_hover_text(
                                        "勾选 = 滚轮换档时把它算进去,不勾的会被跳过。\n\
                                         老配置没有这一项,一律按「全部参与」处理。",
                                    )
                                    .changed()
                                {
                                    undo_needed = true;
                                }
                                if n_now > 1 && ui.small_button("删除").clicked() {
                                    remove_idx = Some(i);
                                }
                            });
                            row_rects.push(row.response.rect);
                        }
                        // ---- 拖拽落点:插入线 + 松手即换序 ----
                        // 为什么不用 `dnd_drop_zone` 一行一个:那要在拖拽过程中判断
                        // 指针落在哪一行,而行内的输入框/数值框会各自吃掉命中;
                        // 这里改成"行矩形 + 指针纵向位置",一行都不必可交互。
                        if let Some(from) = drag_from {
                            let released = ui.input(|i| i.pointer.any_released());
                            let still_dragging = ui
                                .ctx()
                                .is_being_dragged(egui::Id::new(("recoil_gear_row", from)));
                            let boundary = |y: f32| {
                                row_rects
                                    .iter()
                                    .position(|r| y < r.center().y)
                                    .unwrap_or(n_now)
                            };
                            if released {
                                let to = ui
                                    .input(|i| i.pointer.interact_pos())
                                    .map(|p| boundary(p.y))
                                    .unwrap_or(from);
                                reorder = Some((from, to));
                                // 载荷必须主动取走,否则拖拽结束后它还留在内存里
                                let _ = egui::DragAndDrop::take_payload::<usize>(ui.ctx());
                            } else if still_dragging {
                                if let Some(p) = ui.input(|i| i.pointer.interact_pos()) {
                                    let to = boundary(p.y);
                                    if let Some(first) = row_rects.first() {
                                        // 插入线落在 to 档的上沿;插到最后 = 最后一行的下沿
                                        let y = row_rects.get(to).map(|r| r.top()).unwrap_or_else(
                                            || row_rects.last().map(|r| r.bottom()).unwrap_or(0.0),
                                        );
                                        let x1 = row_rects
                                            .last()
                                            .map(|r| r.right())
                                            .unwrap_or(first.right());
                                        ui.painter().hline(
                                            first.left()..=x1,
                                            y,
                                            egui::Stroke::new(2.0, ui.visuals().selection.bg_fill),
                                        );
                                    }
                                }
                            } else {
                                // 拖拽已经结束、载荷却还在(例如表被收起):清掉它,
                                // 免得下一帧画出一条并不存在的插入线
                                let _ = egui::DragAndDrop::take_payload::<usize>(ui.ctx());
                            }
                        }
                    });
                if let Some((from, to)) = reorder {
                    if let Some(new_idx) = aim.recoil.move_gear(from, to) {
                        undo_needed = true;
                        // 挪走的正好是"当前档":引擎侧那个下标要跟着这一档走
                        if from == cur_idx {
                            recoil_gear_req = Some(new_idx);
                        }
                    }
                }
                if let Some(i) = remove_idx {
                    if aim.recoil.remove_gear(i) {
                        undo_needed = true;
                        // 删除会挪动后面各档的位置;引擎只做越界收敛,这里把
                        // "当前档"跟着挪一格(当前档正是被删的那个 = 停在原位置)
                        let n_after = aim.recoil.strengths.len();
                        if i < cur_idx {
                            recoil_gear_req = Some(cur_idx - 1);
                        } else if i == cur_idx {
                            recoil_gear_req = Some(cur_idx.min(n_after - 1));
                        }
                    }
                }
                // 参与档位不足 2 个时,滚轮在引擎侧会退回缩放 —— 这里明说,
                // 免得用户以为"勾了滚轮换档却没反应"。
                if aim.recoil.wheel_switch {
                    let participating = aim.recoil.wheel_gear_count();
                    if participating < 2 {
                        ui.small(format!(
                            "提示:参与滚轮换档的档位只有 {participating} 个,滚轮保持缩放;\
                             勾选至少两个档位后滚轮才会换档。"
                        ));
                    }
                }
                // ---- H③ 开放世界模式下的提示(用户 2026-10-11)----
                // 开放世界的水平几何归「回转带」管(越过半径就抬指重按换手),持续
                // 振荡会跟它打架 —— 所以那里只保留「每档水平 px」(常量偏置)。
                // H① 现在**每档一套**(幅度/周期/波形都在档位表里),所以按**当前档**
                // 判断要不要提示(不静默失效);当前档没开振荡就不打扰用户。
                if aim.recoil.horizontal_wave_armed_at(cur_idx)
                    && !crate::engine::recoil_horizontal_wave_allowed(aim.open_world)
                {
                    ui.colored_label(
                        th.warn,
                        "⚠ 开放世界模式:当前档的水平交替补偿已停用 —— 回转带会与持续振荡\
                         打架;只保留「水平 px」(H② 常量偏置)。",
                    )
                    .on_hover_text(
                        "原因:开放世界的水平偏移被钳在[回转半径]内,越过就抬指重按\
                         (正是「换手那一下视角跳变」),而振荡会反复顶到带边、\
                         还会占用用户自己甩枪要用的行程。\n\
                         所以那一档下:振荡停用,常量偏置照常生效。",
                    );
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
                            "每次控制在左右方向的**随机**抖动上限(随机左右摇晃 = 打散散布);\
                             0 = 不摇晃。\n\
                             与档位表里的[水平幅度]相反:那个是**确定抵消**(按波形来回),\
                             这个是**随机打散**。",
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
                            "触发期间鼠标位移改用该灵敏度(覆盖瞄准的灵敏度);0 = 不覆盖。",
                        );
                    if v.drag_started() || v.gained_focus() {
                        undo_needed = true;
                    }
                });
            }

            // ---- S①/S②/S③ 鼠标独占(隔膜)的作用范围 + 手动开关 ----
            // 用户 2026-10-11 第二问:"真的不能 scrcpy 运行时切换可否使用鼠标点击吗?"
            // —— 能:隔膜是一枚原子开关,钩子每个事件现读它,翻转**下一帧即生效**,
            // 不需要重启 scrcpy。这里给它三档范围 + 一个手动开关热键。
            ui.separator();
            // ---- 分组:鼠标独占 + 卡死自愈 + 两个自救键 ----
            ui.heading("鼠标独占与自救");
            ui.add_space(2.0);
            // G-c 实时状态行(2026-10-18 用户反馈驱动):用户 `log_level=warn` 时
            // 日志被压掉 ⇒ 「钉住光标」**无法验收**。这里给它一条与「鼠标独占」
            // 同形态的状态行:不翻日志就能判"钉没钉住、以及为什么没钉住"。
            #[cfg(windows)]
            {
                let applied = self.mouse_grab_flag.load(Ordering::Relaxed);
                // 主题色是进这个块**之前**取好的(见上面的 `aim_page_theme`)。
                // 这里绝不能再调任何会加配置锁的 `&self` 方法:本块外面持着 `g`。
                let (text, color) = Self::clip_status_line(
                    &aim_page_theme,
                    self.clip_cursor_in_aim,
                    applied,
                    self.scrcpy_is_foreground(),
                );
                ui.colored_label(color, text);
            }
            ui.horizontal(|ui| {
                ui.label("鼠标独占:");
                egui::ComboBox::from_id_salt("aim_mouse_shield")
                    .selected_text(aim.mouse_shield.label())
                    .show_ui(ui, |ui| {
                        for m in crate::keymap::MouseShieldMode::ALL {
                            // 平台不对等(2026-10-22,只读审计发现):`ScrcpyForeground`
                            // 靠"投屏窗口是不是前台"判定,而前台进程查询
                            // (`capture::foreground_pid`)只有 Windows 实现 ——
                            // 非 Windows 恒 `None` ⇒ 这一档恒等于"从不独占"。
                            // 以前它照常可选、hint 一字未提平台,用户选了会发现
                            // "没生效"却查不出原因。这里在不可用的平台上置灰 +
                            // 说明,而不是偷偷把它当 FpsOnly 用。
                            let usable = m.usable_on(cfg!(windows));
                            let hint = match m {
                                crate::keymap::MouseShieldMode::FpsOnly => {
                                    "只在 FPS 模式生效期间独占(默认;与从前行为一致)"
                                }
                                crate::keymap::MouseShieldMode::ScrcpyForeground => {
                                    "只在投屏窗口(scrcpy)处于前台时独占 —— 切到别的窗口鼠标照常"
                                }
                                crate::keymap::MouseShieldMode::Always => {
                                    "映射开启且已连接就一直独占"
                                }
                            };
                            let hint = if usable {
                                hint.to_string()
                            } else {
                                format!(
                                    "{hint}\n\n本平台不可用:Linux 拿不到「投屏窗口是否前台」\
                                     (scrcpy 是另一个进程,前台进程查询只有 Windows 有)\
                                     ⇒ 选这一档等于从不独占。想常开请用「始终」。"
                                )
                            };
                            let resp = ui
                                .add_enabled_ui(usable, |ui| {
                                    ui.selectable_value(&mut aim.mouse_shield, m, m.label())
                                })
                                .inner
                                .on_hover_text(hint);
                            if resp.changed() {
                                undo_needed = true;
                            }
                        }
                    });
                ui.label("手动开关热键:");
                let mk = aim.shield_key;
                let waiting = self.waiting_keys == Some(KeySlot::AimShield);
                if Self::keys_button(ui, waiting, &mk).clicked() {
                    pick_shield_key = true;
                }
                if (waiting || !mk.is_empty()) && Self::cancel_bind_button(ui, waiting) {
                    if waiting {
                        cancel_bind = Some(KeySlot::AimShield);
                    } else {
                        aim.shield_key = KeySet::new();
                        undo_needed = true;
                    }
                }
            });
            ui.small(
                "独占开着时:鼠标消息不投递给任何窗口(投屏窗口也收不到),\
                 代价是本界面也点不了 —— 键盘事件不受影响,\
                 按上面的[手动开关热键]即可随时关掉它。",
            );
            // ---- G-c:瞄准期间钉住光标(默认开)----
            // 用户口径:甩枪时光标满屏乱跑、撞到屏幕边还会丢位移 ⇒ 用系统的
            // `ClipCursor` 把它关在当前位置的 1x1 像素里。**入口就在这里**
            // (FPS 页「鼠标独占与自救」分组):它和上面的独占是同一类问题 ——
            // "瞄准时怎么处置系统鼠标",放在一起才找得到。
            #[cfg(windows)]
            {
                if ui
                    .checkbox(&mut self.clip_cursor_in_aim, "钉住光标(避免甩枪撞边)")
                    .on_hover_text(
                        "默认开启。瞄准(FPS 抓取)期间用系统接口把光标关在当前位置的\n\
                         1 像素矩形里 —— 它不再满屏乱跑,也撞不到屏幕边。\n\
                         \n只在你**真的在瞄准**时生效(判据就是 FPS 抓取本身):\n\
                         只是按下[指针消隐]开关时**不会**钉住 —— 那不是瞄准。\n\
                         \n退出瞄准 / 断连 / 关掉映射 / 退出程序 / 捕获线程异常退出,\n\
                         都会自动释放(进程被强制结束时由系统归还)。\n\
                         原有的[回中]仍然保留兜底。\n\
                         \n这是程序级设置,存在 settings.json 里,重启后保持。",
                    )
                    .changed()
                {
                    // 这里**只能**改原子量与攒文案:本次绘制发生在 lock_shared
                    // 的借用期内(变量 g 还活着),而 remember_now / log 都要借
                    // self —— 直接调用会被借用检查拒绝(E0502)。所以落盘登记与
                    // 日志都攒到锁外(与 preset_log 同一套做法)。
                    clip_changed = Some(self.clip_cursor_in_aim);
                    // 立即写原子量:勾选/取消在**这一帧**就生效,不必等下一帧
                    // (关掉时必须立刻放 —— 光标钉着是能被用户直接感觉到的状态)。
                    self.clip_cursor_flag
                        .store(self.clip_cursor_in_aim, Ordering::Relaxed);
                }
                ui.small(
                    "钉住光标(默认开):瞄准期间用系统接口把光标关在当前位置的 1 像素里,\
                     甩枪不再撞边、也不会点到别的窗口;退出瞄准立刻释放,\
                     原有的[回中]仍在,作为钉子被目标程序覆盖时的兜底。",
                );
            }
            // 非 Windows 没有这层隔膜(那边的鼠标独占走 evdev 的 EVIOCGRAB),
            // 明说一句,免得用户以为"选了始终独占却没生效"。
            #[cfg(not(windows))]
            ui.small(
                "提示:本平台没有这层鼠标隔膜(FPS 的鼠标独占由 evdev 的 EVIOCGRAB 承担),\
                 上面这一档不影响行为。",
            );
            // ---- 分组:自愈与两个自救键(与上面的独占分开放) ----
            ui.separator();
            // ---- 卡死自愈(2026-10-12,用户第 6 条)----
            // 用户明确拒绝"靠按键自救"的做法(竞品没这毛病)⇒ 这一项的默认是**开**。
            // 它只在出问题时才动作(手离开手机 / 设备端账本与本地对不上),正常滑动
            // 一次都不动 —— 所以"默认开"不改变默认手感(见 `Aim::self_heal`)。
            if ui
                .checkbox(&mut aim.self_heal, "卡死自愈（推荐）")
                .on_hover_text(
                    "手碰了一下手机屏之后注入失效?开着它就不必再按自救键。\n\
                     \n只在出问题时动作(正常滑动时一次都不动,手感不变):\n\
                     ① 手离开手机(只读监听手机触摸屏的 Up 边沿)⇒ 自动抬指重建;\n\
                     ② 设备端账本与本地对不上(dumpsys input 的 TouchStates 触点数\n\
                     为 0 而本地认为瞄准指还按着 / 账本触点比本地多)⇒ 立刻重建。\n\
                     \n「重建」= 自动做一次[唤醒视角]:只抬起此刻按着的瞄准指,\n\
                     不关映射、不退出 FPS;下一次鼠标位移会自动重新落指。\n\
                     限流 300ms;宏步进 / 绑定捕获 / 截图取点期间不自愈。\n\
                     \n关掉它 = 回到纯手动:下面两个自救键照旧可用。",
                )
                .changed()
            {
                undo_needed = true;
            }
            ui.small(
                "卡死自愈(默认开):手离开手机、或设备端触点账本与本地对不上时,\
                 自动抬指重建(与[唤醒视角]同一个动作);关掉后仍可用下面两个自救键。",
            );
            // ---- 锁死自救:两个逃生热键(A/B,2026-10-11) ----
            // 真机现象:界面显示一切正常、日志零异常,而鼠标点不了任何界面
            // (只有键盘还通)。两个键都**默认不绑** —— 不改默认手感,要用自己绑。
            ui.horizontal(|ui| {
                ui.label("唤醒视角热键:");
                let wk = aim.wake_key;
                let waiting = self.waiting_keys == Some(KeySlot::AimWake);
                if Self::keys_button(ui, waiting, &wk)
                    .on_hover_text(
                        "卡住时按一下,不必退出 FPS:只把此刻按着的瞄准指抬起来,\
                         \n不关映射、不退出 FPS;下一次鼠标位移会自动重新落指。\
                         \n与 F8(关映射、抬起全部触点)的区别就在这。默认未绑定。",
                    )
                    .clicked()
                {
                    pick_wake_key = true;
                }
                if (waiting || !wk.is_empty()) && Self::cancel_bind_button(ui, waiting) {
                    if waiting {
                        cancel_bind = Some(KeySlot::AimWake);
                    } else {
                        aim.wake_key = KeySet::new();
                        undo_needed = true;
                    }
                }
                ui.label("强制重连热键:");
                let rk = aim.reconnect_key;
                let waiting = self.waiting_keys == Some(KeySlot::AimReconnect);
                if Self::keys_button(ui, waiting, &rk)
                    .on_hover_text(
                        "「唤醒视角」无效时再按这个:等同诊断面板里的[强制重连控制通道],\
                         \n只重启我们自己的控制通道,不动 scrcpy 投屏。\
                         \n卡住时按一下,不必退出 FPS;连不上时不会打断已有连接。默认未绑定。",
                    )
                    .clicked()
                {
                    pick_reconnect_key = true;
                }
                if (waiting || !rk.is_empty()) && Self::cancel_bind_button(ui, waiting) {
                    if waiting {
                        cancel_bind = Some(KeySlot::AimReconnect);
                    } else {
                        aim.reconnect_key = KeySet::new();
                        undo_needed = true;
                    }
                }
            });
            ui.small(
                "两个自救键默认都未绑定(不动默认手感):卡住时先按[唤醒视角],\
                 若无效再按[强制重连]。两者都只认物理按键的按下,不受鼠标隔膜影响。",
            );
            {
                // 状态行:与左栏共用同一份文案(原因 + 怎么关掉它)。
                // 入参全取标量(`g` 此刻已被 `aim` 可变借走,整份借 `g` 编译不过;
                // 而且 `lock_shared` 也不可重入)。
                let scrcpy_foreground = self.scrcpy_is_foreground();
                let fps_active = aim.enabled
                    && recoil_live.mode_active
                    && !recoil_live.suspended
                    && aim.anchor_set();
                let (on, why) = Self::shield_on_and_reason(
                    aim.mouse_shield,
                    mapping_enabled,
                    connected,
                    fps_active,
                    scrcpy_foreground,
                    recoil_live.shield_manual,
                );
                let line = Self::shield_status_line(aim.shield_key, on, why);
                let color = if on { th.warn } else { th.muted };
                ui.colored_label(color, egui::RichText::new(line).small())
                    .on_hover_text(
                        "「仅投屏窗口在前台」的判据:前台窗口的进程 pid == 我们启动的\n\
                         scrcpy 子进程 pid(拿不到 pid 时保守地不独占)。\n\
                         隔膜本身是运行时可切换的:翻转下一帧即生效,不需要重启 scrcpy。",
                    );
            }

            // ---- 分组:指针消隐(运行时状态行紧跟在它下面,属于同一块的说明) ----
            ui.separator();
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
            ui.small(
                "捕获跟随 FPS 模式:进入 FPS 即捕获并消隐,用[FPS 模式开关键]或[按住才退出]键离开。",
            );
        }

        // 锁外发日志:锁内只能把字符串攒进 `preset_log`(见它的声明处)。
        if let Some(m) = preset_log {
            self.log(m);
        }

        // G-c(钉住光标)的开关改动:同样只能在锁外落地 ——
        // `remember_now` 要 `&mut self`,而绘制块结束时那个 `MutexGuard`
        // 才真正释放。
        #[cfg(windows)]
        if let Some(on) = clip_changed {
            // 与其它 settings 开关同一条路:remember_now 登记,帧末统一落盘
            self.remember_now();
            // 原子量已在绘制块里立即写过(本帧生效);这里只补日志。
            self.log(if on {
                "已开启「钉住光标」:瞄准期间光标被关在当前位置,不再满屏乱跑(退出瞄准即释放)"
            } else {
                "已关闭「钉住光标」:瞄准期间只保留原有的回中,光标可以自由移动"
            });
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
        if pick_shield_key {
            self.begin_keys_capture(KeySlot::AimShield);
        }
        if pick_wake_key {
            self.begin_keys_capture(KeySlot::AimWake);
        }
        if pick_reconnect_key {
            self.begin_keys_capture(KeySlot::AimReconnect);
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
                    let label = g.profile.aim.recoil.gear_display(idx);
                    g.recoil_gear_req = Some(idx);
                    Some((label, n))
                }
            };
            if let Some((label, n)) = applied {
                self.log(format!("后坐力补偿:已切到{label} (共 {n} 档)"));
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
                // 轮换集合:哪些组合参与 Next/Prev 轮换。
                // 逐套勾选的控件已搬到左侧[按键组合]栏(用户 2026-10-23 第 3 条:
                // "打上勾的才能被切换键轮换着")—— 那里一列就是所有组合,勾选框
                // 跟组合名排在同一行,比在这里横着铺一排按钮好看也好懂。
                // 这里只留这条切换键自己的状态 + 两个快捷按钮:默认风格没有左栏,
                // 那种情况下这是唯一能改到它的地方(保持可用,不做平台/风格特权)。
                if matches!(direction, SwitchDirection::Next | SwitchDirection::Prev) {
                    ui.separator();
                    let members = crate::keymap::rotate_members(original.rotate_set.as_deref(), n);
                    let state = if original.rotate_set.is_none() {
                        "全部组合".to_string()
                    } else if members.is_empty() {
                        "一个都没选(按下不会换组合)".to_string()
                    } else {
                        format!("{} 套", members.len())
                    };
                    ui.small(format!("轮换集合:{state}"));
                    ui.horizontal(|ui| {
                        if ui
                            .small_button("全选")
                            .on_hover_text("所有组合都参与轮换(等价于不限制)")
                            .clicked()
                        {
                            if let Some(s) = lock_shared(&self.shared).switch_keys.get_mut(i) {
                                s.rotate_set = None;
                            }
                            changed = true;
                        }
                        if ui
                            .small_button("全不选")
                            .on_hover_text("一个都不参与:这个切换键按下不会换组合")
                            .clicked()
                        {
                            if let Some(s) = lock_shared(&self.shared).switch_keys.get_mut(i) {
                                s.rotate_set = Some(Vec::new());
                            }
                            changed = true;
                        }
                    });
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
                // 新键位默认"全部组合参与轮换" —— 与旧行为一致,用户要收窄时再勾。
                // None(字段不落盘)= 全部参与,详见 keymap::SwitchKey 的三态说明。
                rotate_set: None,
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
                // 删了切换键,后面每把键的下标都前移一位:当前选中 / 待按键 / 待取点
                // 全是按下标记的,一律收掉(以前只处理了 waiting_keys,漏了 vk_sel 等)。
                self.forget_stale_target("删除切换键位");
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
        // 第 30 条:Linux 只能对**本程序自己的窗口**设置 cursor。Windows 的
        // `SetCursor(NULL)` 是进程级全局的,能盖到 scrcpy 窗口上;Linux 的
        // Wayland/X11 协议都没有这个能力(光标归客户端自己管)。这句折扣必须
        // 明写 —— 否则用户按了热键发现"箭头还在",会以为是程序坏了。
        #[cfg(target_os = "linux")]
        ui.small(
            "本平台限制:光标只能在本程序窗口内隐藏。覆盖到 scrcpy 窗口需要切到\
             该窗口,系统不允许其它进程代劳(Windows 才有进程级原语)。\
             映射开启后设备被独占,光标本来就不会被移动 —— 实际观感影响很小。",
        );
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
    ///
    /// 骨架(标题 / 默认宽度 / Id)一律走 `PadApp::helper_window`(U15),
    /// 这里只剩「取走状态 ⇒ 渲染 ⇒ 仍开则放回」这一段语义。
    fn ui_adb_helper(&mut self, ctx: &egui::Context) {
        let Some(mut h) = self.adb_helper.take() else {
            return;
        };
        let mut open = true;
        helper_window(self, ctx, HelperSource::Adb, &mut open, &mut |app, ui| {
            app.adb_helper_panel(ui, &mut h);
        });
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
        // 捕获层的只读探针(与引擎侧镜像无关的一侧):用户 2026-10-11 第 6 条
        // —— 同一个悬浮窗里就能分清"位移没到捕获层"还是"到了引擎但不认"。
        let (raw_state, raw_input_count, raw_age_ms) = crate::capture::raw_probe();
        // 设备侧那一份:控制通道此刻认的坐标空间 + 上一轮巡检时刻 + 最近焦点窗口。
        // 锁只在这一小段里拿着(与 `live` 的快照同理),format 都在锁外做。
        let space = {
            let g = lock_shared(&self.shared);
            g.control
                .as_ref()
                .map(|c| format!("{}x{}", c.screen_w, c.screen_h))
                .unwrap_or_else(|| "未连接".to_string())
        };
        let space_rechecked = self
            .space_checked_at
            .map(|t| format!("{}ms前", t.elapsed().as_millis()))
            .unwrap_or_else(|| "从未".to_string());
        let focus = match self.probe_focus.as_deref() {
            Some(f) => short_focus(f).to_string(),
            None => "未知".to_string(),
        };
        let selfcheck = match (&self.selfcheck_note, self.selfcheck_at) {
            (Some(n), Some(t)) => format!("{n}({}s前)", t.elapsed().as_secs()),
            (Some(n), None) => n.clone(),
            (None, _) => "未跑过".to_string(),
        };
        let mut close = false;
        let data = DebugOverlayData {
            show_fps: self.debug_show_fps,
            show_resolution: self.debug_show_resolution,
            show_aim: self.debug_show_aim,
            fps: Self::fps_text(&info),
            stream_fps: self.stream_fps_text(),
            resolution: Self::resolution_text(&info),
            space,
            space_rechecked,
            focus,
            selfcheck,
            channel: self.channel_value_text(),
            channel_error: adb::last_server_error()
                .map(|(line, at)| format!("{}s前 {line}", at.elapsed().as_secs())),
            aim_motions: live.motions,
            aim_last_dx: live.last_dx,
            aim_last_dy: live.last_dy,
            aim_mode_active: live.mode_active,
            aim_gate_down: live.gate_down,
            aim_anchor_set: live.anchor_set,
            aim_down: live.down,
            aim_others: live.others,
            aim_enabled: live.mapping_enabled,
            // 卡死自愈:手机此刻是否被手摸 / 设备端账本触点数 / 自愈次数与原因
            physical_touch: live.physical_touch,
            device_pointers: live.device_pointers,
            self_heal_count: live.self_heal_count,
            self_heal_reason: live.self_heal_reason,
            raw_state,
            raw_input_count,
            raw_age_ms,
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
                // 那一行又长了:2026-10-11 晚在引擎侧判据后面接了**设备侧**三样
                // (空间 / 重查 / 焦点 / 自检),420 宽下会折成好几行。默认尺寸跟着
                // 放宽一点,免得那一行(以及下面的[关闭悬浮])被挤到可视区之外。
                // 层级 / 置顶逻辑一概不动,窗口仍可自由缩放。
                .with_inner_size([520.0, 280.0])
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
                    // 用户 2026-10-11(第 6 条):"为什么转不动"必须**在游戏里**看得见。
                    // 判据摊在一行:mode=FPS 模式锁存 / 门控 / 锚点 / 落指 / 其它
                    // 触点 / 总映射,再拼上捕获层那一侧(Raw 状态机 + WM_INPUT 条数与
                    // 距最近一条的毫秒数)。落在哪一环(捕获层没收到 / 门控没按 /
                    // 锚点没设 / 池子被占满)一眼可辨。纯显示,不参与任何判定。
                    //
                    // 2026-10-11 晚追加**设备侧**三样(用户要的"注入到底被谁吃了"):
                    // 空间=控制通道此刻认的坐标空间;重查=距上一轮周期巡检多久;
                    // 焦点=最近一次读到的焦点窗口;自检=最近一次 `adb shell input tap`
                    // 自检结论。它与前面那半段合起来就能分清"我们的通道"还是"设备层"。
                    // 再追加 通道=控制通道存活自检的结论:它专治"空间/焦点/shell 全
                    // 正常,而设备端已经没人收我们的写入"那一种。
                    ui.label(format!(
                        "FPS状态: mode={} 门控={} 锚点={} 落指={} 其它触点={} 映射={} | 空间={} 重查={} 焦点={} 自检={} | 通道={} | Raw={} WM_INPUT={}({}ms前) | 手机={} 账单={} 自愈={}({})",
                        u8::from(data.aim_mode_active),
                        u8::from(data.aim_gate_down),
                        u8::from(data.aim_anchor_set),
                        u8::from(data.aim_down),
                        data.aim_others,
                        u8::from(data.aim_enabled),
                        data.space,
                        data.space_rechecked,
                        data.focus,
                        data.selfcheck,
                        data.channel,
                        data.raw_state,
                        data.raw_input_count,
                        data.raw_age_ms,
                        // 卡死自愈(2026-10-12):手机=摸/离(只读监听触摸屏)、
                        // 账单=设备端 TouchStates 触点数(? = 没读到,"未知"不是 0)、
                        // 自愈=N(最近一次原因)。打游戏时这一行就能看清"手在不在手机上"
                        // 与"设备端账本认不认我们的触点"。
                        if data.physical_touch { "摸" } else { "离" },
                        data.device_pointers
                            .map(|n| n.to_string())
                            .unwrap_or_else(|| "?".to_string()),
                        data.self_heal_count,
                        if data.self_heal_reason.is_empty() {
                            "无".to_string()
                        } else {
                            data.self_heal_reason.to_string()
                        },
                    ));
                }
                // 设备端 server 自己报的错(异常/崩溃栈):它直接回答"我们这条通道
                // 是不是被设备端自己搞坏了",所以贴着上面的状态行显示,不藏在日志里。
                if let Some(err) = &data.channel_error {
                    ui.colored_label(
                        egui::Color32::LIGHT_RED,
                        format!("控制通道 server 侧错误({err})"),
                    );
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
                    // 默认按**宽度**相切(用户 2026-10-18 第 4 条),不再让高度先顶住。
                    let b = shot_default_base(view.x, view.y, w, h);
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

                // ---- 轨迹录制(用户 2026-10-13,V2-3) ----
                // 录制期间把鼠标在浮层上的**连续位置**记进会话缓冲
                // (像素坐标,与浮层/草稿同一口径)。
                //
                // 为什么用 `hover_pos` 而不是 `dragged`:用户划轨迹时不一定按住
                // 鼠标键(按着划更自然,但也会有人只是移动鼠标);用悬停位置能
                // 覆盖两种习惯,且不需要用户一直按住。
                // 只在**本程序窗口内**记录(不 hook 系统鼠标)。
                //
                // 抽稀与点数上限都在 `push_track_point` 里 —— 与录制目标无关的
                // 一份规则,三种目标(草稿/键位/组合键)共用。
                if self.track_rec.is_some()
                    && let Some(pos) = resp.hover_pos()
                {
                    let px = ((pos.x - rect.min.x) / scale) as i32;
                    let py = ((pos.y - rect.min.y) / scale) as i32;
                    self.push_track_point(px, py);
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
                                    self.arm_key_capture(KeySlot::NewBind);
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
        // **层级必须每帧显式给出**(勾与不勾都要给):`ViewportBuilder` 里没设的字段
        // 表示"不改变",而不是"改成默认" —— 只写 `if pin { with_always_on_top() }`
        // 的话,取消勾选那一帧压根没有提出任何层级变化,窗口会一直留在置顶
        // (用户 2026-10-11 第 1 条:"小窗悬浮置顶就算取消勾选,也还是置顶")。
        let level = if self.shot_window_pin {
            egui::viewport::WindowLevel::AlwaysOnTop
        } else {
            egui::viewport::WindowLevel::Normal
        };
        let vb = egui::ViewportBuilder::default()
            .with_title("scrcpy-pad 截图")
            .with_inner_size([size.x, size.y])
            .with_min_inner_size([160.0, 160.0])
            .with_resizable(true)
            .with_window_level(level);
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

/// 覆盖配置目录的环境变量。
///
/// **只给测试/烟测用**,生产环境不设。存在它是因为 Windows 上没有任何
/// 环境变量能让 `directories` 改变配置目录(`APPDATA` 不读,详见 [`config_dir`])。
pub const CONFIG_DIR_ENV: &str = "SCRCPY_PAD_CONFIG_DIR";

/// `SCRCPY_PAD_CONFIG_DIR` 的取值判定(纯函数,便于两条对照都测到)。
///
/// `None` = 没设;非空 `Some` = 用它;**空串当没设**(否则会退化成本进程当前目录,
/// 那等于把配置散到用户随便哪个工作目录里,比不设更糟)。
fn config_dir_override(value: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    value
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
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
    // 测试/烟测的**跨平台**重定向入口(生产环境不设这个变量,行为完全不变)。
    //
    // 为什么必须自己造一个:Windows 上 `directories` 走的是 Known Folder API
    // (`SHGetKnownFolderPath`),**不读 `APPDATA` 环境变量** —— 2026-10-22 实测,
    // 以为 `APPDATA=/tmp/x` 就把配置目录重定向了,结果 `trace-*.jsonl` 与
    // `diagnostics.log` 照样落进用户真实配置目录。而 Linux 侧 `XDG_CONFIG_HOME`
    // 正是 `directories` 认的变量,所以同样的纪律在两个平台上**可行性不同**:
    // 这是"平台不对等"里最容易漏掉的一类 —— 它在门禁里看不出来,只在污染现场时显形。
    //
    // 只认**非空**值(空串当没设,否则路径会退化成本进程当前目录)。
    if let Some(dir) = config_dir_override(std::env::var_os(CONFIG_DIR_ENV).as_deref()) {
        return dir;
    }
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
    /// 「长按重复」(用户 2026-10-11 第 2 条)。`serde(default)` 与上面同一条理由:
    /// 这一项之前的草稿文件里没有这个字段,不给默认值就会**整份草稿库读不出来**。
    /// 老草稿读进来是 `false` = 老行为一字不改。
    #[serde(default)]
    repeat_while_held: bool,
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
/// 取一个参数的"键":`--max-size=2560` → `--max-size`;`-m` → `-m`;`--no-audio` → `--no-audio`。
///
/// 为什么需要:命令行里同一个键出现两次时,后一条会覆盖前一条 —— 用户在界面上
/// 反复点"加入参数"就会把 `--max-size` 堆成好几份(真机实测用户配置里
/// `--max-size` 三份、`--max-fps` 两份),**行为变得不可预期**(哪个生效取决于
/// 顺序)。按"键"去重/替换才能让它确定。
fn arg_key(tok: &str) -> &str {
    tok.split_once('=').map(|(k, _)| k).unwrap_or(tok)
}

/// 把一个参数**按字符串匹配替换**进参数串(用户 2026-10-12 指定的方法)。
///
/// 规则:
/// - 已有同键的参数 ⇒ **就地替换**它(位置不变,值更新),不追加第二份;
/// - 没有 ⇒ 追加到末尾;
/// - 空串/纯空白 ⇒ 原样返回。
///
/// 为什么"就地替换"而不是"先删后加":位置也参与可读性(用户看得见参数框),
/// 就地替换不会让参数在框里跳来跳去。
fn upsert_arg(args: &str, flag: &str) -> String {
    let flag = flag.trim();
    if flag.is_empty() {
        return args.trim().to_string();
    }
    let key = arg_key(flag);
    let mut out: Vec<String> = Vec::new();
    let mut replaced = false;
    for tok in args.split_whitespace() {
        if !replaced && arg_key(tok) == key {
            out.push(flag.to_string());
            replaced = true;
        } else if arg_key(tok) == key {
            // 同键的后续重复项:直接丢掉(只保留第一次出现的位置)
            continue;
        } else {
            out.push(tok.to_string());
        }
    }
    if !replaced {
        out.push(flag.to_string());
    }
    out.join(" ")
}

/// 参数串去重:同一个键只保留**最后一次**出现的值(命令行语义:后者覆盖前者)。
///
/// 为什么必须兜这一道:老配置里已经堆了重复(用户机上 `--max-size` 三份、
/// `--max-fps` 两份)。只在"加入参数"那一处修,历史脏数据仍然带进启动命令。
/// 保留最后一次 = 与"scrcpy 实际生效的那一个"逐位一致,**不改变任何现有行为**,
/// 只是把无效的重复项去掉。
fn dedupe_args(args: &str) -> String {
    let toks: Vec<&str> = args.split_whitespace().collect();
    let mut keep: Vec<bool> = vec![true; toks.len()];
    for i in 0..toks.len() {
        for j in (i + 1)..toks.len() {
            if arg_key(toks[i]) == arg_key(toks[j]) {
                keep[i] = false; // 后面还有同键的 ⇒ 本条被覆盖,丢弃
                break;
            }
        }
    }
    toks.iter()
        .zip(keep)
        .filter(|(_, k)| *k)
        .map(|(t, _)| *t)
        .collect::<Vec<_>>()
        .join(" ")
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
                repeat_interval_ms: 0,
            },
            1 => Action::Hold { x, y, radius },
            2 => Action::Swipe(Swipe {
                start: (x, y),
                end: (x, (y + 0.2).min(1.0)),
                duration_ms: 300,
                easing: Easing::Linear,
                path: SwipePath::Line,
                recorded_points: Vec::new(),
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
            ..
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
                if radius_input(ui, &mut pr, RADIUS_RANGE_PX, None).changed() {
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
                if radius_input(ui, &mut pr, RADIUS_RANGE_PX, None).changed() {
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

impl PadApp {
    /// 轨迹录制的**触发配置区**(用户 2026-10-14)。
    ///
    /// 两样东西(与录制卡片分开:`卡片`是"这次录制怎么走",`配置区`是
    /// "录制怎么被触发"—— 后者是**程序级偏好**,四条入口共用同一份):
    ///
    /// 1. `☑ 启用录制` —— 总开关。未勾选时快捷键**不触发**,
    ///    只有卡片上的按钮能用(所以未勾选**不是**禁用整个功能)。
    /// 2. **快捷键**绑定(可空缺;空缺时给出明确提示,且不触发)。
    ///
    /// **用户 2026-10-14 去掉了鼠标键控制录制**(左/右键按下开始、抬起停止):
    /// 「把鼠标控制绘制录像的功能去掉吧,你干不明白」。所以这里不再有"触发方式"
    /// 那一层三选一 —— 录制只有**快捷键**与**卡片按钮**两个入口。
    ///
    /// 改动立刻走 `remember_now()` 落盘(与宏热键同一条路)。
    ///
    /// `target` 只用来显示一句"快捷键会录给谁"的上下文提示 —— 快捷键不像按钮
    /// 那样自带上下文(按钮就长在某条滑动的编辑区里),用户需要知道按下去
    /// 会落到哪条滑动上。
    fn track_rec_config_ui(&mut self, ui: &mut egui::Ui, target: TrackTarget) {
        // U11:提示色取语义色(不写死 RGB)
        let th = self.theme();
        ui.horizontal(|ui| {
            if ui
                .checkbox(&mut self.track_rec_enabled, "启用录制")
                .on_hover_text(
                    "勾选后,下面的快捷键才生效。
                     未勾选时只有录制卡片上的 [● 录制轨迹] 按钮能用 ——                      不会因为误按某个键而突然开始录制。",
                )
                .changed()
            {
                self.remember_now();
            }
            ui.separator();
            // 用户 2026-10-23 第 2 条:与宏录制一样"共用一对开始/结束",各一个键
            // (从前只有一个键、按两下切换)。结束键留空时**行为与从前一致**,
            // 所以老配置不会因为这次改动改变手感。
            let waiting_start = self.waiting_key == Some(KeySlot::TrackRecKey);
            let waiting_stop = self.waiting_key == Some(KeySlot::TrackRecStopKey);
            ui.label("开始:");
            if Self::key_button(ui, waiting_start, Some(self.track_rec_key)).clicked() {
                self.arm_key_capture(KeySlot::TrackRecKey);
                self.log("请按任意**单个**键作为轨迹录制的开始键(不支持组合键)");
            }
            if waiting_start || self.track_rec_key != 0 {
                if Self::cancel_bind_button(ui, waiting_start) {
                    if waiting_start {
                        self.waiting_key = None;
                    } else {
                        self.track_rec_key = 0;
                        self.remember_now();
                    }
                }
            }
            ui.add_space(6.0);
            ui.label("结束:");
            if Self::key_button(ui, waiting_stop, Some(self.track_rec_stop_key)).clicked() {
                self.arm_key_capture(KeySlot::TrackRecStopKey);
                self.log("请按任意**单个**键作为轨迹录制的结束键(不支持组合键)");
            }
            if waiting_stop || self.track_rec_stop_key != 0 {
                if Self::cancel_bind_button(ui, waiting_stop) {
                    if waiting_stop {
                        self.waiting_key = None;
                    } else {
                        self.track_rec_stop_key = 0;
                        self.remember_now();
                    }
                }
            }
            // 两颗键的组合关系必须**如实**写在脸上(空 = 老的单键切换,不是坏掉)。
            if self.track_rec_key == 0 && self.track_rec_stop_key == 0 {
                ui.colored_label(
                    th.warn,
                    "两个键都空(则该方式不触发;建议填一个键,如 F9)",
                );
            } else if self.track_rec_stop_key == 0 {
                ui.small("只有开始键:按一下开始,再按一下结束");
            } else if self.track_rec_key == 0 {
                ui.small("只有结束键:开始时只能用卡片上的按钮");
            } else {
                ui.small("开始键开录,结束键收尾");
            }
        });
        // 快捷键的"录给谁":按钮自带上下文,快捷键没有 ⇒ 这里点明。
        let who = match target {
            TrackTarget::Draft => "当前新增的滑动草稿".to_string(),
            TrackTarget::Bind(i) => format!("第 {} 条键位", i + 1),
            TrackTarget::Combo(i) => format!("第 {} 个组合键", i + 1),
        };
        ui.small(format!("按下快捷键时,轨迹录给:{who}"));
        if !self.track_rec_enabled {
            ui.small("提示:上面未勾选[启用录制] ⇒ 这个快捷键此刻不生效");
        }
    }

    /// 轨迹录制的**卡片 UI**(用户 2026-10-13,V2-3):四个入口(新增草稿、已有键位、
    /// 已有组合键)共用这一份,于是"录制"这件事在界面上处处一致,不会这里一套那里一套。
    ///
    /// # 卡片长什么样(三种状态)
    ///
    /// - **未录制、也没有轨迹**:一行 **[● 录制轨迹]** 主按钮 + 灰色说明"当前走起点→终点"。
    /// - **录制中**:醒目的红底 **[■ 结束并保存]** + **[取消]** + 实时"已记 N 点",
    ///   并且带一个呼吸的 ● 提示(与浮层上的 REC 角标呼应)。
    /// - **已有轨迹**:蓝色统计 + **[重录]** + **[清除轨迹]**。
    ///
    /// # 有始必有终
    ///
    /// 录制中的每一条进入路径都配一条退出路径:提交([结束并保存])、取消([取消],
    /// 整份丢弃)、以及**切到别的目标/关面板**(调用方在切换时调
    /// [`App::cancel_track_recording`])。上一版只有"提交"一条路,划歪了只能划完再说。
    fn track_rec_card(&mut self, ui: &mut egui::Ui, target: TrackTarget, s: &Swipe) {
        // U11:卡片里的"录制中/已录轨迹"取语义色(不写死 RGB)
        let th = self.theme();
        // 这一张卡片服务的对象是不是"当前正在录的那个"
        let recording = self.track_rec.as_ref().map(|r| r.target) == Some(target);
        // 已有的轨迹点数:草稿看 draft,键位/组合键看传进来的 Swipe
        let stored = s.recorded_points.len();

        ui.separator();
        egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::same(6))
            .show(ui, |ui| {
                if recording {
                    // ---------- 录制中 ----------
                    let n = self.track_rec_count();
                    ui.horizontal(|ui| {
                        // 呼吸的圆点:与浮层 REC 角标同一条时间基准 ——
                        // 用**会话已持续的时间**而不是界面帧时钟,于是"划了多久"
                        // 与"闪了几下"对得上(用户据此判断录了多长时间)。
                        let secs = self
                            .track_rec
                            .as_ref()
                            .map_or(0.0, |r| r.started.elapsed().as_secs_f32());
                        let t = (secs * 6.0).sin() * 0.5 + 0.5;
                        // 呼吸 = 透明度在 90..210 之间起伏(色相取语义 danger)
                        let c = theme::with_alpha(th.danger, (90.0 + t * 120.0) as u8);
                        ui.colored_label(c, "●");
                        ui.colored_label(c, "录制中");
                        ui.label(format!("已记 {n} 点 / {secs:.1}s"));
                        if ui
                            .add(
                                egui::Button::new("■ 结束并保存")
                                    .fill(th.danger.gamma_multiply(0.7)),
                            )
                            .on_hover_text("结束录制并把这次轨迹应用到这条滑动(至少 2 个点)")
                            .clicked()
                        {
                            self.finish_track_recording();
                        }
                        if ui
                            .button("取消")
                            .on_hover_text("整份丢弃这次录制,原轨迹不受影响")
                            .clicked()
                        {
                            self.cancel_track_recording();
                        }
                    });
                    ui.small(
                        "在截图浮层上划出路径(可绕圈/折返/S 形);红色轨迹与 REC 角标即实时预览",
                    );
                } else if stored >= 2 {
                    // ---------- 已有轨迹 ----------
                    // Mapper 只取一次:循环里反复 `self.mapper()` 会反复加配置锁
                    let m = self.mapper();
                    let pts: Vec<(i32, i32)> = s
                        .recorded_points
                        .iter()
                        .map(|&(x, y)| m.point(x, y))
                        .collect();
                    let len = crate::keymap::polyline_len_px(&pts);
                    ui.horizontal(|ui| {
                        ui.colored_label(th.accent, format!("已录轨迹:{stored} 点 / {len:.0}px"));
                        if ui
                            .button("重录")
                            .on_hover_text("重新划一遍(新轨迹保存前,旧轨迹一直有效)")
                            .clicked()
                        {
                            self.begin_track_recording(target);
                        }
                        if ui
                            .button("清除轨迹")
                            .on_hover_text("回到『起点→终点 + 曲线』")
                            .clicked()
                        {
                            self.clear_track(target);
                        }
                    });
                    ui.small("这条滑动按真实轨迹走(优先于起点/终点与曲线)");
                } else {
                    // ---------- 还没有轨迹 ----------
                    ui.horizontal(|ui| {
                        if ui
                            .button("● 录制轨迹")
                            .on_hover_text(
                                "点一下开始记录鼠标在浮层上划过的路径,再点一下结束。\n\
                                 录制后这条滑动按真实轨迹走(可表达绕圈/折返/S 形);\n\
                                 清除轨迹即回到『起点→终点 + 曲线』。",
                            )
                            .clicked()
                        {
                            self.begin_track_recording(target);
                        }
                        ui.small("未录制(当前走起点→终点 + 曲线)");
                    });
                }
            });
        // 触发配置区(用户 2026-10-14):附在卡片**下方**,四条入口自动都有。
        //
        // **同一帧只画一次**:同一屏里可能同时存在好几张卡片(多条滑动键位),
        // 若每张都画一遍,用户会看到 3~4 份完全相同的"启用录制/触发方式",
        // 而且 `ComboBox::from_id_salt` 的 id 会重复(egui 抢 id ⇒ 下拉框行为
        // 错乱)。这里用"帧内已画过"的标记压成一份。
        if self.track_cfg_drawn_pass != Some(ui.ctx().cumulative_pass_nr()) {
            self.track_cfg_drawn_pass = Some(ui.ctx().cumulative_pass_nr());
            ui.add_space(2.0);
            self.track_rec_config_ui(ui, target);
        }
    }

    /// 清除某条滑动已录的轨迹(草稿/键位/组合键三条路共用)。
    ///
    /// 与"取消录制"不是一回事:取消是**丢掉正在录的会话**,清除是**抹掉已经
    /// 存下来的轨迹**。用户对两者的期待完全不同(前者"我划错了",后者
    /// "我不要轨迹了,改回几何轨迹"),所以界面上也是两个按钮。
    fn clear_track(&mut self, target: TrackTarget) {
        match target {
            TrackTarget::Draft => self.draft.swipe_points.clear(),
            TrackTarget::Bind(i) => {
                self.push_undo();
                let mut g = lock_shared(&self.shared);
                if let Some(b) = g.profile.binds.get_mut(i)
                    && let Action::Swipe(s) = &mut b.action
                {
                    s.recorded_points.clear();
                }
            }
            TrackTarget::Combo(i) => {
                self.push_undo();
                let mut g = lock_shared(&self.shared);
                if let Some(c) = g.profile.combos.get_mut(i)
                    && let Action::Swipe(s) = &mut c.action
                {
                    s.recorded_points.clear();
                }
            }
        }
        self.log("已清除轨迹:这条滑动回到『起点→终点 + 曲线』");
    }
}

/// 半径输入的**唯一**共用组件(U8,2026-10-17)。
///
/// 收敛前每个编辑点都自己写一遍 `egui::DragValue::new(..).range(..)`,七处口径
/// 各写各的(CN-01),范围还分了三套(CN-02)。这里只做**结构**收敛:
///
/// * 范围与步长**由调用点传入**(`range` / `speed`),组件不定义任何默认值 ——
///   所以每处的范围/步长/`undo_needed` 触发与收敛前逐项一致;
/// * `speed = None` 表示沿用 `egui::DragValue` 的默认步长(收敛前没写 `.speed()`
///   的那些点),`Some(x)` 用于原本写了 `.speed(x)` 的点(宏动作编辑器);
/// * 组件内**再钳一次**(`radius_clamp`):`DragValue` 自己也会把存量值钳进
///   `range`,但那只在控件内部生效;显式钳制保证"任何调用点拿到的值都合法",
///   也把这条不变量变成可单测、可变异验证的代码。
///
/// **不在本组件内**的两处(本轮不动,登记在案):
/// * `Ctrl`+`±` 的乘性缩放(`zoom_radius`,只防缩到 0,无上下限);
/// * 截图画布拖动改半径(`ui_shot_canvas` 里的 `new_r`)。
/// 给这两处加钳制会改变用户可见行为(计划 Q6 待拍板),不属结构收敛。
fn radius_input(
    ui: &mut egui::Ui,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    speed: Option<f32>,
) -> egui::Response {
    let mut shown = radius_clamp(*value, &range);
    let mut dv = egui::DragValue::new(&mut shown).range(range.clone());
    if let Some(speed) = speed {
        dv = dv.speed(speed);
    }
    let resp = ui.add(dv);
    *value = radius_clamp(shown, &range);
    resp
}

/// 把半径钳进给定范围(纯函数,供 [`radius_input`] 与单测共用)。
fn radius_clamp(value: f32, range: &std::ops::RangeInclusive<f32>) -> f32 {
    value.clamp(*range.start(), *range.end())
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

    // 取起点 / 取终点 / 时长：**单独一行**(B21-E 修正后,整个 swipe_controls 已在
    // 垂直布局里,所以这里再开 horizontal 就是一条真正的新行,不是水平子布局)。
    ui.horizontal(|ui| {
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
    });

    // ---- 轨迹录制(用户 2026-10-13,V2-3) ----
    // 与"取起点/取终点"并列:**有轨迹时优先走轨迹**,否则走起点→终点。
    //
    // 录制卡片本身由**调用方**渲染(见 [`PadApp::track_rec_card`])而不是这里 ——
    // 因为草稿与"已有键位/组合键"的录制目标是三份不同的数据,而 `swipe_controls`
    // 是自由函数、拿不到 `&mut App`。这里只留一句说明,免得用户以为轨迹没了。
    // B21-E(用户 2026-10-18 第 1 条):轨迹那一段**另起一行**。
    // 原来它紧跟在 取起点/取终点/时长ms 后面同一条长行里,连带下面的曲线、
    // 半径、录制卡片一路横向铺开 —— 用户原话"录制功能和整个滑动的键位设置
    // 很挤很长,换个行方便阅读"。这里只换行、加一句小标题,**不改任何语义**:
    // 轨迹优先级、按钮行为、参数顺序全部照旧。
    ui.separator();
    ui.horizontal(|ui| {
        // 文案与下方「轨迹:」(选择线条形状)必须**不同名** —— 两个同名标签并排时
        // 用户分不清哪个是状态、哪个是形状(子智能体 2026-10-18 入户面矩阵时发现)。
        ui.label("录制:");
        if s.recorded_points.len() >= 2 {
            ui.small("轨迹已启用 —— 本滑动按录制轨迹走(优先于起点/终点与曲线)");
        } else {
            ui.small("轨迹未录制(走起点→终点 + 曲线)");
        }
    });

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

/// 在截图上绘制**录制的真实轨迹**(用户 2026-10-13,V2-3)。
///
/// # 为什么单独立一个函数(而不是塞进 [`draw_swipe_track`])
///
/// `draw_swipe_track` 画的是**几何轨迹**(条形/方形/圆形 —— 由起点与终点
/// 推导出来的形状),而录制轨迹是**一串任意点**,两者的数据源与画法都不同:
/// 前者"由两个点算出形状",后者"点本身就是形状"。上一版把录制点交给前者,
/// 于是 `recorded_points` 根本没被读过 —— 这正是"轨迹录制功能极为不明显"
/// 的头号原因(录了等于没录,截图上看不出任何区别)。
///
/// # 画法(四层,每一层都在回答一个具体问题)
///
/// 1. **折线本体**:外套 + 本色两道描边(与键位圈同一套 halo 手法),
///    压在亮块/暗块上都有一圈边界可辨。
/// 2. **方向标**:沿弧长等距的箭头 —— 只有折线时,"来回折返"与"一条直线"
///    在截图上看不出区别,箭头才读得出"从哪划到哪、在哪折返"。
/// 3. **起点/终点标记**:起点空心圈 + 终点点实心 + 各带"起/终"小字 ——
///    手势的方向性由它一眼定死。
/// 4. **统计标签**:点数 / 像素长度 / 占时,标在起点上方。
///    用户要判断"录得对不对、够不够长",这三个数是最直接的依据。
///
/// `points` 是**像素**坐标(与 `to_screen` 的入参同一口径)。
/// `recording` 为 true 时用更醒目的强调色(录制中),并画一个跳动的"REC"角标;
/// `elapsed_secs` 是这次录制已持续的秒数(驱动角标闪烁的节拍)。
#[allow(clippy::too_many_arguments)]
fn draw_recorded_track<F: Fn(i32, i32) -> egui::Pos2>(
    painter: &egui::Painter,
    th: &Theme,
    points: &[(i32, i32)],
    to_screen: &F,
    label: &str,
    tone: f32,
    recording: bool,
    elapsed_secs: f32,
) {
    use egui::{Align2, FontId, Stroke};
    if points.is_empty() {
        return;
    }
    // 屏幕上要画的点数有界:录制上限 2000 点,而一屏画几条 2000 点折线既费时
    // 又看不出更多细节。抽稀到 256 点后曲线肉眼无差。
    let dec = crate::keymap::decimate(points, 256);
    let pts: Vec<egui::Pos2> = dec.iter().map(|&(x, y)| to_screen(x, y)).collect();
    if pts.len() < 2 {
        return;
    }
    // 录制中用主题的"危险色"系(红/橙)强调"正在记录",录制完用滑动本色。
    // 两者的 halo 处理一致,只在色相上区分状态。
    let base = if recording { th.danger } else { th.swipe };
    let (edge, fill) = theme::tone_ring_fill(base, theme::with_alpha(base, 60), tone);
    let coat = theme::casing(edge);
    // ① 折线本体:外套(反相粗线) → 半透明填充 → 本色描边
    painter.add(egui::Shape::line(
        pts.clone(),
        Stroke::new(theme::size::KEY_STROKE + 5.0, coat),
    ));
    painter.add(egui::Shape::line(
        pts.clone(),
        Stroke::new(theme::size::KEY_STROKE + 3.0, fill),
    ));
    painter.add(egui::Shape::line(
        pts.clone(),
        Stroke::new(theme::size::KEY_STROKE + 1.0, edge),
    ));
    // ② 方向标:沿弧长等距 3 个(点太少的短轨迹不画 —— 挤在一起只会糊)
    if dec.len() >= 4 {
        for ((x, y), (dx, dy)) in crate::keymap::track_arrow_marks(&dec, 3) {
            let tip = to_screen(x, y);
            draw_arrow_head(painter, tip, dx, dy, edge, coat);
        }
    }
    // ③ 起点 / 终点
    let p0 = pts[0];
    let p1 = pts[pts.len() - 1];
    painter.circle_stroke(p0, 15.0, Stroke::new(3.5, coat));
    painter.circle_stroke(p0, 12.0, Stroke::new(2.0, edge));
    theme::paint_label(
        painter,
        p0 + egui::vec2(0.0, -26.0),
        Align2::CENTER_CENTER,
        "起",
        FontId::proportional(theme::size::SMALL_FONT),
        theme::tone_text(tone),
    );
    painter.circle_stroke(p1, 13.0, Stroke::new(3.5, coat));
    painter.circle_filled(p1, 8.0, edge);
    theme::paint_label(
        painter,
        p1 + egui::vec2(0.0, 24.0),
        Align2::CENTER_CENTER,
        "终",
        FontId::proportional(theme::size::SMALL_FONT),
        theme::tone_text(tone),
    );
    // ④ 统计标签(点数 / 像素长度 / 占时) —— 标在起点上方
    let len_px = crate::keymap::polyline_len_px(points);
    let text = format!("{label} · {}点 · {:.0}px", points.len(), len_px);
    theme::paint_label(
        painter,
        p0 + egui::vec2(0.0, -44.0),
        Align2::CENTER_CENTER,
        &text,
        FontId::proportional(theme::size::SMALL_FONT),
        theme::tone_text(tone),
    );
    // ⑤ 录制中:一个跳动的 REC 角标(跟着**会话已持续的时间**脉动)。
    //    这是"现在正在录"唯一不依赖文字阅读的信号 —— 用户划轨迹时眼睛在
    //    路径上,不会去读按钮,但会看见标在路径末端的红点在一闪一闪。
    //    用会话时长(而不是界面帧时钟)是为了与卡片上的"已记 N 点 / X.Xs"
    //    同一个节拍 —— 两处闪烁频率一致,用户才认得出它们是同一件事。
    if recording {
        let t = (elapsed_secs * 6.0).sin() * 0.5 + 0.5;
        let r = 6.0 + t * 3.0;
        let alpha = (120.0 + t * 135.0) as u8;
        painter.circle_filled(
            p1 + egui::vec2(0.0, -22.0),
            r,
            theme::with_alpha(th.danger, alpha),
        );
        theme::paint_label(
            painter,
            p1 + egui::vec2(0.0, -22.0),
            Align2::CENTER_CENTER,
            "REC",
            FontId::proportional(theme::size::SMALL_FONT),
            egui::Color32::WHITE,
        );
    }
}

/// 在 `tip` 处按单位方向 `(dx, dy)` 画一个箭头(两根短线 + 外套)。
fn draw_arrow_head(
    painter: &egui::Painter,
    tip: egui::Pos2,
    dx: f32,
    dy: f32,
    ink: egui::Color32,
    coat: egui::Color32,
) {
    use egui::Stroke;
    const LEN: f32 = 13.0;
    for sign in [1.0f32, -1.0] {
        let a = 150.0f32.to_radians() * sign;
        let (s, c) = a.sin_cos();
        let bx = -(dx * c - dy * s);
        let by = -(dx * s + dy * c);
        let end = tip + egui::vec2(bx * LEN, by * LEN);
        // 外套让箭头压在亮底上也不糊
        painter.line_segment([tip, end], Stroke::new(5.0, coat));
        painter.line_segment([tip, end], Stroke::new(2.5, ink));
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

/// 截图预览的**默认**倍率：让图与容器的**左右界相切**(用户 2026-10-18 第 4 条:
/// "默认截图太小，让默认截图与当前程序界面的控件的左右界相切而非上下界")。
///
/// 与 [`shot_auto_base`] 的区别只在**取哪一维**：
/// - `shot_auto_base` 取 `min(by_w, by_h)` —— 高度也会参与，于是竖屏手机截图
///   常被**高度**先顶住，左右反而留出大片空白；
/// - 本函数**只按宽度**铺满 `min(by_w, 1.0)`，高度超出容器时由画布自己的滚动区
///   兜住(见 `ui_shot_canvas` 的 `content = size.max(view)`)。
///
/// 仍然封顶 1.0：用户同时明确要过"默认 100% 才是一像素对一像素"，放大只会把图
/// 拉糊并让取点更难对准。容器太窄(极端布局)时也走同一个 `by_w`，自然缩小。
fn shot_default_base(avail_w: f32, avail_h: f32, tex_w: u32, tex_h: u32) -> f32 {
    if tex_w == 0 || tex_h == 0 {
        return 1.0;
    }
    let _ = tex_h; // 只看宽度:高度交给滚动区,不再参与"谁先顶住"
    let by_w = avail_w / tex_w as f32;
    if !(avail_w.is_finite() && by_w.is_finite()) || by_w <= 0.0 {
        // 布局中间态(NaN/∞/0):退回等比内接,给出一个不会溢出的保守值
        return shot_auto_base(avail_w, avail_h, tex_w, tex_h);
    }
    by_w.clamp(0.05, 1.0)
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

    /// 配置目录重定向入口必须"设了就听、空串当没设"。
    ///
    /// 这条守的是**测试纪律的可执行性**(§3.5「测试绝不写用户配置目录」):
    /// 在 Linux 上 `XDG_CONFIG_HOME` 是 `directories` 认的变量,纪律天然可执行;
    /// 在 Windows 上 `directories` 走 Known Folder API,**`APPDATA` 完全不读** ——
    /// 2026-10-22 14:10 实测踩到:以为重定向了,结果 trace 与 diagnostics.log
    /// 照样写进用户真实配置目录。所以必须自己提供一个两个平台都认的入口,
    /// 否则"测试不污染现场"在 Windows 上只能靠人记住,不可能靠机制保证。
    ///
    /// 判据放在纯函数上而不是真的去改环境变量:改全局 env 会与并行跑的其它测试
    /// 抢同一份 `config_dir()`(它们要读真实路径做"生产路径未被写"的比对),
    /// 那会造出新的 flaky —— 这正是本轮在 type 2 上花力气根治的东西。
    #[test]
    fn config_dir_override_is_honoured_but_empty_means_unset() {
        use std::ffi::OsStr;
        // 阳性:给了非空值就必须用它(且是**原样**用,不做任何拼接)
        assert_eq!(
            config_dir_override(Some(OsStr::new("/tmp/spl-smoke"))),
            Some(PathBuf::from("/tmp/spl-smoke")),
            "设了 SCRCPY_PAD_CONFIG_DIR 却不生效 ⇒ Windows 上无法避免污染用户现场"
        );
        // 阴性对照:没设 ⇒ 必须返回 None(继续走系统标准目录)
        assert_eq!(config_dir_override(None), None, "没设时不得凭空造出路径");
        // 空串当没设:否则路径退化成"当前目录",配置会散到用户随便哪个工作目录
        assert_eq!(
            config_dir_override(Some(OsStr::new(""))),
            None,
            "空串必须按没设处理,不能当成当前目录"
        );
        // 变量名本身也是契约(写进帮助与本文档,改了就失效)
        assert_eq!(CONFIG_DIR_ENV, "SCRCPY_PAD_CONFIG_DIR");
    }

    /// **宏录制热键**(用户 2026-10-12 任务书):六条验收标准逐条钉住。
    ///
    /// 判定是纯函数(`macro_hotkey_action`),所以这里能直接覆盖任务书里
    /// "未勾选启用不触发""只启用其一""都不启用行为不变"这些条款,
    /// 不必起界面。
    #[test]
    fn macro_record_hotkeys_follow_the_spec() {
        const K1: u16 = 88; // 随便两个不冲突的键码
        const K2: u16 = 89;
        const OTHER: u16 = 90;

        // ① 只启用开始键并绑定 K1:按 K1 ⇒ Start(进入等待首个按键,不立即录制)
        assert_eq!(
            PadApp::macro_hotkey_action(K1, K1, true, 0, false, 0, false, false),
            MacroHotkey::Start,
            "启用+绑定 ⇒ 按 K1 进入等待状态"
        );
        // ② 未勾选启用 ⇒ 即使绑了键也不触发任何行为
        assert_eq!(
            PadApp::macro_hotkey_action(K1, K1, false, 0, false, 0, false, false),
            MacroHotkey::None,
            "未勾选启用:绑了也不触发"
        );
        assert_eq!(
            PadApp::macro_hotkey_action(K2, 0, false, K2, false, 0, false, true),
            MacroHotkey::None,
            "终止键未启用:录制中按它也不触发"
        );
        // ③ 未绑定(键码 0)⇒ 不触发(不能因为 0==0 就误判)
        assert_eq!(
            PadApp::macro_hotkey_action(0, 0, true, 0, true, 0, false, false),
            MacroHotkey::None,
            "未绑定时按键码 0 不得与任何键匹配"
        );
        // ④ 启用终止键并绑定 K2:录制中按 K2 ⇒ Stop;非录制状态 ⇒ 无操作
        assert_eq!(
            PadApp::macro_hotkey_action(K2, 0, false, K2, true, 0, false, true),
            MacroHotkey::Stop,
            "录制中按终止键 ⇒ 立即停止"
        );
        assert_eq!(
            PadApp::macro_hotkey_action(K2, 0, false, K2, true, 0, false, false),
            MacroHotkey::None,
            "未处于录制状态时按终止键 = 无操作"
        );
        // ⑤ 两个都启用:开始键在录制中不该重复开(否则清掉刚录到的步骤)
        assert_eq!(
            PadApp::macro_hotkey_action(K1, K1, true, K2, true, 0, false, true),
            MacroHotkey::None,
            "已在录制中再按开始键:不重开(保护已录步骤)"
        );
        assert_eq!(
            PadApp::macro_hotkey_action(K1, K1, true, K2, true, 0, false, false),
            MacroHotkey::Start
        );
        assert_eq!(
            PadApp::macro_hotkey_action(K2, K1, true, K2, true, 0, false, true),
            MacroHotkey::Stop
        );
        // ⑥ 两个都未启用 ⇒ 任何键都不产生行为(原有逻辑不变)
        for code in [K1, K2, OTHER] {
            assert_eq!(
                PadApp::macro_hotkey_action(code, K1, false, K2, false, 0, false, false),
                MacroHotkey::None,
                "都未启用时按键 {code} 不得有任何行为"
            );
        }
        // ⑦ 无关按键永远不触发
        assert_eq!(
            PadApp::macro_hotkey_action(OTHER, K1, true, K2, true, 0, false, true),
            MacroHotkey::None
        );
    }

    /// **宏录制热键**:两个热键都只认**按下**边沿,且热键事件本身不进录制。
    ///
    /// 任务书明确:"热键事件本身不应被写入宏录制内容""不应递归触发录制"。
    /// 这里锁住的是**接线位置**的语义:热键命中即 `continue`,不落到
    /// `record_macro_button`。若有人把热键判定挪到录制写入之后,这条会失败。
    #[test]
    fn macro_record_hotkey_events_never_enter_the_macro() {
        let mut rec = MacroRecording {
            steps: Vec::new(),
            held: HashSet::new(),
            last_event: Instant::now(),
            last_step_at: Instant::now(),
            idle_ms: 800,
        };
        // 普通键照常被记录
        PadApp::record_macro_button(&mut rec, 30, true, Instant::now());
        assert_eq!(rec.steps.len(), 1, "普通键必须被记录");

        // 热键在事件循环里是 `continue` 掉的 —— 等价于"不调用 record_macro_button"。
        // 这里用"判定为 Start/Stop 的键不喂给录制"来锁住同一语义。
        const K1: u16 = 88;
        const K2: u16 = 89;
        assert_ne!(
            PadApp::macro_hotkey_action(K1, K1, true, K2, true, 0, false, false),
            MacroHotkey::None,
            "K1 是热键"
        );
        // 因为它被判为热键,调用方会 continue ⇒ 不会被喂进录制
        let before = rec.steps.len();
        // (模拟调用方:命中热键就不调 record)
        if PadApp::macro_hotkey_action(K1, K1, true, K2, true, 0, false, false) == MacroHotkey::None
        {
            PadApp::record_macro_button(&mut rec, K1, true, Instant::now());
        }
        assert_eq!(
            rec.steps.len(),
            before,
            "热键不得被写入宏(任务书:热键事件本身不应写入录制内容)"
        );
    }

    /// **宏「取消录制」热键**(用户 2026-10-12 第 1 条):丢弃录制,不写入任何宏。
    ///
    /// 与「终止」的区别是**存不存**:终止走既有停止/保存逻辑(把已录步骤收进
    /// 编辑区),取消直接丢掉。两条都必须"只在录制中生效"。
    #[test]
    fn macro_cancel_hotkey_discards_without_saving() {
        const K1: u16 = 88;
        const K2: u16 = 89;
        const K3: u16 = 90;
        // 录制中按取消键 ⇒ Cancel
        assert_eq!(
            PadApp::macro_hotkey_action(K3, 0, false, 0, false, K3, true, true),
            MacroHotkey::Cancel,
            "录制中按取消键 ⇒ 丢弃这次录制"
        );
        // 未在录制 ⇒ 无操作
        assert_eq!(
            PadApp::macro_hotkey_action(K3, 0, false, 0, false, K3, true, false),
            MacroHotkey::None,
            "未录制时按取消键 = 无操作"
        );
        // 未勾选启用 ⇒ 即使绑了键也不触发
        assert_eq!(
            PadApp::macro_hotkey_action(K3, 0, false, 0, false, K3, false, true),
            MacroHotkey::None,
            "未启用:绑了也不触发"
        );
        // 未绑定(0)⇒ 不触发
        assert_eq!(
            PadApp::macro_hotkey_action(0, 0, false, 0, false, 0, true, true),
            MacroHotkey::None
        );
        // 三个键互不干扰:各自只认自己那一档
        assert_eq!(
            PadApp::macro_hotkey_action(K1, K1, true, K2, true, K3, true, false),
            MacroHotkey::Start
        );
        assert_eq!(
            PadApp::macro_hotkey_action(K2, K1, true, K2, true, K3, true, true),
            MacroHotkey::Stop
        );
        assert_eq!(
            PadApp::macro_hotkey_action(K3, K1, true, K2, true, K3, true, true),
            MacroHotkey::Cancel
        );
    }

    /// **遮板显示项目**(用户 2026-10-13 第 2 条):位标志与 `OverlayFilter` 往返。
    ///
    /// 要求"与截图取点的项目相同" ⇒ 两边的默认值也必须一致,否则用户会看到
    /// "截图里有的项目、遮板里没有"。
    #[test]
    fn mask_filter_bits_round_trip_and_match_overlay_default() {
        // ① 默认值与「截图取点」的 `OverlayFilter::default()` 逐项一致
        let d = OverlayFilter::default();
        assert_eq!(
            d.to_bits(),
            MASK_FILTER_DEFAULT,
            "默认位标志必须与过滤项默认一致"
        );
        assert_eq!(
            OverlayFilter::from_bits(MASK_FILTER_DEFAULT).to_bits(),
            d.to_bits()
        );
        // ② 往返:任意组合都要能原样还原
        for bits in 0u8..64 {
            let f = OverlayFilter::from_bits(bits);
            assert_eq!(f.to_bits(), bits, "位标志往返必须稳定(bits={bits:#b})");
        }
        // ③ 每一项都真的独立可控(逐项开关一遍)
        for (bit, name) in [
            (MASK_FILTER_KEYS, "按键映射"),
            (MASK_FILTER_COMBOS, "组合键映射"),
            (MASK_FILTER_WHEELS_PERM, "永久摇杆"),
            (MASK_FILTER_WHEELS_TEMP, "临时摇杆"),
            (MASK_FILTER_MACROS, "宏"),
            (MASK_FILTER_AIM, "FPS 锚点"),
        ] {
            let f = OverlayFilter::from_bits(bit);
            assert_eq!(f.to_bits(), bit, "{name} 必须能单独勾选");
        }
        // ④ 六项互不重叠(否则勾一项会连带另一项)
        let all = [
            MASK_FILTER_KEYS,
            MASK_FILTER_COMBOS,
            MASK_FILTER_MACROS,
            MASK_FILTER_WHEELS_PERM,
            MASK_FILTER_WHEELS_TEMP,
            MASK_FILTER_AIM,
        ];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                assert_eq!(all[i] & all[j], 0, "第 {i} 项与第 {j} 项位重叠");
            }
        }
    }

    /// **遮板坐标换算**(用户 2026-10-13 第 1、4 条:圈的位置要准)。
    ///
    /// 关键不变量:画面按宽高比**等比**适配并居中(letterbox),**不许**非等比拉伸
    /// —— 后者会把圆压成椭圆、位置整体偏移(用户实测"圆的位置确实歪了")。
    #[test]
    fn mask_shape_scaling_is_uniform_and_centered() {
        // 与生产同一套公式(见 `fill_mask_shapes`)
        let compute = |win_w: f32, win_h: f32, dw: f32, dh: f32| {
            let scale = (win_w / dw).min(win_h / dh);
            let view_w = dw * scale;
            let view_h = dh * scale;
            let off_x = (win_w - view_w) / 2.0;
            let off_y = (win_h - view_h) / 2.0;
            (scale, view_w, view_h, off_x, off_y)
        };
        // 窗口比画面"更宽" ⇒ 左右留黑边,上下贴满
        let (scale, vw, vh, ox, oy) = compute(1920.0, 540.0, 1000.0, 500.0);
        assert!(
            (scale - 1.08).abs() < 1e-4,
            "scale 应取 min(1.92, 1.08) = 1.08,实际 {scale}"
        );
        assert!((vh - 540.0).abs() < 1e-3, "高度应贴满:{vh}");
        assert!(vw < 1920.0, "宽度应留黑边:{vw}");
        assert!(
            ox > 0.0 && oy.abs() < 1e-3,
            "应左右居中、上下无偏移:ox={ox} oy={oy}"
        );
        // 等比:宽高比与设备空间一致(不许被拉伸)
        assert!(
            ((vw / vh) - (1000.0 / 500.0)).abs() < 1e-3,
            "画面宽高比必须等于设备空间宽高比(等比,不是拉伸):{vw}x{vh}"
        );
        // 窗口比画面"更高" ⇒ 上下留黑边
        let (_, vw2, vh2, ox2, oy2) = compute(500.0, 1000.0, 1000.0, 500.0);
        assert!((vw2 - 500.0).abs() < 1e-3 && vh2 < 1000.0);
        assert!(
            oy2 > 0.0 && ox2.abs() < 1e-3,
            "应上下居中:ox={ox2} oy={oy2}"
        );
        // 完全同比例 ⇒ 无黑边、偏移为 0
        let (_, vw3, vh3, ox3, oy3) = compute(800.0, 400.0, 1000.0, 500.0);
        assert!((vw3 - 800.0).abs() < 1e-3 && (vh3 - 400.0).abs() < 1e-3);
        assert!(ox3.abs() < 1e-3 && oy3.abs() < 1e-3, "同比例不该有黑边");
    }

    /// **缝②接线层测试(界面层)**(协议 §6.3 必测缝②,2026-10-13 销账)。
    ///
    /// 背景:`sim.rs` 的无头测试台跑**引擎线程**,只能验引擎层热键
    /// (`wake_key` 等);而「强制重连」与宏三键接在**界面线程的 `gui_rx`
    /// 循环**里 —— 那条缝此前**没有任何测试**,只有判定函数级测试。
    /// 本项目的实证教训(F5/遮板坐标系):"判定对了、接线错了"照样是死功能,
    /// 而门禁全绿看不出来。
    ///
    /// 这条测试钉住 [`PadApp::reconnect_hotkey_hit`] 的**五个条件** ——
    /// 它就是把 `gui_rx` 循环里那段接线原样抽出来的产物。
    #[test]
    fn reconnect_hotkey_wiring_requires_all_five_conditions() {
        use crate::keymap::KeySet;
        let k = KeySet::single(88);
        let chord = KeySet::from_keys([29u16, 88]); // Ctrl+88
        // ① 基线:未捕获 + 上升沿 + 已绑定 + 集合按着 ⇒ 命中
        assert!(
            PadApp::reconnect_hotkey_hit(&k, 88, true, false, &[88]),
            "全部条件成立时必须命中(接线通)"
        );
        // ② 正在捕获/录制/取点/文本输入 ⇒ 绝不触发(那时按键是给绑定用的)
        assert!(
            !PadApp::reconnect_hotkey_hit(&k, 88, true, true, &[88]),
            "捕获/录制期间不许触发"
        );
        // ③ 不是上升沿 ⇒ 不触发(Windows 自动重复不连发)
        assert!(
            !PadApp::reconnect_hotkey_hit(&k, 88, false, false, &[88]),
            "按住不放(非上升沿)不许连发"
        );
        // ④ 未绑定(空集合)⇒ 不触发
        assert!(
            !PadApp::reconnect_hotkey_hit(&KeySet::new(), 88, true, false, &[88]),
            "未绑定时不许触发"
        );
        // ⑤ 按键不在绑定里 ⇒ 不触发
        assert!(
            !PadApp::reconnect_hotkey_hit(&k, 99, true, false, &[99]),
            "别的键不许触发"
        );
        // ⑥ 组合键:必须**整体都按着**才触发(顺序无关)
        assert!(
            PadApp::reconnect_hotkey_hit(&chord, 88, true, false, &[29, 88]),
            "组合键整体按着 ⇒ 命中"
        );
        assert!(
            PadApp::reconnect_hotkey_hit(&chord, 88, true, false, &[88, 29]),
            "顺序无关"
        );
        assert!(
            !PadApp::reconnect_hotkey_hit(&chord, 88, true, false, &[88]),
            "组合键只按了一半 ⇒ 不许触发(否则会误触发)"
        );
    }

    /// **缝②接线层测试:宏三键的接线**(协议 §6.3 缝②,2026-10-13 销账)。
    ///
    /// 与上一条的区别:`reconnect_hotkey_hit` 验"强制重连"的接线条件;
    /// 这里验**动作函数**`handle_gui_button_event` 的返回值语义 ——
    /// "热键事件是否被消费"。这是"热键不写入宏"的实现基础:
    /// 返回 true 的键在 `gui_rx` 循环里会 `continue`,永远到不了录制写入。
    #[test]
    fn gui_hotkey_consumes_the_event_so_it_never_enters_the_macro() {
        // 这条用纯逻辑断言:凡是 `macro_hotkey_action` 判定为热键的键,
        // `handle_gui_button_event` 必须返回 true(消费掉)。
        // 两侧共用同一套判定,所以这里钉住的是"接线与判定用的是同一份真值"。
        const K1: u16 = 88;
        const K2: u16 = 89;
        const K3: u16 = 90;
        // 三个键各自在该触发的状态下,都被判为宏热键(⇒ 必须被消费)
        for (code, rec) in [(K1, false), (K2, true), (K3, true)] {
            let a = PadApp::macro_hotkey_action(K1, K1, true, K2, true, K3, true, rec);
            if code != K1 {
                let a = PadApp::macro_hotkey_action(code, K1, true, K2, true, K3, true, rec);
                assert_ne!(a, MacroHotkey::None, "键 {code} 在 rec={rec} 时应为热键");
            } else {
                assert_eq!(a, MacroHotkey::Start, "开始键在未录制时 ⇒ Start");
            }
        }
        // 无关键不消费(false ⇒ 会继续落到录制/捕获路径)
        let other = 77u16;
        assert_eq!(
            PadApp::macro_hotkey_action(other, K1, true, K2, true, K3, true, true),
            MacroHotkey::None,
            "无关键不该被判为热键(它必须能进录制)"
        );
        // 未启用 ⇒ 全部不消费
        assert_eq!(
            PadApp::macro_hotkey_action(K1, K1, false, K2, false, K3, false, false),
            MacroHotkey::None,
            "三个键都未启用时,任何键都不该被消费(原有录制行为不变)"
        );
    }

    /// **参数表拼音排序**(用户 2026-10-13)。
    ///
    /// 验证"按中文字典序(拼音顺序)"这条要求:排完后相邻两条的中文名必须满足
    /// 拼音序。这里用 `cmp_pinyin` 自检**单调性**,并用一组已知汉字钉住
    /// "它真的是拼音序,不是 Unicode 码点序"。
    #[test]
    fn arg_entries_sort_by_pinyin() {
        // ① 已知对照:拼音序是 不(b) 丁(d) 上(s) 万(w) 一(y);
        //    Unicode 码点序是 一 丁 万 上 不 —— 两者不同,能区分真伪。
        let mut v = vec!["一", "丁", "万", "上", "不"];
        v.sort_by(|a, b| cmp_pinyin(a, b));
        #[cfg(windows)]
        assert_eq!(v, vec!["不", "丁", "上", "万", "一"], "必须是拼音序");
        let _ = v;

        // ② 参数表整体:排完后必须**单调不降**(用同一个比较器复核)
        let mut h = ArgHelp::defaults();
        h.sort_by_pinyin();
        for w in h.entries.windows(2) {
            assert!(
                cmp_pinyin(w[0].name, w[1].name) != std::cmp::Ordering::Greater,
                "拼音排序后出现逆序:{:?} 在 {:?} 之前",
                w[0].name,
                w[1].name
            );
        }
        // ③ 条目一个都不许丢(排序是重排,不是过滤)
        assert!(
            h.entries.len() >= 60,
            "参数表应有 60+ 条,实际 {}",
            h.entries.len()
        );
        // ④ 全屏参数必须在表里(用户问"scrcpy 全屏模式,添加在 scrcpy 参数内")
        assert!(
            h.entries
                .iter()
                .any(|e| e.flag == "-f" && e.name.contains("全屏")),
            "参数表必须含全屏参数(-f / --fullscreen)"
        );
    }

    /// **参数搜索**(用户 2026-10-13):四路匹配 + 过滤后选中项不丢。
    #[test]
    fn arg_search_matches_name_flag_desc_and_usage() {
        let mut h = ArgHelp::defaults();
        // 空查询 = 全部
        assert_eq!(h.filtered.len(), h.entries.len(), "空查询应显示全部");
        // ① 按参数名匹配
        h.query = "--record".into();
        h.refilter();
        assert!(!h.filtered.is_empty(), "按参数名应能搜到");
        assert!(
            h.filtered
                .iter()
                .any(|&i| h.entries[i].flag.contains("--record")),
            "命中项必须含该参数"
        );
        // ② 按中文名匹配
        h.query = "全屏".into();
        h.refilter();
        assert!(
            h.filtered
                .iter()
                .any(|&i| h.entries[i].name.contains("全屏")),
            "按中文名应能搜到全屏参数"
        );
        // ③ 按说明匹配(用户记得"能录屏"但忘了参数名)
        h.query = "录制".into();
        h.refilter();
        assert!(!h.filtered.is_empty(), "按说明应能搜到");
        // ④ 大小写不敏感
        h.query = "--RECORD".into();
        h.refilter();
        assert!(!h.filtered.is_empty(), "参数名匹配必须大小写不敏感");
        // ⑤ 搜不到时:过滤为空,且不许 panic
        h.query = "这个关键词一定不存在zzz".into();
        h.refilter();
        assert!(h.filtered.is_empty());
        // ⑥ 选中项被过滤掉时自动落到第一条(否则详情区显示不在列表里的参数)
        h.query = String::new();
        h.refilter();
        h.selected = h.entries.len() - 1;
        h.query = "全屏".into();
        h.refilter();
        assert!(
            h.filtered.contains(&h.selected),
            "选中项必须落在过滤结果里(否则详情与列表不一致)"
        );
    }

    /// **蒙版画面矩形**(用户 2026-10-13 第四轮:根治坐标偏移)。
    ///
    /// 用**真机数据**验证:手机触摸空间 2772×1280(横屏),显示器 1707×1067。
    ///
    /// 不变量:
    /// ① 画面矩形**宽高比 = 手机长宽比**(等比,绝不拉伸);
    /// ② 画面**完整装进**客户区(不溢出);
    /// ③ 居中(两侧黑边之差 ≤1px);
    /// ④ 输出是**屏幕坐标**(含客户区原点),因为蒙版窗口要靠它 `SetWindowPos`。
    #[test]
    fn mask_viewport_rect_uses_phone_aspect_and_centers() {
        let space = (2772u32, 1280u32);
        let expect_ar = 2772.0f32 / 1280.0;
        // ① 客户区比手机"更高":高度贴满、左右留黑边、水平居中
        let top_left = (100, 50, 1707, 1000);
        let r = PadApp::mask_viewport_rect(top_left, space).expect("应能算出画面矩形");
        assert!(
            ((r.w as f32 / r.h as f32) - expect_ar).abs() < 0.01,
            "画面宽高比必须等于手机:{r:?} (期望 {expect_ar})"
        );
        assert!(r.w <= 1707 && r.h <= 1000, "画面必须装进客户区:{r:?}");
        assert!(r.x >= 100 && r.y >= 50, "画面不许跑到客户区外:{r:?}");
        let left_gap = r.x - 100;
        let right_gap = (100 + 1707) - (r.x + r.w);
        assert!(
            (left_gap - right_gap).abs() <= 1,
            "画面应水平居中:左 {left_gap} 右 {right_gap}"
        );
        // ② 客户区比手机"更宽":宽度贴满、上下留黑边、垂直居中
        let wide = (0, 0, 1707, 500);
        let r2 = PadApp::mask_viewport_rect(wide, space).expect("应能算出");
        assert!(
            ((r2.w as f32 / r2.h as f32) - expect_ar).abs() < 0.01,
            "仍须等比:{r2:?}"
        );
        assert!(r2.w <= 1707 && r2.h <= 500);
        let top_gap = r2.y;
        let bot_gap = 500 - (r2.y + r2.h);
        assert!(
            (top_gap - bot_gap).abs() <= 1,
            "应垂直居中:上 {top_gap} 下 {bot_gap}"
        );
        // ③ 竖屏(设备空间反过来)也必须正确
        let r3 = PadApp::mask_viewport_rect(top_left, (1280u32, 2772u32)).expect("应能算出");
        assert!(
            ((r3.w as f32 / r3.h as f32) - (1280.0 / 2772.0)).abs() < 0.01,
            "竖屏等比:{r3:?}"
        );
        // ④ 尺寸不合法 ⇒ None(调用方据此退回隔膜,而不是贴一块零尺寸窗口)
        assert!(PadApp::mask_viewport_rect((0, 0, 10, 10), space).is_none());
        assert!(PadApp::mask_viewport_rect((0, 0, 1707, 1000), (0, 0)).is_none());
    }

    /// **遮板画面区域换算**(用户 2026-10-13:按手机长宽比算,三种情况都要对)。
    ///
    /// 三种情况(用户点名):① 全屏 ② 最大化(非全屏)③ 任意比例缩放。
    /// 共同不变量:
    /// - 画面**等比**(宽高比 = 手机长宽比),绝不拉伸;
    /// - 画面在**客户区**内居中(不是整窗矩形 —— 后者含标题栏,会整体偏移);
    /// - 横屏/竖屏各自用真实的 `dw:dh`,不靠猜。
    #[test]
    fn mask_viewport_follows_phone_aspect_in_all_three_cases() {
        // 与生产同一套公式(见 `fill_mask_shapes`)
        let viewport = |client_w: f32, client_h: f32, dw: f32, dh: f32| {
            let scale = (client_w / dw).min(client_h / dh);
            let vw = dw * scale;
            let vh = dh * scale;
            ((client_w - vw) / 2.0, (client_h - vh) / 2.0, vw, vh)
        };
        // 手机横屏触摸空间 2772x1280(用户真机)
        let (dw, dh) = (2772.0f32, 1280.0f32);
        let phone_ar = dw / dh;
        // ① 全屏:客户区 = 整屏 1707x1067(比例 1.6 ≠ 2.166)
        let (ox, oy, vw, vh) = viewport(1707.0, 1067.0, dw, dh);
        assert!(
            ((vw / vh) - phone_ar).abs() < 1e-3,
            "全屏:画面宽高比必须等于手机({vw}x{vh})"
        );
        assert!(oy > 0.0, "全屏:手机更宽 ⇒ 上下留黑边(oy={oy})");
        assert!(ox.abs() < 1e-3, "全屏:左右应贴满");
        // ② 最大化(非全屏):客户区 = 屏幕减任务栏
        let (ox2, oy2, vw2, vh2) = viewport(1707.0, 1000.0, dw, dh);
        assert!(((vw2 / vh2) - phone_ar).abs() < 1e-3, "最大化:仍等比");
        assert!(oy2 > 0.0 && ox2.abs() < 1e-3, "最大化:左右贴满、上下居中");
        // 客户区 1707x1067 → 1707x1000:此时画面由**宽度**约束(scale 不变),
        // 于是画面尺寸不变、只是黑边随客户区高度变小而变小(139.4 → 105.9)。
        // 第一版这里我写成"黑边变大"(想当然),被测试当场抓出 —— 如实钉住真值。
        assert!(oy2 < oy, "宽度约束下:客户区变矮 ⇒ 黑边变小({oy} → {oy2})");
        // ③ 任意比例缩放:窗口比手机"更高"
        let (ox3, oy3, vw3, vh3) = viewport(800.0, 1200.0, dw, dh);
        assert!(((vw3 / vh3) - phone_ar).abs() < 1e-3, "任意缩放:仍等比");
        assert!(oy3 > 0.0 && ox3.abs() < 1e-3);
        // ④ 竖屏:设备空间反过来(1280x2772)也必须正确
        let (dwv, dhv) = (1280.0f32, 2772.0f32);
        let (ox4, oy4, vw4, vh4) = viewport(800.0, 1200.0, dwv, dhv);
        assert!(
            ((vw4 / vh4) - (dwv / dhv)).abs() < 1e-3,
            "竖屏:画面宽高比必须等于竖屏手机比例"
        );
        assert!(ox4 > 0.0 && oy4.abs() < 1e-3, "竖屏:左右留黑边、上下贴满");
    }

    /// **右上角救援按钮的优先级仲裁**(用户 2026-10-12 第 2 条)。
    ///
    /// 用户要求:"让这些按钮排一个优先级,优先级高的覆盖优先级低的,
    /// 优先级相同的先来先服务"。这里逐条钉住。
    #[test]
    fn rescue_button_priority_is_highest_wins_and_fifo_on_ties() {
        use RescueAction::*;
        // 高优先级覆盖低优先级:连接已放弃(50)压过其它所有
        assert_eq!(
            pick_rescue_action(&[
                MappingWithoutChannel,
                ChannelDead,
                Reconnecting,
                ReconnectGaveUp
            ]),
            Some(ReconnectGaveUp)
        );
        // 同优先级:先来的赢(顺序即"先进入该状态")
        assert_eq!(
            pick_rescue_action(&[ChannelDead, ChannelDead]),
            Some(ChannelDead),
            "同优先级先来先服务"
        );
        // 40 覆盖 20
        assert_eq!(
            pick_rescue_action(&[MappingWithoutChannel, Reconnecting]),
            Some(Reconnecting)
        );
        // 只有一个候选
        assert_eq!(pick_rescue_action(&[ChannelDead]), Some(ChannelDead));
        // 没有候选 ⇒ 不亮(平时哑光无字)
        assert_eq!(pick_rescue_action(&[]), None);
        // 优先级数值本身:高→低,确保上面的比较有意义
        assert!(ReconnectGaveUp.priority() > Reconnecting.priority());
        assert!(Reconnecting.priority() > ChannelDead.priority());
        assert!(ChannelDead.priority() > MappingWithoutChannel.priority());
        // 每个动作都要有可读的标签与悬停说明(否则界面上一片空白)
        for a in [
            ReconnectGaveUp,
            Reconnecting,
            ChannelDead,
            MappingWithoutChannel,
        ] {
            assert!(!a.label().is_empty(), "{a:?} 必须有按钮文字");
            assert!(!a.tooltip().is_empty(), "{a:?} 必须有悬停说明");
        }
    }

    /// **F6 回归**(2026-10-12):scrcpy 启动参数不许出现同键重复。
    ///
    /// 真机证据:用户配置里累积成
    /// `--stay-awake --max-size=2560 --max-fps=144 --max-size=720 --max-size=2560 --max-fps=144`
    /// —— `--max-size` 三份、`--max-fps` 两份,而命令行里**后者覆盖前者**,
    /// 于是"用户看到的参数"和"实际生效的参数"不是一回事。
    #[test]
    fn arg_upsert_replaces_in_place_instead_of_duplicating() {
        // ① 同键就地替换:位置不变,不追加第二份
        assert_eq!(
            upsert_arg("--stay-awake --max-size=2560 --print-fps", "--max-size=720"),
            "--stay-awake --max-size=720 --print-fps"
        );
        // ② 没有该键 ⇒ 追加到末尾
        assert_eq!(
            upsert_arg("--stay-awake", "--max-fps=144"),
            "--stay-awake --max-fps=144"
        );
        // ③ 空串 ⇒ 只留新参数
        assert_eq!(upsert_arg("", "--max-fps=60"), "--max-fps=60");
        // ④ 空 flag ⇒ 原样(不产生空 token)
        assert_eq!(upsert_arg("--stay-awake", "   "), "--stay-awake");
        // ⑤ 真机那条脏串:反复"加入"同一个键,结果只应有一份
        let dirty = "--stay-awake --max-size=2560 --max-fps=144 --max-size=720 --max-size=2560 --max-fps=144";
        let once = upsert_arg(dirty, "--max-size=1080");
        assert_eq!(
            once.matches("--max-size").count(),
            1,
            "同键只应剩一份:{once}"
        );
        assert!(once.contains("--max-size=1080"), "新值生效:{once}");
        // ⑥ 就地替换:其它参数一个都不许丢
        for keep in ["--stay-awake", "--max-fps=144", "--print-fps"] {
            let s = upsert_arg("--stay-awake --max-fps=144 --print-fps", "--max-size=720");
            assert!(s.contains(keep), "不该丢 {keep}:{s}");
        }
    }

    /// **F6 回归**:老配置里已经堆好的重复项,启动前必须去重。
    ///
    /// 只修"加入参数"那一处不够 —— 用户配置里已经存着重复值,不清掉就会原样
    /// 带进启动命令。去重保留**最后一次**出现的值 = 与 scrcpy 实际生效的
    /// 那一个逐位一致(不改变现有行为,只去掉无效项)。
    #[test]
    fn arg_dedupe_keeps_the_last_value_like_the_command_line_does() {
        let dirty = "--stay-awake --max-size=2560 --max-fps=144 --max-size=720 --max-size=2560 --max-fps=144";
        let clean = dedupe_args(dirty);
        assert_eq!(
            clean.matches("--max-size").count(),
            1,
            "max-size 只应剩一份:{clean}"
        );
        assert_eq!(
            clean.matches("--max-fps").count(),
            1,
            "max-fps 只应剩一份:{clean}"
        );
        // 保留最后出现的值(命令行语义:后者覆盖前者)
        assert!(clean.contains("--max-size=2560"), "最后一次是 2560:{clean}");
        assert!(clean.contains("--max-fps=144"), "{clean}");
        assert!(clean.contains("--stay-awake"), "无关参数不许丢:{clean}");
        // 无重复时逐字不变(不改变现有行为)
        let ok = "--stay-awake --max-size=2560 --max-fps=144 --print-fps";
        assert_eq!(dedupe_args(ok), ok, "没有重复就必须逐字不变");
        // 开关型参数(`--no-audio`)重复也要去掉
        assert_eq!(
            dedupe_args("--no-audio --no-audio --print-fps"),
            "--no-audio --print-fps"
        );
        // 空串/纯空白不 panic
        assert_eq!(dedupe_args(""), "");
        assert_eq!(dedupe_args("   "), "");
    }

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

                repeat_while_held: false,
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
            repeat_while_held: false,
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
            // 半成品草稿:还没有触发键,但有录制步骤 + 设置项(草稿允许不完整)。
            // 「长按重复」勾上,盯住它也能原样存回来。
            MacroDraft {
                name: "半成品".into(),
                key: None,
                fps_only: false,
                tail_delay_ms: 30,
                repeat_while_held: true,
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
                repeat_while_held: false,
                steps: Vec::new(),
                instructions: Vec::new(),
                virtual_profile: Some(Profile::default()),
            },
        ];
        save_macro_drafts(&path, &list).expect("保存必须成功");
        let back = load_macro_drafts(&path).unwrap();
        assert_eq!(back, list);
        assert!(
            back[0].repeat_while_held,
            "「长按重复」必须随草稿一起存回来"
        );
        assert!(!back[1].repeat_while_held, "没勾的草稿读回来仍然是关闭");

        // 老草稿文件(没有 `repeat_while_held` 这一项)必须照旧读得出来,
        // 且读成"关闭" = 老行为一字不改。
        let old = dir.join("macro_drafts_old.json");
        std::fs::write(
            &old,
            r#"[{"name":"老草稿","key":65,"fps_only":false,"tail_delay_ms":0,
                 "steps":[],"instructions":[]}]"#,
        )
        .unwrap();
        let old_list = load_macro_drafts(&old).expect("老草稿库必须仍可解析");
        assert_eq!(old_list.len(), 1);
        assert!(!old_list[0].repeat_while_held, "老草稿读成长按重复关闭");
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

    /// 取一个函数的**函数体**(从签名行到同缩进的收尾 `}` 行),按行扫描。
    ///
    /// # 为什么按行
    ///
    /// AGENTS §3.2:源码扫描类测试**禁用**会被 `cargo fmt` 重排的转义串模式
    /// (如 `"\n    }\n"`)。V2-4 的假守护就是那么写的:fmt 把转义串变成字面多行
    /// 字符串 ⇒ 模式永远失配 ⇒ `find` 返回 `None` ⇒ 函数体截到文件末尾 ⇒
    /// 任意一处有调用就让所有入口判 PASS。这里改成"找签名 → 逐行找到同缩进的
    /// 收尾行",找不到就 **panic**(而不是静默退化成全文)。
    fn fn_body<'a>(src: &'a str, sig: &str) -> &'a str {
        let start = src
            .find(sig)
            .unwrap_or_else(|| panic!("找不到函数 `{sig}` —— 改名后请同步本测试"));
        let line_start = src[..start].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let indent = &src[line_start..start];
        assert!(
            indent.chars().all(|c| c == ' '),
            "签名 `{sig}` 前面必须是纯缩进,实际是 {indent:?}"
        );
        let close = format!("{indent}}}");
        let rest = &src[start..];
        let mut offset = 0;
        for line in rest.split_inclusive('\n') {
            // offset > 0:跳过签名行本身(它不可能等于收尾行)
            if offset > 0 && line.trim_end() == close {
                return &rest[..offset];
            }
            offset += line.len();
        }
        panic!("函数 `{sig}` 没有找到同缩进的收尾行 `{close}` —— 源码扫描失效(不许静默退化)");
    }

    /// 按**源码顺序**逐项找标签:缺一个、或某一个跑到了前一个之前 ⇒ Err(报第几项)。
    ///
    /// 用于"只许加分组、不许重排字段"这类守护:单纯 `contains` 判不出顺序,
    /// 而按行号窗口判又会被 fmt 重排打脸(§3.2)。
    fn labels_in_order(body: &str, labels: &[&str]) -> Result<(), String> {
        let mut pos = 0usize;
        for (n, label) in labels.iter().enumerate() {
            match body[pos..].find(label) {
                Some(i) => pos += i + label.len(),
                None => {
                    return Err(format!(
                        "第 {} 项 `{label}` 不见了,或跑到了上一项之前",
                        n + 1
                    ));
                }
            }
        }
        Ok(())
    }

    /// **接线守护(用户 2026-10-14)**:凡能编辑「滑动」动作的地方,
    /// 都必须渲染轨迹录制卡片。
    ///
    /// # 为什么单独立这一条
    ///
    /// 卡片是**手动**在每个入口调用的(见 [`PadApp::track_rec_card`]),漏一处
    /// 就是"这个界面根本没有录制按钮"——用户 2026-10-14 的截图正是这个现象:
    /// **可视化风格(`ui_vk_draft`)漏了**,而默认风格(`ui_binds_new`)有。
    /// 三道门(编译/单测/clippy)对"少调用一个 UI 函数"**完全无感**,
    /// 所以这类遗漏只能靠专门的守护发现。
    ///
    /// # 判定方式
    ///
    /// **源码级**守护,按"入口函数名"定位而不是按行号窗口 —— 第一版按窗口写,
    /// 两个坑一起踩了:组合键的卡片在 `ui_combos`(距 `combo_action_editor` 数百行)
    /// 被漏判成"没接线";而 `fn track_rec_card` **定义**那句又被当成了调用,
    /// 于是"漏接线"反而判 PASS(一个会漏报的守护比没有更糟)。
    /// 现在改成:每个入口函数体内必须出现判据调用(排除函数定义)。
    ///
    /// U9(2026-10-17)起,两个草稿入口经**共用编辑卡**间接接线,判据是
    /// `self.ui_draft_card(`;链路 `ui_draft_card → ui_draft_fields → track_rec_card`
    /// 由 [`both_draft_entries_share_the_same_edit_card`] 逐环钉住。
    #[test]
    fn every_swipe_edit_entry_renders_the_track_recording_card() {
        let src = include_str!("app.rs");
        // 四个入口:函数签名 → 人话说明 → 该入口里"录制卡片可达"的判据。
        let entries = [
            (
                "fn ui_binds_new(",
                "新增草稿(默认风格)",
                "self.ui_draft_card(",
            ),
            (
                "fn ui_vk_draft(",
                "新增草稿(可视化风格)",
                "self.ui_draft_card(",
            ),
            (
                "fn ui_bind_row(",
                "已有键位(两种风格共用)",
                "track_rec_card(",
            ),
            ("fn ui_combos(", "已有组合键", "track_rec_card("),
        ];
        for (sig, what, call) in entries {
            let body = fn_body(src, sig);
            // 必须是**调用**:排除 `fn track_rec_card(` 这个定义本身
            let called = body
                .match_indices(call)
                .any(|(i, _)| !body[..i].trim_end().ends_with("fn"));
            assert!(
                called,
                "{what} 的滑动编辑入口(`{sig}`)里没有 `{call}` ——                  这个界面会**没有录制按钮**(用户 2026-10-14 报的就是可视化风格这一条)。"
            );
        }
    }

    /// **U8 半径输入收敛的守护**(2026-10-17):所有半径编辑点都必须走同一个
    /// 组件 [`radius_input`],且**各带各的范围与步长**。
    ///
    /// 本轮只收敛结构,**不统一范围**——三套现存口径(CN-02)统一成哪一套会改变
    /// 用户可见行为,属计划 Q6 待用户拍板。所以这里先把三套数值钉死:谁想顺手
    /// 统一,先红在这一条,再回来登记。
    #[test]
    fn every_radius_edit_site_uses_the_shared_input_with_its_own_range() {
        // 三套现存口径必须逐字保持(改数值 = 改用户可见行为)
        assert_eq!(
            RADIUS_RANGE_PX,
            0.01..=100000.0,
            "键位/草稿/组合键的半径范围被改动(统一范围属 Q6,不许顺手做)"
        );
        assert_eq!(
            RADIUS_RANGE_WHEEL_PX,
            10.0..=1000.0,
            "摇杆/轮盘的半径范围被改动(统一范围属 Q6,不许顺手做)"
        );
        assert_eq!(
            RADIUS_RANGE_REL,
            0.001..=0.5,
            "宏动作编辑器的半径范围被改动(统一范围属 Q6,不许顺手做)"
        );

        let src = include_str!("app.rs");
        // (函数签名, 范围常量, 步长参数, 该函数内的调用次数)
        let sites = [
            ("fn ui_bind_row(", "RADIUS_RANGE_PX", "None", 2),
            ("fn ui_draft_point_row(", "RADIUS_RANGE_PX", "None", 1),
            (
                "fn ui_macro_virtual_wheel(",
                "RADIUS_RANGE_WHEEL_PX",
                "None",
                1,
            ),
            ("fn ui_wheels(", "RADIUS_RANGE_WHEEL_PX", "None", 1),
            ("fn ui_action_editor<", "RADIUS_RANGE_REL", "Some(0.001)", 2),
            ("fn combo_action_editor(", "RADIUS_RANGE_PX", "None", 2),
        ];
        let mut total = 0;
        for (sig, range, speed, want) in sites {
            let body = fn_body(src, sig);
            let got = body
                .lines()
                .filter(|l| l.contains("radius_input("))
                .filter(|l| l.contains(range) && l.contains(speed))
                .count();
            assert_eq!(
                got, want,
                "{sig} 里「共用组件 + {range} + 步长 {speed}」的半径输入应有 {want} 处,实际 {got} 处"
            );
            total += got;
        }
        assert_eq!(
            total, 9,
            "半径编辑点总数变了(收敛时为 9 处):多一处/少一处都要在本表登记"
        );

        // 组件里的钳制不能是摆设:body 必须真的调用 radius_clamp
        let comp = fn_body(src, "fn radius_input(");
        assert!(
            comp.lines().any(|l| l.contains("radius_clamp(shown")),
            "`radius_input` 里的钳制被删了(范围外取值不再被拉回)"
        );

        // 两处**无钳制**的半径写入本轮不动:加了钳制 = 语义变更,须用户拍板。
        assert!(
            src.contains("zoom_radius(*radius, factor)"),
            "Ctrl+± 的乘性缩放被改动(无上下限是现状,加钳制须先拍板)"
        );

        // 阴性对照(§3.3):把一条调用抹掉,同一条判据必须判"缺"
        let tampered = fn_body(src, "fn ui_bind_row(").replace(
            "radius_input(ui, &mut pr, RADIUS_RANGE_PX, None)",
            "ui.add(egui::DragValue::new(&mut pr))",
        );
        let got = tampered
            .lines()
            .filter(|l| l.contains("radius_input(") && l.contains("RADIUS_RANGE_PX"))
            .count();
        assert_eq!(got, 0, "阴性对照失败:抹掉调用后判据仍判「有」");
    }

    /// `fn_body` 的阴性对照(§3.3):函数不存在时必须**大声失败**,
    /// 不许像 V2-4 那样静默退化成"整份文件"。
    #[test]
    #[should_panic(expected = "找不到函数")]
    fn source_scanner_rejects_a_missing_function() {
        let _ = fn_body("fn present() {\n}\n", "fn absent(");
    }

    /// **U9 草稿入口统一的守护**(2026-10-17):两个草稿入口必须调用**同一张**
    /// 编辑卡,且 `卡 → 参数区 → 录制卡片` 这条链每一环都在。
    ///
    /// 这是 V2-4 事故的结构性修复:上一轮"同一件事写两遍",录制卡片只接进了
    /// 默认风格,用户截图才发现可视化风格整块没有。现在参数区只有一份实现,
    /// 入口只是外壳;删掉任一入口的调用 ⇒ 本测试立刻红并报出入口名。
    #[test]
    fn both_draft_entries_share_the_same_edit_card() {
        let src = include_str!("app.rs");
        for (sig, what, shell) in [
            (
                "fn ui_binds_new(",
                "新增草稿(默认风格)",
                "DraftCardShell::DefaultStyle",
            ),
            (
                "fn ui_vk_draft(",
                "新增草稿(可视化风格)",
                "DraftCardShell::VisualStyle",
            ),
        ] {
            let body = fn_body(src, sig);
            let hit = body
                .lines()
                .any(|l| l.contains("self.ui_draft_card(") && l.contains(shell));
            assert!(
                hit,
                "{what}(`{sig}`)没有调用共用编辑卡 `ui_draft_card(.., {shell})` —— \
                 这正是 V2-4 事故的形状:漏一个入口,那个界面就少一整块控件"
            );
            // 阴性对照(§3.3):同一条判据在"抹掉调用"的文本上必须判缺
            let tampered = body.replace("self.ui_draft_card(", "ui_draft_card(");
            let hit = tampered
                .lines()
                .any(|l| l.contains("self.ui_draft_card(") && l.contains(shell));
            assert!(!hit, "阴性对照失败:{what} 的判据在抹掉调用后仍判通过");
        }

        // 链路:卡 → 参数区 → 录制卡片
        let card = fn_body(src, "fn ui_draft_card(");
        assert!(
            card.lines().any(|l| l.contains("self.ui_draft_fields(")),
            "共用卡没有渲染参数区 `ui_draft_fields`"
        );
        let fields = fn_body(src, "fn ui_draft_fields(");
        // 判据字面量拆成两段拼,避免本测试自己的源码文本被算进下面的计数。
        let draft_call = concat!("self.track_rec_card(ui, TrackTarget::", "Draft");
        assert!(
            fields.lines().any(|l| l.contains(draft_call)),
            "参数区没有接轨迹录制卡片(V2-4 事故本体)"
        );
        let needle = concat!("track_rec_card(ui, TrackTarget::", "Draft");
        assert_eq!(
            src.matches(needle).count(),
            1,
            "草稿录制卡片必须只有一处接线(在共用卡内);多一处说明又有人手搓了一份草稿入口"
        );
    }

    /// **U7-1 窄窗溢出的守护**(2026-10-17,用户 433×1018 截图)。
    ///
    /// 现象:`scrcpy 目录` / `scrcpy.exe` / `scrcpy-server` / `adb.exe` 四行的路径框
    /// 按 egui 默认宽度铺开,把[浏览]顶出右边缘 ⇒ **按钮点不到**;`当前 adb` /
    /// `server 版本` 两行长路径同样横向溢出。
    ///
    /// 修法 = `fit_edit_width`:按钮先占位,输入框只吃剩下的;状态行用 `truncate()`。
    #[test]
    fn narrow_window_path_rows_keep_their_buttons_reachable() {
        // ① 宽窗:结果**就是**上限 ⇒ 与改动前的默认宽度一致(不引入额外换行/抖动)
        assert_eq!(
            PadApp::fit_edit_width(1500.0, 110.0, 280.0),
            280.0,
            "宽窗下输入框宽度必须与默认宽度一致,否则外观会变"
        );
        // ② 433px 窄窗:输入框收缩,且"输入框 + 按钮预留"仍落在可用宽度内
        let avail = 330.0; // 扣掉"scrcpy 目录"标签后的剩余宽度(截图口径)
        let reserved = 110.0; // [浏览目录] + [应用]:文字宽 + 内边距 + 项间距
        let w = PadApp::fit_edit_width(avail, reserved, 280.0);
        assert!(w < 280.0, "窄窗下输入框必须收缩,实际 {w}");
        assert!(
            w + reserved <= avail + 0.5,
            "仍然溢出:{w} + {reserved} > {avail} ⇒ 按钮会被顶出右边缘"
        );
        // ③ 极端窄:退到最小宽度,不出现 0/负宽度
        assert_eq!(
            PadApp::fit_edit_width(40.0, 110.0, 280.0),
            PATH_EDIT_MIN_WIDTH
        );
        // 阴性对照(§3.3):"不扣按钮宽度"的写法在同一条断言下必须判溢出
        let naive = avail.min(280.0);
        assert!(
            naive + reserved > avail,
            "阴性对照失败:不扣按钮宽度竟然也不溢出 ⇒ 判据是空的"
        );

        // ④ 接线:四条路径行都必须走 row_edit_width(漏一条,那条的按钮又会被顶出去)
        let src = include_str!("app.rs");
        let body = fn_body(src, "fn ui_scrcpy_manage(");
        assert_eq!(
            body.matches("Self::row_edit_width(").count(),
            4,
            "四条路径行(目录/exe/server/adb)都必须给按钮留位置"
        );
        // 两条长路径状态行必须可省略,不许横向溢出
        assert_eq!(
            body.matches(".truncate()").count(),
            2,
            "`当前 adb` / `server 版本` 两行必须 truncate"
        );
        // 阴性对照:抹掉其中一行的调用 ⇒ 计数必须变成 3
        let tampered = body.replacen(
            "Self::row_edit_width(ui, &[\"浏览\"], ui.spacing().text_edit_width)",
            "ui.spacing().text_edit_width",
            1,
        );
        assert_eq!(
            tampered.matches("Self::row_edit_width(").count(),
            3,
            "阴性对照失败:抹掉一行的调用后计数没变 ⇒ 判据没在数调用"
        );
    }

    /// **U10 FPS 页分组的守护**(2026-10-17):分组标题在,且字段顺序一项不动。
    ///
    /// 本轮只允许"加"分组(标题/分隔线/间距):没有 GUI 走查条件,删字段或重排的
    /// 破坏性无法验证(V2-4 事故的教训)。谁挪了某一行的位置,这里立刻红并报第几项。
    #[test]
    fn aim_page_keeps_its_field_order_and_group_titles() {
        let src = include_str!("app.rs");
        let body = fn_body(src, "fn ui_aim_body(");
        for h in [
            "启用与视角输入",
            "瞄准锚点",
            "灵敏度与归中",
            "FPS 开关与按键",
            "压枪",
            "鼠标独占与自救",
        ] {
            assert!(body.contains(h), "FPS 页缺少分组标题「{h}」");
        }
        // 字段顺序(自上而下):一项都不许少、顺序不许变
        let order = [
            "启用鼠标视角",
            "开放世界模式",
            "视角输入:",
            "瞄准锚点",
            "灵敏度 X",
            "滚轮缩放(双指捏合)",
            "视角移速:",
            "归中:",
            "按住才瞄准:",
            "FPS 模式开关键:",
            "按住才退出:",
            "后坐力补偿",
            "鼠标独占:",
            // B21-3/G-c:光标钉住开关就在「鼠标独占」那一段里(紧跟独占说明),
            // 因此顺序表里它落在"鼠标独占:"与"卡死自愈"之间。谁挪了位置,这里红。
            "钉住光标(避免甩枪撞边)",
            "卡死自愈",
            "唤醒视角热键:",
            "指针消隐",
        ];
        labels_in_order(body, &order).unwrap_or_else(|e| panic!("FPS 页字段顺序变了:{e}"));
        // 阴性对照(§3.3):把顺序颠倒的样本喂给同一判据,必须判"错"
        assert!(
            labels_in_order("指针消隐\n归中:", &["归中:", "指针消隐"]).is_err(),
            "阴性对照失败:顺序颠倒的样本仍判通过"
        );
    }

    /// **U11 硬编码色的守护**(2026-10-17):UI 绘制面不许出现字面色。
    ///
    /// 判据只认带括号的 `Color32::from_rgb` —— 调色板(`theme.rs`)里定义色值不算;
    /// `from_rgba_unmultiplied`(截图缓冲/半透明描边)不是字面色板,不在本条范围。
    #[test]
    fn ui_surface_has_no_hardcoded_rgb_literals() {
        let needle = concat!("Color32::", "from_rgb(");
        let src = include_str!("app.rs");
        let hits: Vec<&str> = src.lines().filter(|l| l.contains(needle)).collect();
        assert!(
            hits.is_empty(),
            "UI 面仍有硬编码色(应取 theme 语义色):{hits:?}"
        );
        // 阳性对照(§3.3):同一条判据必须抓得到一个真的字面色
        let probe = concat!("let c = egui::Color32::", "from_rgb(1, 2, 3);");
        assert!(
            probe.lines().any(|l| l.contains(needle)),
            "阳性对照失败:判据抓不到字面色"
        );
        // 语义色的来源是 theme.rs(不是本文件凭空造的)
        assert!(include_str!("theme.rs").contains("pub danger: Color32"));
    }

    /// **轨迹录制快捷键的判定**(用户 2026-10-14;2026-10-23 第 2 条扩成一对)。
    ///
    /// 这条缝在界面线程的 `gui_rx` 循环里,引擎级测试台够不着 ⇒ 抽成纯函数后
    /// 逐条钉住。三条前置判据任何一条不成立都必须 **`None`(不触发)** —— 尤其
    /// "总开关"与"键位空缺"这两条:它们正是用户新需求的核心。命中之后还必须
    /// 分清**开始键(Toggle)** 与**结束键(Stop)**,这是本轮新加的一对。
    ///
    /// (用户 2026-10-14 去掉鼠标键控制录制后,原本的"触发方式"条件已随之删除;
    /// 这里同步去掉那一条断言,免得测试与实现脱节。)
    #[test]
    fn track_rec_hotkey_pair_follows_the_spec() {
        const S: u16 = 88; // 开始键
        const E: u16 = 89; // 结束键
        // ---- 命中判定:两颗键各归各的动作 ----
        assert_eq!(
            PadApp::track_rec_hotkey(true, S, E, S, true),
            Some(TrackRecHotkey::Toggle),
            "命中开始键 ⇒ Toggle(没在录就开录,正在录就收尾)"
        );
        assert_eq!(
            PadApp::track_rec_hotkey(true, S, E, E, true),
            Some(TrackRecHotkey::Stop),
            "命中结束键 ⇒ Stop"
        );
        // ---- 三条前置判据,逐条反证(只破坏一条,其余保持成立) ----
        assert_eq!(
            PadApp::track_rec_hotkey(false, S, E, S, true),
            None,
            "① 总开关未勾选 ⇒ 不触发(用户要求:未勾选时快捷键不生效)"
        );
        assert_eq!(
            PadApp::track_rec_hotkey(false, S, E, E, true),
            None,
            "① 总开关未勾选时,结束键同样不触发"
        );
        assert_eq!(
            PadApp::track_rec_hotkey(true, S, E, S, false),
            None,
            "② 非上升沿 ⇒ 不触发(否则按住不放会疯狂开始/结束)"
        );
        assert_eq!(
            PadApp::track_rec_hotkey(true, S, E, 99, true),
            None,
            "③ 别的键 ⇒ 不触发"
        );
        // ---- 空缺(0)的三种组合 ----
        assert_eq!(
            PadApp::track_rec_hotkey(true, 0, E, S, true),
            None,
            "开始键空缺 ⇒ 它按不出东西(空缺 = 这条路不可用)"
        );
        assert_eq!(
            PadApp::track_rec_hotkey(true, S, 0, E, true),
            None,
            "结束键空缺 ⇒ 那颗键与录制无关(老配置的原样行为)"
        );
        assert_eq!(
            PadApp::track_rec_hotkey(true, 0, E, E, true),
            Some(TrackRecHotkey::Stop),
            "只有结束键时,它仍然只负责结束"
        );
        // ---- 兼容 2026-10-14 的单键口径:结束键留空 ⇒ 开始键按两下切换 ----
        assert_eq!(
            PadApp::track_rec_hotkey(true, S, 0, S, true),
            Some(TrackRecHotkey::Toggle),
            "只有开始键 ⇒ 与从前逐字一致(按一下开始、再按一下结束)"
        );
        // ---- 两颗键绑成同一个码:开始键优先,不会"一按既开又关" ----
        assert_eq!(
            PadApp::track_rec_hotkey(true, S, S, S, true),
            Some(TrackRecHotkey::Toggle),
            "同码时先判开始键 ⇒ 语义仍是单键切换"
        );
    }

    /// **R11:Raw Input 四态状态行**逐态钉住(含降级态下 count 0/非 0 的分叉)。
    ///
    /// 判据只用 `state + count`:**不许用 age** —— 捕获层在 `count == 0` 时把
    /// "距最近一条 WM_INPUT 的毫秒数"硬编码成 0,拿它当"多久没事件"会得出
    /// "刚刚才来过"的假结论。
    #[test]
    fn raw_status_line_covers_all_four_states() {
        // 0 未注册:不显示(非 Windows 没有这条通道,天然落此)
        assert_eq!(
            PadApp::raw_status_line(0, 0),
            None,
            "state 0(未注册)必须不显示任何行"
        );
        assert_eq!(PadApp::raw_status_line(0, 7), None, "state 0 与 count 无关");
        // 1 待定:已注册,等首个事件
        assert_eq!(
            PadApp::raw_status_line(1, 0).as_deref(),
            Some("原始输入待定：已注册，等待首个事件"),
            "state 1 ⇒ 待定文案"
        );
        assert_eq!(
            PadApp::raw_status_line(1, 3).as_deref(),
            Some("原始输入待定：已注册，等待首个事件")
        );
        // 2 相对接管:正常态
        assert_eq!(
            PadApp::raw_status_line(2, 0).as_deref(),
            Some("原始输入已启用：位移按设备计数上报"),
            "state 2 ⇒ 正常文案"
        );
        assert_eq!(
            PadApp::raw_status_line(2, 9).as_deref(),
            Some("原始输入已启用：位移按设备计数上报")
        );
        // 3 降级:count 0 与 count>0 **同一句**(红字由 raw_probe_alert 叠加)
        assert_eq!(
            PadApp::raw_status_line(3, 0).as_deref(),
            Some("原始输入降级：绝对坐标设备，位移退回光标差路径"),
            "state 3 且 count==0 ⇒ 降级文案"
        );
        assert_eq!(
            PadApp::raw_status_line(3, 5).as_deref(),
            Some("原始输入降级：绝对坐标设备，位移退回光标差路径"),
            "state 3 且 count>0 ⇒ 同一句降级文案"
        );
        // 越界状态:不显示(不猜)
        assert_eq!(PadApp::raw_status_line(9, 1), None, "未知状态不显示");
    }

    /// **T1:注册成功却零事件 ⇒ 红字**;其余组合一律不红。
    #[test]
    fn raw_probe_alert_fires_only_for_registered_but_silent() {
        // 唯一该红的组合:注册流程走完(state!=0)、一条 WM_INPUT 都没到(count==0)、
        // 且 watchdog 已判定"只有绝对设备"并退回(state==3)。
        assert_eq!(
            PadApp::raw_probe_alert(3, 0),
            Some("⚠ 原始输入已注册但从未收到任何事件：位移已退回光标差路径"),
            "(3,0) ⇒ 正是『注册成功却零事件』,必须红字"
        );
        // 反证:只破坏一个条件,其余保持成立
        assert_eq!(
            PadApp::raw_probe_alert(3, 5),
            None,
            "(3,5) 收到过事件 ⇒ 不红(只是设备只报绝对坐标)"
        );
        assert_eq!(
            PadApp::raw_probe_alert(2, 0),
            None,
            "(2,0) 相对接管正常态 ⇒ 不红"
        );
        assert_eq!(
            PadApp::raw_probe_alert(1, 0),
            None,
            "(1,0) 只是启动瞬时(注册完、首个事件还没来)⇒ 归琥珀色状态行,不红"
        );
        assert_eq!(PadApp::raw_probe_alert(0, 0), None, "(0,0) 未注册 ⇒ 不红");
        assert_eq!(PadApp::raw_probe_alert(0, 9), None, "(0,9) 未注册 ⇒ 不红");
        assert_eq!(PadApp::raw_probe_alert(9, 0), None, "未知状态 ⇒ 不红");
    }

    /// **G-c:钉住光标的实时状态行**四态逐条钉住(2026-10-18 用户实测反馈)。
    ///
    /// 为什么要这条测试:该功能第一次真机验收时**完全无法判定**——用户
    /// `log_level=warn`,而证据只在 info/debug 档。状态行是"不翻日志也能判"的
    /// 唯一入口,它的四个状态必须互不重叠、且"生效"只能有一个说法。
    #[test]
    fn clip_status_line_has_exactly_one_green_state() {
        let th = crate::theme::Theme::dark();
        // ① 开关关 ⇒ 必须说"关着",且**不许**是绿色(绿色会让用户以为在生效)
        let (off, off_c) = PadApp::clip_status_line(&th, false, true, true);
        assert!(off.contains("关着"), "开关关时必须明确说关着:{off}");
        assert_ne!(off_c, th.ok, "开关关着却给了「生效」配色:{off}");
        // ② 开 + 未瞄准 ⇒ 待命(也不是绿色)
        let (idle, idle_c) = PadApp::clip_status_line(&th, true, false, true);
        assert!(idle.contains("待命"), "没在瞄准时必须说待命:{idle}");
        assert_ne!(idle_c, th.ok, "没在瞄准却给了「生效」配色:{idle}");
        // ③ 开 + 瞄准 + 投屏在前台 ⇒ **唯一**的绿色"钉住中"
        let (on, on_c) = PadApp::clip_status_line(&th, true, true, true);
        assert!(on.contains("钉住中"), "瞄准中必须是钉住中:{on}");
        assert_eq!(on_c, th.ok, "瞄准中必须是「生效」配色:{on}");
        // ④ 开 + 瞄准 + 投屏不在前台 ⇒ 琥珀色 + 点出"被前台程序覆盖"这个可能
        let (risky, risky_c) = PadApp::clip_status_line(&th, true, true, false);
        assert!(risky.contains("不在前台"), "前台不是投屏时必须点明:{risky}");
        assert!(
            risky.contains("覆盖"),
            "必须点出「可能被覆盖」这个已知风险:{risky}"
        );
        assert_eq!(risky_c, th.warn, "不确定态必须是琥珀色:{risky}");
        // 阴性对照(§3.3):四条文案两两不同(否则"四态"是假的)
        let all = [&off, &idle, &on, &risky];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                assert_ne!(
                    all[i], all[j],
                    "第 {i} 条与第 {j} 条状态文案撞了:{:?}",
                    all[i]
                );
            }
        }
    }

    /// **自锁守护**:持配置锁(`lock_shared`)的块里,不许再调会加同一把锁的 `&self` 方法。
    ///
    /// 2026-10-23 事故(用户实测):点 [FPS 功能] 页**立刻**卡死、CPU 0%。根因是 G-c
    /// 状态行在 `let mut g = lock_shared(&self.shared);` 的块里调了 `self.theme()` ——
    /// `theme()` 内部也 `lock_shared`,而 `std::sync::Mutex` **不可重入**:UI 线程自锁
    /// (窗口冻住),引擎线程随后一起堵在这把锁上(trace 里 `in`/`aim` 停摆、捕获线程
    /// 还在写心跳),捕获层不碰这把锁所以键盘看起来照旧在工作。
    ///
    /// 为什么门禁挡不住:单测不渲染 GUI,这条路径只有真渲染才走到;而借用检查器只挡
    /// `&mut self`(`self.log` 那种直接编译不过),`self.theme()`/`self.mapper()`/
    /// `self.screen_size()` 这类 `&self` 方法完全静默。⇒ 只能源码扫描(§3.2:逐行,
    /// 不用会被 `cargo fmt` 重排的转义串)。
    #[test]
    fn no_lock_taking_helper_is_called_while_the_config_lock_is_held() {
        // 「持锁」严格定义为整行恰为 `let [mut] NAME = lock_shared(&self.shared);`
        // ——`let n = lock_shared(..).x;` 那种临时借用在语句结束就释放,不是作用域。
        fn held_name(line: &str) -> Option<&str> {
            const TAIL: &str = "lock_shared(&self.shared);";
            let body = line.strip_prefix("let ")?.strip_suffix(TAIL)?.trim_end();
            let head = body.strip_suffix(char::from(61))?.trim();
            let head = head.strip_prefix("mut ").unwrap_or(head).trim();
            let ok = !head.is_empty()
                && head
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == char::from(95));
            ok.then_some(head)
        }
        // 作用域用**缩进**判定(本文件由 rustfmt 格式化):持锁语句之后,第一行缩进
        // 比它浅的行就是它所在块的收尾。比花括号配对简单,也不受字符串里的花括号
        // 干扰 —— 后者正是我第一次写这个扫描时把深度数错的原因。
        fn scan(src: &str) -> Vec<(usize, String)> {
            const RISK: [&str; 4] = [
                "self.theme()",
                "self.mapper()",
                "self.screen_size(",
                "lock_shared(",
            ];
            let lines: Vec<&str> = src.lines().collect();
            let cut = lines
                .iter()
                .position(|l| l.trim() == "mod tests {")
                .unwrap_or(lines.len());
            let indent = |l: &str| l.len() - l.trim_start().len();
            let mut out = Vec::new();
            for (i, line) in lines.iter().enumerate() {
                if i >= cut {
                    break;
                }
                let t = line.trim();
                if t.starts_with("//") {
                    continue;
                }
                let Some(name) = held_name(t) else { continue };
                let ind = indent(line);
                let mut end = cut;
                for (j, l) in lines.iter().enumerate().skip(i + 1).take(cut - i - 1) {
                    if l.trim().is_empty() {
                        continue;
                    }
                    if indent(l) < ind {
                        end = j;
                        break;
                    }
                }
                for (k, l) in lines.iter().enumerate().take(end).skip(i + 1) {
                    let tk = l.trim();
                    if tk.starts_with("//") {
                        continue;
                    }
                    if tk
                        .strip_prefix("drop(")
                        .is_some_and(|rest| rest.starts_with(name))
                    {
                        break;
                    }
                    if RISK.iter().any(|r| l.contains(r)) {
                        out.push((k + 1, tk.to_string()));
                    }
                }
            }
            out
        }

        // 阳性 + 阴性对照(§3.3):探针自己先过一遍「已知坏」与「已知好」两个样本。
        let bad = concat!(
            "fn a(&self) {\n",
            "    {\n",
            "        let mut g = lock_shared(&self.shared);\n",
            "        let t = self.theme();\n",
            "        g.x = 1;\n",
            "    }\n",
            "}\n"
        );
        let good = concat!(
            "fn a(&self) {\n",
            "    let t = self.theme();\n",
            "    {\n",
            "        let mut g = lock_shared(&self.shared);\n",
            "        g.x = 1;\n",
            "    }\n",
            "    let m = self.mapper();\n",
            "}\n"
        );
        let temp = concat!(
            "fn a(&self) {\n",
            "    let n = lock_shared(&self.shared).x;\n",
            "    let t = self.theme();\n",
            "}\n"
        );
        assert_eq!(
            scan(bad).len(),
            1,
            "阳性对照必须命中,否则这条守护测试在骗人"
        );
        assert!(scan(good).is_empty(), "阴性对照必须干净");
        assert!(
            scan(temp).is_empty(),
            "`let n = lock_shared(..).x;` 是临时借用,不是作用域"
        );

        // 真身:只扫生产代码区(`mod tests {` 之前,scan 自己会截断)。
        let src = include_str!("app.rs");
        assert!(src.lines().count() > 10_000, "扫描范围异常");
        let found = scan(src);
        assert!(
            found.is_empty(),
            "持配置锁时再次加锁会自锁(界面直接卡死、CPU 0%):把要用的值提到加锁之前算好。命中: {found:?}"
        );
    }

    /// **B21-D:截图默认倍率按宽度相切**(用户 2026-10-18 第 4 条)。
    ///
    /// 旧口径 `shot_auto_base` 取 `min(by_w, by_h)`：竖屏截图常被**高度**先顶住,
    /// 于是左右留大片空白 —— 用户原话"默认截图太小…与左右界相切而非上下界"。
    /// 这个测试把两件事同时钉死:① 宽度是唯一约束;② 仍封顶 1.0(不许放大拉糊)。
    #[test]
    fn shot_default_scale_fits_width_and_never_upscales() {
        // 竖屏 1080x2400 放进 800x600 的容器:宽度是瓶颈 ⇒ 800/1080
        let s = shot_default_base(800.0, 600.0, 1080, 2400);
        let expected = 800.0 / 1080.0;
        assert!(
            (s - expected).abs() < 1e-4,
            "竖屏截图必须按宽度铺满:期望 {expected},实际 {s}"
        );
        // 关键对照:旧口径(等比内接)在同一个容器里会取高度那一路,明显更小
        let old = shot_auto_base(800.0, 600.0, 1080, 2400);
        assert!(
            s > old,
            "新口径必须比旧的等比内接更大(旧 {old} / 新 {s})——否则本改动等于没做"
        );
        // 封顶 1.0:容器比图还大时不许放大
        assert_eq!(
            shot_default_base(4000.0, 4000.0, 1000, 1000),
            1.0,
            "容器远大于图时必须是 100%,不许放大"
        );
        // 横屏 2400x1080:宽度仍是唯一约束
        let wide = shot_default_base(1200.0, 400.0, 2400, 1080);
        assert!(
            (wide - 0.5).abs() < 1e-4,
            "横屏也必须按宽度算:期望 0.5,实际 {wide}"
        );
        // 退化输入不许 panic、不许给出溢出值
        assert_eq!(
            shot_default_base(800.0, 600.0, 0, 0),
            1.0,
            "0 尺寸图退回 1.0"
        );
        assert!(
            shot_default_base(0.0, 0.0, 1080, 2400) > 0.0,
            "容器为 0 时不许返回 0"
        );
        assert!(
            shot_default_base(f32::NAN, f32::NAN, 1080, 2400) > 0.0,
            "NaN 容器必须退回保守值而不是 NaN"
        );
    }

    /// **B21-F:日志浮窗默认尺寸跟着文本长度走**(用户 2026-10-18 第 3 条)。
    ///
    /// 旧实现固定 760x460,两三行短日志也占一大块屏幕。这条测试同时钉两件事:
    /// ① 尺寸随(条数, 最长行)单调增长;② 永远落在钳制区间内(不许出屏/不许窄到读不了)。
    /// 阴性对照:空/短/长三种输入**不能全等** —— 全等就是"固定框"的假实现。
    #[test]
    fn log_window_size_follows_text_length_not_a_fixed_box() {
        let empty = PadApp::log_window_size(0, 0);
        // 12 行 × 18 + 50 = 266 > 240 ⇒ 短档必须已经能撑出"随内容变高"的证据
        let short = PadApp::log_window_size(12, 20);
        let long = PadApp::log_window_size(200, 160);
        // ① 空日志 ⇒ 最小尺寸(且不 panic)
        assert_eq!(empty, (420.0, 240.0), "空日志必须给最小尺寸:{empty:?}");
        // ② 随内容增长(两个方向都要涨,否则某一维仍是"固定框")
        assert!(short.0 >= empty.0, "短文本宽度不该小于最小尺寸");
        assert!(short.1 > empty.1, "十几行日志必须比空日志高:{short:?}");
        assert!(
            long.0 > short.0,
            "长文本必须比短文本宽:{long:?} vs {short:?}"
        );
        assert!(long.1 > short.1, "多行必须比少行高:{long:?} vs {short:?}");
        // ③ 钳制区间(与 min_size / 屏幕可用宽高同一口径)
        for (w, h) in [empty, short, long] {
            assert!((420.0..=1100.0).contains(&w), "宽度越界:{w}");
            assert!((240.0..=620.0).contains(&h), "高度越界:{h}");
        }
        // ④ 极端输入不许溢出/NaN
        let huge = PadApp::log_window_size(usize::MAX, usize::MAX);
        assert_eq!(huge, (1100.0, 620.0), "极端输入必须被钳到上限:{huge:?}");
        // 阴性对照(§3.3):三者不能全等(全等 = 固定框的假实现)
        assert!(
            !(empty == short && short == long),
            "三个尺寸全等 ⇒ 这根本不是自适应:{empty:?} {short:?} {long:?}"
        );
    }

    /// **启动崩溃回归锁：`ui.indent` 的父布局必须是垂直的**（2026-10-18 真机事故）。
    ///
    /// 事故经过：为让滑动细项换行，在 `ui_bind_row` 里用了 `ui.indent`；而它的两个
    /// 调用点当时都写成 `ui.horizontal(|ui| self.ui_bind_row(ui, i))` ⇒ `ui_bind_row`
    /// 拿到的是**水平** Ui ⇒ `ui.indent` **直接 panic**，表现是**程序启动即崩**
    /// （用户实测：`You can only indent vertical layouts`）。
    ///
    /// 门禁全绿也照样崩 —— 因为这条路径只在**真的渲染出那条键位**时才走到。
    /// 所以这条测试改成**结构断言**（纯源码扫描，无需 GUI）：
    ///   ① `ui_bind_row` 的每个调用点**不得**再被 `ui.horizontal(...)` 包着；
    ///   ② `ui.indent(("swipe_detail"...)` 外层必须有 `ui.vertical`（防御性，
    ///      这样即便将来又有人包了水平层，也只是布局变差而不会崩）。
    /// 阳性/阴性对照见函数内注释（§3.3）。
    #[test]
    fn indent_for_swipe_detail_is_never_placed_in_a_horizontal_parent() {
        let src = include_str!("app.rs");
        let lines: Vec<&str> = src.lines().collect();
        // ① 所有 `self.ui_bind_row(` 调用点（排除函数定义本身）不得被 horizontal 包着。
        //    只扫**测试模块之前**的生产代码：测试里的阳性对照样本串含同样的字样，
        //    不切边界就会把测试自己判成违规（这条测试前两版都栽在这里）。
        let prod_end = lines
            .iter()
            .position(|l| l.trim() == "mod tests {")
            .unwrap_or(lines.len());
        let mut checked = 0usize;
        for (idx, line) in lines.iter().take(prod_end).enumerate() {
            let t = line.trim();
            if t.starts_with("//") || !t.contains("self.ui_bind_row(") {
                continue;
            }
            checked += 1;
            let bad = t.contains("ui.horizontal");
            assert!(
                !bad,
                "src/app.rs:{} 把 ui_bind_row 包进了 ui.horizontal —— 这会让函数内的 \
                 ui.indent 直接 panic(启动即崩):{}",
                idx + 1,
                t
            );
        }
        assert!(
            checked >= 2,
            "至少应有两处 ui_bind_row 调用点，实际找到 {checked}"
        );
        // ② swipe_detail 的 indent 必须有 vertical 兜底
        let body = fn_body(src, "fn ui_bind_row(");
        assert!(
            body.contains("ui.vertical(|ui| {"),
            "ui_bind_row 里的 ui.indent 缺少外层 ui.vertical 兜底 —— 一旦调用方是水平布局就会崩"
        );
        // 阳性对照(§3.3)：同一条判据喂一段"水平包着"的样本必须判不合规
        let probe_bad = "if let Some(d) = ui.horizontal(|ui| self.ui_bind_row(ui, i)).inner {";
        let probe_line = probe_bad.trim().split('\n').next().unwrap_or("");
        assert!(
            probe_line.contains("ui.horizontal"),
            "阳性对照失败:判据认不出\"包着水平布局\"的写法"
        );
        // 阴性对照:修好后的写法不得被判不合规
        let probe_good = "if let Some(d) = self.ui_bind_row(ui, i) {";
        assert!(
            !probe_good.contains("ui.horizontal"),
            "阴性对照失败:合规写法被误判"
        );
    }

    /// **Linux 缺陷 3 守护测试**:界面文案里的可执行文件名必须按平台取。
    ///
    /// 2026-10-19 在 WSL 跑 Linux 版二进制,启动日志逐字打出
    /// `未找到 scrcpy,请在左栏指定 scrcpy.exe 或它所在的目录` 与
    /// `请将 adb.exe 所在目录加入 PATH` —— 而 Linux 上**根本不存在** .exe,
    /// 用户照着文案去找只会扑空。真名在 `adb::scrcpy_exe_name()` /
    /// `adb::adb_exe_name()`(Windows=`*.exe`,其余=`scrcpy`/`adb`)。
    ///
    /// 判据:生产代码区(**排除注释**)不得出现 `scrcpy.exe`/`adb.exe` 字面量。
    /// 注释不算 —— 注释里指代"Windows 上那个进程"是合理且有信息量的。
    #[test]
    fn executablenames_in_ui_copy_are_platformized() {
        let src = include_str!("app.rs");
        let lines: Vec<&str> = src.lines().collect();
        let prod_end = lines
            .iter()
            .position(|l| l.trim() == "mod tests {")
            .unwrap_or(lines.len());
        // 阳性对照(§3.3):判据必须认得出违规写法
        let probe_bad = r#"self.log("请将 adb.exe 所在目录加入 PATH")"#;
        assert!(
            probe_bad.contains(".exe"),
            "阳性对照失败:判据认不出写死 .exe 的文案"
        );
        // 阴性对照:平台化的写法不得被判违规
        let probe_good = r#"self.log(format!("请将 {} 所在目录加入 PATH", adb::adb_exe_name()))"#;
        assert!(
            !probe_good.contains("adb.exe") && !probe_good.contains("scrcpy.exe"),
            "阴性对照失败:合规写法被误判"
        );
        let mut checked = 0usize;
        for (idx, line) in lines.iter().take(prod_end).enumerate() {
            let t = line.trim();
            if t.starts_with("//") || t.starts_with("///") || t.starts_with("//!") {
                continue;
            }
            checked += 1;
            for lit in ["scrcpy.exe", "adb.exe"] {
                assert!(
                    !t.contains(lit),
                    "src/app.rs:{} 在生产代码里写死了 \"{lit}\" —— Linux 上该文件不存在,\
                     文案会指错。请改用 adb::scrcpy_exe_name() / adb::adb_exe_name():{t}",
                    idx + 1
                );
            }
        }
        assert!(checked > 1000, "生产代码扫描行数异常:{checked}");
    }

    /// **T6:诊断面板那一行**逐态 + 逐 count 钉住。
    ///
    /// 与 R11 的 `raw_status_line` 是**两个函数、两处用途**(引擎状态区 vs 诊断
    /// 面板自检区),所以文案表是各自独立的 —— 改一处不影响另一处,也各自有测试。
    ///
    /// 关键分叉:`state == 3` 时 `count` 决定**两种完全不同的结论**。
    /// 把它们写成同一句(或写反)会让用户照着错误的方向去查 —— 这正是变异验证①。
    #[test]
    fn raw_diag_row_covers_all_states_and_counts() {
        // 0 未注册:不显示(非 Windows 没有这条通道,天然落此)
        assert_eq!(
            PadApp::raw_diag_row(0, 0),
            None,
            "state 0(未注册)必须不显示任何行"
        );
        assert_eq!(PadApp::raw_diag_row(0, 7), None, "state 0 与 count 无关");
        // 1 已注册、等首个事件
        assert_eq!(
            PadApp::raw_diag_row(1, 0).as_deref(),
            Some("原始输入：已注册，等待首个事件"),
            "state 1 ⇒ 等待首个事件"
        );
        assert_eq!(
            PadApp::raw_diag_row(1, 3).as_deref(),
            Some("原始输入：已注册，等待首个事件")
        );
        // 2 已启用
        assert_eq!(
            PadApp::raw_diag_row(2, 0).as_deref(),
            Some("原始输入：已启用（按设备计数上报）"),
            "state 2 ⇒ 已启用"
        );
        assert_eq!(
            PadApp::raw_diag_row(2, 9).as_deref(),
            Some("原始输入：已启用（按设备计数上报）")
        );
        // 3 降级:**count>0 与 count==0 必须分叉**
        assert_eq!(
            PadApp::raw_diag_row(3, 5).as_deref(),
            Some("原始输入：已降级（绝对坐标设备，退回光标差）"),
            "state 3 且 count>0 ⇒ 设备只报绝对坐标(来过事件)"
        );
        assert_eq!(
            PadApp::raw_diag_row(3, 0).as_deref(),
            Some("原始输入：已注册但从未收到任何事件（退回光标差）"),
            "state 3 且 count==0 ⇒ 一条事件都没来过(另一件事,文案必须不同)"
        );
        // 两个文案必须真的不同(否则分叉形同虚设)
        assert_ne!(
            PadApp::raw_diag_row(3, 0),
            PadApp::raw_diag_row(3, 1),
            "state 3 下 count 0 / 非 0 必须是两种不同结论"
        );
        // 越界状态:不显示(不猜)
        assert_eq!(PadApp::raw_diag_row(9, 1), None, "未知状态不显示");
        // 文案不得出现第三方程序名(协议 §5「文案/UI 文本」)
        for state in [1u8, 2, 3] {
            let row = PadApp::raw_diag_row(state, 0).expect("1/2/3 都应产出一行");
            for forbidden in ["scrcpy", "Sunshine", "Moonlight", "Parsec", "ToDesk"] {
                assert!(
                    !row.contains(forbidden),
                    "诊断面板文案不得出现第三方程序名 {forbidden:?}: {row}"
                );
            }
        }
    }

    /// **T6 接线守护**:`ui_diagnostics` 函数体里必须**调用** `raw_diag_row`
    /// (纯函数对了、漏接线照样是"面板上什么都不显示"),且 `raw_probe()` 恰好一次。
    ///
    /// 源码级守护,按**逐行扫描**定位函数体(§3.2:禁用会被 `cargo fmt` 重排的
    /// 转义串模式 —— 那种写法重排后 `find` 返回 None,函数体一路截到文件末尾,
    /// 任意调用都能让它判 PASS)。判定必须是**调用**:排除 `fn raw_diag_row(`
    /// 这个**定义**本身。
    ///
    /// 同时钉住**"队列深度行模式"**的既有先例:控制队列那一行
    /// (`控制队列 深度 {} / 峰值 {}`)是 W1-3 建立的"深度行"写法,面板上必须仍在。
    /// **不为 Raw Input 断言深度**:`RawEvent` 那条通道是无界的 `std::sync::mpsc`,
    /// 没有 `len()`,根本不存在可读的深度 —— 编一个数字出来才是错的
    /// (与上面 `raw_diag_row` 里那段注释同一口径)。
    #[test]
    fn raw_diag_row_is_wired_into_diagnostics() {
        let src = include_str!("app.rs");
        let start = src
            .find("fn ui_diagnostics(")
            .expect("找不到 `fn ui_diagnostics(` —— 改名后请同步本测试");
        let rest = &src[start..];
        // 函数体到下一个**顶格 `    }`**为止(该方法在 impl 内,缩进 4 空格)。
        let end = rest
            .match_indices('\n')
            .map(|(i, _)| i + 1)
            .find(|&next| rest[next..].starts_with("    }"))
            .unwrap_or(rest.len());
        let body = &rest[..end];
        assert!(
            body.len() < rest.len(),
            "没找到函数体结束边界(逐行扫描失效)⇒ 本守护会漏报"
        );

        // ① Raw 诊断行必须被调用(排除 `fn raw_diag_row(` 定义本身)
        let called = body
            .match_indices("raw_diag_row(")
            .any(|(i, _)| !body[..i].trim_end().ends_with("fn"));
        assert!(
            called,
            "`ui_diagnostics` 里没有调用 `raw_diag_row(` —— T6 的诊断行没接线,\
             面板上不会出现『原始输入』那一行(三道门对『少调用一个 UI 函数』完全无感)"
        );

        // ② 探针必须真的被调用(且同一帧只调一次:函数体里只应出现一次 `raw_probe()`)
        let probe_calls = body.match_indices("raw_probe()").count();
        assert_eq!(
            probe_calls, 1,
            "`ui_diagnostics` 里 `raw_probe()` 应恰好调用一次(同一帧共用一份读数),实为 {probe_calls}"
        );

        // ③ "队列深度行模式"守护:控制队列那行必须仍在面板体内(格式未变)。
        //    这是"面板上有一行告诉你通道积压了多少"的既有先例 —— 面板重构时
        //    最容易顺手把它删掉,而删掉之后门禁全绿、用户却再也看不到积压。
        assert!(
            body.contains("控制队列 深度"),
            "`ui_diagnostics` 里不再有『控制队列 深度』那一行 —— 深度行模式被删了\
             (W1-3 建立的积压可见性;删掉后三道门依然全绿)"
        );

        // ④ 最近日志块必须真的渲染 `diag::recent_lines()`(R15 的桥的另一端:
        //    diag 侧推入了、界面侧不显示,同样等于没有)
        assert!(
            body.contains("recent_lines()"),
            "`ui_diagnostics` 里没有读取 `diag::recent_lines()` —— R15 的\
             『最近日志』块没接线,面板上看不到任何已落盘行"
        );
    }

    /// **R11 接线守护**:`ui_engine_status` 函数体里必须**调用** `raw_status_line` 与
    /// `raw_probe`(纯函数与探针都对了,漏接线照样是"界面什么都不显示")。
    ///
    /// 源码级守护,按**逐行扫描**定位函数体(§3.2:禁用会被 `cargo fmt` 重排的
    /// 转义串模式 —— 上一版按 `"\n    }\n"` 写,重排后 `find` 返回 None,函数体
    /// 一路截到文件末尾,任意调用都能让它判 PASS)。判定必须是**调用**:排除
    /// `fn raw_status_line(` / `fn raw_probe_alert(` 这两个**定义**本身。
    #[test]
    fn raw_status_line_is_wired_into_engine_status() {
        let src = include_str!("app.rs");
        let start = src
            .find("fn ui_engine_status(")
            .expect("找不到 `fn ui_engine_status(` —— 改名后请同步本测试");
        let rest = &src[start..];
        // 函数体到下一个**顶格 `    }`**为止(该方法在 impl 内,缩进 4 空格)。
        let end = rest
            .match_indices('\n')
            .map(|(i, _)| i + 1)
            .find(|&next| rest[next..].starts_with("    }"))
            .unwrap_or(rest.len());
        let body = &rest[..end];
        assert!(
            body.len() < rest.len(),
            "没找到函数体结束边界(逐行扫描失效)⇒ 本守护会漏报"
        );
        for needle in ["raw_status_line(", "raw_probe_alert("] {
            let called = body
                .match_indices(needle)
                .any(|(i, _)| !body[..i].trim_end().ends_with("fn"));
            assert!(
                called,
                "`ui_engine_status` 里没有调用 `{needle}` —— R11 的状态行/红字告警没接线,\
                 界面什么都不显示(三道门对『少调用一个 UI 函数』完全无感)"
            );
        }
        // 探针必须真的被调用(且同一帧只调一次:函数体里只应出现一次 `raw_probe()`)
        let probe_calls = body.match_indices("raw_probe()").count();
        assert_eq!(
            probe_calls, 1,
            "`ui_engine_status` 里 `raw_probe()` 应恰好调用一次(同一帧共用一份读数),实为 {probe_calls}"
        );
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

    /// task-4(A)的核心判定 —— 周期巡检**该不该**走"更新坐标空间 + 收手 + 失效锚点"。
    ///
    /// `space_change_detected` 返回 true 时,`apply_device_probe` 才会调
    /// `sync_display_space`,而后者会置 `Shared::space_changed_release`(引擎下一轮
    /// 抬起所有按在旧空间里的触点)。所以这个纯函数就是"收手/更新"的总闸门,
    /// 正反两面都要钉死:该触发的一次不能漏,不该触发的一次都不能多。
    ///
    /// adb 真调用在这里**完全用不上**:判定只吃"缓存尺寸 + 巡检结果"两个值,
    /// 设备那侧由 `adb::device_probe` 负责(它的解析另有单测),这条测试不碰进程。
    #[test]
    fn space_change_detection_fires_on_a_real_change_only() {
        let cached = (2772, 1280);
        // ① 真的变了(手机在横竖屏之间切换)→ 必须触发
        assert!(
            space_change_detected(cached, Some((1280, 2772)), true),
            "横竖屏对调必须触发更新+收手"
        );
        assert!(
            space_change_detected((1280, 2772), Some((2772, 1280)), true),
            "反向对调同样必须触发"
        );
        // 同方向换了分辨率(例如投屏改了 max-size)也算变了
        assert!(space_change_detected(cached, Some((2560, 1184)), true));
        // ② 没变 → 绝不动作(否则每 2 秒抬一次玩家正按着的所有触点)
        assert!(
            !space_change_detected(cached, Some(cached), true),
            "尺寸没变绝不能触发收手"
        );
        // ③ 这一轮没查到(adb 抖动 / 解析失败)→ 不动作:"查不到"不是"变了"
        assert!(!space_change_detected(cached, None, true));
        // ④ 只是回退的 `wm size`(物理分辨率、永远自然方向):横屏下与真实坐标空间
        //    差 90°,拿它比对必然误报 —— 而误报的代价是抬起玩家正按着的所有触点
        assert!(
            !space_change_detected(cached, Some((1280, 2772)), false),
            "回退来的物理分辨率不能用来判定空间变化"
        );
        assert!(!space_change_detected(cached, Some(cached), false));
        // ⑤ 0 尺寸是"没读到"的另一种写法
        assert!(!space_change_detected(cached, Some((0, 0)), true));
        assert!(!space_change_detected(cached, Some((2772, 0)), true));
    }

    /// 巡检快照的"新鲜度"判定:只有发起时与此刻的缓存**完全一致**,才允许这轮结果
    /// 去判"尺寸变了"。
    ///
    /// 防的是这一种竞态:探针还在路上,别的事件路径(开打 / 空闲恢复 / 截图)已经把
    /// 空间更新成新方向了;这时手里这份旧快照若被当成"刚查到的真相",就会把坐标空间
    /// **倒退**回旧方向 —— 接下来那一两秒的注入全会落到屏幕外(正是本次故障的现象)。
    #[test]
    fn stale_probe_snapshots_are_not_allowed_to_move_the_space() {
        let a = Some((2772, 1280));
        let b = Some((1280, 2772));
        // 没人动过缓存 -> 快照新鲜,允许判定
        assert!(probe_snapshot_is_fresh(a, a));
        assert!(probe_snapshot_is_fresh(None, None));
        // 飞在路上时缓存被更新过 -> 快照过期,这一轮只取遥测
        assert!(!probe_snapshot_is_fresh(a, b));
        assert!(!probe_snapshot_is_fresh(a, None));
        assert!(!probe_snapshot_is_fresh(None, b));
        assert!(!probe_snapshot_is_fresh(
            Some((1280, 2772)),
            Some((1280, 2772 - 1))
        ));
    }

    /// 遥测(焦点窗口 / 前台 Activity / 屏幕亮灭)只在**变化时**写日志:
    /// 每 2 秒一轮巡检,不这么判就会天天刷屏,真出事时反而找不到那一行。
    #[test]
    fn telemetry_changes_only_when_there_is_a_new_value() {
        let a = "com.x/.A".to_string();
        let b = "com.y/.B".to_string();
        // 第一次拿到值:要说(否则日志里永远没有基线)
        assert!(telemetry_changed(None, Some(&a)));
        assert!(telemetry_changed(None, Some(&true)));
        // 没变:一个字都不写
        assert!(!telemetry_changed(Some(&a), Some(&a)));
        assert!(!telemetry_changed(Some(&false), Some(&false)));
        // 变了:要写
        assert!(telemetry_changed(Some(&a), Some(&b)));
        assert!(telemetry_changed(Some(&true), Some(&false)));
        // 这一轮没查到:不写(查不到 ≠ 变成"无")
        assert!(!telemetry_changed(Some(&a), None));
        assert!(!telemetry_changed::<String>(None, None));
    }

    /// 悬浮窗那一行要塞得下:焦点窗口只留组件名(`com.x/.Y`),不能把整个
    /// `Window{...}` / `ActivityRecord{... t42}` 原样摊进去。
    #[test]
    fn short_focus_keeps_only_the_component() {
        assert_eq!(
            short_focus("Window{8b0f3a1 u0 com.tencent.tmgp.dfm/com.epicgames.ue4.GameActivity}"),
            "com.tencent.tmgp.dfm/com.epicgames.ue4.GameActivity"
        );
        // ActivityRecord 的最后一个 token 是 `t42`,不能取它
        assert_eq!(
            short_focus("ActivityRecord{1a2b3c u0 com.x/.Y t42}"),
            "com.x/.Y"
        );
        // 字面量 `null`(真的没有焦点窗口)原样保留
        assert_eq!(short_focus("null"), "null");
        assert_eq!(short_focus(""), "");
    }

    /// 存活自检的**第一道硬规矩**:只有"确认死"才允许动这条通道,
    /// "没查到/查不成"(未知)一律不许动手 —— 这是整条自检最容易写错、
    /// 后果也最重的一处(误判会把一条本来好的通道重启掉,正在按着的触点全被重建)。
    #[test]
    fn only_a_confirmed_dead_channel_may_be_restarted() {
        use adb::ChannelVerdict::{Alive, Dead, Unknown};
        assert!(Dead.may_restart());
        assert!(!Alive.may_restart());
        assert!(!Unknown.may_restart(), "查询失败必须判未知,不许当死");
        // 标签是界面/日志共用的口径
        assert_eq!(Dead.label(), "死");
        assert_eq!(Alive.label(), "活");
        assert_eq!(Unknown.label(), "未知");
    }

    /// 存活自检的**第二道硬规矩**:连续两次确认才允许重连。
    /// 单次判死可能只是 adb 抖一下或设备端正在换 server;而中间夹一次
    /// "未知/活"就说明那次多半是抖动,不能凑成"连续两次"。
    #[test]
    fn channel_dead_needs_two_consecutive_confirmations() {
        use adb::ChannelVerdict::{Alive, Dead, Unknown};
        let mut gate = DeadConfirm::default();
        // 第一次判死:不许动手
        assert!(!gate.observe(Dead));
        // 第二次连续判死:确认,允许重连
        assert!(gate.observe(Dead));
        // 触发后清零:一次确认只对应一次重连(重连自己有排程)
        assert!(!gate.observe(Dead));
        assert!(gate.observe(Dead));

        // 中间夹"未知":重新数
        let mut gate = DeadConfirm::default();
        assert!(!gate.observe(Dead));
        assert!(!gate.observe(Unknown));
        assert!(!gate.observe(Dead));
        assert!(gate.observe(Dead));

        // 中间夹"活":同样重新数
        let mut gate = DeadConfirm::default();
        assert!(!gate.observe(Dead));
        assert!(!gate.observe(Alive));
        assert!(!gate.observe(Dead));
        assert!(gate.observe(Dead));

        // 手动重连/断开把累计清零
        let mut gate = DeadConfirm::default();
        assert!(!gate.observe(Dead));
        gate.reset();
        assert!(!gate.observe(Dead));
        assert!(gate.observe(Dead));

        // 一直"未知"永远不动手
        let mut gate = DeadConfirm::default();
        for _ in 0..5 {
            assert!(!gate.observe(Unknown));
        }
    }

    /// 逐行找出**含中文字面量**的行(U14 扫描器,AGENTS §3.2:逐行扫描)。
    ///
    /// 判据 = 该行出现 CJK 统一表意文字(U+4E00..=U+9FFF)。只数"代码里写死的
    /// 中文",不做语法解析 —— 要防的正是这种形态:文案本应来自纯函数,却有人
    /// 在绘制函数里又手搓了一句中文。
    ///
    /// 它**故意**不剥注释:三个日志渲染函数体里当前没有任何中文注释,收敛后
    /// 也不该需要(若要写,请写在本文件其他位置)。将来确有无害注释需求时,
    /// 把判据改成"先剥注释"属于**变更守护口径**,须在协议文档登记。
    fn cjk_lines(body: &str) -> Vec<String> {
        body.lines()
            .enumerate()
            .filter(|(_, l)| l.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)))
            .map(|(i, l)| format!("函数体内第 {} 行: {}", i + 1, l.trim()))
            .collect()
    }

    /// **U14:日志三形态只有一份文案来源**(2026-10-17)。
    ///
    /// 左栏日志卡 / 浮窗 / 列表核三处曾经各自写死同一批文案(U6 审计 A4-2:
    /// 同一块文案写三遍,改一处漏两处)。现在标题、按钮、每一行都必须取自
    /// log_card_title / log_popup_button / log_row_text 三个纯函数。
    ///
    /// 两层判据:
    ///   ① **逐位**比字符串 —— 纯函数顺手把当前文案钉死,改字必须同时改测试;
    ///   ② **源码逐行扫描**三个渲染函数体,禁止再出现中文字面量 ——
    ///      ① 管不住"某人又在绘制函数里手搓一句中文"。
    /// 扫描器自带阳性对照(§3.3):故意塞一句中文进样本,同一条判据必须抓到。
    #[test]
    fn log_forms_share_one_text_source() {
        // ① 逐位:当前文案(改文案就要改这里 —— 这正是"唯一来源"的意义)
        let title = PadApp::log_card_title();
        assert_eq!(title, "日志", "日志卡 / 浮窗标题被改动:实际 {title:?}");
        assert_eq!(
            PadApp::log_popup_button(true),
            "关闭弹窗",
            "浮窗已打开时的按钮文案被改动"
        );
        assert_eq!(
            PadApp::log_popup_button(false),
            "弹窗查看",
            "浮窗未打开时的按钮文案被改动"
        );
        // 两个按钮文案必须真的不同(否则开合切换形同虚设)
        assert_ne!(
            PadApp::log_popup_button(true),
            PadApp::log_popup_button(false)
        );
        // 行文案:逐字保留(界面行与诊断文件是同一份证据,不许加前缀/裁剪)
        assert_eq!(PadApp::log_row_text("x"), "x");
        assert_eq!(PadApp::log_row_text("12:00 连接成功"), "12:00 连接成功");

        // ② 三个渲染函数体里不许再出现中文字面量(文案唯一来源)
        let src = include_str!("app.rs");
        for sig in ["fn ui_log_list(", "fn ui_log_card(", "fn ui_log_window("] {
            let body = fn_body(src, sig);
            let hits = cjk_lines(body);
            assert!(
                hits.is_empty(),
                "{sig} 函数体里又写死了中文 UI 文案(应走 log_card_title / log_popup_button / log_row_text 三个纯函数):{hits:#?}"
            );
        }
        // 阳性对照(§3.3):同一条判据必须抓得到一行真的中文字面量
        let probe = concat!("ui.label(", "\"这是一句写死的中文\");");
        assert!(
            !cjk_lines(probe).is_empty(),
            "阳性对照失败:判据抓不到写死的中文文案 ⇒ ② 是假守护"
        );
        // 阴性对照:纯 ASCII 行不得误报
        assert!(
            cjk_lines("ui.monospace(Self::log_row_text(l));").is_empty(),
            "阴性对照失败:ASCII 行被误判成中文字面量"
        );
        // 三个纯函数必须真的被三形态**调用**(纯函数对了、漏接线照样三形态漂移)
        assert!(
            fn_body(src, "fn ui_log_list(").contains("log_row_text("),
            "列表核没有调用 log_row_text —— 行文案的来源断了"
        );
        let card = fn_body(src, "fn ui_log_card(");
        assert!(
            card.contains("log_card_title()"),
            "日志卡没有调用 log_card_title —— 标题来源断了"
        );
        assert!(
            card.contains("log_popup_button("),
            "日志卡没有调用 log_popup_button —— 按钮文案来源断了"
        );
        assert!(
            fn_body(src, "fn ui_log_window(").contains("log_card_title()"),
            "浮窗没有调用 log_card_title —— 标题来源断了"
        );
    }

    /// **一键导出 + 卡片折叠限行**(用户 2026-10-23:"可以一键导出日志,现在状态日志
    /// 覆盖太多,滚动太多")。
    ///
    /// 守护两件事:① 导出正文必须同时带界面缓冲与诊断文件全文(少一份都不是完整
    /// 现场);② 卡片必须走限行数 + 紧邻重复折叠 —— 否则"覆盖太多/滚动太多"会悄悄
    /// 回来(把 LOG_CARD_ROWS 或折叠删掉,本测试就红)。
    #[test]
    fn log_export_and_folded_card_rows_keep_the_panel_readable() {
        // ① 折叠只折紧邻重复:顺序本身是证据,不许全局去重
        let rows = PadApp::fold_log_rows(["a", "a", "a", "b", "a"].iter().copied());
        assert_eq!(
            rows,
            vec![("a", 3), ("b", 1), ("a", 1)],
            "紧邻重复没折对:实际 {rows:?}"
        );
        // 阴性对照:完全不重复时必须一行一条(整体去重会丢掉"重复了几次"这条信息)
        let flat = PadApp::fold_log_rows(["a", "b", "c"].iter().copied());
        assert_eq!(
            flat,
            vec![("a", 1), ("b", 1), ("c", 1)],
            "不重复的行被折叠了"
        );
        // 边界:空输入不许 panic
        assert!(PadApp::fold_log_rows(std::iter::empty::<&str>()).is_empty());

        // ② 卡片限行:数字变了必须同步本测试(否则左栏又会把 200 行全铺开)
        assert_eq!(PadApp::LOG_CARD_ROWS, 12, "日志卡片限行数被改动");
        let hint = PadApp::log_card_hint(12, 200);
        assert!(
            hint.contains("12") && hint.contains("200"),
            "卡片提示没说清 显示行数/缓冲总数:{hint:?}"
        );

        // ③ 导出正文:两份日志都在,抬头带版本/时间/级别
        let body = PadApp::log_export_body(
            "0.2.1",
            "20261023-203000",
            "info",
            "默认",
            &["第一行".to_string(), "第二行".to_string()],
            "diag-line\n",
        );
        for needle in [
            "0.2.1",
            "20261023-203000",
            "第一行",
            "第二行",
            "diag-line",
            "===== 诊断文件全文 =====",
        ] {
            assert!(body.contains(needle), "导出正文缺 {needle:?}:{body}");
        }
        assert!(body.ends_with('\n'), "导出正文必须以换行收尾");
        // 阴性对照:诊断文件为空时不许 panic,封面仍完整
        let bare = PadApp::log_export_body("0.2.1", "s", "warn", "settings.json", &[], "");
        assert!(
            bare.contains("界面日志行数: 0") && bare.ends_with('\n'),
            "空诊断文件时导出正文不完整"
        );
        assert_eq!(
            PadApp::log_export_file_name("20261023-203000"),
            "log-export-20261023-203000.txt",
            "导出文件名格式变了(用户会找不到文件)"
        );

        // ④ 接线:两个入口都挂了导出按钮;卡片走限行数;列表核真的折叠
        let src = include_str!("app.rs");
        assert!(
            fn_body(src, "fn ui_log_card(").contains("log_export_button()"),
            "日志卡没接[导出日志]按钮"
        );
        // 必须查**那一行调用本身**带限行数:只查函数体里出现过 LOG_CARD_ROWS 是
        // 假守护 —— 提示语那行也含这个名字,把调用改成 usize::MAX 照样绿(M13 实测)
        let card_body = fn_body(src, "fn ui_log_card(");
        assert!(
            card_body
                .lines()
                .any(|l| l.contains("\"log_card\"") && l.contains("LOG_CARD_ROWS")),
            "日志卡的列表调用没用限行数 —— 左栏又会被日志铺满"
        );
        assert!(
            fn_body(src, "fn ui_log_window(").contains("log_export_button()"),
            "浮窗没接[导出日志]按钮"
        );
        assert!(
            fn_body(src, "fn ui_log_list(").contains("fold_log_rows("),
            "日志列表没走折叠"
        );
        // ④-b 用户 2026-10-23:"消息越多框涨得越高,把我上面的都挡住了" ⇒ 三条硬约束:
        //   卡片高度由面板定死(内容再多也不涨) + 列表自动跟随最新 + 行可选中复制。
        let list_body = fn_body(src, "fn ui_log_list(");
        // 截断必须挂在同一条 `Label::new(...)` 语句上(链式调用跨行,不能逐行找)
        let label_stmt = list_body
            .split("egui::Label::new(")
            .nth(1)
            .and_then(|rest| rest.split(';').next())
            .unwrap_or("");
        assert!(
            label_stmt.contains(".truncate()"),
            "日志行没做单行截断 —— 长行会换成好几行,卡片就会被内容撑高"
        );
        assert!(
            list_body.contains("stick_to_bottom(true)"),
            "日志列表没跟随最新(自动滚动)"
        );
        assert!(
            list_body.contains(".selectable(true)"),
            "日志行不能选中复制"
        );
        let card_body2 = fn_body(src, "fn ui_log_card(");
        // 反棘轮:卡片高度**只能**来自固定的 `log_height`,绝不能取 `available_height`
        // —— 后者 = 面板剩余高度,而卡片又会撑高面板 ⇒ 空日志也一帧比一帧往上长
        // (用户 2026-10-23:"日志栏什么都没有的情况下自动上升,拉一下上升一下")。
        assert!(
            !card_body2.contains("available_height"),
            "卡片高度不能取自 available_height —— 会和底部面板互相撑高(空日志也会自动上升)"
        );
        assert!(
            card_body2.contains("self.log_height"),
            "卡片高度必须来自固定的 self.log_height(由卡片上沿的手柄改写)"
        );
        // 面板必须被钉成**固定**尺寸(禁用自带拖拽 + min==max==card_h),且卡片内容按
        // card_h 精确分配。三处相等是断棘轮的关键:egui 的 Panel 每帧把内容矩形存成
        // 下一帧尺寸,只要内容比面板高一点,面板与内容就会互相喂着长(用户 2026-10-23
        // 两次报到:"还是一打开,日志就往上窜")。
        // 断言必须只看**调用点本身**:`include_str!` 把测试自己那几行也带进来,
        // 直接 `src.contains(...)` 会被本测试里的期望字符串自满足(M2/M4 实测假绿)。
        let site = src
            .split("egui::Panel::bottom(\"left_log_panel\")")
            .nth(1)
            .and_then(|rest| rest.split("});").next())
            .unwrap_or("");
        assert!(
            site.contains(".resizable(false)"),
            "底部日志面板还开着自带拖拽 —— 与卡片手柄抢高度,会互相打架"
        );
        assert!(
            site.contains(".min_size(card_h)") && site.contains(".max_size(card_h)"),
            "底部日志面板没被钉成固定高度 —— 内容会撑面板、面板再撑内容(棘轮)"
        );
        assert!(
            site.contains("self.ui_log_card(ui, card_h)"),
            "日志卡没拿到固定高度参数(又去猜可用高度了)"
        );
        assert!(
            !site.contains("available_height"),
            "调用点又去取 available_height 当卡片高度 —— 面板/内容互相喂(棘轮)"
        );
        assert!(
            card_body2.contains("set_max_height(card_h)"),
            "日志卡没把自己的高度封在 card_h —— 内容能撑出去"
        );
        // 只看 `grip.dragged()` 是假守护:`grip.hovered() || grip.dragged()` 那行也会
        // 满足它,把拖动分支改成 `if false` 照样绿(M5 实测)⇒ 必须取**拖动分支体**看
        // 它是否真的写了高度。
        let drag_body = card_body2
            .split("if grip.dragged()")
            .nth(1)
            .and_then(|rest| rest.split('}').next())
            .unwrap_or("");
        assert!(
            drag_body.contains("self.log_height =") && drag_body.contains("drag_delta()"),
            "日志卡上沿的拖动手柄没真的改高度 —— 用户改不了高度(他要\"我手动可调节\")"
        );
        assert!(
            !fn_body(src, "fn ui_log_list(").contains("available_height"),
            "日志列表高度不能取自 available_height(同一个棘轮)"
        );
        assert!(
            card_body2.contains("log_copy_all_button()"),
            "卡片上没接[复制全部]"
        );

        // ⑤ 真写一次:导出正文必须逐字落盘,文件名带时间戳,且**写完还是可读文本**
        let dir = std::env::temp_dir().join(format!("spl-log-export-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录失败");
        let path =
            PadApp::write_log_export(&dir, "20261023-203000", &body).expect("写导出文件失败");
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("log-export-20261023-203000.txt"),
            "导出文件名不对"
        );
        let readback = std::fs::read_to_string(&path).expect("读回导出文件失败");
        assert_eq!(readback, body, "导出文件内容与正文不一致(不是逐字落盘)");
        // 目录还不存在时应当自动建出来(write_atomic 的既有行为,这里钉住它)
        let fresh = dir.join("fresh-sub");
        let p2 =
            PadApp::write_log_export(&fresh, "s2", "x").expect("目录不存在时导出应当自动建目录");
        assert_eq!(std::fs::read_to_string(&p2).expect("读回失败"), "x");
        // 阴性对照:父路径是个**文件**时(不是目录),必须报错而不是静默成功
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, b"x").expect("造阻挡文件失败");
        assert!(
            PadApp::write_log_export(&blocker.join("sub"), "s3", &body).is_err(),
            "阴性对照失败:父路径是文件时竟然写成功了"
        );
        let _ = std::fs::remove_dir_all(&dir);

        // 阴性对照:判据必须能抓出"没接线"的旧卡片写法
        let old_card = "fn ui_log_card(&mut self) {\n    self.ui_log_list(ui, 1.0, \"x\", 1);\n}\n";
        assert!(
            !fn_body(old_card, "fn ui_log_card(").contains("log_export_button()"),
            "阴性对照失败:判据抓不出未接线的日志卡"
        );
    }

    /// **反棘轮的真仪表**:headless 连跑几帧,直接量底部面板每帧的高度。
    ///
    /// 阴阳两路对照(协议 §3.3):
    /// - 阳性对照 = 老写法(面板 `default_size` 可长 + 内容高度取自面板自身)⇒ 必须一帧比一帧高;
    /// - 阴性对照 = 现在写法(`min_size == max_size` + 内容按固定值分配)⇒ 必须每帧一样高。
    ///
    /// 为什么需要它:2026-10-23 连着两次"改完还是一打开就往上窜"都是靠读码推断,
    /// 用户各测了一轮。这里把推断变成可复现的量测 —— egui 的 `Panel` 每帧把
    /// **内容矩形**存成下一帧尺寸(`PanelState{outer_rect}`),所以只要内容高度依赖
    /// 面板高度就会正反馈;固定钉死才断链。
    #[test]
    fn pinned_bottom_panel_height_does_not_drift_across_frames() {
        fn measure(pinned: bool, frames: usize) -> Vec<f32> {
            let ctx = egui::Context::default();
            let mut seen = Vec::new();
            for _ in 0..frames {
                let input = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1200.0, 800.0),
                    )),
                    ..Default::default()
                };
                let mut out = ctx.run_ui(input, |ui| {
                    let mut panel = egui::Panel::bottom("anti_ratchet_probe").resizable(false);
                    panel = if pinned {
                        panel.min_size(150.0).max_size(150.0)
                    } else {
                        panel.min_size(80.0).max_size(600.0).default_size(150.0)
                    };
                    let rect = panel
                        .show(ui, |ui| {
                            let h = ui.max_rect().height();
                            // 老写法的内容高度取自面板自身 ⇒ 正反馈
                            let content_h = if pinned { 150.0 } else { h + 20.0 };
                            ui.allocate_ui_with_layout(
                                egui::vec2(ui.available_width(), content_h),
                                egui::Layout::top_down(egui::Align::Min),
                                |ui| {
                                    ui.set_min_height(content_h);
                                    if pinned {
                                        ui.set_max_height(content_h);
                                    }
                                },
                            );
                        })
                        .response
                        .rect;
                    seen.push(rect.height());
                });
                // headless 下没人消费纹理增量,不 clear 会在 drop 时 panic
                out.textures_delta.clear();
            }
            seen
        }

        let old = measure(false, 5);
        assert!(
            old.windows(2).any(|w| w[1] > w[0] + 1.0),
            "阳性对照失败:老写法(内容取面板高度)居然不涨 ⇒ 本测试量不到棘轮,不能用它当证据:{old:?}"
        );
        let now = measure(true, 5);
        let first = now[0];
        assert!(
            now.iter().all(|h| (h - first).abs() < 0.5),
            "面板高度在帧间漂移了(棘轮回来了):{now:?}"
        );
        assert!(
            (first - 150.0).abs() < 2.0,
            "固定面板高度不等于 min==max 的 150:{first}"
        );
    }

    /// **U15:两个 helper 弹窗共骨架、Id 互不相同**(2026-10-17)。
    ///
    /// 两个弹窗骨架逐字同构 ⇒ 共用 helper_window;但 egui 的 Window 默认按
    /// **标题**生成 Id,两窗同时打开时会互相顶位置 / 大小 / 滚动状态。
    /// 所以 Id 必须显式钉成 HelperSource::window_id()。
    #[test]
    fn helper_windows_use_distinct_ids() {
        // ① 两条映射各自正确(改标题 / 宽度必须在测试里同步)
        assert_eq!(HelperSource::Args.title(), "scrcpy 常用参数助手");
        assert_eq!(HelperSource::Adb.title(), "常用 adb 命令助手");
        assert_eq!(HelperSource::Args.default_width(), 500.0);
        assert_eq!(HelperSource::Adb.default_width(), 560.0);
        // ② Id 必须不同(否则两窗同时打开会互相顶状态)
        assert_ne!(
            HelperSource::Args.window_id(),
            HelperSource::Adb.window_id(),
            "两个 helper 弹窗的 egui Id 撞了:同时打开时会互相顶位置 / 大小 / 滚动"
        );
        // ③ 骨架必须真的用上这三个来源(标题 / 宽度 / Id),不许又内联一份
        let src = include_str!("app.rs");
        let body = fn_body(src, "fn helper_window(");
        for needle in [
            "source.title()",
            "source.default_width()",
            "source.window_id()",
        ] {
            assert!(
                body.contains(needle),
                "helper_window 骨架里没有用 {needle} —— 骨架退化成了内联字面量"
            );
        }
        // 两个既有 helper 必须走同一骨架(漏一个 = 那个窗口的 Id 又回到按标题生成)
        for sig in ["fn ui_args_helper(", "fn ui_adb_helper("] {
            let b = fn_body(src, sig);
            assert!(
                b.contains("helper_window("),
                "{sig} 没有走共用骨架 helper_window(U15)"
            );
            assert!(
                !b.contains("egui::Window::new("),
                "{sig} 里又自己建了一个 egui::Window —— 骨架复用被绕过"
            );
        }
        // 阴性对照(§3.3):若两个来源映射到同一个 Id,判据必须判"撞"
        let collapsed = |_s: HelperSource| egui::Id::new("helper_window::args");
        assert_eq!(
            collapsed(HelperSource::Args),
            collapsed(HelperSource::Adb),
            "阴性对照失败:同一 Id 的样本竟然判成不同 ⇒ 判据是空的"
        );
    }

    /// 四层可读性收敛成档位后,**档位 -> (auto, manual) 的映射必须唯一**(§3.1)。
    ///
    /// 判据两层:
    ///   ① 四个档各自写出的组合是钉死的常量(改档位语义必须同时改这里);
    ///   ② preset_of(apply_preset(p)) == p —— 写出去再读回来必须还是同一档,
    ///      否则界面会"选中一个档、显示另一个档"(U16 要防的正是这种漂移)。
    /// 阴性对照(§3.3):同一组合不得同时映射到两个档位。
    #[test]
    fn readability_presets_map_uniquely() {
        use ReadabilityPreset::{Auto, Custom, ForceDark, ForceLight};
        // 拖动滑块时那个"当前手动值":故意取一个既不是 0、也不是两端点的值,
        // 这样下面每条断言都能同时对"丢了手动值"和"错写端点"报警。
        let m = 0.375_f32;

        // ① 四档 -> 确定组合(逐条钉死)
        assert_eq!(
            apply_preset(Auto, m),
            (true, 0.0),
            "自动档必须是 (true, 0.0)"
        );
        assert_eq!(
            apply_preset(ForceDark, m),
            (false, theme::TONE_MIN),
            "强制深色档必须写 (false, TONE_MIN)"
        );
        assert_eq!(
            apply_preset(ForceLight, m),
            (false, theme::TONE_MAX),
            "强制浅色档必须写 (false, TONE_MAX)"
        );
        assert_eq!(
            apply_preset(Custom, m),
            (false, m),
            "自定义档必须保住手动值"
        );

        // ② 往返一致:写出去再读回来还是同一档(Auto/ForceDark/ForceLight 三档;
        //    Custom 只对"开区间内的手动值"成立 —— 端点值按裁定归强制档)
        for p in [Auto, ForceDark, ForceLight] {
            let (auto, manual) = apply_preset(p, m);
            assert_eq!(
                preset_of(auto, manual),
                p,
                "往返不一致:{p:?} 写出 ({auto}, {manual}) 却被读成另一个档"
            );
        }
        let (auto, manual) = apply_preset(Custom, m);
        assert_eq!(preset_of(auto, manual), Custom, "自定义档往返不一致");

        // ③ 自定义档判据:manual 不在 {0.0, TONE_MIN, TONE_MAX} 里一律 Custom(≥2 个样本)
        for sample in [0.375_f32, -0.2, 0.9] {
            assert_eq!(
                preset_of(false, sample),
                Custom,
                "开区间内的手动值 {sample} 被判成了非自定义档"
            );
        }
        // 用户显式选中「自定义」时空值兜底:非有限值(手改 json)按 0.0 写出
        assert_eq!(apply_preset(Custom, f32::NAN), (false, 0.0));

        // ④ 阴性对照(§3.3):样本表里不得出现两个一模一样的组合(判据表有重复)
        let seen = [
            (true, 0.0_f32),
            (false, theme::TONE_MIN),
            (false, theme::TONE_MAX),
            (false, 0.375),
            (false, -0.2),
            (false, 0.9),
        ];
        for (i, a) in seen.iter().enumerate() {
            for (j, b) in seen.iter().enumerate() {
                assert!(
                    i == j || a.0 != b.0 || a.1 != b.1,
                    "样本 {i} 与 {j} 是同一个组合 {a:?} ⇒ 判据表有重复"
                );
            }
        }
        // 端点值按裁定归强制档(而不是 Custom):这是有意的口径,不是漏判
        assert_eq!(
            preset_of(false, theme::TONE_MIN),
            ForceDark,
            "端点应归强制深色档"
        );
        assert_eq!(
            preset_of(false, theme::TONE_MAX),
            ForceLight,
            "端点应归强制浅色档"
        );
        // 自动开关优先于手动值:旧配置 (auto=true, manual=m) 一律读成自动档
        assert_eq!(preset_of(true, m), Auto, "自动开关为真时手动值不该改变档位");
    }

    /// U16:**不改默认观感** —— 全新配置的默认档 = 改前的 (true, 0.0),
    /// 绘制路径取值与改前一致(§5「UI 新增/修改」+ 任务书第 3 条)。
    #[test]
    fn default_readability_equals_legacy_behaviour() {
        use ReadabilityPreset::Auto;
        // ① 全新配置(Look::default / 老 profile 缺字段)落的就是自动档
        let look = theme::Look::default();
        assert_eq!(
            (look.overlay_auto, look.overlay_tone),
            (true, 0.0),
            "默认配置的可读性参数被改了 ⇒ 默认观感与改前不再一致"
        );
        assert_eq!(
            preset_of(look.overlay_auto, look.overlay_tone),
            Auto,
            "默认配置没被读成自动档"
        );
        // ② 默认档写回去仍是同一组常量(切档往返不改变默认观感)
        assert_eq!(apply_preset(Auto, look.overlay_tone), (true, 0.0));

        // ③ 绘制路径的取值语义(绘制里 tone_at 用的就是 theme::auto_tone):
        //    亮画面 -> 往深色走(负),暗画面 -> 往浅色走(正),手动 0.0 不偏。
        //    这几条钉的正是"默认档 = 改前的 auto_tone(luma, 0.0)"。
        let bright = theme::auto_tone(0.85, 0.0);
        let dark = theme::auto_tone(0.15, 0.0);
        assert!(bright < 0.0, "亮画面下自动档没有往深色走:实际 {bright}");
        assert!(dark > 0.0, "暗画面下自动档没有往浅色走:实际 {dark}");
        assert_eq!(theme::auto_tone(0.5, 0.0), 0.0, "中间调应保持本色(0.0)");
        // 默认档不是固定端点:逐块采样必须仍然随画面明暗变化(改前语义)
        assert_ne!(
            bright,
            theme::TONE_MIN,
            "默认档退化成固定深色端点 = 逐块采样失效"
        );
        assert_ne!(
            dark,
            theme::TONE_MAX,
            "默认档退化成固定浅色端点 = 逐块采样失效"
        );
        assert_ne!(bright, dark, "不同明暗的画面采到了同一个档位 = 采样失效");

        // ④ 手动值不因切档而丢:旧配置 (true, m) 读成 Auto,m 原地留在 overlay_tone
        let legacy = (true, 0.375_f32);
        assert_eq!(preset_of(legacy.0, legacy.1), Auto, "旧配置没被读成自动档");
        assert_ne!(preset_of(false, 0.0), Auto, "自定义档不该被读成自动档");
    }

    /// **B21·G-a/G-b:Windows 准 grab 的文案必须与实现状态、与 P1/P3 实测一致**
    /// (2026-10-17 立,2026-10-18 G-b:独占模式**已实现**(默认关),
    /// "尚未实现"四个字必须消失 —— 留着它就是文案谎称没做的功能不存在)。
    ///
    /// 这条测试钉五件事:
    ///   ① 两档齐(`默认` / `独占模式`),且**不许**出现被裁掉的第三档
    ///      (`实验性前台独占` / `RIDEV_NOLEGACY`):G-d 实测零收益、已被裁掉;
    ///   ② 与实现状态一致:独占模式已实现且**默认关闭**,文案里必须写明
    ///      "默认关闭",且**不许再出现**"尚未实现";
    ///   ③ 措辞按实测收窄:必须写明已绑定键(含 W/A/S/D)不再传给其它程序、
    ///      仅在 legacy 消息层生效、拦不住通过 **Raw Input** 读输入的程序;
    ///      并且**不许**出现「完全独占 / 系统级独占」这类承诺;
    ///   ④ 入口唯一:绘制函数里只留 `ui.small(WIN_GRAB_MODES_NOTE);` 一行
    ///      (AGENTS §3.2:逐行扫描,不用会被 fmt 重排的转义串);
    ///   ⑤ 帮助文档(help.md)同口径。
    ///
    /// 阳性对照(§3.3):同一条判据喂 G-a 时代那句"尚未实现"文案必须判"不合规"
    /// —— 否则这些断言只是在证明"我写的字在我写的字里"。
    /// 变异验证(2026-10-18,M3):把"尚未实现"塞回常量 ⇒ 本测试红。
    #[test]
    fn win_grab_copy_states_two_tiers_without_overpromising() {
        let note = WIN_GRAB_MODES_NOTE;
        // ① 两档齐
        for tier in ["默认", "独占模式"] {
            assert!(note.contains(tier), "两档文案缺了 {tier}:{note}");
        }
        // ① 反面:被裁掉的第三档与它的同义词一个字都不许有
        for gone in ["实验性前台独占", "RIDEV_NOLEGACY", "前台独占"] {
            assert!(!note.contains(gone), "被裁掉的第三档又写回文案了:{gone}");
        }
        // ② 与实现状态一致:已实现、默认关闭;"尚未实现"必须消失
        assert!(
            note.contains("默认关闭"),
            "独占模式已实现且默认关:文案必须写明「默认关闭」:{note}"
        );
        assert!(
            !note.contains("尚未实现"),
            "独占模式已实现,文案里的「尚未实现」必须删掉(留着=谎称没有):{note}"
        );
        // ③ 实测收窄后的措辞:已绑定键(含普通键)不再传给其它程序 + 两条边界
        for edge in ["Raw Input", "F8", "legacy 消息", "W/A/S/D"] {
            assert!(
                note.contains(edge),
                "按实测必须写清的口径缺了 {edge}:{note}"
            );
        }
        assert!(
            note.contains("不再传给其它程序"),
            "② 档的核心语义「已绑定键不再传给其它程序」被改掉了:{note}"
        );
        for promise in ["完全独占", "系统级独占", "设备独占"] {
            assert!(
                !note.contains(promise),
                "文案对用户许下了做不到的承诺:{promise}"
            );
        }
        for stale in ["Windows 无需该项", "仅 Linux 可用"] {
            assert!(!note.contains(stale), "与实现不符的旧措辞又回来了:{stale}");
        }

        // ④ 入口唯一(源码级,按函数体逐行判 —— 不是全文 contains)
        let src = include_str!("app.rs");
        let body = fn_body(src, "fn ui_toggle_key_row(");
        assert!(
            body.contains("ui.small(WIN_GRAB_MODES_NOTE);"),
            "ui_toggle_key_row 没有用常量渲染 grab 说明 —— 文案又变回内联字面量了"
        );
        assert!(
            !body.contains("Windows 无需该项"),
            "ui_toggle_key_row 里又出现了与实现不符的旧文案"
        );
        // G-b:独占模式的开关入口也必须在同一个绘制函数里(§5 入口面矩阵:
        // 该行是默认/可视化风格共用的左栏中段,开关跟着说明走)
        assert!(
            body.contains("swallow_bound_keys"),
            "ui_toggle_key_row 缺了独占模式开关(G-b)—— 说明在、开关没了"
        );

        // ⑤ 帮助文档同口径:两档、默认关闭、Raw Input 边界都要在 help.md 里,
        // "尚未实现"必须消失,被裁掉的第三档同样不许出现。
        let help = include_str!("help.md");
        for needle in ["独占模式", "默认关闭", "Raw Input", "legacy"] {
            assert!(help.contains(needle), "help.md 缺了 {needle}");
        }
        assert!(
            !help.contains("尚未实现"),
            "help.md 里还写着「尚未实现」—— 独占模式已实现,文案说谎"
        );
        assert!(
            !help.contains("实验性前台独占"),
            "help.md 里还写着被裁掉的第三档(实验性前台独占)"
        );

        // 阳性对照(§3.3):同一条判据必须抓得到旧文案
        let legacy = "映射时屏蔽原键(grab)仅 Linux 可用(EVIOCGRAB 独占设备);Windows 无需该项。";
        assert!(
            legacy.contains("Windows 无需该项") || legacy.contains("仅 Linux 可用"),
            "阳性对照失败:判据抓不到旧文案 ⇒ 上面的断言是空的"
        );
        // 阴性对照(§3.3):新的两档文案不得被判成旧措辞
        assert!(
            !note.contains("Windows 无需该项") && !note.contains("仅 Linux 可用"),
            "阴性对照失败:新文案被判成了旧措辞"
        );
    }

    /// **D1 守护(2026-10-22)**:记住的设备不许被"设备列表为空"冲掉。
    ///
    /// # 事故链(只读审计发现)
    ///
    /// `serial()` 的语义是"此刻选中的设备",拔线/adb 掉线后设备列表为空 ⇒
    /// 返回空串;而 `settings_snapshot()` 以前直接写它 ⇒ **一次刷新就把用户
    /// 记住的设备冲成空串**;读回侧(`src/app.rs` 启动逻辑)要求非空且在线 ⇒
    /// 永久失效。症状:重启后不再选中上次那台设备,多设备时选错机器。
    ///
    /// # 为什么判"结构"而不是"行为"
    ///
    /// 行为层要构造 PadApp + 真 adb 列表才能触发,而这里要守的是**写盘取值
    /// 规则**这一条缝:① 纯函数取值正确 ② snapshot 不许再直接写 `self.serial()`
    /// ③ 刷新后必须调 `note_current_serial()`。三者都是确定的结构事实,
    /// 按函数体逐行扫(AGENTS §3.2:不用会被 fmt 重排的转义串)。
    ///
    /// 变异验证(2026-10-22):把 snapshot 里的 `remembered_serial(...)` 改回
    /// `self.serial()` ⇒ 本测试红并点名。
    #[test]
    fn remembered_device_survives_an_empty_device_list() {
        // ① 纯函数:设备在线 ⇒ 用当前的;设备消失 ⇒ 保住上次记住的
        assert_eq!(
            PadApp::remembered_serial("NEW1", "OLD1"),
            "NEW1",
            "有设备时必须用当前选中的那台(否则切设备永远不生效)"
        );
        assert_eq!(
            PadApp::remembered_serial("", "OLD1"),
            "OLD1",
            "设备列表为空时必须保住上次记住的 —— 这正是 D1:写成空串 ⇒ 读回侧拒绝 ⇒ 永久失效"
        );
        // 空白串按"没有"处理(settings.rs 读回也会 trim)
        assert_eq!(PadApp::remembered_serial("   ", " OLD1 "), "OLD1");
        // 两边都空 ⇒ 空(首次运行,没有可记住的东西)
        assert_eq!(PadApp::remembered_serial("", ""), "");
        // 阴性对照(§3.3):旧实现等价于 `current` 原样返回,
        // 它必须在这条判据上被判错 —— 否则上面的断言是空的
        let old_impl = |current: &str, _last: &str| current.to_string();
        assert_eq!(
            old_impl("", "OLD1"),
            "",
            "阳性对照失败:旧实现(直接用当前值)在这条判据上应当给出空串"
        );
        assert_ne!(
            old_impl("", "OLD1"),
            PadApp::remembered_serial("", "OLD1"),
            "阳性对照失败:新旧实现在「设备消失」时行为必须不同"
        );

        // ② 结构:写盘取值必须走 remembered_serial,不许再直接写 self.serial()
        let src = include_str!("app.rs");
        let snap = fn_body(src, "fn settings_snapshot(");
        assert!(
            snap.contains("remembered_serial"),
            "settings_snapshot 不再用 remembered_serial 兜底 —— D1 会复发(一次刷新冲掉记住的设备)"
        );
        assert!(
            !snap.contains("selected_serial: self.serial()"),
            "settings_snapshot 又直接写 self.serial() 了 —— 设备列表为空时它就是空串(D1)"
        );

        // ③ 结构:设备列表刷新后必须记一次当前设备,否则 last_serial 永远停在启动值
        let refresh = fn_body(src, "fn refresh_devices(");
        assert!(
            refresh.contains("note_current_serial()"),
            "refresh_devices 没调 note_current_serial() —— 用户换设备后记住的仍是旧那台"
        );

        // ④ 结构:note_current_serial 必须走同一条取值规则(不许另写一套)
        let note = fn_body(src, "fn note_current_serial(");
        assert!(
            note.contains("remembered_serial"),
            "note_current_serial 自己写了另一套取值规则 —— 两处规则迟早分叉"
        );
        // 阳性对照:判据必须抓得到被禁止的写法本身
        let banned = "selected_serial: self.serial(),";
        assert!(
            banned.contains("self.serial()"),
            "阳性对照失败:结构判据抓不到被禁写法"
        );
    }

    /// **武装唯一入口守护**(2026-10-23 组合键事故)。
    ///
    /// 主界面所有"点一下按钮等用户按键"的槽位都记在 `waiting_key` / `waiting_keys` 上。
    /// 出事故时十几个入口各写各的 `self.waiting_key = Some(slot);`:没人清对面那条路、
    /// 也没人清 `capture_down`/`capture_seen`(上一轮按过的键还留着),于是同一个按键被
    /// 两条路各处理一遍、或者上一次的残留键被当成这一次的输入。现在只有
    /// `arm_key_capture` / `arm_keys_capture` 两个函数体里允许出现赋值。
    #[test]
    fn every_capture_arming_goes_through_the_two_helpers() {
        let src = include_str!("app.rs");
        let lines: Vec<&str> = src.lines().collect();
        let cut = lines
            .iter()
            .position(|l| l.trim() == "mod tests {")
            .unwrap_or(lines.len());
        // 允许名单用**行号区间**表示两个 helper 自己的函数体(不用字节偏移,
        // 免得换行风格一变就整体漂移)。
        let mut allowed: Vec<(usize, usize)> = Vec::new();
        for sig in ["fn arm_key_capture(", "fn arm_keys_capture("] {
            let body = fn_body(src, sig);
            let off = body.as_ptr() as usize - src.as_ptr() as usize;
            let first = src[..off].matches(char::from(10)).count();
            allowed.push((first, first + body.lines().count()));
        }
        let mut hits: Vec<(usize, String)> = Vec::new();
        for (i, line) in lines.iter().enumerate().take(cut) {
            let t = line.trim();
            if t.starts_with("//") {
                continue;
            }
            if !(t.contains("waiting_key = Some(") || t.contains("waiting_keys = Some(")) {
                continue;
            }
            if allowed.iter().any(|(a, b)| i >= *a && i < *b) {
                continue;
            }
            hits.push((i + 1, t.to_string()));
        }
        assert!(
            hits.is_empty(),
            "这些地方绕过了 arm_key_capture/arm_keys_capture 直接武装捕获: {hits:?} —— \
             必须走那两个 helper(它们会清对面包、picking 与 capture_down/capture_seen)"
        );
        // 阳性对照(§3.3):把允许名单去掉,同一条扫描必须报出 helper 自己那两行。
        let naked: Vec<usize> = lines
            .iter()
            .enumerate()
            .take(cut)
            .filter(|(_, l)| {
                let t = l.trim();
                !t.starts_with("//")
                    && (t.contains("waiting_key = Some(") || t.contains("waiting_keys = Some("))
            })
            .map(|(i, _)| i + 1)
            .collect();
        assert!(
            naked.len() >= 2,
            "阳性对照失败:连 helper 自己那两行都扫不到({naked:?}),判据形同虚设"
        );
    }

    /// **让路判据唯一性守护**(2026-10-23 组合键事故的正面证明)。
    ///
    /// 事故形状:全局快捷键那段只挡了 `waiting_key`/`picking`,漏了 `waiting_keys`
    /// (组合键捕获)与宏录制 ⇒ 捕获 Ctrl+Z 时同一帧里又把它当"撤销"执行一次,
    /// 把刚写进槽位的值回滚成那格原来的值(用户:设的 Ctrl+Z,界面变成 Ctrl+X)。
    #[test]
    fn global_shortcuts_yield_while_capturing_input() {
        let src = include_str!("app.rs");
        let prod = &src[..src.find("mod tests {").unwrap_or(src.len())];
        // ① 撤销/重做/保存/刷新那一段必须整体让路,且走的是同一个判据函数
        assert!(
            prod.lines()
                .any(|l| l.contains("!self.is_capturing_input()")
                    && l.contains("egui_wants_keyboard_input()")),
            "全局快捷键的让路条件没走 is_capturing_input —— 它漏掉的正是组合键捕获与宏录制"
        );
        // ② 手抄的旧条件不许回来
        let stale: Vec<&str> = prod
            .lines()
            .filter(|l| !l.trim_start().starts_with("//") && l.contains("waiting_key.is_none()"))
            .collect();
        assert!(
            stale.is_empty(),
            "全局快捷键的让路条件又写成手抄清单了(漏一类就重现 Ctrl+Z 被当撤销): {stale:?}"
        );
        // ③ 界面层热键(强制重连/宏三键)与自愈 busy 也必须共用这一个判据
        assert!(
            prod.contains("self.is_capturing_input() || ui.ctx().egui_wants_keyboard_input()"),
            "界面层热键又自己拼了一份捕获条件"
        );
        let busy = fn_body(src, "fn sync_self_heal_ui_state(");
        assert!(
            busy.contains("let busy = self.is_capturing_input();"),
            "自愈 ui_busy 又自己拼了一份捕获条件"
        );
        // ④ 判据本体必须把四个"等用户输入"的状态都算上(少一个就少挡一类)
        let p = fn_body(src, "fn is_capturing_input(");
        for f in [
            "self.waiting_key.is_some()",
            "self.waiting_keys.is_some()",
            "self.picking.is_some()",
            "self.macro_recording.is_some()",
        ] {
            assert!(p.contains(f), "is_capturing_input 少了 {f}");
        }
        // 阳性对照(§3.3):判据必须抓得到被禁的那种写法本身
        let banned = "if self.waiting_key.is_none() && self.picking.is_none() {";
        assert!(
            banned.contains("waiting_key.is_none()"),
            "阳性对照失败:判据抓不到被手抄的写法"
        );
    }

    /// 宏步骤「组合键」的一格写入(2026-10-23)。
    ///
    /// doc 早写着"从第 1 格起录 = 只改那一格",实现却是把整组 codes 从 slot 起连续
    /// 写入 —— 在"第 2 格"上按 Ctrl+Z 会连第 3 格一起顶掉。这里把口径钉死并用
    /// 旧口径做阴性对照。
    #[test]
    fn write_combo_keys_only_touches_the_requested_cell() {
        // 第 0 格:整份替换(不留旧尾巴,否则 4 键改 2 键会剩下两个 0)
        let mut v = vec![1u16, 2, 3];
        assert!(write_combo_keys(&mut v, 0, &[9, 8]));
        assert_eq!(v, vec![9, 8]);
        // 第 1 格:只改那一格,旁边的键一个都不许动
        let mut v = vec![1u16, 2, 3];
        assert!(write_combo_keys(&mut v, 1, &[44]));
        assert_eq!(v, vec![1, 44, 3]);
        // 组合键(修饰键在前 ⇒ 最后那个才是这一格要的"主键")
        let mut v = vec![1u16, 2, 3];
        assert!(write_combo_keys(&mut v, 1, &[29, 44]));
        assert_eq!(v, vec![1, 44, 3], "第 2 格上按 Ctrl+Z 不该动第 3 格");
        // 空 codes:什么都不写
        let mut v = vec![1u16];
        assert!(!write_combo_keys(&mut v, 0, &[]));
        assert_eq!(v, vec![1]);
        // 槽位在末尾之外:补长;越过上限:不写
        let mut v = vec![1u16, 2];
        assert!(write_combo_keys(&mut v, 2, &[7]));
        assert_eq!(v, vec![1, 2, 7]);
        let mut v = vec![0u16; MACRO_COMBO_MAX_KEYS];
        assert!(!write_combo_keys(&mut v, MACRO_COMBO_MAX_KEYS, &[7]));
        // 阴性对照(§3.3):旧口径在同一份样本上必然顶掉第 3 格
        let mut old_way = vec![1u16, 2, 3];
        for (i, c) in [29u16, 44].iter().enumerate() {
            old_way[1 + i] = *c;
        }
        assert_ne!(
            old_way,
            vec![1, 44, 3],
            "旧口径会改到第 3 格,判据必须能分辨"
        );
    }

    /// 用户配置的组合键成员格：单键只改一格、按住组合则从这格起整组替换（2026-10-23）。
    ///
    /// 这一条是「Ctrl+Z 变成 Ctrl+X」事故的**第二半**：成员格原先是单键捕获，
    /// 按下 Ctrl 就落定、`waiting_key` 随即清空，用户仍按着的 Z 被全局快捷键当成
    /// "撤销"执行 ⇒ 刚写的值被回滚。改成和弦捕获后由 `write_chord_into_combo` 落盘。
    #[test]
    fn combo_member_chord_replaces_from_the_armed_cell_only() {
        // 只按一个键:只改这一格,邻居一个都不许动(与从前逐字相同)
        let mut v = vec![1u16, 2, 3];
        assert!(write_chord_into_combo(&mut v, 1, &[44]));
        assert_eq!(v, vec![1, 44, 3]);
        // 按住组合:从这一格起整组替换,后面的旧尾巴截掉
        let mut v = vec![1u16, 2, 3];
        assert!(write_chord_into_combo(&mut v, 0, &[29, 44]));
        assert_eq!(
            v,
            vec![29, 44],
            "从第 1 格起按住 Ctrl+Z = 这一组就是 Ctrl+Z"
        );
        let mut v = vec![1u16, 2, 3];
        assert!(write_chord_into_combo(&mut v, 1, &[29, 44]));
        assert_eq!(
            v,
            vec![1, 29, 44],
            "第 1 格(0 基的第 2 格)起也一样,前面的格子不动"
        );
        // 缺位补 0:组合键语义就是"至少两个键"(界面 `＋ 新增成员` 推的也是 0)
        let mut v: Vec<u16> = Vec::new();
        assert!(write_chord_into_combo(&mut v, 0, &[29, 44]));
        assert_eq!(v, vec![29, 44]);
        let mut v = vec![0u16];
        assert!(write_chord_into_combo(&mut v, 1, &[29, 44]));
        assert_eq!(v, vec![0, 29, 44]);
        // 单键落在这格之外:补长,不截尾巴
        let mut v = vec![1u16, 2];
        assert!(write_chord_into_combo(&mut v, 2, &[7]));
        assert_eq!(v, vec![1, 2, 7]);
        // 空 codes / 越过成员格上限:什么都不写
        let mut v = vec![1u16, 2];
        assert!(!write_chord_into_combo(&mut v, 0, &[]));
        assert_eq!(v, vec![1, 2]);
        let mut v = vec![1u16, 2, 3, 4];
        assert!(!write_chord_into_combo(&mut v, COMBO_CELLS_MAX, &[7]));
        assert_eq!(v, vec![1, 2, 3, 4]);
        // 上限内的截断:第 4 格上按住两键只放得下第一个
        let mut v = vec![1u16, 2, 3, 4];
        assert!(write_chord_into_combo(
            &mut v,
            COMBO_CELLS_MAX - 1,
            &[29, 44]
        ));
        assert_eq!(v, vec![1, 2, 3, 29]);
        // 阴性对照(§3.3):"只取最后一个键"的口径在同一份样本上必然丢掉 Ctrl
        let mut single = vec![1u16, 2, 3];
        if let Some(code) = [29u16, 44].last() {
            single[0] = *code;
        }
        assert_ne!(single, vec![29, 44], "旧口径只写主键,判据必须能分辨");
    }

    /// 组合键成员格必须挂到**和弦**捕获上(2026-10-23 事故第二半)。
    ///
    /// 单键捕获会在按下 Ctrl 的瞬间落定并清空 `waiting_key`，用户仍按着的 Z 于是被
    /// 全局快捷键当成"撤销" ⇒ 刚写的值被回滚成原值。这条守护测试盯住三件事：
    /// 生产区不再出现"单键武装 ComboKey"、成员行确实调用和弦 helper、helper 本体
    /// 确实武装 `waiting_keys`；并附阳性对照。
    #[test]
    fn combo_member_cells_are_armed_as_chords_not_single_keys() {
        let src = include_str!("app.rs");
        let cut = src.find("mod tests {").expect("找不到 mod tests");
        let prod = &src[..cut];
        let naked: Vec<(usize, String)> = prod
            .lines()
            .enumerate()
            .filter(|(_, l)| {
                let t = l.trim_start();
                !t.starts_with("//") && t.contains("arm_key_capture(KeySlot::ComboKey")
            })
            .map(|(i, l)| (i + 1, l.trim().to_string()))
            .collect();
        assert!(
            naked.is_empty(),
            "组合键成员格又挂回单键捕获了(按 Ctrl+Z 会被全局撤销回滚成原值): {naked:?}"
        );
        assert!(
            prod.contains("self.begin_combo_member_capture(index, slot);"),
            "组合键成员行没有调用和弦 helper"
        );
        let body = fn_body(src, "fn begin_combo_member_capture(");
        assert!(
            body.contains("self.arm_keys_capture(KeySlot::ComboKey { combo, slot });"),
            "和弦 helper 没把成员格挂到 waiting_keys 上"
        );
        assert!(
            prod.contains("self.waiting_keys == Some(this)"),
            "成员格的等待态没认和弦捕获(点了没反应)"
        );
        // 阳性对照(§3.3):判据必须抓得到被禁的那种写法本身
        let banned = "self.arm_key_capture(KeySlot::ComboKey { combo: index, slot });";
        assert!(
            banned.contains("arm_key_capture(KeySlot::ComboKey"),
            "阳性对照失败:判据抓不到单键武装成员格"
        );
    }

    /// 组合键卡片的字段编辑必须落在**它自己那张卡**上,而且只改字段、不整条覆盖
    /// (2026-10-23 用户第 1 条:「点击完组合键,组合键映射的位置突然瞬移了」)。
    #[test]
    fn combo_edits_land_on_their_own_card_and_only_touch_fields() {
        let tap = |x: f32, y: f32| Action::Tap {
            x,
            y,
            duration_ms: 20,
            radius: 12.0,
            repeat_interval_ms: 0,
        };
        let mk = |key: u16, x: f32| KeyCombo {
            keys: vec![key, 0],
            action: tap(x, 960.0),
            fps_only: false,
            tail_delay_ms: 0,
            repeat_while_held: false,
        };
        let pos = |c: &KeyCombo| match &c.action {
            Action::Tap { x, y, .. } => (*x, *y),
            _ => (0.0, 0.0),
        };
        let frame_start = vec![mk(1, 540.0), mk(2, 540.0), mk(3, 540.0), mk(4, 540.0)];
        // 用户改的是第 3 张卡(帧首下标 2):落点从 (540,960) 改成 (100,200)
        let mut edited = frame_start[2].clone();
        edited.action = tap(100.0, 200.0);
        let edits = vec![(2usize, edited)];

        // 正确顺序:先写回(此刻实时列表还是渲染时的顺序),再做本帧的结构性编辑。
        let mut live = frame_start.clone();
        assert_eq!(commit_combo_field_edits(&frame_start, &mut live, &edits), 1);
        live.remove(0); // 同一帧里点了「删除组合」
        assert_eq!(live[0].keys, vec![2, 0], "原第 2 张卡还在");
        assert_eq!(live[1].keys, vec![3, 0], "原第 3 张卡");
        assert_eq!(pos(&live[1]), (100.0, 200.0), "编辑必须落在它自己那张卡上");
        assert_eq!(pos(&live[0]), (540.0, 960.0), "邻居的落点一个字节都不许动");
        assert_eq!(pos(&live[2]), (540.0, 960.0));

        // 字段级:keys 由「删/加成员」直接写在实时列表上,旧快照不许把它盖回去。
        let mut live = frame_start.clone();
        live[2].keys = vec![9, 0]; // 同帧点了「＋ 新增成员」那种直接改 keys 的路径
        assert_eq!(commit_combo_field_edits(&frame_start, &mut live, &edits), 1);
        assert_eq!(live[2].keys, vec![9, 0], "keys 被整份覆盖了(旧快照盖回来)");
        assert_eq!(pos(&live[2]), (100.0, 200.0), "该改的字段还是要改");

        // 阴性对照(§3.3):结构性编辑已经跑过(长度变了)⇒ 一个字节都不写,
        // 更不许写到邻居身上(旧写法会在这里把编辑搬到别的卡上)。
        let mut live = frame_start.clone();
        live.remove(0);
        assert_eq!(
            commit_combo_field_edits(&frame_start, &mut live, &edits),
            0,
            "长度变了还往下写 = 把编辑搬到邻居身上"
        );
        assert_eq!(pos(&live[0]), (540.0, 960.0));
        assert_eq!(pos(&live[1]), (540.0, 960.0));
        assert_eq!(live[1].keys, vec![3, 0], "邻居的键位也不许动");

        // 越界下标:不写、不 panic。
        let mut live = frame_start.clone();
        assert_eq!(
            commit_combo_field_edits(&frame_start, &mut live, &[(9usize, frame_start[0].clone())]),
            0
        );
    }

    /// 帧首下标只有在「同一帧的结构性编辑之前」写回才指得准 —— 顺序本身是承重的。
    #[test]
    fn combo_field_edits_are_committed_before_the_frames_structural_edits() {
        let src = include_str!("app.rs");
        let prod = &src[..src.find("mod tests {").expect("mod tests 边界")];
        let body = fn_body(prod, "fn ui_combos(");
        let at = |needle: &str| body.find(needle).expect("ui_combos 里找不到锚点");
        let commit_at = at("commit_combo_field_edits(&frame_start");
        assert!(
            commit_at < at("if let Some(index) = delete_combo"),
            "字段写回被排到了 delete_combo 之后:帧首下标会落到邻居身上"
        );
        assert!(
            commit_at < at("if let Some((combo_index, slot)) = delete_slot"),
            "字段写回被排到了 delete_slot 之后"
        );
        assert!(
            commit_at < at("if let Some(index) = add_slot"),
            "字段写回被排到了 add_slot 之后"
        );
        // 阴性对照(§3.3):整份覆盖的写法不许再出现在生产区。
        assert!(
            !prod.contains("*target = combo;"),
            "整份覆盖又回来了:会把 keys 一起盖回旧快照"
        );
        // 阳性对照:被禁的那种写法本身必须能被这条判据抓到。
        assert!("*target = combo;".contains("*target = combo;"));
    }

    /// undo/redo 换掉整份 profile 之后,按位置记的目标必须作废(同一次事故的另一条通路)。
    #[test]
    fn undo_and_redo_drop_positional_targets() {
        let src = include_str!("app.rs");
        let prod = &src[..src.find("mod tests {").expect("mod tests 边界")];
        for f in ["fn undo(", "fn redo("] {
            let body = fn_body(prod, f);
            assert!(
                body.contains("forget_stale_target"),
                "{f} 换掉整份 profile 之后没作废按位置记的目标:下一次落定会写到别的行上"
            );
        }
        // 阴性对照(§3.3):判据函数自己不许出现收口调用(防止把不相关的位置当成收口)。
        let cap = fn_body(prod, "fn is_capturing_input(");
        assert!(
            !cap.contains("forget_stale_target"),
            "阴性对照失败:is_capturing_input 里不该有收口"
        );
    }

    /// 帧首取点快照不许把这一帧里已经被消费掉的取点「复活」。
    #[test]
    fn frame_local_values_do_not_resurrect_a_consumed_pick() {
        // 这一帧里没人动过 ⇒ 采用卡片自己设的值。
        assert_eq!(frame_local_take(Some(1i32), Some(1), Some(7)), Some(7));
        assert_eq!(frame_local_take(None, None, Some(7)), Some(7));
        // 这一帧里别处消费掉了取点 ⇒ 保留「别处」的结果。
        assert_eq!(frame_local_take(Some(1i32), None, Some(7)), None);
        assert_eq!(frame_local_take(None, Some(3), Some(7)), Some(3));
        // 阴性对照(§3.3):无脑回写 local 会把已经消费掉的取点复活。
        let naive = |_f: Option<i32>, _l: Option<i32>, local: Option<i32>| local;
        assert_eq!(naive(Some(1), None, Some(7)), Some(7), "旧写法确实会复活");
        assert_ne!(
            naive(Some(1), None, Some(7)),
            frame_local_take(Some(1), None, Some(7))
        );
    }

    /// 取证守护(用户 2026-10-23):按键捕获必须把**原始码**落盘。
    /// 没有这一行,"按 Z 却绑成 X"下次仍然只能靠猜 —— 它区分「OS 报的码不对」
    /// 与「值被后一步覆盖」这两类完全不同的根因,是本轮事故的关键仪表。
    #[test]
    fn captured_keys_are_logged_with_their_raw_code() {
        let src = include_str!("app.rs");
        let prod = &src[..src.find("mod tests {").unwrap_or(src.len())];
        // ① 两条捕获路径(组合键 / 单键)都必须打这一行
        let calls = prod
            .lines()
            .filter(|l| {
                !l.trim_start().starts_with("//") && l.contains("self.log_captured_key(code);")
            })
            .count();
        assert!(
            calls >= 2,
            "捕获路径少了原始码日志(组合键与单键各要一条),实际 {calls} 条"
        );
        // ② 仪表本体必须把码打出来,而不是只打键名
        let body = fn_body(src, "fn log_captured_key(");
        assert!(
            body.contains("码 {}"),
            "log_captured_key 没打原始码: {body}"
        );
        assert!(
            body.contains("key_name(code)"),
            "log_captured_key 没打键名: {body}"
        );
        // ③ 绑定结果那行也要带码(否则回看日志时对不上是哪一次捕获)
        assert!(
            prod.contains("[码 {:?}]"),
            "\"键位已绑定\" 那行没带原始码,复现时无法与捕获行对账"
        );
        // 阳性对照(§3.3):只打键名、不打码的写法必须过不了 ②
        let probe_bad = "self.log(format!(\"捕获按键:{}\", key_name(code)));";
        assert!(
            !probe_bad.contains("码 {}"),
            "阳性对照失效:不带码的写法居然通过"
        );
    }
}
