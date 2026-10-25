//! 全局输入捕获层(平台无关接口,平台实现见下方 cfg 模块)。
//!
//! Linux  : evdev 读取 `/dev/input/event*`(Wayland/X11 皆可),需 input 组权限;
//!          grab 模式用于映射开启时屏蔽原始按键,鼠标 grab 用于 FPS 瞄准时冻结光标。
//! Windows: 自己装的低级键盘/鼠标钩子(WH_KEYBOARD_LL / WH_MOUSE_LL),无需管理员权限。
//!
//! # 为什么不用 rdev(Windows)
//!
//! 原先 Windows 侧用 rdev 0.5.3 的低级钩子。它有个致命开销:它的回调在**调用我们的
//! 闭包之前**,会对每个 `KeyPress` 调一次 `Keyboard::get_name()`,而那里做了
//! `GetForegroundWindow` + `AttachThreadInput(当前线程, 前台窗口线程)` +
//! `ToUnicodeEx`。`AttachThreadInput` 会同步附着到**别的线程**的输入队列上,
//! 对方不泵消息就会一直阻塞。Windows 对低级钩子有硬时限
//! (`LowLevelHooksTimeout`,默认 300ms):超时的事件被**静默丢弃**,
//! 反复超时还会把整个钩子**静默摘掉** —— 用户侧的表现就是"按了没反应,
//! 而且切过窗口之后尤其容易触发"(切窗口正是 `GetForegroundWindow` 变化的时刻)。
//!
//! 讽刺的是 `event.name` 我们从未使用过。自己装钩子之后,回调里只做
//! "读 vkCode → 查表 → 塞进队列"这三件事,一次系统调用都不做,超时风险随之消失。
//!
//! # 两条平台共用的可靠性机制
//!
//! 1. **热插拔重扫**:设备只在启动时枚举一次是不够的(外接键盘挂起唤醒、拔插、
//!    USB 复位都会换节点号,旧 fd 直接作废且不会报错)。两边都有后台线程定期重扫。
//! 2. **丢失 release 的自愈**:底层一旦漏掉一个 KeyRelease,引擎侧那个键就会
//!    永远留在"按下"状态(幻影触点 + 上升沿不再产生)。所以捕获层自己维护一份
//!    "已上报为按下"的集合,并定期与**权威真值**比对,把多出来的补发一次抬起:
//!      * Linux  : `EVIOCGKEY`,直接问内核"此刻哪些键真的按着";
//!      * Windows: `GetAsyncKeyState`,逐个问系统。

use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

/// 统一的输入事件。
///
/// 键盘按键与鼠标按键共用同一 evdev 码空间(鼠标左/右/中键 = BTN_LEFT/RIGHT/MIDDLE,
/// 即 272/273/274),因此键位绑定流程无需区分二者;
/// 鼠标位移单独作为 Motion 事件,供 FPS 瞄准子系统使用。
///
/// # 事件时刻(W1-1)
///
/// 每个事件都带 `at`:它在**捕获层拿到事件的那一刻**取得,而不是引擎/界面
/// 从队列里取出来的那一刻。两者之间隔着钩子→中转线程→引擎三段排队,原来
/// "静止归中计时""手柄回中计时""宏录制间隔"都是拿后者算的 —— 处理抖动
/// (宏步进、UI 卡顿、批量收事件)会直接变成几个毫秒的计时误差,宏录制尤其
/// 明显:两次真实间隔 10ms 的按键,录出来可能是 8ms 或 15ms。
///
/// Windows 侧取的是钩子回调进场时刻(`Instant::now()`),没有使用钩子结构体
/// 里的 `time` 字段(GetTickCount 系毫秒):那个字段要跨时钟基准换算,还有
/// 挂起/恢复跳变与 49.7 天回绕两个坑,而它的分辨率(1ms,且依赖 timeBeginPeriod)
/// 并不比回调进场时刻更准。Linux 侧由 evdev 时间戳折算(见 `event_time`)。
#[derive(Debug, Clone, Copy)]
pub enum CaptureEvent {
    /// 按键状态变化(键盘按键或鼠标按键)
    Button {
        code: u16,
        pressed: bool,
        /// 捕获时刻(进程单调时钟)
        at: Instant,
    },
    /// 鼠标相对位移(设备计数,非像素)
    Motion { dx: f32, dy: f32, at: Instant },
}

impl CaptureEvent {
    /// 事件自带的捕获时刻(W1-1);引擎与宏录制都按它计时。
    pub fn at(&self) -> Instant {
        match *self {
            CaptureEvent::Button { at, .. } | CaptureEvent::Motion { at, .. } => at,
        }
    }
}

/// 「拦截系统默认行为」候选键(用户 2026-10-07 要求):键码 → 位。
///
/// 规则:**只有**这几个"有系统默认键位功能"的键会被拦截,而且只有当映射开启
/// (enabled)且当前配置里确实绑定了它时才拦 —— 按下时钩子吞噬事件,前台程序
/// 再也收不到(Esc 不再退出全屏、右键不再弹菜单),映射动作照常走我们自己的
/// 管线送进手机。
///
/// 为什么只选这几个:它们是纯"系统命令"键,被映射后几乎必然是想要"抢过来"的;
/// 而 Space/Tab/Enter/字母数字是输入键,映射开启时用户仍可能需要打字(聊天框),
/// 一刀切拦截会制造新问题。四个滚轮方向只在被绑定了滚轮键、或 FPS 滚轮缩放
/// 生效时才拦(FPS 缩放由引擎状态决定,界面每帧据此更新掩码)。
///
/// 2026-10-10(用户第 2 条"继续补全滚轮逻辑"):掩码由 `u8` 加宽为 `u16`,
/// 给横向滚轮 279/280 留位 —— 捕获层两侧本来就在发这两个码,以前没有位可置,
/// "滚轮左滚/右滚"于是永远拦不住系统的横向滚动。
///
/// 效率:钩子回调里只有"查一次位表 + 读一次原子掩码"(见 `Capture::swallow`),
/// 掩码在界面线程按配置预先算好,回调不做任何锁操作。
pub fn swallow_bit(code: u16) -> Option<u16> {
    match code {
        // BTN_LEFT(272):左键本身**没有**"系统默认行为",所以一直没进这张表 —— 但它是
        // 隔膜最需要吞的一个:绑了开火却把左键放行给 scrcpy,就等于双份输入(投屏窗口
        // 自己也会点一下)。放进表里之后,界面按"有没有被绑定"决定置不置位,于是
        // **绑定才吞、没绑定放行** —— 正是用户 2026-10-11 第 4 条要的语义。
        272 => Some(1 << 10),
        1 => Some(1 << 0),   // KEY_ESC
        87 => Some(1 << 1),  // KEY_F11(全屏)
        273 => Some(1 << 2), // BTN_RIGHT(右键菜单)
        274 => Some(1 << 3), // BTN_MIDDLE(自动滚屏)
        275 => Some(1 << 4), // BTN_SIDE(后退)
        276 => Some(1 << 5), // BTN_EXTRA(前进)
        277 => Some(1 << 6), // BTN_WHEEL_UP(系统滚动)
        278 => Some(1 << 7), // BTN_WHEEL_DOWN(系统滚动)
        279 => Some(1 << 8), // BTN_WHEEL_LEFT(系统横向滚动)
        280 => Some(1 << 9), // BTN_WHEEL_RIGHT(系统横向滚动)
        _ => None,
    }
}

/// 低级鼠标钩子(`WH_MOUSE_LL`)会投递的鼠标消息码(取 Win32 `WM_*` 的数值)。
///
/// 为什么在这里写数值字面量、而不是引 `windows_sys` 的同名常量:
/// [`mouse_shield_swallows`] 与 [`swallow_bit`] 同类,是**平台无关的纯判定** ——
/// Linux 侧也要能编译、也要能单测(那边鼠标走 evdev,根本用不到它)。
/// 这些值是 Win32 ABI 的一部分,不会变;真要改,只可能是微软发新消息,
/// 那时补进 [`MOUSE_HOOK_MSG_CODES`] 即可。
///
/// `cfg_attr(not(windows), allow(dead_code))`:Linux 构建里这一族常量只有单测
/// 用得到(生产路径走 evdev),不加这句 `cargo check --target x86_64-unknown-linux-gnu`
/// 会报 15 条 `never used`。**只压制非 Windows 构建**,Windows 上仍然照常报警。
#[cfg_attr(not(windows), allow(dead_code))]
mod wm {
    pub const MOUSEMOVE: u32 = 0x0200;
    pub const LBUTTONDOWN: u32 = 0x0201;
    pub const LBUTTONUP: u32 = 0x0202;
    pub const LBUTTONDBLCLK: u32 = 0x0203;
    pub const RBUTTONDOWN: u32 = 0x0204;
    pub const RBUTTONUP: u32 = 0x0205;
    pub const RBUTTONDBLCLK: u32 = 0x0206;
    pub const MBUTTONDOWN: u32 = 0x0207;
    pub const MBUTTONUP: u32 = 0x0208;
    pub const MBUTTONDBLCLK: u32 = 0x0209;
    pub const MOUSEWHEEL: u32 = 0x020A;
    pub const XBUTTONDOWN: u32 = 0x020B;
    pub const XBUTTONUP: u32 = 0x020C;
    pub const XBUTTONDBLCLK: u32 = 0x020D;
    pub const MOUSEHWHEEL: u32 = 0x020E;
}

/// 「FPS 鼠标隔膜」覆盖的**全部**消息码 —— 清单本身就是覆盖面,单测逐条断言。
///
/// 前 15 条是 MSDN 为 `LowLevelMouseProc` 列举的客户区消息(移动、左/右/中/X
/// 键的按下抬起与双击、横竖滚轮);后 13 条是非客户区(`WM_NC*`)变体,
/// 光标压在标题栏/边框上时系统递的就是它们 —— 漏掉就会在窗口边框上"漏点"。
#[cfg_attr(not(windows), allow(dead_code))]
const MOUSE_HOOK_MSG_CODES: [u32; 28] = [
    wm::MOUSEMOVE,
    wm::LBUTTONDOWN,
    wm::LBUTTONUP,
    wm::LBUTTONDBLCLK,
    wm::RBUTTONDOWN,
    wm::RBUTTONUP,
    wm::RBUTTONDBLCLK,
    wm::MBUTTONDOWN,
    wm::MBUTTONUP,
    wm::MBUTTONDBLCLK,
    wm::MOUSEWHEEL,
    wm::XBUTTONDOWN,
    wm::XBUTTONUP,
    wm::XBUTTONDBLCLK,
    wm::MOUSEHWHEEL,
    0x00A0, // WM_NCMOUSEMOVE
    0x00A1, // WM_NCLBUTTONDOWN
    0x00A2, // WM_NCLBUTTONUP
    0x00A3, // WM_NCLBUTTONDBLCLK
    0x00A4, // WM_NCRBUTTONDOWN
    0x00A5, // WM_NCRBUTTONUP
    0x00A6, // WM_NCRBUTTONDBLCLK
    0x00A7, // WM_NCMBUTTONDOWN
    0x00A8, // WM_NCMBUTTONUP
    0x00A9, // WM_NCMBUTTONDBLCLK
    0x00AB, // WM_NCXBUTTONDOWN
    0x00AC, // WM_NCXBUTTONUP
    0x00AD, // WM_NCXBUTTONDBLCLK
];

/// 「FPS 鼠标隔膜」判定(纯函数,便于单测):开关打开时,这条鼠标消息要不要
/// 从系统输入流里吞掉(钩子回调 `return 1`)。
///
/// 背景(用户 2026-10-11 根因定位):投屏时鼠标事件被**双份消费** —— 我们的
/// 低级钩子读它、映射成手机触摸;scrcpy 窗口也照样收到它,自己又往手机发一套。
/// 于是点击有反馈、快速甩动时两套触摸打架(抖动/跳变)。用户用 Notepad 盖住
/// 投屏窗口做蒙版后"极其流畅",反证了这条根因。蒙版要干的事就是我们这里
/// 干的事:让鼠标消息**不投递给任何窗口**。
///
/// 语义(用户 2026-10-11 拍板,**2026-10-11 晚扩为可选范围 S①**):**FPS 功能启用时
/// 自动加隔膜,其他模式鼠标照常点投屏**;现在"什么时候独占"由
/// `Aim::mouse_shield` 三档选择(仅 FPS / 仅投屏窗口在前台 / 始终),
/// 判据是纯函数 [`crate::keymap::mouse_shield_on`]。这里仍然只认一个 bool ——
/// 钩子回调不做任何判定,开关由界面每帧重算(见 `app.rs` 的 `mouse_shield_flag`),
/// 翻转下一帧即生效(**不需要重启 scrcpy**)。
///
/// 覆盖面只认 [`MOUSE_HOOK_MSG_CODES`]:关掉时对**任何**消息都是 false(`false &&
/// ...` 短路),也就是说"隔膜没开时的行为逐字不变"。
///
/// 本次**只做鼠标**:键盘那半边 scrcpy 同样会把按键转发给手机(双重输入),
/// 但"把已绑定的键全部吞掉"会让 FPS 里彻底没法打字(聊天框/其它程序都收不到),
/// 与现有"映射开着也能打字"的设计意图正面冲突(《Windows-grab可行性-2026-10-10.md》
/// §4 也把它列为"要做成开关"的路线)。用户这次只点了鼠标,故不动键盘 ——
/// `keyboard_proc` 照旧只吞 8 个候选键。
///
/// 顺带(用户 2026-10-11 第二问):**键盘事件始终是"先转发后吞"**,所以
/// 「鼠标独占」的手动开关热键(`Aim::shield_key`)不受隔膜影响 —— 隔膜开着时,
/// 用户仍然能按那个键把它关掉。
///
/// 边界(知会,不实现):低级钩子吞的只是**投递给窗口的 legacy 鼠标消息**。
/// scrcpy 默认 `--mouse=sdk` 走窗口消息,能被吞掉;若它跑在相对鼠标模式
/// (SDL 相对模式在 Windows 上读 Raw Input / `WM_INPUT`),那条路**任何低级钩子
/// 都拦不住** —— 只有注册者自己能用 `RIDEV_NOLEGACY` 不再转 legacy 消息。
/// 真遇到这种配置,后备是让 scrcpy 用 `--mouse=disabled` 或给它加壳
/// (同文档 §3.6 / §5 的"阻止别的进程 Raw Input"一栏),本次**不做**。
/// 「鼠标独占」这一条消息该不该吞(用户 2026-10-11 第 4 条)。
///
/// 判据(顺序即优先级):
/// 1. 隔膜关着 → 一律不吞;
/// 2. 不是鼠标消息 → 不吞(键盘那半边由 `keyboard_proc` 按绑定表决定,与本函数无关);
/// 3. **鼠标按键/滚轮消息 → 只看它有没有被绑定**(`swallow` 掩码 = 界面按当前配置
///    预计算的"有系统默认功能且被绑定"位表)。没绑定的一律**直接转发原生事件给
///    scrcpy** —— 右键=返回、中键=主页、侧键=前进/后退、滚轮上下这些 scrcpy 自带的
///    功能因此全部保留;
/// 4. 其余(移动等)在隔膜期间照旧吞:位移是"双份输入"那条根因的要害,放行等于让
///    投屏窗口跟着一起拖动。
#[cfg_attr(not(windows), allow(dead_code))]
pub fn mouse_shield_swallows(shield_on: bool, msg: u32, swallow: u16) -> bool {
    if !shield_on || !MOUSE_HOOK_MSG_CODES.contains(&msg) {
        return false;
    }
    match mouse_button_bits(msg) {
        Some(bits) => swallow & bits != 0,
        None => true,
    }
}

// ============================================================================
// B21-3 / G-c:用 `ClipCursor` 把光标**物理钉住**(Windows,FPS 瞄准期间)
// ============================================================================
//
// 为什么需要:Raw Input 的位移**不来自光标位置**(W1-2),所以"光标满屏乱跑"
// 对**位移精度**本身没有影响。但它有两件真实代价:① 光标跑到别的窗口/显示器上
// 之后,钩子发出去的点击就落到那个窗口上(点一下就把焦点切走);② 用户眼睁睁
// 看着光标乱飞,没底。`ClipCursor` 让**系统**把光标关在一个极小的矩形里,
// 从源头不让它跑出去 —— 与既有的"回中"路径(`RAW_RECENTER_INTERVAL`)并存,
// **不取代**它(见消费线程里那段回中的注释)。
//
// 这里放三个纯函数(单测见 `clip_cursor_tests`):
//   * [`clip_rect_for`]   — 光标位置 → 裁剪矩形;
//   * [`should_clip`]     — 抓取 / 消隐 / 开关 → 此刻该不该钉住(真值表);
//   * [`clip_step`]       — 状态机:该钉 → 施加,该放 → 释放(唯一决策点)。
//
// **真正调用 `ClipCursor` 的地方在 Windows 消费线程里**
// (`windows::clip_cursor_to` / `windows::clip_cursor_release`):它是 Win32
// **进程级全局**状态,测试里造不出来(同一进程跑测试会真把跑测试这台机器的光标
// 钉住)。所以这里只测**决策**,不测副作用 —— 如实登记,不假装有覆盖。

/// 裁剪矩形边长(像素)。
///
/// MSDN 明确允许把一个**极小**的矩形交给 `ClipCursor`(光标被关在这个矩形内)。
/// 取 1 而不是 0:零面积矩形在部分 DPI/显示器配置下的行为未定义;1x1 与用户的
/// 目标("光标别动")等价,又是文档里被明确提到的用法。
pub(crate) const CLIP_RECT_SIDE: i32 = 1;

/// 光标位置 → 裁剪矩形 `(left, top, right, bottom)`(Win32 `RECT` 语义:右下开区间)。
///
/// **故意不做边界钳制** —— 这不是疏漏,是取舍:
/// ① 屏幕左上角那种点(如 `(0,0)`)如果钳到 `[0, CLIP_RECT_SIDE]`,`right` 就
///    等于 `left`,成了零宽度矩形,反而可能被系统当成"面积为零"拒掉;
/// ② 负坐标(多显示器,副屏在主屏左边/上面)钳到 0 会把光标**拽到别的屏幕**上,
///    比不钉更糟(用户会看到光标莫名其妙跳一下)。
/// 所以:给什么点就钉在什么点。越界的矩形由系统按"该点所在显示器"处理 ——
/// 那是 Windows 的既有行为。本批明确**禁止手搓屏幕坐标**:布局(多屏/DPI/旋转)
/// 只有系统知道,调用方拿到的点必须来自 `GetCursorPos`。
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn clip_rect_for(pt: (i32, i32)) -> (i32, i32, i32, i32) {
    (pt.0, pt.1, pt.0 + CLIP_RECT_SIDE, pt.1 + CLIP_RECT_SIDE)
}

/// 此刻该不该把光标钉住。
///
/// 三个输入里**只有"抓取"能触发**,这是本函数存在的全部意义:
/// * `setting`(`Settings::clip_cursor_in_aim`,默认开)= 用户开关;
/// * `grabbing` = 既有的 `mouse_grab`(engine.rs 写:
///   `enabled && pointer_should_be_hidden`)—— 它才是"此刻真的在瞄准"。本批**不另造判据**;
/// * `cursor_hide` = 独立的「全局指针消隐」(engine.rs 的 `cursor_toggle_key`)。
///   **它单独为真时必须落 false**:用户可能只是想让光标消失,并不是在瞄准;
///   少了这一条,一按消隐键光标就被钉在屏幕上,那是另一个 bug。
///
/// 反例写法(必须被真值表测试挡住):`setting && (grabbing || cursor_hide)`。
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn should_clip(grabbing: bool, cursor_hide: bool, setting: bool) -> bool {
    if !setting {
        return false; // 开关关着:与从前逐位一致,光标一根手指都不动
    }
    match (grabbing, cursor_hide) {
        // ① 抓取中 ⇒ 钉住(同时消隐与否无关:瞄准就是瞄准)
        (true, _) => true,
        // ② **消隐单独为真 ⇒ 不钉** —— 关键的一支,见函数注释
        (false, true) => false,
        // ③ 既没抓取也没消隐 ⇒ 不钉
        (false, false) => false,
    }
}

/// 裁剪状态机的一步:要钉才施加、不要钉才释放。
///
/// 为什么要有它:`ClipCursor` 是**进程级全局**状态,"开始"与"结束"必须成对。
/// 单调的 `if want_clip { pin() }` 会在抓取退出后把光标**永久钉住**;
/// 把它做成一个纯函数,那个翻转就能在单测里逐条钉死(见 `clip_cursor_tests`
/// 的 `leaving_grab_always_releases_the_clip`),而不是只能靠真机发现。
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClipAction {
    /// 现在施加裁剪(光标还在外面)
    Apply,
    /// 现在释放裁剪(光标要还给用户)
    Release,
    /// 状态没变,什么都不做(`ClipCursor` 是全局调用,能不打就不打)
    Nothing,
}

/// `(当前是否已钉住, 此刻该不该钉住)` → 下一步动作。**施加与释放共用同一个决策**。
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn clip_step(applied: bool, want: bool) -> ClipAction {
    match (applied, want) {
        (false, true) => ClipAction::Apply,
        (true, false) => ClipAction::Release,
        _ => ClipAction::Nothing,
    }
}

/// 鼠标**按键/滚轮**类消息 → 它涉及的吞掉位(或位组)。移动类消息返回 `None`。
///
/// 双击与按下抬起归同一个键(系统把它们都当"那个键的动作");客户区与非客户区
/// 两套码各写一份,因为鼠标移到窗口边框上时走的是 `WM_NC*` 那一套。
///
/// 两个已知的近似:①**侧键 1/2 分不开** —— 具体是哪一个在 `mouseData` 的高位里,
/// 而这里只拿得到 `wparam`;于是"任一被绑定就吞"(绑了一个就放行另一个的情况极少,
/// 真要区分得把 `lparam` 传进来,不值得为此改钩子签名);②滚轮上下同理,按"竖滚
/// 这一对""横滚这一对"取位组 —— 界面本来也是成对设的(缩放在生效时同时吞上下)。
#[cfg_attr(not(windows), allow(dead_code))]
fn mouse_button_bits(msg: u32) -> Option<u16> {
    use crate::keymap::{BTN_WHEEL_DOWN, BTN_WHEEL_LEFT, BTN_WHEEL_RIGHT, BTN_WHEEL_UP};
    let group = |codes: &[u16]| -> u16 {
        codes
            .iter()
            .filter_map(|code| swallow_bit(*code))
            .fold(0u16, |acc, bit| acc | bit)
    };
    match msg {
        // 左键
        0x0201 | 0x0202 | 0x0203 | 0x00A1 | 0x00A2 | 0x00A3 => Some(group(&[272])),
        // 右键
        0x0204 | 0x0205 | 0x0206 | 0x00A4 | 0x00A5 | 0x00A6 => Some(group(&[273])),
        // 中键
        0x0207 | 0x0208 | 0x0209 | 0x00A7 | 0x00A8 | 0x00A9 => Some(group(&[274])),
        // 侧键 1/2
        0x020B | 0x020C | 0x020D | 0x00AB | 0x00AC | 0x00AD => Some(group(&[275, 276])),
        // 竖滚 / 横滚
        0x020A => Some(group(&[BTN_WHEEL_UP, BTN_WHEEL_DOWN])),
        0x020E => Some(group(&[BTN_WHEEL_LEFT, BTN_WHEEL_RIGHT])),
        _ => None,
    }
}

/// 「已绑定键不再传给其它程序」的 256 位 vk 位图(B21-2/G-b,独占模式)。
///
/// 与 swallow_bit 的固定 10 键候选表不同:这里放的是**当前配置里全部被绑定
/// 的键**对应的每一个 Windows 虚拟键码 —— 一个 evdev 码可能有多个 vk 别名
/// (NumLock 开/关时小键盘报不同的 vk,见 vktable 的 vks_for_evdev)。
///
/// 两个线程读写,全部 Relaxed:
/// - 界面线程每帧**全量重算**后逐 word store_words(见 app.rs 的
///   bound_keys_flag):刻意**不**先 clear 再置 —— clear 会让钩子回调恰好
///   在读的瞬间"什么都不吞";逐 word store 短暂不一致,最多少吞/多吞一帧,
///   无害(与任务勘验结论一致);
/// - 钩子回调只读一次(contains_vk),不做锁、不做遍历。
///
/// 为什么用 4×AtomicU64 而不是 [AtomicBool; 256]:回调热路径上一次位测试
/// (load + shift + and)即可,而且"全量重算 + 逐 word 换入"天然按字对齐。
#[derive(Debug)]
pub struct BoundKeys {
    words: [AtomicU64; 4],
}

impl Default for BoundKeys {
    fn default() -> Self {
        Self {
            words: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

impl BoundKeys {
    pub fn new() -> Self {
        Self::default()
    }

    /// 置一个 vk 的位(位号按低 8 位取模,与 contains_vk 同口径)。
    /// 生产路径走 store_words 全量换入;本方法给测试与将来的增量路径用。
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn set_vk(&self, vk: u16) {
        let i = vk as usize & 0xFF;
        self.words[i / 64].fetch_or(1u64 << (i % 64), Ordering::Relaxed);
    }

    /// 这个 vk 此刻在不在位图里
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn contains_vk(&self, vk: u16) -> bool {
        let i = vk as usize & 0xFF;
        self.words[i / 64].load(Ordering::Relaxed) & (1u64 << (i % 64)) != 0
    }

    /// 清空(测试与"捕获层未启动"路径用;生产每帧走 store_words 全量重算)
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn clear(&self) {
        for w in &self.words {
            w.store(0, Ordering::Relaxed);
        }
    }

    /// 全量换入一组预计算好的 word(界面线程每帧调用;见结构体注释)
    pub fn store_words(&self, words: [u64; 4]) {
        for (dst, src) in self.words.iter().zip(words) {
            dst.store(src, Ordering::Relaxed);
        }
    }
}

/// 独占模式下这条键盘事件该不该吞(纯函数,便于表驱动单测;B21-2/G-b)。
///
/// - enabled 关(默认)⇒ 恒 false,**与从前逐位一致**(一个键都不多吞);
/// - 按下与抬起**同判**(_pressed 刻意不参与判定):只吞一半会让前台收到
///   孤儿抬起 —— 与候选键吞掉分支(keyboard_proc 里 swallow_bit 那段)
///   同一条纪律;参数保留是为了让"两态同判"成为被单测钉住的显式契约。
///
/// 边界(原理性,知会不实现):吞的只是 legacy 消息层的投递,拦不住通过
/// Raw Input 读输入的程序。
#[cfg_attr(not(windows), allow(dead_code))]
pub fn keyboard_swallows(enabled: bool, _pressed: bool, vk: u16, bound: &BoundKeys) -> bool {
    enabled && bound.contains_vk(vk)
}

/// 一组"已绑定的 evdev 键码(带 fps_only 标志)"→ 256 位 vk 位图的 4 个 word。
///
/// - fps_only 的绑定在非 FPS 态**不进位图**(与 app.rs 每帧算 swallow 掩码的
///   continue 同口径:那条绑定此刻并不生效,不该占着吞键位);
/// - 一个 evdev 码可能有**多个** vk(NumLock 别名,见 vktable 的
///   vks_for_evdev),全部置位 —— 漏置一个,NumLock 状态一变就"说吞没吞";
/// - 组合键(combo)里的键**本期不进位图**:组合键的"生效中"语义复杂
///   (部分按下算不算绑定生效),如实留待后续批次(G-b 范围外)。
#[cfg(any(windows, test))]
pub fn bound_key_words(keys: impl Iterator<Item = (u16, bool)>, fps_active: bool) -> [u64; 4] {
    let mut w = [0u64; 4];
    for (code, fps_only) in keys {
        if fps_only && !fps_active {
            continue;
        }
        for vk in vktable::vks_for_evdev(code) {
            let i = vk as usize & 0xFF;
            w[i / 64] |= 1u64 << (i % 64);
        }
    }
    w
}

/// 非 Windows 且非测试构建:没有 vk 概念(那边的独占走 EVIOCGRAB 整把抓取),
/// 恒空位图 —— 保持同形接口,界面两侧都能编译。
#[cfg(all(not(windows), not(test)))]
pub fn bound_key_words(_keys: impl Iterator<Item = (u16, bool)>, _fps_active: bool) -> [u64; 4] {
    [0; 4]
}
/// 前台窗口所属进程的 pid(Windows 专有;拿不到一律 `None`)。
///
/// 用途(S①):判定"投屏窗口(scrcpy)是不是在前台" —— 拿它跟
/// [`crate::adb::launch_scrcpy`] 保留的 scrcpy 子进程 pid 比较。
/// 为什么用 pid 而不是窗口标题:标题会被用户的 `--window-title` 参数改掉,
/// pid 不会,而且"我们的子进程"这件事本身就是唯一无歧义的判据。
///
/// 拿不到时(没有前台窗口 / 非 Windows / 拿不到 scrcpy 的 pid)调用方必须
/// **保守地不独占**(见 [`crate::keymap::mouse_shield_on`])。
#[cfg(windows)]
pub fn foreground_pid() -> Option<u32> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId,
    };
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        (pid != 0).then_some(pid)
    }
}

/// 非 Windows:没有"前台窗口"这个概念(那边的鼠标独占走 `EVIOCGRAB`,
/// 是设备级抓取,与窗口无关),恒 `None`。
#[cfg(not(windows))]
pub fn foreground_pid() -> Option<u32> {
    None
}

/// **只读探针**(平台无关入口):Raw Input 通道的此刻状态,给悬浮窗显示用。
///
/// 返回 `(raw_state, WM_INPUT 累计条数, 距最近一条 WM_INPUT 的毫秒数)`。
/// 不写任何状态、不参与任何判定;捕获层未启动时 `(0, 0, 0)`。
/// 非 Windows 没有 Raw Input 这条通道(位移走 evdev),恒 `(0, 0, 0)` ——
/// 保持同形接口,界面两侧都能编译。
pub fn raw_probe() -> (u8, u64, u64) {
    #[cfg(windows)]
    {
        windows::raw_probe()
    }
    #[cfg(not(windows))]
    {
        (0, 0, 0)
    }
}

/// B21-2/G-b:独占模式(已绑定键吞掉)的位图与判定的表驱动单测。
/// 变异验证(同会话):删掉 `keyboard_swallows` 的 enabled=false 早退 ⇒
/// `disabled_switch_never_swallows_any_vk` 必须红;删掉 `bound_key_words`
/// 的 fps_only continue ⇒ `fps_only_binds_stay_out_of_the_bitmap_unless_fps_active` 必须红。
#[cfg(test)]
mod bound_keys_tests {
    use super::{BoundKeys, bound_key_words, keyboard_swallows};

    /// T1:表驱动 —— 开关关(默认)时对**每一个** vk 都不吞,按下/抬起同判;
    /// 开关开时**只有 set 过的 vk** 才吞,其余 256 位一个不多。
    #[test]
    fn disabled_switch_never_swallows_and_enabled_swallows_only_set_vks() {
        let bound = BoundKeys::new();
        // 挑几个有代表性的键置位:A / W / S / D / Esc / F12 / VK_CLEAR / 0xFF
        for vk in [0x41u16, 0x57, 0x53, 0x44, 0x1B, 0x7B, 0x0C, 0xFF] {
            bound.set_vk(vk);
        }
        for vk in 0u16..=255 {
            for pressed in [true, false] {
                // 开关关(默认):一律不吞 —— 与从前逐位一致
                assert!(
                    !keyboard_swallows(false, pressed, vk, &bound),
                    "开关关闭时 vk {vk:#04x}(pressed={pressed})不许被吞"
                );
                // 开关开:仅 set 过的 vk 吞,且按下/抬起两态同判
                let set = matches!(vk, 0x41 | 0x57 | 0x53 | 0x44 | 0x1B | 0x7B | 0x0C | 0xFF);
                assert_eq!(
                    keyboard_swallows(true, pressed, vk, &bound),
                    set,
                    "开关开启时 vk {vk:#04x}(pressed={pressed})判定错误"
                );
            }
        }
        // clear 之后:开着开关也一个不吞
        bound.clear();
        for vk in 0u16..=255 {
            assert!(!keyboard_swallows(true, true, vk, &bound));
            assert!(!keyboard_swallows(true, false, vk, &bound));
        }
    }

    /// T2:NumLock 双 vk 特例 —— KEY_KP5(evdev 76;任务勘验里写的 82 是笔误,
    /// 以 VK_TABLE 为准:0x0C 与 0x65 都映射到 76,见既有测试
    /// `vks_for_evdev(76) == [0x0C, 0x65]`)必须把**两个 vk 都置进位图** ——
    /// 漏一个,NumLock 状态一变就"说吞没吞"。
    #[test]
    fn kp5_sets_both_numlock_alias_vks() {
        let words = bound_key_words(std::iter::once((76u16, false)), false);
        let bound = BoundKeys::new();
        bound.store_words(words);
        assert!(bound.contains_vk(0x0C), "VK_CLEAR 必须在位图里");
        assert!(bound.contains_vk(0x65), "VK_NUMPAD5 必须在位图里");
        // 对照:没绑的普通键不在
        assert!(!bound.contains_vk(0x41), "没绑 A 就不许置 A 的位");
    }

    /// M2 的常驻断言:fps_only 的绑定在非 FPS 态**不进位图**(那条绑定此刻
    /// 并不生效);FPS 态才进。
    #[test]
    fn fps_only_binds_stay_out_of_the_bitmap_unless_fps_active() {
        // F8(87)= 默认总开关键,这里只当样本用
        let off = bound_key_words(std::iter::once((87u16, true)), false);
        let on = bound_key_words(std::iter::once((87u16, true)), true);
        assert_eq!(off, [0; 4], "非 FPS 态:fps_only 绑定不许占吞键位");
        assert_ne!(on, [0; 4], "FPS 态:fps_only 绑定必须进位图");
        // 普通绑定不受 fps_active 影响
        let plain_off = bound_key_words(std::iter::once((87u16, false)), false);
        let plain_on = bound_key_words(std::iter::once((87u16, false)), true);
        assert_eq!(plain_off, plain_on, "普通绑定与 FPS 态无关");
    }

    /// store_words 全量换入 / set_vk / clear 的口径一致(位号跨 64 位边界)
    #[test]
    fn bitmap_words_cover_the_full_vk_range() {
        let bound = BoundKeys::new();
        for vk in [0u16, 63, 64, 127, 128, 200, 255] {
            bound.set_vk(vk);
            assert!(bound.contains_vk(vk), "vk {vk} 置位后必须查得到");
        }
        bound.clear();
        for vk in [0u16, 63, 64, 127, 128, 200, 255] {
            assert!(!bound.contains_vk(vk), "clear 后 vk {vk} 必须查不到");
        }
    }
}

#[cfg(test)]
mod swallow_tests {
    use super::{MOUSE_HOOK_MSG_CODES, mouse_shield_swallows, swallow_bit};

    /// 候选表的位必须两两不同,且覆盖用户点名的 Esc / 右键。
    /// 候选键(含四个滚轮方向)两两占不同的位,且位置与文档一致。
    #[test]
    fn swallow_bits_are_distinct_and_cover_named_keys() {
        let candidates = [1u16, 87, 273, 274, 275, 276, 277, 278, 279, 280];
        let mut seen = 0u16;
        for c in candidates {
            let bit = swallow_bit(c).expect("候选键必须有位");
            assert_eq!(seen & bit, 0, "位重复: {c}");
            seen |= bit;
        }
        // 每个候选键恰好占一位
        assert_eq!(seen.count_ones() as usize, candidates.len());
        assert_eq!(swallow_bit(1), Some(1), "Esc 在第 0 位");
        assert_eq!(swallow_bit(273), Some(1 << 2), "右键在第 2 位");
        assert_eq!(swallow_bit(279), Some(1 << 8), "左滚在第 8 位");
        assert_eq!(swallow_bit(280), Some(1 << 9), "右滚在第 9 位");
    }

    /// 非候选键(字母/空格/回车/Tab/Shift 等输入键)永远不拦 ——
    /// 映射开启时用户仍可能需要打字(聊天框),一刀切会制造新问题。
    #[test]
    fn input_keys_are_never_swallowed() {
        for c in [
            30u16, /*A*/
            57,    /*Space*/
            28,    /*Enter*/
            15,    /*Tab*/
            14,    /*Backspace*/
            42,    /*LShift*/
        ] {
            assert_eq!(swallow_bit(c), None, "输入键 {c} 不允许拦截");
        }
    }

    /// 「FPS 鼠标隔膜」的两半语义:
    /// - 关:对**任何**消息都不吞(非 FPS 模式鼠标照常点投屏,行为逐字不变);
    /// - 开:移动/左/右/中/X 键的按下抬起双击 + 横竖滚轮(含非客户区变体)一条不漏。
    #[test]
    fn mouse_shield_off_never_swallows_and_on_covers_every_mouse_message() {
        // 关:清单里的鼠标消息一条都不吞
        for m in MOUSE_HOOK_MSG_CODES {
            assert!(
                !mouse_shield_swallows(false, m, u16::MAX),
                "隔膜关闭时不许吞鼠标消息 {m:#06x}"
            );
        }
        // 关:连"不是鼠标消息"的那些也不吞(下面这组是本测试自己挑的对照样本)
        for m in [
            0u32, 1, 0x0100, /*WM_KEYDOWN*/
            0x0101, 0x0010, /*WM_CLOSE*/
            0xFFFF,
        ] {
            assert!(
                !mouse_shield_swallows(false, m, u16::MAX),
                "隔膜关闭时 {m:#06x} 不能吞"
            );
        }

        // 开:清单逐条必须被吞(清单就是覆盖面,新增消息要同步补清单+这里)
        for m in MOUSE_HOOK_MSG_CODES {
            assert!(
                mouse_shield_swallows(true, m, u16::MAX),
                "隔膜开启时 {m:#06x} 必须被吞"
            );
        }
        // 开:用户点名的六类逐条点名断言,避免将来有人"顺手改小清单"却仍然全绿
        for (m, name) in [
            (0x0200u32, "移动 WM_MOUSEMOVE"),
            (0x0201, "左键按下"),
            (0x0202, "左键抬起"),
            (0x0203, "左键双击"),
            (0x0204, "右键按下"),
            (0x0205, "右键抬起"),
            (0x0206, "右键双击"),
            (0x0207, "中键按下"),
            (0x0208, "中键抬起"),
            (0x0209, "中键双击"),
            (0x020A, "竖滚 WM_MOUSEWHEEL"),
            (0x020B, "侧键1按下 WM_XBUTTONDOWN"),
            (0x020C, "侧键1抬起"),
            (0x020D, "侧键双击"),
            (0x020E, "横滚 WM_MOUSEHWHEEL"),
        ] {
            assert!(
                mouse_shield_swallows(true, m, u16::MAX),
                "{name}({m:#06x})必须被吞"
            );
        }

        // 开:清单之外的消息不受影响 —— 隔膜只对鼠标消息生效(键盘那半边用户没要求,
        // 我们也不做:见 `keyboard_proc`,那里只吞被绑定的候选键)
        for m in [
            0u32, 0x0100, /*WM_KEYDOWN*/
            0x0104, /*WM_SYSKEYDOWN*/
            0x0010, /*WM_CLOSE*/
        ] {
            assert!(
                !mouse_shield_swallows(true, m, u16::MAX),
                "{m:#06x} 不是鼠标消息,不该被吞"
            );
        }
    }

    /// 第 4 条(用户 2026-10-11):**未绑定的鼠标键直接转发原生事件给 scrcpy**。
    ///
    /// 以前是"隔膜开着就把所有鼠标消息吞掉",于是没绑定的右键/中键/侧键/滚轮也到不了
    /// scrcpy,它自带的那套(scrcpy 默认右键=返回、中键=主页、滚轮=滚动)全没了。
    /// 现在只看绑定掩码:绑定过的键才吞,没绑定的一律放行。
    #[test]
    fn shield_only_swallows_bound_mouse_buttons() {
        use crate::keymap::{BTN_WHEEL_DOWN, BTN_WHEEL_UP};
        // 只绑了右键:右键类消息吞,左键/中键/侧键一律放行
        let only_right = swallow_bit(273).unwrap();
        for m in [0x0204u32, 0x0205, 0x0206, 0x00A4, 0x00A5, 0x00A6] {
            assert!(
                mouse_shield_swallows(true, m, only_right),
                "右键 {m:#06x} 该吞"
            );
        }
        for m in [
            0x0201u32, 0x0202, 0x0203, // 左键
            0x0207, 0x0208, 0x0209, // 中键
            0x020B, 0x020C, 0x020D, // 侧键
            0x00A1, 0x00A7, 0x00AB, // 非客户区变体
        ] {
            assert!(
                !mouse_shield_swallows(true, m, only_right),
                "未绑定的按钮 {m:#06x} 必须放行给 scrcpy"
            );
        }
        // 滚轮:缩放生效时界面会把上下两位置进掩码,那时才吞;没置位就放行
        let wheel = swallow_bit(BTN_WHEEL_UP).unwrap() | swallow_bit(BTN_WHEEL_DOWN).unwrap();
        assert!(
            mouse_shield_swallows(true, 0x020A, wheel),
            "缩放生效时竖滚该吞"
        );
        assert!(
            !mouse_shield_swallows(true, 0x020A, only_right),
            "没绑定时竖滚放行"
        );
        // **位移永远吞**:放行等于让投屏窗口跟着一起拖动(双份输入那条根因)
        assert!(mouse_shield_swallows(true, 0x0200, 0), "移动消息必须吞");
        assert!(
            mouse_shield_swallows(true, 0x00A0, 0),
            "非客户区移动也必须吞"
        );
    }
}

/// 位移合并窗口的时长(W1-3)。取 1ms:一次合并最多给位移加 1ms 延迟,
/// 而把 8000Hz 鼠标的消息数压到 ≤1000/s。窗口再大手感会开始"橡皮化"——
/// 这是方案里定的起点,以后调参只动这里。
#[cfg_attr(not(windows), allow(dead_code))]
const MOTION_WINDOW: Duration = Duration::from_millis(1);

/// 位移合并器(W1-3):把窗口期内到达的多条位移合成一条 `CaptureEvent::Motion`。
///
/// 为什么可以合并:`dx/dy` 是**可加量**(Windows 设备计数、evdev 相对轴,都是
/// 整数),时刻取窗口内最后一条 —— 引擎拿到"总位移 + 最后时刻"与逐条送进去
/// 等价。灵敏度缩放发生在引擎侧且是线性的,先加后缩 === 先缩后加,所以方案里
/// 预留的"f64 残差"不需要(整数相加本就不丢精度)。
///
/// 为什么不加配置项:窗口只压缩高回报率设备的**消息数**,不改变任何计算路径;
/// 真正的旋钮(灵敏度/速度)在配置里。等 W0 的 bench 能量化 1000/8000Hz 的
/// 差异之后再决定要不要暴露,免得加一个没人能调对的开关。
///
/// Linux 侧用不到(`device_loop` 本就按 ~2ms 轮询批合并,消息率已 ≤500/s);
/// 为让单元测试两平台都跑,这里用 allow(dead_code) 而不是 cfg(windows)。
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Default)]
struct MotionCoalescer {
    dx: f32,
    dy: f32,
    /// 窗口内最后一条位移的时刻(合并结果的 `at`)
    at: Option<Instant>,
    /// 窗口截止时刻(= 第一条位移的 `at` + `MOTION_WINDOW`)
    deadline: Option<Instant>,
    /// 累计进入窗口的原始事件数(诊断:与 `batches` 一比就知道在不在合并)
    events: u64,
    /// 累计发出的合并结果数(诊断)
    batches: u64,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl MotionCoalescer {
    fn new() -> Self {
        Self::default()
    }

    /// 收进一条位移。窗口从**第一条**的 `at` 起算:若这条的时间已经晚于截止
    /// 时刻(中转线程被卡住后一次醒来收一批),下一次 `due`/`wait_hint` 会立刻
    /// 放行,不在已有的延迟上再叠一层。
    fn push(&mut self, dx: f32, dy: f32, at: Instant) {
        self.dx += dx;
        self.dy += dy;
        self.at = Some(at);
        self.events += 1;
        if self.deadline.is_none() {
            self.deadline = Some(at + MOTION_WINDOW);
        }
    }

    /// 窗口里有没有攒着位移
    fn pending(&self) -> bool {
        self.at.is_some()
    }

    /// 窗口是否已到点
    fn due(&self, now: Instant) -> bool {
        matches!(self.deadline, Some(d) if now >= d)
    }

    /// 给 `recv_timeout` 用的等待时长:有攒着的位移就等到窗口截止,
    /// 否则按调用方给的闲时上限。
    fn wait_hint(&self, now: Instant, idle: Duration) -> Duration {
        match self.deadline {
            Some(d) if self.pending() => d.saturating_duration_since(now),
            _ => idle,
        }
    }

    /// 取走合并结果。两条方向相消的位移(净位移为 0)不发:注进去的位置不会变,
    /// 省掉一次 `touch_move` 与一次引擎派发;诊断计数照记。
    fn take(&mut self) -> Option<(f32, f32, Instant)> {
        let at = self.at.take()?;
        self.deadline = None;
        let (dx, dy) = (std::mem::take(&mut self.dx), std::mem::take(&mut self.dy));
        if dx == 0.0 && dy == 0.0 {
            return None;
        }
        self.batches += 1;
        Some((dx, dy, at))
    }

    /// 到点就发
    fn flush_due(&mut self, tx: &Sender<CaptureEvent>, now: Instant) -> bool {
        if !self.due(now) {
            return false;
        }
        self.flush(tx)
    }

    /// 无条件发。调用点都是"顺序点":来了按键/另一路位移、退出循环、
    /// 周期性检查之前 —— 保证先位移后其它,不让 1ms 的窗口打乱事件先后。
    fn flush(&mut self, tx: &Sender<CaptureEvent>) -> bool {
        match self.take() {
            Some((dx, dy, at)) => {
                let _ = tx.send(CaptureEvent::Motion { dx, dy, at });
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod motion_coalescer_tests {
    use super::*;
    use std::sync::mpsc::channel;

    #[test]
    fn merges_within_window_and_keeps_the_last_time() {
        let mut c = MotionCoalescer::new();
        let t = Instant::now();
        c.push(1.0, 2.0, t);
        c.push(3.0, -1.0, t + Duration::from_micros(300));
        assert!(c.pending());
        assert_eq!(c.take(), Some((4.0, 1.0, t + Duration::from_micros(300))));
        assert!(!c.pending(), "取走后窗口必须清空");
        assert_eq!(c.events, 2);
        assert_eq!(c.batches, 1);
    }

    #[test]
    fn window_runs_from_the_first_event_and_resets_after_take() {
        let mut c = MotionCoalescer::new();
        let t = Instant::now();
        c.push(1.0, 0.0, t);
        assert!(!c.due(t + Duration::from_micros(999)));
        assert!(c.due(t + MOTION_WINDOW));
        let _ = c.take();
        // 第二次开窗从新的一条重新计时,不继承上一条的截止时刻
        let t2 = t + Duration::from_millis(10);
        c.push(1.0, 0.0, t2);
        assert!(!c.due(t2 + Duration::from_micros(999)));
    }

    #[test]
    fn zero_net_motion_is_not_sent_but_is_counted() {
        let (tx, rx) = channel();
        let mut c = MotionCoalescer::new();
        let t = Instant::now();
        c.push(5.0, 0.0, t);
        c.push(-5.0, 0.0, t);
        assert!(!c.flush(&tx), "净位移为 0 不该发");
        assert!(rx.try_recv().is_err());
        assert_eq!(c.events, 2);
        assert_eq!(c.batches, 0);
        assert!(!c.pending());
    }

    #[test]
    fn flush_due_only_fires_at_the_deadline_and_preserves_order() {
        let (tx, rx) = channel();
        let mut c = MotionCoalescer::new();
        let t = Instant::now();
        c.push(1.0, 1.0, t);
        assert!(
            !c.flush_due(&tx, t + Duration::from_micros(500)),
            "未到点不许发"
        );
        assert!(rx.try_recv().is_err());
        assert!(c.flush_due(&tx, t + MOTION_WINDOW));
        match rx.try_recv() {
            Ok(CaptureEvent::Motion { dx, dy, at }) => {
                assert_eq!((dx, dy), (1.0, 1.0));
                assert_eq!(at, t);
            }
            other => panic!("应收到合并后的 Motion,实得 {other:?}"),
        }
    }

    #[test]
    fn wait_hint_is_the_deadline_while_pending_and_idle_otherwise() {
        let mut c = MotionCoalescer::new();
        let idle = Duration::from_millis(50);
        let t = Instant::now();
        assert_eq!(c.wait_hint(t, idle), idle, "空窗口按闲时上限等");
        c.push(1.0, 0.0, t);
        assert_eq!(
            c.wait_hint(t + Duration::from_micros(400), idle),
            MOTION_WINDOW - Duration::from_micros(400)
        );
        // 已经过点的窗口:等待时长为 0(立刻放行),不回绕也不 panic
        assert_eq!(
            c.wait_hint(t + Duration::from_millis(5), idle),
            Duration::ZERO
        );
    }
}

/// 心跳判定的门限(W1-4):3 秒没有心跳/探针的回应就当"掉了"。
/// 与方案一致 —— 1s 节拍、连续 3 次无回音。
#[cfg(any(windows, test))]
pub(crate) const HB_STALL_MS: u64 = 3_000;

/// 捕获层心跳状态(W1-4)。界面按它显示"卡死/被摘"两类告警。
#[cfg(any(windows, test))]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum HbVerdict {
    /// 正常(或还无从判断)
    Ok,
    /// 心跳消息 3 秒没被钩子线程处理:消息泵卡死,输入事件流已停
    PumpStall,
    /// 探针连续 3 次没被自己的钩子看到:钩子已被系统静默摘除
    HookDead,
}

/// 心跳判定(纯函数;判定与重装都在各自线程里做,这里只回答"界面此刻该说什么")。
///
/// - `pump == 0` = 心跳还没跑起来(启动初期/已退出),不判 —— 免得把"刚启动"
///   误报成"卡死"。
/// - `dead_ms` 由钩子线程在触发重装时写下、由**消费侧取走**时才清零 —— 事件式
///   标记,不做"多久没见过探针"的时间比较:探针在路上的几毫秒不会被误判成掉线。
#[cfg(any(windows, test))]
pub(crate) fn hb_verdict(now: u64, pump: u64, dead_ms: u64) -> HbVerdict {
    if pump == 0 {
        return HbVerdict::Ok;
    }
    if now.saturating_sub(pump) >= HB_STALL_MS {
        return HbVerdict::PumpStall;
    }
    if dead_ms != 0 {
        return HbVerdict::HookDead;
    }
    HbVerdict::Ok
}

/// 上一枚探针没被自己的钩子看到?(W1-4;钩子线程逐拍数,连续 3 次就重装)
///
/// **参数语义 = 计数,不是时间戳**(2026-10-12 修 F5,真机稳定复现的误判):
/// 低级钩子的回调会在 `SendInput` **期间**同步/重入地跑在我们的钩子线程上 ——
/// 回调里写"看到了"发生在 `SendInput` **返回之前**,而"发出了"只能在返回
/// **之后**写。用时间戳比较时**每一拍**都 `seen < sent`,于是永远判"无回音":
/// 连续 3 拍重装 → 2 轮后 `GiveUp` → **永久停用钩子自愈**并常驻"请重启"告警
/// (用户机上稳定复现;同形态的手写探测实验 16/16 全看到,正是差在这一步)。
/// 改用计数后:重入时两者在返回后都等于同一代计数(健康);钩子真死时回调不跑,
/// `seen` 会落后于 `sent` —— 判据表达式不变,语义才正确。
#[cfg(any(windows, test))]
pub(crate) fn probe_missed(sent: u64, seen: u64) -> bool {
    sent != 0 && seen < sent
}

/// 连续多少枚探针没回音就重装(1s 一拍)
#[cfg(any(windows, test))]
pub(crate) const HB_MISS_LIMIT: u32 = 3;

/// 探针"重装后仍无回音"的容忍轮数(W1-4 复核 #3):连续这么多轮重装后探针
/// 还是没回音,就当环境(安全软件/注入过滤)把注入事件吞了,停用探针自愈。
/// 取 2:第一次重装本来就能修好"别的钩子排在我们前面"这一类问题,不该误停;
/// 真被吞时也只多绕一轮 —— 每轮重装都会连累好着的鼠标钩子,不能任它循环。
#[cfg(any(windows, test))]
pub(crate) const PROBE_FRUITLESS_LIMIT: u32 = 2;

/// `probe_step` 给出的本拍动作。
#[cfg(any(windows, test))]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum ProbeStep {
    /// 健康 / 还在等回音 / 键盘钩子缺席 —— 什么都不做
    Idle,
    /// 连续 `HB_MISS_LIMIT` 拍没回音:重装钩子
    Reinstall,
    /// 又一轮重装后仍无回音:环境吞注入,停用探针自愈
    GiveUp,
}

/// 探针的逐拍策略(纯函数;2026-10-06 复核后从钩子线程里抽出,便于单测)。
///
/// 返回 `(新 miss, 新 fruitless, 本拍动作)`。
/// 不变量:`expect_kb == false`(键盘钩子没装上)时永远 `Idle` —— 没有钩子
/// 能看见探针,记 miss 纯属误报,重装更是每 3 秒一轮的空转循环(复核 #1b:
/// 自愈重装会把"键盘钩子装没装上"的结果刷新,探针期望必须跟着新结果走)。
#[cfg(any(windows, test))]
pub(crate) fn probe_step(
    miss: u32,
    sent: u64,
    seen: u64,
    expect_kb: bool,
    fruitless: u32,
) -> (u32, u32, ProbeStep) {
    if !expect_kb || !probe_missed(sent, seen) {
        // 上一枚看到了(或压根没发过):健康,连 fruitless 一起清零
        return (0, 0, ProbeStep::Idle);
    }
    let m = miss + 1;
    if m < HB_MISS_LIMIT {
        return (m, fruitless, ProbeStep::Idle);
    }
    let f = fruitless + 1;
    if f >= PROBE_FRUITLESS_LIMIT {
        (0, f, ProbeStep::GiveUp)
    } else {
        (0, f, ProbeStep::Reinstall)
    }
}

/// 钩子线程"上一拍距现在太久"是否该按"钩子出事"处理(纯函数;复核 #4)。
///
/// 只有**不是**消费侧自己卡住(`consumer_gap < HB_STALL_MS`)才算 ——
/// 系统睡眠/被抢 CPU 时两个线程都没有心跳,pump 变旧说明不了钩子线程有事。
#[cfg(any(windows, test))]
pub(crate) fn stall_hit(now: u64, prev: u64, consumer_gap: u64) -> bool {
    prev != 0 && now.saturating_sub(prev) > HB_STALL_MS && consumer_gap < HB_STALL_MS
}

/// 兜底重装判据①:这么久**完全**没有键盘/鼠标钩子事件,才怀疑钩子又没了。
///
/// DF-10(2026-10-11):`probe_off` 之后探针这条路永久停用,残余的 `stall_hit`
/// 只覆盖"消息泵也卡住"那一类 —— 而系统静默摘钩时泵并不卡,于是键盘+鼠标
/// 钩子可以一起失灵到重启。这条判据不依赖探针回音。
#[cfg(any(windows, test))]
pub(crate) const FALLBACK_QUIET_MS: u64 = 5_000;

/// 兜底重装的限流间隔:最多这么久重装一次。
///
/// 与心跳同量级(心跳 1s 一拍)但远大于它 —— 重装会先卸后装两把钩子,期间
/// 好着的鼠标钩子也被连累,不能密。
#[cfg(any(windows, test))]
pub(crate) const FALLBACK_COOLDOWN_MS: u64 = 10_000;

/// 兜底重装的累计次数上限:用完就彻底放弃(界面常驻"失灵请重启")。
///
/// 取 3:真被环境吞掉时再装几次也不会有奇迹;而偶发的一次静默摘钩只需要
/// 一次兜底就够。到了上限还继续每 10 秒卸装一轮,只会让情况更糟。
#[cfg(any(windows, test))]
pub(crate) const FALLBACK_MAX: u32 = 3;

/// 探针自愈停用后的**兜底重装**判定(纯函数;DF-10,2026-10-11,单测锁住)。
///
/// 四个条件全中才放行:
/// - `probe_off`:只有"探针自愈已停用"才走兜底;探针还活着时由它自己管,
///   这条路径不参与(否则两套自愈会互相打架);
/// - ① `now - last_event_ms >= FALLBACK_QUIET_MS`:这么久**一条**钩子事件都
///   没收到(`mark` 没被调用过 —— 键盘按键、鼠标按键、滚轮都算);
/// - ② `key_down`:`GetAsyncKeyState` 说此刻**确实有键按着**。这是"用户在手按
///   而我们收不到"的判据 —— 只有①没有②,那只是"用户就是没按";
/// - ③ 限流:距上次兜底重装 ≥ `FALLBACK_COOLDOWN_MS`,且累计 < `FALLBACK_MAX`。
///
/// 注意 `last_event_ms == 0`(从未有过事件)按 `now` 起算:程序跑了很久还一条
/// 事件都没有、同时又真有键按着,那正是要救的场景。
#[cfg(any(windows, test))]
pub(crate) fn fallback_reinstall_due(
    probe_off: bool,
    now: u64,
    last_event_ms: u64,
    key_down: bool,
    last_fallback_ms: u64,
    fallback_count: u32,
) -> bool {
    if !probe_off || !key_down || fallback_count >= FALLBACK_MAX {
        return false;
    }
    if now.saturating_sub(last_event_ms) < FALLBACK_QUIET_MS {
        return false;
    }
    if last_fallback_ms != 0 && now.saturating_sub(last_fallback_ms) < FALLBACK_COOLDOWN_MS {
        return false;
    }
    true
}

/// 滚轮一"齿"的增量(`WHEEL_DELTA`;Windows 的 `WM_MOUSEWHEEL` 用它 120 的倍数)
#[cfg(any(windows, test))]
pub(crate) const WHEEL_DELTA: i32 = 120;

/// 一次滚轮事件最多补多少次点击 —— 与 Linux 侧一致的护栏
/// (见 `device_loop` 的 `unsigned_abs().min(10)`):异常驱动/宏注入可能一次
/// 送来上百齿,不能变成上百次点按把设备端打爆。
#[cfg(any(windows, test))]
pub(crate) const WHEEL_NOTCH_CAP: i32 = 10;

/// 把滚轮增量累加成"齿数"(纯函数;W1-5)。
///
/// 物理滚轮(尤其高精度/带刻度的)常把一齿拆成多个小 `delta`,而钩子路径
/// 之前只取符号 —— 多齿的一次物理滚动会被压成 1 齿。这里按 `WHEEL_DELTA`
/// 累加:不足一齿的余量留在 `acc` 里等下一枚事件凑;超过一齿的按齿数发出
/// (除法朝零取整,正负对称)。超过上限的齿照常从 `acc` 里扣掉(不攒爆),
/// 但只发出上限次数 —— 与 Linux 侧"丢掉多余"的口径一致。
#[cfg(any(windows, test))]
pub(crate) fn wheel_notches(acc: &mut i32, delta: i32) -> i32 {
    *acc += delta;
    let n = *acc / WHEEL_DELTA;
    *acc -= n * WHEEL_DELTA;
    n.clamp(-WHEEL_NOTCH_CAP, WHEEL_NOTCH_CAP)
}

#[cfg(test)]
mod wheel_tests {
    use super::*;

    #[test]
    fn a_full_notch_is_one_click() {
        let mut a = 0;
        assert_eq!(wheel_notches(&mut a, WHEEL_DELTA), 1);
        assert_eq!(a, 0, "整齿不留余量");
    }

    #[test]
    fn sub_notch_deltas_accumulate_until_a_whole_notch() {
        let mut a = 0;
        assert_eq!(wheel_notches(&mut a, 40), 0, "不足一齿先攒着");
        assert_eq!(wheel_notches(&mut a, 40), 0);
        assert_eq!(wheel_notches(&mut a, 40), 1, "凑满一齿才发");
        assert_eq!(a, 0);
    }

    #[test]
    fn multi_notch_deltas_emit_the_matching_count() {
        let mut a = 0;
        assert_eq!(wheel_notches(&mut a, 3 * WHEEL_DELTA), 3, "一次多齿滚动");
        assert_eq!(wheel_notches(&mut a, -2 * WHEEL_DELTA), -2, "反向对称");
        assert_eq!(a, 0);
    }

    #[test]
    fn opposite_directions_cancel_in_the_accumulator() {
        let mut a = 0;
        assert_eq!(wheel_notches(&mut a, 80), 0);
        assert_eq!(wheel_notches(&mut a, -80), 0, "反向余量互相抵消");
        assert_eq!(a, 0);
    }

    #[test]
    fn absurd_deltas_are_capped_but_still_drained() {
        let mut a = 0;
        assert_eq!(wheel_notches(&mut a, 100 * WHEEL_DELTA), WHEEL_NOTCH_CAP);
        assert_eq!(a, 0, "多余齿从累计里扣掉,不攒爆");
        assert_eq!(wheel_notches(&mut a, -100 * WHEEL_DELTA), -WHEEL_NOTCH_CAP);
        assert_eq!(a, 0);
    }
}

#[cfg(test)]
mod hb_tests {
    use super::*;

    #[test]
    fn verdict_is_ok_before_the_first_heartbeat() {
        assert_eq!(hb_verdict(10_000, 0, 0), HbVerdict::Ok, "还没跑起来不判");
    }

    #[test]
    fn verdict_flags_a_stalled_pump_after_three_seconds() {
        assert_eq!(hb_verdict(3_999, 1_000, 0), HbVerdict::Ok);
        assert_eq!(hb_verdict(4_000, 1_000, 0), HbVerdict::PumpStall);
    }

    #[test]
    fn verdict_prefers_pump_stall_over_hook_dead() {
        assert_eq!(hb_verdict(9_000, 1_000, 5_000), HbVerdict::PumpStall);
    }

    #[test]
    fn verdict_reports_hook_dead_from_the_flag() {
        assert_eq!(hb_verdict(4_000, 3_900, 3_900), HbVerdict::HookDead);
    }

    #[test]
    fn verdict_is_ok_when_the_clock_looks_reversed() {
        // now 比 pump 小(理论上不该发生):saturating 后不 panic、不误报
        assert_eq!(hb_verdict(100, 500, 0), HbVerdict::Ok);
    }

    #[test]
    fn probe_missed_only_counts_a_probe_that_was_sent() {
        assert!(!probe_missed(0, 0), "还没发过探针不算掉线");
        assert!(!probe_missed(1_000, 1_000), "上一枚看到了");
        assert!(probe_missed(1_000, 999), "上一枚没看到");
        assert!(probe_missed(1_000, 0), "从来没看到过");
    }

    #[test]
    fn probe_step_reinstalls_only_after_three_unanswered_beats() {
        assert_eq!(
            probe_step(0, 100, 100, true, 0),
            (0, 0, ProbeStep::Idle),
            "上一枚看到了:健康,累计清零"
        );
        assert_eq!(
            probe_step(2, 100, 100, true, 0),
            (0, 0, ProbeStep::Idle),
            "看到即清零"
        );
        assert_eq!(probe_step(0, 100, 0, true, 0), (1, 0, ProbeStep::Idle));
        assert_eq!(probe_step(1, 100, 0, true, 0), (2, 0, ProbeStep::Idle));
        assert_eq!(
            probe_step(2, 100, 0, true, 0),
            (0, 1, ProbeStep::Reinstall),
            "第 3 拍无回音才重装"
        );
    }

    #[test]
    fn probe_step_never_fires_without_a_keyboard_hook() {
        // 复核 #1b/#3:键盘钩子没装上时探针永远不会有回音 —— 既不能记 miss,
        // 更不能重装,否则就是每 3 秒一轮的钩子重装循环(还连累鼠标钩子)。
        assert_eq!(probe_step(2, 100, 0, false, 1), (0, 0, ProbeStep::Idle));
        assert_eq!(probe_step(2, 0, 0, false, 0), (0, 0, ProbeStep::Idle));
    }

    #[test]
    fn probe_step_gives_up_after_repeated_fruitless_heals() {
        // 复核 #3:第一次重装(还没前科)→ 重装;第二次仍无回音 → 停用探针。
        assert_eq!(probe_step(2, 100, 0, true, 0), (0, 1, ProbeStep::Reinstall));
        assert_eq!(probe_step(2, 100, 0, true, 1), (0, 2, ProbeStep::GiveUp));
    }

    /// **F5 回归**(2026-10-12,真机稳定复现的误判):低级钩子回调会在 `SendInput`
    /// **期间**重入本线程 —— 回调记"看到了"发生在返回之前,而"发出了"只能在返回
    /// 之后记。旧实现用**时间戳**比较,于是**每一拍**都 `seen < sent` ⇒ 连续 3 拍
    /// 重装 ⇒ 2 轮后永久停用钩子自愈(用户机上每次启动都必然发生)。
    ///
    /// 这条测试用**计数**语义钉住修法:重入场景下返回后两者相等(健康),
    /// 只有钩子真的没跑回调时 `seen` 才落后。
    #[test]
    fn probe_miss_is_counted_not_timestamped() {
        // ① 健康:本拍发出第 N 枚,回调(重入)也记到第 N 枚 ⇒ 不判 miss。
        //    旧的时间戳写法在这里会看到 `seen(早) < sent(晚)` 而误判 true。
        assert!(!probe_missed(1, 1), "发出 1 枚、看到 1 枚:健康");
        assert!(!probe_missed(7, 7), "同代计数相等:健康");
        // ② 真死:发出 3 枚、回调一枚都没跑 ⇒ seen 落后 ⇒ 判 miss。
        assert!(probe_missed(3, 0), "一枚都没看到:判 miss");
        assert!(probe_missed(3, 2), "看到 2 枚但发了 3 枚:判 miss");
        // ③ 还没发过:不判(启动初期)。
        assert!(!probe_missed(0, 0), "还没发过探针");
        // ④ 端到端:重入场景(seen 与 sent 同代)连续多拍都不该重装。
        let (miss, fruitless, step) = probe_step(0, 1, 1, true, 0);
        assert_eq!((miss, fruitless, step), (0, 0, ProbeStep::Idle));
        let (miss, fruitless, step) = probe_step(0, 5, 5, true, 0);
        assert_eq!(
            (miss, fruitless, step),
            (0, 0, ProbeStep::Idle),
            "重入场景永远不该走到重装/停用(这就是 F5 的回归点)"
        );
    }

    #[test]
    fn stall_hit_ignores_a_stall_of_the_consumer_itself() {
        // 复核 #4:睡眠/被抢 CPU 时两个线程都没心跳,不能算钩子线程的账
        assert!(
            stall_hit(10_000, 1_000, 50),
            "消费侧正常,那就是钩子线程卡了"
        );
        assert!(
            !stall_hit(10_000, 1_000, 10_000),
            "消费侧自己也卡着:不下结论"
        );
        assert!(!stall_hit(10_000, 0, 50), "还没种过时间戳(启动初期)");
        assert!(!stall_hit(2_000, 1_000, 50), "只差 1 秒:没到门限");
    }

    /// DF-10 兜底重装:四条判据必须全部满足,而且限流真的拦得住。
    ///
    /// 覆盖任务要求的四类:①不满足(还收得到事件)、②不满足(没有键按着)、
    /// 限流拦截、正常放行。
    #[test]
    fn fallback_reinstall_requires_quiet_and_a_held_key() {
        // 正常放行:探针停用 + 8 秒没有事件 + 系统说有键按着 + 从没兜底过
        assert!(fallback_reinstall_due(true, 20_000, 12_000, true, 0, 0));

        // ① 不满足:刚刚才收到过钩子事件(3 秒 < 5 秒门限)
        assert!(!fallback_reinstall_due(true, 20_000, 17_000, true, 0, 0));
        // 门限边界:正好 5 秒算"够久"
        assert!(fallback_reinstall_due(true, 20_000, 15_000, true, 0, 0));

        // ② 不满足:没人按键 —— 那只是用户没在操作,不是钩子死了
        assert!(!fallback_reinstall_due(true, 20_000, 0, false, 0, 0));

        // 探针还活着时不走兜底(那是 `probe_step` 的活,两套自愈不能打架)
        assert!(!fallback_reinstall_due(false, 20_000, 0, true, 0, 0));
    }

    /// DF-10 兜底重装的限流:冷却期内的拍子一律拦掉,冷却过后才放行;
    /// 累计次数到上限后**永久**放弃(和界面"失灵请重启"的红字一致)。
    #[test]
    fn fallback_reinstall_is_rate_limited_and_capped() {
        // 10 秒冷却:上次兜底在 15s、现在 20s(只过 5s)→ 拦
        assert!(!fallback_reinstall_due(true, 20_000, 0, true, 15_000, 1));
        // 冷却边界:正好 10 秒 / 超过 10 秒 → 放行
        assert!(fallback_reinstall_due(true, 25_000, 0, true, 15_000, 1));
        assert!(fallback_reinstall_due(true, 26_000, 0, true, 15_000, 1));

        // 次数上限:到了 `FALLBACK_MAX` 就再也不放行,哪怕冷却早过了
        assert!(fallback_reinstall_due(
            true,
            99_000,
            0,
            true,
            50_000,
            FALLBACK_MAX - 1
        ));
        assert!(!fallback_reinstall_due(
            true,
            99_000,
            0,
            true,
            50_000,
            FALLBACK_MAX
        ));
    }
}

/// 钩子安装结果的位标志(W0-11)。编码(钩子线程)与解码(界面自检)都走这里,
/// 免得两边各写一份位运算、把键盘和鼠标对应反了 —— 那会把"键盘没装上"
/// 报成"鼠标没装上",反而误导排查方向。
///
/// 只有 Windows 会装低级钩子,所以在 Linux 构建里这一组编解码函数用不上
/// (`cargo check` 在 WSL 下会报 3 条 dead_code)。**不能直接把函数注释掉**:
/// Windows 的钩子线程与界面自检都在用它们,一注释 Windows 就编不过。
/// 按平台标注才是对的 —— 与"跨平台代码只在一边用得上"这件事本身一致。
#[cfg_attr(not(windows), allow(dead_code))]
pub fn hook_bits_encode(keyboard: bool, mouse: bool) -> u8 {
    (keyboard as u8) | ((mouse as u8) << 1)
}

/// 键盘低级钩子是否已装上(bit0);`hook_ok == 0` 时表示"未知",同样返回 false
#[cfg_attr(not(windows), allow(dead_code))]
pub fn hook_bits_keyboard_ok(bits: u8) -> bool {
    bits & 1 != 0
}

/// 鼠标低级钩子是否已装上(bit1);`hook_ok == 0` 时表示"未知",同样返回 false
#[cfg_attr(not(windows), allow(dead_code))]
pub fn hook_bits_mouse_ok(bits: u8) -> bool {
    bits & 2 != 0
}

/// B21-3/G-c:光标钉住的**决策**单测(裁剪矩形 / 真值表 / 状态机)。
///
/// # 这里测什么、不测什么(如实登记)
///
/// 测:纯函数 `clip_rect_for` / `should_clip` / `clip_step` —— 也就是"该不该钉、
/// 钉在哪个矩形、什么时候放"这三个决策。
/// **不测**:`ClipCursor` 本身。它是 Win32 **进程级全局**调用,在测试进程里跑会
/// 真把跑测试这台机器的光标钉住(而且测试并行跑,`GetCursorPos` 读到的点随时在变)。
/// 所以真正调用它的地方只在 Windows 消费线程里,没有单测 —— 这一条在交付里
/// 作为**已知未覆盖项**列出,不用"有测试"的说法糊过去。
#[cfg(test)]
mod clip_cursor_tests {
    use super::{CLIP_RECT_SIDE, ClipAction, clip_rect_for, clip_step, should_clip};

    /// 取 `needle` 那一行**之后**最多 `n` 行(到同缩进的 `}` 为止),用于判分支体。
    ///
    /// 为什么不直接 `src.contains("clip_guard.release();")`:那样整个文件里任何一处
    /// 出现都算数(比如只留在退出路径、却被从循环里删掉了),判据就废了。
    fn block_after(lines: &[&str], needle: &str, n: usize) -> Vec<String> {
        let start = lines
            .iter()
            .position(|l| l.trim() == needle)
            .unwrap_or_else(|| panic!("源码里找不到 `{needle}` —— 改名后请同步本测试"));
        let base = lines[start].len() - lines[start].trim_start().len();
        let mut out = Vec::new();
        for line in &lines[start + 1..(start + 1 + n).min(lines.len())] {
            if line.trim() == "}" && line.len() - line.trim_start().len() == base {
                break;
            }
            out.push((*line).to_string());
        }
        out
    }

    /// T1:裁剪矩形 —— 表驱动(正常点 / 屏幕四角 / 负坐标 / 大坐标)。
    ///
    /// 重点在于**矩形必须是以给定点开头的 1x1**:宽高恒等于 `CLIP_RECT_SIDE`。
    /// 这条不变量是"不做边界钳制"这个取舍的可执行表述 —— 一旦有人把坐标钳到
    /// 屏幕范围里,角落与负坐标两行立刻红(钳制会把 `left` 改成 0)。
    #[test]
    fn clip_rect_is_a_one_pixel_box_at_the_cursor_point() {
        // (光标点, 期望 left, 期望 top)
        let cases: &[(&str, (i32, i32), i32, i32)] = &[
            ("屏幕中间的普通点", (960, 540), 960, 540),
            ("左上角原点", (0, 0), 0, 0),
            ("右上角", (1919, 0), 1919, 0),
            ("左下角", (0, 1079), 0, 1079),
            ("右下角", (1919, 1079), 1919, 1079),
            ("副屏在主屏左边的负坐标", (-1920, -1080), -1920, -1080),
            ("只在一个轴为负", (-1, 500), -1, 500),
            ("大坐标(4K 竖屏叠放)", (3840, 2160), 3840, 2160),
        ];
        for &(why, pt, want_left, want_top) in cases {
            let (l, t, r, b) = clip_rect_for(pt);
            assert_eq!(
                (l, t),
                (want_left, want_top),
                "{why}:左上角必须**就是**光标点(不许钳制、不许四舍五入)"
            );
            assert_eq!(
                (r - l, b - t),
                (CLIP_RECT_SIDE, CLIP_RECT_SIDE),
                "{why}:矩形必须是 {CLIP_RECT_SIDE}x{CLIP_RECT_SIDE} —— 宽高跟着坐标变了"
            );
            // 负坐标再单独钉一次:钳制会把它变成 0,而那会把光标**拽到别的屏幕**
            if want_left < 0 || want_top < 0 {
                assert!(
                    l < 0 || t < 0,
                    "{why}:负坐标被钳制了(left={l}, top={t})—— 多显示器下等于把光标拽走"
                );
            }
        }
    }

    /// T2:真值表 —— `(grabbing, cursor_hide, setting)` 的全部 8 种组合。
    ///
    /// **关键的一支是第 5 行**:`cursor_hide` 单独为真(没在瞄准,只是开了
    /// 指针消隐)⇒ **不许钉**。把 `should_clip` 写成
    /// `setting && (grabbing || cursor_hide)` 时,只有这一行会红 —— 这正是
    /// 本测试存在的理由(变异验证 M1 打的就是它)。
    #[test]
    fn should_clip_truth_table_pins_only_while_grabbing() {
        // (grabbing, cursor_hide, setting) => 期望
        let table: &[(bool, bool, bool, bool, &str)] = &[
            (false, false, false, false, "什么都没开"),
            (true, false, false, false, "抓取中但用户关了开关 ⇒ 尊重开关"),
            (false, true, false, false, "只消隐 + 开关关"),
            (true, true, false, false, "抓取 + 消隐 + 开关关 ⇒ 仍然不钉"),
            (
                false,
                true,
                true,
                false,
                "**只消隐 + 开关开 ⇒ 不许钉**(那不是瞄准)",
            ),
            (false, false, true, false, "开关开但没在瞄准"),
            (true, false, true, true, "瞄准 + 开关开 ⇒ 钉"),
            (
                true,
                true,
                true,
                true,
                "瞄准 + 同时消隐 ⇒ 照样钉(瞄准就是瞄准)",
            ),
        ];
        for &(grabbing, cursor_hide, setting, want, why) in table {
            assert_eq!(
                should_clip(grabbing, cursor_hide, setting),
                want,
                "({grabbing},{cursor_hide},{setting}) {why}"
            );
        }
        // 阴性对照(§3.3):把被禁止的那种反例写法真算一遍,必须与正确答案**不同** ——
        // 证明上面的表确实在区分东西,不是恒真断言
        let bad = |g: bool, h: bool, s: bool| s && (g || h);
        assert_ne!(
            bad(false, true, true),
            should_clip(false, true, true),
            "阴性对照失败:错误写法与正确实现在关键一行上竟然一致"
        );
        assert_eq!(
            bad(true, false, true),
            should_clip(true, false, true),
            "阳性对照:正常瞄准那一行两者应当一致"
        );
    }

    /// T3:状态机 —— **离开抓取必须释放**。
    ///
    /// 这是"施加与释放成对"的可执行表述:单调的 `if want { pin() }` 能通过
    /// T2,却会把光标永久钉住。变异验证 M3 删掉消费线程里 `Release` 那一支时,
    /// 这条测试守的是**决策层**仍然要求 Release;消费线程真的调没调用,由
    /// 源码级断言另测(见 `consumer_loop_calls_the_clip_state_machine`)。
    #[test]
    fn leaving_grab_always_releases_the_clip() {
        // 从"已钉住"走到任何一种"不该钉"的状态,都必须得到 Release
        for &(g, h, s, why) in &[
            (false, false, true, "瞄准结束"),
            (false, true, true, "瞄准结束后又开了消隐"),
            (true, false, false, "瞄准中用户取消勾选"),
            (false, true, false, "取消勾选 + 消隐"),
        ] {
            let want = should_clip(g, h, s);
            assert_eq!(
                clip_step(true, want),
                ClipAction::Release,
                "{why}:已钉住却没被要求释放 —— 光标会永久钉死在屏幕上"
            );
        }
        // 反向:该钉就钉、状态没变就不动(避免每圈都去打全局 API)
        assert_eq!(clip_step(false, true), ClipAction::Apply);
        assert_eq!(clip_step(true, true), ClipAction::Nothing);
        assert_eq!(clip_step(false, false), ClipAction::Nothing);
        // 全程真值表:任意 (已钉, 该钉) 组合都必须有确定动作,一个不漏
        for applied in [false, true] {
            for want in [false, true] {
                let a = clip_step(applied, want);
                let expect = match (applied, want) {
                    (false, true) => ClipAction::Apply,
                    (true, false) => ClipAction::Release,
                    _ => ClipAction::Nothing,
                };
                assert_eq!(a, expect, "({applied},{want}) 状态机判错");
            }
        }
    }

    /// T4:消费线程里的**接线**必须在(源码级,逐行扫描)。
    ///
    /// 为什么需要它:T1–T3 只证明"决策正确",证明不了"生产代码真的按决策在做"。
    /// 第一版守护测试的事故(V2-4)就是判据永远 PASS、删掉被保护的调用照样绿。
    /// 这里钉住三件事,任何一件消失都红:
    ///   ① 每圈真的算了 `should_clip`;
    ///   ② `Release` 分支真的调了 `clip_guard.release()`;
    ///   ③ 退出路径 / Drop 里真的放了(释放路径②⑤)。
    #[test]
    fn consumer_loop_calls_the_clip_state_machine() {
        let src = include_str!("capture.rs");
        let lines: Vec<&str> = src.lines().collect();
        let has = |needle: &str| lines.iter().any(|l| l.trim_start().starts_with(needle));
        assert!(
            has("let want_clip = should_clip("),
            "消费线程没有每圈调用 should_clip —— 开关变了也不会生效"
        );
        assert!(
            has("match clip_step(clip_applied, want_clip) {"),
            "消费线程没有走 clip_step 状态机 —— 施加/释放不再成对"
        );
        // **必须看分支体内那几行**:只判 "ClipAction::Release => {" 存在是不够的 ——
        // 变异验证 M3 实测过:把分支里的 `clip_guard.release();` 删掉,这条断言照样绿
        // (空分支还在)。改成按**窗口**取分支体里的连续行,逐行找调用。
        let release_body = block_after(&lines, "ClipAction::Release => {", 8);
        assert!(
            release_body
                .iter()
                .any(|l| l.trim() == "clip_guard.release();"),
            "消费线程的 Release 分支里缺 `clip_guard.release();` —— 退出抓取时不会释放,\n\
             光标会被永久钉在屏幕上(M3 变异正是这一支)。分支体实际内容:{release_body:?}"
        );
        assert!(
            release_body
                .iter()
                .any(|l| l.trim() == "clip_applied = false;"),
            "Release 分支没有把 clip_applied 落回 false —— 下一圈会重复释放/状态错位"
        );
        assert!(
            has("windows::clip_cursor_release();"),
            "Capture::drop 里没有释放 —— 捕获层被重建/退出时光标会留在钉住状态"
        );
        assert!(
            has("impl Drop for ClipGuard {"),
            "缺 ClipGuard 的 Drop 实现 —— 消费线程 panic 时没人把光标还回去"
        );
        // 阴性对照:同一条判据对一个**不存在**的调用必须判 false
        assert!(
            !has("let want_clip = should_clip_but_not_really("),
            "阴性对照失败:判据对任意字符串都返回 true"
        );
    }
}

#[cfg(test)]
mod hook_bits_tests {
    use super::*;

    /// 编码与解码必须互相对应:反了会把"键盘没装上"报成"鼠标没装上"。
    /// 这里逐个组合验证,四种情况一个不漏。
    #[test]
    fn encode_and_decode_agree_for_all_combinations() {
        for (kb, ms) in [(false, false), (true, false), (false, true), (true, true)] {
            let bits = hook_bits_encode(kb, ms);
            assert_eq!(
                hook_bits_keyboard_ok(bits),
                kb,
                "键盘位对不上(encode({kb},{ms}) = {bits:#04b})"
            );
            assert_eq!(
                hook_bits_mouse_ok(bits),
                ms,
                "鼠标位对不上(encode({kb},{ms}) = {bits:#04b})"
            );
        }
    }

    /// 0 是"未知"(还不该被当成"装好了"),解码必须全 false
    #[test]
    fn unknown_zero_decodes_to_nothing_ok() {
        assert!(!hook_bits_keyboard_ok(0));
        assert!(!hook_bits_mouse_ok(0));
    }
}

pub struct Capture {
    /// 键盘抓取(Linux 专用;映射开启时置 true,原始按键不再传给其它程序)
    pub grab: Arc<AtomicBool>,
    /// 「拦截系统默认行为」掩码(位定义见 [`swallow_bit`]):某位=1 表示对应
    /// 候选键此刻正在被映射,钩子回调把它从系统输入流里吞掉。
    /// Windows 专用;Linux 侧 `grab` 的 EVIOCGRAB 整把抓取,天然满足同一需求。
    pub swallow: Arc<AtomicU16>,
    /// 「已绑定键不再传给其它程序」的 256 位 vk 位图(B21-2/G-b,独占模式):
    /// 界面线程每帧按当前绑定**全量重算**写进来,钩子回调只读一次位测试
    /// (见 [`keyboard_swallows`] 与 [`BoundKeys`])。Windows 专用;Linux 侧
    /// `grab` 的 EVIOCGRAB 整把抓取天然满足同一需求(字段两侧都在,界面要 clone)。
    pub bound_keys: Arc<BoundKeys>,
    /// 独占模式总开关(B21-2/G-b):**默认 false** = 与从前逐位一致,一个键都
    /// 不多吞。true 时钩子回调对位图里的 vk(按下与抬起都)返回 1。
    /// 由界面每帧按 `Settings::swallow_bound_keys × 映射开启` 重算写入;
    /// 映射一关必须落回 false(绝不能让关了映射还在吞用户的键)。
    pub swallow_bound_keys: Arc<AtomicBool>,
    /// **瞄准期间把光标钉住**(B21-3/G-c):做不做由界面每帧按
    /// `Settings::clip_cursor_in_aim` 写进来(**默认 true**);"什么时候做"
    /// 不在这里、也不看它 —— 只看既有的 `mouse_grab`(见 [`should_clip`])。
    ///
    /// 与 `mouse_grab` 分开的理由:"要不要钉"是**用户偏好**,"此刻在不在瞄准"
    /// 是**状态**。混成一个原子量,界面就没法把开关和状态分开表达,退出瞄准时
    /// 也分不清"该放"是因为状态变了还是因为用户关了开关。
    /// Windows 专用:Linux 没有 `ClipCursor` 这种原语,捕获层不读它(字段本身
    /// 两侧都要有:界面要 clone)。
    pub clip_cursor: Arc<AtomicBool>,
    /// 「FPS 鼠标隔膜」开关(2026-10-11 用户根因定位,见 [`mouse_shield_swallows`]):
    /// true = 鼠标消息**一律**不让系统投递给任何窗口(含投屏窗口 scrcpy.exe)。
    ///
    /// 界面线程每帧按「映射开启 × FPS 模式生效 × 控制通道在线」重算写进来
    /// (见 `app.rs` 的 `mouse_shield_flag`);钩子回调只读一次原子量,不做锁。
    /// Windows 专用:Linux 侧 FPS 的鼠标独占由引擎写 `mouse_grab`(EVIOCGRAB)完成,
    /// 那边界面照样每帧写这个开关,但捕获层不读它(本字段本身两侧都要读:界面要 clone)。
    pub mouse_shield: Arc<AtomicBool>,
    /// 鼠标抓取(FPS 瞄准开启时置 true;Linux 走 EVIOCGRAB,Windows 走光标回中)
    pub mouse_grab: Arc<AtomicBool>,
    /// 独立的光标消隐请求(不影响鼠标事件抓取/回中;Windows 使用透明系统光标)
    pub cursor_hide: Arc<AtomicBool>,
    /// 是否检测到鼠标设备(FPS 瞄准的前提;用于界面诊断)
    pub mouse_found: Arc<AtomicBool>,
    /// 钩子回调延迟是否超标(最近一轮 5 秒汇总 p99 > 5ms)。界面据此预警
    /// "捕获层延迟异常";Linux 无此统计,恒为 false。
    pub hook_lag: Arc<AtomicBool>,
    /// 钩子心跳结论(W1-4):0=正常,1=消息泵 ≥3s 无响应,2=钩子探针 3 次
    /// 无回音(已被系统摘除,自愈已触发)。界面据此提示"捕获层已失效/正在自愈";
    /// Linux 无钩子概念,恒为 0。
    pub hb_state: Arc<AtomicU8>,
    /// Windows 低级钩子安装结果(位标志:bit0=键盘钩子,bit1=鼠标钩子;0 = 未知)。
    ///
    /// 为什么要有它:装钩子是在后台线程里做的,装失败以前只写一行诊断日志 ——
    /// 用户看到的现象是"按了没反应",界面却一切正常,连排查方向都没有。
    /// 现在安装结果会同步回 `start()` 并显示在自检里(W0-11)。
    /// Linux 无钩子概念,恒为 0(界面不查这一项)。
    pub hook_ok: Arc<AtomicU8>,
    stop: Arc<AtomicBool>,
    /// Windows:消息循环所在线程 id,退出时 `PostThreadMessage(WM_QUIT)` 唤醒它
    /// (`GetMessage` 会阻塞,光置 stop 标志它看不到)。Linux 侧恒为 None。
    #[cfg_attr(not(windows), allow(dead_code))]
    wake_thread: Option<Arc<AtomicU32>>,
    /// Windows hook 线程句柄。退出时必须等它真正卸载钩子后再返回，
    /// 避免低层鼠标钩子在进程退出瞬间仍被系统调用。
    #[cfg(windows)]
    hook_thread: Option<std::thread::JoinHandle<()>>,
}

impl Capture {
    pub fn start(tx: Sender<CaptureEvent>) -> Result<Self> {
        let grab = Arc::new(AtomicBool::new(false));
        let mouse_grab = Arc::new(AtomicBool::new(false));
        let swallow = Arc::new(AtomicU16::new(0));
        // 独占模式(B21-2/G-b):初始必须是**空位图 + 开关关** —— 程序刚起来
        // 还没映射,绝不能吞用户任何键。
        let bound_keys = Arc::new(BoundKeys::new());
        let swallow_bound_keys = Arc::new(AtomicBool::new(false));
        // G-c(钉住光标):初值必须是 **false** —— 界面第一帧就会按
        // `Settings::clip_cursor_in_aim` 写进来(默认 true)。初值给 true 会让
        // "捕获层起来了但界面还没画第一帧"这一小段默认允许钉住,而那时引擎
        // 可能正处在任意状态;给 false 则最坏只是晚一帧生效(120ms 帧循环)。
        let clip_cursor = Arc::new(AtomicBool::new(false));
        // 「FPS 鼠标隔膜」(2026-10-11):初始必须是 false —— 程序刚起来、还没
        // 进 FPS 模式时绝不能吞用户鼠标。
        let mouse_shield = Arc::new(AtomicBool::new(false));
        let cursor_hide = Arc::new(AtomicBool::new(false));
        let mouse_found = Arc::new(AtomicBool::new(false));
        let hook_lag = Arc::new(AtomicBool::new(false));
        let hb_state = Arc::new(AtomicU8::new(0));
        let hook_ok = Arc::new(AtomicU8::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        #[cfg(target_os = "linux")]
        let wake_thread = linux::start(&grab, &mouse_grab, &cursor_hide, &mouse_found, &stop, tx)?;
        #[cfg(windows)]
        let (wake_thread, hook_thread) = {
            // 装钩子发生在钩子线程里,结果要等它报回来(W0-11)。
            // 最多等 500ms:这是启动期的一次性等待,换来"到底装上没有"的确定答案;
            // 超时就当"未知" —— 界面会显示"状态未知",而不是谎报正常。
            let (ready_tx, ready_rx) = std::sync::mpsc::channel::<u8>();
            let threads = windows::start(
                &grab,
                &mouse_grab,
                &swallow,
                &bound_keys,
                &swallow_bound_keys,
                &clip_cursor,
                &mouse_shield,
                &cursor_hide,
                &mouse_found,
                &hook_lag,
                &hb_state,
                &hook_ok,
                &stop,
                tx,
                ready_tx,
            )?;
            match ready_rx.recv_timeout(std::time::Duration::from_millis(500)) {
                Ok(bits) => hook_ok.store(bits, Ordering::Relaxed),
                Err(_) => crate::diag_warn!("capture", "等待钩子安装结果超时(500ms),捕获状态未知"),
            }
            threads
        };

        Ok(Self {
            grab,
            swallow,
            bound_keys,
            swallow_bound_keys,
            mouse_shield,
            clip_cursor,
            mouse_grab,
            cursor_hide,
            mouse_found,
            hook_lag,
            hb_state,
            hook_ok,
            stop,
            wake_thread,
            #[cfg(windows)]
            hook_thread: Some(hook_thread),
        })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.cursor_hide.store(false, Ordering::Relaxed);
        // 「FPS 鼠标隔膜」先解除再收钩子:下面 join 钩子线程可能要等一会儿,
        // 期间钩子还挂着 —— 隔膜开着就还在吞用户的鼠标。这一枚 store 让
        // "退出/重建捕获层"这条路也立刻把鼠标还给用户(安全底线见
        // `mouse_shield_swallows`:钩子随进程退出被系统摘掉,不存在永久锁死)。
        self.mouse_shield.store(false, Ordering::Relaxed);
        // G-c 释放路径⑤(捕获层被 Drop = 退出/重建):光标裁剪是**全局**状态,
        // 绝不能留着。这里无条件放一次 —— 它幂等,而且比依赖消费线程的退出
        // 分支更稳(消费线程可能已经走了)。
        #[cfg(windows)]
        windows::clip_cursor_release();
        #[cfg(windows)]
        windows::set_cursor_visible(true);
        // Windows 的消息循环阻塞在 GetMessage 上,必须显式叫醒；随后等待
        // hook 线程完成 UnhookWindowsHookEx，保证低层鼠标钩子不会拖到进程
        // 退出阶段才被系统异步清理。
        #[cfg(windows)]
        {
            if let Some(tid) = &self.wake_thread {
                let tid = tid.load(Ordering::Relaxed);
                if tid != 0 {
                    windows::post_quit(tid);
                }
            }
            if let Some(handle) = self.hook_thread.take() {
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
                while !handle.is_finished() && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                if handle.is_finished() {
                    let _ = handle.join();
                } else {
                    // 2026-10-22(只读审计发现):超时以前是**静默**的 —— 钩子线程
                    // 没退出就丢下不管,进程退出阶段由系统异步清理。这条日志不是
                    // 为了"修好它"(500ms 已经够宽,真超时说明 GetMessage 循环
                    // 卡在别处),而是让"退出时可能残留钩子"这件事**可见**:
                    // 否则下次遇到"退出了但鼠标还被吞"根本查不到原因。
                    crate::diag_warn!(
                        "capture",
                        "等待低级钩子线程退出超时(500ms),已放弃 join —— 进程退出时由系统清理该钩子"
                    );
                }
            }
        }
    }
}

/// Called from the GUI thread when capture starts/stops.  The Windows backend
/// installs/restores transparent system cursor images, so the cursor disappears
/// over scrcpy and any other focused window too.
#[cfg(windows)]
pub fn set_cursor_visible_from_ui(visible: bool) {
    windows::set_cursor_visible(visible);
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn set_cursor_visible_from_ui(_visible: bool) {}

/// Re-apply a NULL cursor shape after egui/window painting.  The transparent
/// system cursor remains the global fallback across other applications.
#[cfg(windows)]
pub fn hide_cursor_shape_from_ui() {
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::SetCursor(std::ptr::null_mut());
    }
}

/// Linux 光标可见性(第 30 条,2026-10-21 补实现)。
///
/// 这里**只记状态**,真正落地由窗口层负责 —— 因为 Linux 没有 `ShowCursor` 这种
/// "进程级、立刻生效、跨窗口"的原语:
///
/// * **X11**:`ShowCursor` 的对应物是 `XDefineCursor(window, None)` 与
///   `XDefineCursor(window, invisible_cursor)`,**必须**作用在某个具体窗口上。
///   我们自己的只有 eframe 的窗口;拿不到它的 X11 Window ID 就只能影响自己,
///   影响不到"用户正在看的那块屏幕"(scrcpy 是**另一个进程**的窗口)。
///   如实登记为未覆盖 —— 见文件末尾 `linux_cursor_note`。
/// * **Wayland**:更没有全局原语;协议规定光标由**客户端**对自己 surface 的
///   pointer 设置(egui 的 `CursorIcon::None` 走的就是这条路),进程外无法干预。
///
/// 两条路都只能作用于本程序自己的窗口,所以"进 FPS 模式就看不到箭头"这个
/// Windows 语义在 Linux 上**做不到**同样强度。与其写一个只在自己窗口里生效、
/// 用户却以为全局生效的实现(比空实现更糟:它会制造"已经隐藏了"的错觉),
/// 这里就让下层保持空实现、由窗口层用 `CursorIcon::None` 尽力而为。
///
/// Linux 侧的"恢复显示"同样是**空操作**,但必须存在:界面在启动时、映射关闭时、
/// 程序退出时都会调它来"把光标交还系统"。
///
/// 为什么能是空的:Wayland/X11 下光标是**按 surface** 由客户端自己设置的
/// (见 [`hide_cursor_shape_from_ui`]),没有 `ShowCursor` 那种进程级全局句柄
/// 需要"还回去"。真正让光标重新出现的动作,是界面不再请求
/// `egui::CursorIcon::None`(`app.rs` 每帧按状态决定要不要请求)。
///
/// 2026-10-21 修正:上一版这里存了一个 `LINUX_CURSOR_HIDDEN` 原子量,但
/// **没有任何读者**(界面用的是自己的 `cursor_hide_flag`)⇒ Windows 交叉编译
/// 报 `static is never used`。删掉它,只保留这个空实现 —— 少一份会失真、
/// 又没人读的镜像状态,比多一份"看起来有记录"好。
#[cfg(target_os = "linux")]
pub fn set_cursor_visible_from_ui(_visible: bool) {}

#[cfg(target_os = "linux")]
pub fn hide_cursor_shape_from_ui() {
    // 每帧被界面调用,两平台同形;Linux 侧真正的落地在 `app.rs` 的
    // `ctx.set_cursor_icon(egui::CursorIcon::None)`(egui-winit 会转到
    // `window.set_cursor_visible(false)`,只作用于本窗口 —— 这是 Wayland/X11
    // 都支持的最大强度)。这里留空是为了让上层调用点不必区分平台。
}

// ===================================================================
//  Windows 虚拟键码 <-> evdev 键码(平台无关的纯数据 + 纯函数)
// ===================================================================
//
// 为什么放在 cfg 之外:这张表是**纯数据映射**,不依赖任何 Win32 调用,
// 因此可以在 Linux 上直接跑单元测试。跨平台配置能不能通用、小键盘与多媒体键
// 能不能绑上,全靠它 —— 这种表最容易抄错一个码,必须有测试兜着。

#[cfg(any(windows, test))]
mod vktable {
    use std::sync::OnceLock;

    /// (Windows 虚拟键码, evdev 键码, 可读名)
    ///
    /// 名称沿用 evdev 的叫法(`KEY_A` / `BTN_LEFT`),这样 Windows 与 Linux
    /// 上看到的名字一致 —— 两个平台共用同一份配置,名字不该是两套。
    ///
    /// 表中**特意包含**了 rdev 那份枚举没有覆盖的部分:小键盘(`VK_NUMPAD*`、
    /// 四则运算符)、多媒体/浏览器键。旧实现里这些一律落到 `Key::Unknown`
    /// 然后被 `_ => return None` 静默丢弃,用户按小键盘改键会发现"怎么按都没反应"。
    pub const VK_TABLE: &[(u16, u16, &str)] = &[
        // ---- 主键区 ----
        (0x08, 14, "KEY_BACKSPACE"),
        (0x09, 15, "KEY_TAB"),
        (0x0D, 28, "KEY_ENTER"),
        (0x1B, 1, "KEY_ESC"),
        (0x20, 57, "KEY_SPACE"),
        (0x14, 58, "KEY_CAPSLOCK"),
        // ---- 修饰键:虚拟键码可能报"通用"也可能是"左右具体" ----
        (0x10, 42, "KEY_LEFTSHIFT"),
        (0x11, 29, "KEY_LEFTCTRL"),
        (0x12, 56, "KEY_LEFTALT"),
        (0xA0, 42, "KEY_LEFTSHIFT"),
        (0xA1, 54, "KEY_RIGHTSHIFT"),
        (0xA2, 29, "KEY_LEFTCTRL"),
        (0xA3, 97, "KEY_RIGHTCTRL"),
        (0xA4, 56, "KEY_LEFTALT"),
        (0xA5, 100, "KEY_RIGHTALT"),
        (0x5B, 125, "KEY_LEFTMETA"),
        (0x5C, 126, "KEY_RIGHTMETA"),
        (0x5D, 127, "KEY_COMPOSE"),
        // ---- 数字行 ----
        (0x30, 11, "KEY_0"),
        (0x31, 2, "KEY_1"),
        (0x32, 3, "KEY_2"),
        (0x33, 4, "KEY_3"),
        (0x34, 5, "KEY_4"),
        (0x35, 6, "KEY_5"),
        (0x36, 7, "KEY_6"),
        (0x37, 8, "KEY_7"),
        (0x38, 9, "KEY_8"),
        (0x39, 10, "KEY_9"),
        // ---- 字母区 ----
        (0x41, 30, "KEY_A"),
        (0x42, 48, "KEY_B"),
        (0x43, 46, "KEY_C"),
        (0x44, 32, "KEY_D"),
        (0x45, 18, "KEY_E"),
        (0x46, 33, "KEY_F"),
        (0x47, 34, "KEY_G"),
        (0x48, 35, "KEY_H"),
        (0x49, 23, "KEY_I"),
        (0x4A, 36, "KEY_J"),
        (0x4B, 37, "KEY_K"),
        (0x4C, 38, "KEY_L"),
        (0x4D, 50, "KEY_M"),
        (0x4E, 49, "KEY_N"),
        (0x4F, 24, "KEY_O"),
        (0x50, 25, "KEY_P"),
        (0x51, 16, "KEY_Q"),
        (0x52, 19, "KEY_R"),
        (0x53, 31, "KEY_S"),
        (0x54, 20, "KEY_T"),
        (0x55, 22, "KEY_U"),
        (0x56, 47, "KEY_V"),
        (0x57, 17, "KEY_W"),
        (0x58, 45, "KEY_X"),
        (0x59, 21, "KEY_Y"),
        (0x5A, 44, "KEY_Z"),
        // ---- 标点(OEM 键位随键盘布局变化,但物理位置固定) ----
        (0xBA, 39, "KEY_SEMICOLON"),
        (0xBB, 13, "KEY_EQUAL"),
        (0xBC, 51, "KEY_COMMA"),
        (0xBD, 12, "KEY_MINUS"),
        (0xBE, 52, "KEY_DOT"),
        (0xBF, 53, "KEY_SLASH"),
        (0xC0, 41, "KEY_GRAVE"),
        (0xDB, 26, "KEY_LEFTBRACE"),
        (0xDC, 43, "KEY_BACKSLASH"),
        (0xDD, 27, "KEY_RIGHTBRACE"),
        (0xDE, 40, "KEY_APOSTROPHE"),
        (0xE2, 86, "KEY_102ND"),
        // ---- 功能键 ----
        (0x70, 59, "KEY_F1"),
        (0x71, 60, "KEY_F2"),
        (0x72, 61, "KEY_F3"),
        (0x73, 62, "KEY_F4"),
        (0x74, 63, "KEY_F5"),
        (0x75, 64, "KEY_F6"),
        (0x76, 65, "KEY_F7"),
        (0x77, 66, "KEY_F8"),
        (0x78, 67, "KEY_F9"),
        (0x79, 68, "KEY_F10"),
        (0x7A, 87, "KEY_F11"),
        (0x7B, 88, "KEY_F12"),
        // ---- 编辑/导航 ----
        (0x2C, 99, "KEY_SYSRQ"),
        (0x91, 70, "KEY_SCROLLLOCK"),
        (0x13, 119, "KEY_PAUSE"),
        (0x2D, 110, "KEY_INSERT"),
        (0x2E, 111, "KEY_DELETE"),
        (0x24, 102, "KEY_HOME"),
        (0x23, 107, "KEY_END"),
        (0x21, 104, "KEY_PAGEUP"),
        (0x22, 109, "KEY_PAGEDOWN"),
        (0x25, 105, "KEY_LEFT"),
        (0x26, 103, "KEY_UP"),
        (0x27, 106, "KEY_RIGHT"),
        (0x28, 108, "KEY_DOWN"),
        // ---- 小键盘(旧实现完全没有,用户根本绑不上) ----
        (0x90, 69, "KEY_NUMLOCK"),
        // VK_CLEAR:NumLock 关闭时的小键盘 5(W1-5;它没有别的含义,直接进表)。
        // 注意与 0x65(VK_NUMPAD5)同码:反查取小值 = 0x0C,两个 vk 都指向 KP5。
        (0x0C, 76, "KEY_KP5"),
        (0x60, 82, "KEY_KP0"),
        (0x61, 79, "KEY_KP1"),
        (0x62, 80, "KEY_KP2"),
        (0x63, 81, "KEY_KP3"),
        (0x64, 75, "KEY_KP4"),
        (0x65, 76, "KEY_KP5"),
        (0x66, 77, "KEY_KP6"),
        (0x67, 71, "KEY_KP7"),
        (0x68, 72, "KEY_KP8"),
        (0x69, 73, "KEY_KP9"),
        (0x6A, 55, "KEY_KPASTERISK"),
        (0x6B, 78, "KEY_KPPLUS"),
        (0x6C, 121, "KEY_KPCOMMA"),
        (0x6D, 74, "KEY_KPMINUS"),
        (0x6E, 83, "KEY_KPDOT"),
        (0x6F, 98, "KEY_KPSLASH"),
        // ---- 多媒体 ----
        (0xAD, 113, "KEY_MUTE"),
        (0xAE, 114, "KEY_VOLUMEDOWN"),
        (0xAF, 115, "KEY_VOLUMEUP"),
        (0xB0, 163, "KEY_NEXTSONG"),
        (0xB1, 165, "KEY_PREVIOUSSONG"),
        (0xB2, 166, "KEY_STOPCD"),
        (0xB3, 164, "KEY_PLAYPAUSE"),
        // ---- 浏览器 ----
        (0xA6, 158, "KEY_BACK"),
        (0xA7, 159, "KEY_FORWARD"),
        (0xA8, 173, "KEY_REFRESH"),
        (0xAA, 217, "KEY_SEARCH"),
        (0xAC, 172, "KEY_HOMEPAGE"),
    ];

    /// 鼠标按键的 evdev 码 -> Windows 虚拟键码。
    ///
    /// 为什么要这张小表:`GetAsyncKeyState` 认的是 VK_* 而不是 evdev 码,
    /// 而鼠标键也需要参与"丢失 release"的对账 —— 卡住的右键比卡住的键盘键更常见。
    pub const MOUSE_VK: &[(u16, u16)] = &[
        (272, 0x01), // BTN_LEFT   -> VK_LBUTTON
        (273, 0x02), // BTN_RIGHT  -> VK_RBUTTON
        (274, 0x04), // BTN_MIDDLE -> VK_MBUTTON
        (275, 0x05), // BTN_SIDE   -> VK_XBUTTON1
        (276, 0x06), // BTN_EXTRA  -> VK_XBUTTON2
    ];

    /// vk 索引 -> evdev 码(0 = 未映射)。
    ///
    /// 低级钩子每个事件都要查一次,所以用 O(1) 的数组而不是遍历:
    /// 回调里省下的每一纳秒,都是在远离那个 300ms 的超时上限。
    static VK_TO_EVDEV: OnceLock<Box<[u16; 256]>> = OnceLock::new();
    /// evdev 码 -> vk(只在对账时用,查表次数少,线性找即可)
    static EVDEV_TO_VK: OnceLock<Vec<(u16, u16)>> = OnceLock::new();

    fn vk_to_evdev() -> &'static [u16; 256] {
        VK_TO_EVDEV.get_or_init(|| {
            let mut t = Box::new([0u16; 256]);
            // 同一个 vk 出现两次时以**先出现**的为准:表中"通用修饰键"排在
            // "左右具体"之前,而真实低级钩子报的是具体的左右键,两者都映射到
            // 同一个 evdev 码(见 VK_TABLE 的分组注释)。
            for &(vk, code, _) in VK_TABLE {
                let i = vk as usize;
                if i < 256 && t[i] == 0 {
                    t[i] = code;
                }
            }
            t
        })
    }

    /// Windows 虚拟键码 -> evdev 键码
    pub fn map_vk(vk: u16) -> Option<u16> {
        let c = vk_to_evdev()[vk as usize & 0xFF];
        (c != 0).then_some(c)
    }

    /// NumLock 关闭时,小键盘的 0-9 与 `.` 报的是导航键的 VK(同一物理键,
    /// NumLock 状态决定报哪个 VK;小键盘 5 报 VK_CLEAR,已在 `VK_TABLE` 里)。
    /// 低级钩子只有靠 **`LLKHF_EXTENDED` 标志**才能把两者分开:真正的导航区
    /// 按键带扩展标志(MSDN"Extended Keys"清单),NumLock 关闭时小键盘报到
    /// 它们头上**不带** —— 所以"不带标志"才认作小键盘(W1-5)。
    pub const NUMPAD_NAV_VK: &[(u16, u16)] = &[
        (0x2D, 82), // VK_INSERT -> KEY_KP0
        (0x23, 79), // VK_END    -> KEY_KP1
        (0x28, 80), // VK_DOWN   -> KEY_KP2
        (0x22, 81), // VK_NEXT   -> KEY_KP3
        (0x25, 75), // VK_LEFT   -> KEY_KP4
        (0x27, 77), // VK_RIGHT  -> KEY_KP6
        (0x24, 71), // VK_HOME   -> KEY_KP7
        (0x26, 72), // VK_UP     -> KEY_KP8
        (0x21, 73), // VK_PRIOR  -> KEY_KP9
        (0x2E, 83), // VK_DELETE -> KEY_KPDOT
    ];

    /// 小键盘 Enter 的 evdev 码(主键盘 Enter 是 `KEY_ENTER`=28,两者可分开绑定)
    pub const KEY_KPENTER: u16 = 96;

    /// 带扩展标志地映射一个 vk(W1-5,给低级钩子用)。
    ///
    /// - `extended`(= `LLKHF_EXTENDED`)为真时,`VK_RETURN` 是小键盘 Enter ——
    ///   MSDN 的扩展键清单把"小键盘 ENTER"列了进去,主键盘 Enter 不在;
    /// - 为假时,清单里的那些导航 VK 可能来自 NumLock 关闭的小键盘 → 认回
    ///   `KEY_KPn`;
    /// - 其余情况沿用 `map_vk`(它按 vk 的唯一含义/最先出现映射)。
    pub fn map_vk_ex(vk: u16, extended: bool) -> Option<u16> {
        if extended {
            if vk == 0x0D {
                return Some(KEY_KPENTER);
            }
        } else if let Some(&(_, code)) = NUMPAD_NAV_VK.iter().find(|&&(v, _)| v == vk) {
            return Some(code);
        }
        map_vk(vk)
    }

    /// evdev 键码 -> Windows 虚拟键码(对账时问系统"这个键还按着吗")
    pub fn vk_for_evdev(code: u16) -> Option<u16> {
        if let Some(&(_, vk)) = MOUSE_VK.iter().find(|(c, _)| *c == code) {
            return Some(vk);
        }
        let table = EVDEV_TO_VK.get_or_init(|| {
            let mut v: Vec<(u16, u16)> = VK_TABLE.iter().map(|&(vk, c, _)| (c, vk)).collect();
            // 同一个 evdev 码对应多个 vk 时,取**最小**的那个:它们是等价的
            // (例如 err 左右 Shift 都映射到 KEY_LEFTSHIFT 之外的错误),取小值稳定可复现
            v.sort_unstable();
            v.dedup_by_key(|(c, _)| *c);
            v
        });
        table
            .binary_search_by_key(&code, |&(c, _)| c)
            .ok()
            .map(|i| table[i].1)
    }

    /// 同一个物理键在 NumLock 开/关时上报不同 VK 的"别名"表(W1-5,对账用):
    /// 反查只给一个代表 VK(`vk_for_evdev`),但问系统"还按着吗"必须把别名都
    /// 问上 —— 否则 NumLock 状态一变,按住的键就会被误判成已释放(卡键)。
    /// 已经在 `VK_TABLE` 里作为主 VK 的(如 VK_NUMPAD0 0x60)不重复列。
    pub const EVDEV_EXTRA_VK: &[(u16, u16)] = &[
        (0x2D, 82),          // VK_INSERT   <-> KEY_KP0
        (0x23, 79),          // VK_END      <-> KEY_KP1
        (0x28, 80),          // VK_DOWN     <-> KEY_KP2
        (0x22, 81),          // VK_NEXT     <-> KEY_KP3
        (0x25, 75),          // VK_LEFT     <-> KEY_KP4
        (0x65, 76),          // VK_NUMPAD5  <-> KEY_KP5(主 VK 是 VK_CLEAR)
        (0x27, 77),          // VK_RIGHT    <-> KEY_KP6
        (0x24, 71),          // VK_HOME     <-> KEY_KP7
        (0x26, 72),          // VK_UP       <-> KEY_KP8
        (0x21, 73),          // VK_PRIOR    <-> KEY_KP9
        (0x2E, 83),          // VK_DELETE   <-> KEY_KPDOT
        (0x0D, KEY_KPENTER), // VK_RETURN   <-> KEY_KPENTER(主 Enter 同 VK,问不出分别)
    ];

    /// evdev 键码 -> 可能代表同一物理键的**全部** VK(W1-5;`vk_for_evdev` 的
    /// 代表值排第一,后面是别名)。空 = 没映射,对账时按"问不着"跳过。
    ///
    /// 宁多问不漏问:多问一个别名最多把"其实已松开"多留一小会儿(宽限期 +
    /// 下一个真事件兜底);漏问则会把还按着的键判成已释放 —— 那正是卡键和
    /// Hold 连环重按的来源。
    pub fn vks_for_evdev(code: u16) -> Vec<u16> {
        let mut v: Vec<u16> = vk_for_evdev(code).into_iter().collect();
        for &(vk, c) in EVDEV_EXTRA_VK {
            if c == code && !v.contains(&vk) {
                v.push(vk);
            }
        }
        v
    }

    /// 只在"名称查询"里出现、刻意**不进** `VK_TABLE` 的码(W1-5):进表会破坏
    /// `reverse_lookup_round_trips` 的不变量 —— 它要求每个表项反查后还能映射
    /// 回原码,而 VK_RETURN 的代表 evdev 码必须是 28(主 Enter)。
    const NAME_ONLY: &[(u16, &str)] = &[(KEY_KPENTER, "KEY_KPENTER")];

    /// evdev 键码 -> 可读名(Windows 侧 `keymap::key_name` 用它)
    pub fn evdev_name(code: u16) -> String {
        if let Some(&(_, _, name)) = VK_TABLE.iter().find(|&&(_, c, _)| c == code) {
            return name.to_string();
        }
        if let Some(&(_, name)) = NAME_ONLY.iter().find(|&&(c, _)| c == code) {
            return name.to_string();
        }
        if let Some(&(c, _)) = MOUSE_VK.iter().find(|&&(c, _)| c == code) {
            return match c {
                272 => "BTN_LEFT".into(),
                273 => "BTN_RIGHT".into(),
                274 => "BTN_MIDDLE".into(),
                275 => "BTN_SIDE".into(),
                _ => "BTN_EXTRA".into(),
            };
        }
        format!("Key({code})")
    }

    /// 未被映射的虚拟键码只提示一次(用位图去重,避免按住某个未知键时刷屏)
    static UNKNOWN_SEEN: OnceLock<Box<[AtomicBool; 256]>> = OnceLock::new();

    /// 记录一个"我们没有映射"的虚拟键码;首次见到时返回 true(调用方据此打日志)。
    ///
    /// 为什么值得记:用户报"某个键改键时怎么按都没反应"时,日志里直接就能看到
    /// 是哪个 vkCode 没覆盖,补一行表就能修好 —— 否则只能靠猜。
    pub fn note_unknown_vk(vk: u16) -> bool {
        let seen =
            UNKNOWN_SEEN.get_or_init(|| Box::new(std::array::from_fn(|_| AtomicBool::new(false))));
        !seen[vk as usize & 0xFF].swap(true, Ordering::Relaxed)
    }

    use std::sync::atomic::{AtomicBool, Ordering};

    #[cfg(test)]
    mod tests {
        use super::*;

        /// 表里不能有"同一个 vk 映射到两个不同 evdev 码"的矛盾项 ——
        /// 那会让行为取决于遍历顺序,极难排查
        #[test]
        fn no_conflicting_vk_mappings() {
            let mut seen: Vec<(u16, u16)> = Vec::new();
            for &(vk, code, _) in VK_TABLE {
                if let Some(&(_, prev)) = seen.iter().find(|&&(v, _)| v == vk) {
                    assert_eq!(prev, code, "vk {vk:#04x} 同时映射到 {prev} 与 {code}");
                } else {
                    seen.push((vk, code));
                }
            }
        }

        /// 名称要**按 evdev 码**唯一:两个 vk 指向同一个 evdev 码是合法的
        /// (通用修饰键 VK_SHIFT 与具体的 VK_LSHIFT 都落到 KEY_LEFTSHIFT),
        /// 但它们必须给出同一个名字 —— 否则同一个键在不同来源下显示成两个名字。
        #[test]
        fn names_are_unique_per_evdev_code() {
            let mut by_code: Vec<(u16, &str)> = Vec::new();
            for &(_, code, name) in VK_TABLE {
                if let Some(&(_, prev)) = by_code.iter().find(|&&(c, _)| c == code) {
                    assert_eq!(prev, name, "evdev 码 {code} 有两个名字: {prev} / {name}");
                } else {
                    by_code.push((code, name));
                }
            }
        }

        /// 抽样核对关键映射 —— 这些码抄错一位就会"按 A 出 B",而且很难发现
        #[test]
        fn key_positions_match_the_linux_table() {
            assert_eq!(map_vk(0x41), Some(30), "VK_A -> KEY_A");
            assert_eq!(map_vk(0x44), Some(32), "VK_D -> KEY_D");
            assert_eq!(map_vk(0x57), Some(17), "VK_W -> KEY_W");
            assert_eq!(map_vk(0x53), Some(31), "VK_S -> KEY_S");
            assert_eq!(map_vk(0x31), Some(2), "VK_1 -> KEY_1");
            assert_eq!(map_vk(0x30), Some(11), "VK_0 -> KEY_0");
            assert_eq!(map_vk(0x1B), Some(1), "VK_ESCAPE -> KEY_ESC");
            assert_eq!(map_vk(0x0D), Some(28), "VK_RETURN -> KEY_ENTER");
            assert_eq!(map_vk(0x20), Some(57), "VK_SPACE -> KEY_SPACE");
            assert_eq!(map_vk(0x77), Some(66), "VK_F8 -> KEY_F8(默认总开关键)");
            assert_eq!(map_vk(0x7B), Some(88), "VK_F12 -> KEY_F12");
            assert_eq!(map_vk(0x26), Some(103), "VK_UP -> KEY_UP");
            assert_eq!(map_vk(0x28), Some(108), "VK_DOWN -> KEY_DOWN");
        }

        /// 小键盘必须能绑上(旧实现里它整个落到 Unknown 被丢弃)
        #[test]
        fn numpad_and_media_keys_are_covered() {
            assert_eq!(map_vk(0x60), Some(82), "VK_NUMPAD0 -> KEY_KP0");
            assert_eq!(map_vk(0x69), Some(73), "VK_NUMPAD9 -> KEY_KP9");
            assert_eq!(map_vk(0x6A), Some(55), "VK_MULTIPLY -> KEY_KPASTERISK");
            assert_eq!(map_vk(0x6B), Some(78), "VK_ADD -> KEY_KPPLUS");
            assert_eq!(map_vk(0x6D), Some(74), "VK_SUBTRACT -> KEY_KPMINUS");
            assert_eq!(map_vk(0x6F), Some(98), "VK_DIVIDE -> KEY_KPSLASH");
            assert_eq!(map_vk(0xAF), Some(115), "VK_VOLUME_UP -> KEY_VOLUMEUP");
            assert_eq!(
                map_vk(0xB3),
                Some(164),
                "VK_MEDIA_PLAY_PAUSE -> KEY_PLAYPAUSE"
            );
        }

        /// W1-5:NumLock 关闭时的小键盘 5 报 VK_CLEAR —— 它必须还能绑到 KEY_KP5
        #[test]
        fn numlock_off_numpad_five_reports_vk_clear() {
            assert_eq!(map_vk(0x0C), Some(76), "VK_CLEAR -> KEY_KP5");
            assert_eq!(map_vk_ex(0x0C, false), Some(76));
        }

        /// W1-5:小键盘 Enter 与主 Enter 靠 LLKHF_EXTENDED 分开(只有它能分)
        #[test]
        fn numpad_enter_is_distinguishable_from_main_enter() {
            assert_eq!(
                map_vk_ex(0x0D, true),
                Some(96),
                "小键盘 Enter -> KEY_KPENTER"
            );
            assert_eq!(map_vk_ex(0x0D, false), Some(28), "主 Enter -> KEY_ENTER");
        }

        /// W1-5:NumLock 关闭的小键盘不带扩展标志,真导航区带 —— 两者不能再混为一谈
        #[test]
        fn numpad_nav_keys_need_the_missing_extended_flag() {
            assert_eq!(map_vk_ex(0x24, false), Some(71), "小键盘 7 -> KEY_KP7");
            assert_eq!(map_vk_ex(0x24, true), Some(102), "主键盘 Home -> KEY_HOME");
            assert_eq!(map_vk_ex(0x26, false), Some(72), "小键盘 8 -> KEY_KP8");
            assert_eq!(map_vk_ex(0x26, true), Some(103), "主键盘 Up -> KEY_UP");
            assert_eq!(map_vk_ex(0x2E, false), Some(83), "小键盘 . -> KEY_KPDOT");
            assert_eq!(
                map_vk_ex(0x2E, true),
                Some(111),
                "主键盘 Delete -> KEY_DELETE"
            );
            assert_eq!(map_vk_ex(0x41, false), Some(30), "普通键不受影响");
            assert_eq!(map_vk_ex(0x41, true), Some(30));
        }

        /// W1-5:对账要问全"同一物理键的别名 VK"(NumLock 两种状态报不同 VK),
        /// 少问一个就会把还按着的键判成已释放
        #[test]
        fn vks_for_evdev_covers_numpad_aliases() {
            assert_eq!(
                vks_for_evdev(71),
                vec![0x67, 0x24],
                "KEY_KP7: VK_NUMPAD7 + VK_HOME"
            );
            assert_eq!(vks_for_evdev(82), vec![0x60, 0x2D], "KEY_KP0: + VK_INSERT");
            assert_eq!(vks_for_evdev(76), vec![0x0C, 0x65], "主 VK 是 VK_CLEAR");
            assert_eq!(vks_for_evdev(96), vec![0x0D], "KEY_KPENTER 只有 VK_RETURN");
            assert_eq!(vks_for_evdev(30), vec![0x41], "普通键只有一个 VK");
            assert!(vks_for_evdev(9999).is_empty(), "没映射就问不着");
        }

        /// W1-5:小键盘 Enter 得有名有姓(它按设计不进 VK_TABLE)
        #[test]
        fn keypad_enter_has_a_name() {
            assert_eq!(evdev_name(96), "KEY_KPENTER");
        }

        /// 左右修饰键各归各位:左右混用会让"按住才瞄准"这类门控键时灵时不灵
        #[test]
        fn left_and_right_modifiers_are_distinct() {
            assert_eq!(map_vk(0xA0), Some(42), "左 Shift");
            assert_eq!(map_vk(0xA1), Some(54), "右 Shift");
            assert_eq!(map_vk(0xA2), Some(29), "左 Ctrl");
            assert_eq!(map_vk(0xA3), Some(97), "右 Ctrl");
            assert_eq!(map_vk(0xA4), Some(56), "左 Alt");
            assert_eq!(map_vk(0xA5), Some(100), "右 Alt");
        }

        /// 反查(evdev -> vk)必须能找回原值;对账靠它问系统"还按着吗"
        #[test]
        fn reverse_lookup_round_trips() {
            for &(vk, code, name) in VK_TABLE {
                let got = vk_for_evdev(code);
                assert!(got.is_some(), "{name}({code}) 反查不到 vk");
                // 同一个 evdev 码可能有多个 vk(见表),反查回来的必须仍然映射到同一 evdev 码
                let back = map_vk(got.unwrap());
                assert_eq!(
                    back,
                    Some(code),
                    "{name}: vk {vk:#04x} 反查后映射到了 {back:?}"
                );
            }
        }

        /// 鼠标键的 vk 映射:BUTTON 系列与键盘共用同一套对账流程
        #[test]
        fn mouse_buttons_have_vk_codes() {
            assert_eq!(vk_for_evdev(272), Some(0x01));
            assert_eq!(vk_for_evdev(273), Some(0x02));
            assert_eq!(vk_for_evdev(274), Some(0x04));
            assert_eq!(vk_for_evdev(275), Some(0x05), "侧键 1:旧实现会错位成 276");
            assert_eq!(vk_for_evdev(276), Some(0x06), "侧键 2");
        }

        /// 未映射的 vk 要能稳定返回 None,并且"只提示一次"要真的只提示一次
        #[test]
        fn unknown_vk_is_reported_once() {
            assert_eq!(map_vk(0xFF), None);
            assert_eq!(map_vk(0x00), None);
            let vk = 0xF7; // 随便挑一个表里没有的
            assert!(note_unknown_vk(vk), "首次应当提示");
            assert!(!note_unknown_vk(vk), "第二次不应再提示");
        }

        /// 名称查询:已知码出名字,未知码兜底成 Key(N) 而不是空串
        #[test]
        fn name_lookup_falls_back_gracefully() {
            assert_eq!(evdev_name(30), "KEY_A");
            assert_eq!(evdev_name(272), "BTN_LEFT");
            assert_eq!(evdev_name(276), "BTN_EXTRA");
            assert_eq!(evdev_name(9999), "Key(9999)");
        }
    }
}

#[cfg(windows)]
pub fn win_key_name(code: u16) -> String {
    vktable::evdev_name(code)
}

// ============================ Linux 实现 ============================

#[cfg(target_os = "linux")]
mod linux {
    use super::CaptureEvent;
    use crate::{diag_debug, diag_info, diag_warn};
    use anyhow::Result;
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::Sender;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant, SystemTime};

    /// 后台线程心跳(热插拔重扫与按键对账都按它的倍数计)
    const HEARTBEAT_MS: u64 = 100;
    /// 重扫 `/dev/input` 的间隔(心跳数):5 × `HEARTBEAT_MS`(100ms)= **≈500ms**。
    /// 插上键盘到能用之间最多等这么久。
    ///
    /// 2026-10-22 从 20(≈2s)收到 5:2 秒是"插上外接键盘,按两下没反应,以为没插好"
    /// 的量级 —— Windows 侧是消息驱动、插上几乎即时可用,Linux 侧不该差两个数量级的体感。
    /// 代价只有"每 500ms 多扫一次 `/dev/input`":已经接管的节点只做统计、
    /// 已判过"既非键盘也非鼠标"的节点记在 `State::ignored` 里不会重复 open,
    /// 所以一次重扫实际只有 stat + 少数几次 read_dir,量级是 µs。
    /// 按键对账(`RECONCILE_TICKS`)不跟着变:那条要 open/ioctl 设备,400ms 已够。
    const RESCAN_TICKS: u32 = 5;
    /// 与内核对账按键状态的间隔(心跳数):≈400ms。
    ///
    /// 为什么不需要更快:对账只处理"release 丢了"这种异常,而异常多等 400ms
    /// 没有任何影响;但它会 open/ioctl 设备,太快反而增加无谓的系统调用。
    const RECONCILE_TICKS: u32 = 4;

    /// 空转时单次 `poll(2)` 的最长阻塞时间(毫秒)。
    ///
    /// 这个数**不是**事件延迟:设备有事件时内核立刻让 `poll` 返回(本机 uinput
    /// A/B 实测:poll 路径 p50 = 0.002ms,旧的非阻塞+sleep(2ms) 路径 p50 = 1.045ms)。
    /// 它只决定"什么事件都没有时,多久回头重看一次那些非事件状态" ——
    /// 停止标志、`grab` 开关、槽位令牌。20ms = 每秒 50 次空醒(旧实现 500 次/秒)。
    const POLL_TIMEOUT_MS: i32 = 20;

    /// 等这个 fd 变成可读,最多 `timeout_ms` 毫秒。返回 true 表示可以读了。
    ///
    /// 为什么用 `poll` 而不是把设备设成阻塞再 `read`:阻塞 `read` 睡着之后,
    /// "用户关掉映射要放开 grab"这件事只能等到下一次真实按键才被看到 ——
    /// 而设备被独占时**根本不会有下一次按键**。`poll` 带超时同时满足两者:
    /// 有事件立刻醒(零额外延迟),没事件也每 20ms 醒一次看开关。
    ///
    /// `POLLERR` 也算"可读":设备拔掉/复位时内核用错误事件唤醒,让上层走
    /// 既有的读失败计数路径(而不是在这里空等到超时,那会把拔线察觉拖慢 20ms 的倍数)。
    fn wait_readable(fd: i32, timeout_ms: i32) -> bool {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // 被信号打断(EINTR)不是错误,当作"没等到"返回,循环会再次进来。
        unsafe { libc::poll(&mut pfd, 1, timeout_ms) > 0 }
    }

    /// 单个已打开设备在共享状态里的登记项
    struct Slot {
        /// 该设备当前线程的唯一令牌。线程退出时只注销**属于自己的**那一份,
        /// 否则"设备拔掉 → 新设备复用同一节点名"时会把新线程的登记也一起抹掉。
        token: u64,
        keyboard: bool,
        mouse: bool,
    }

    #[derive(Default)]
    struct State {
        alive: HashMap<PathBuf, Slot>,
        /// 已判定为"既不像键盘也不像鼠标"的节点。
        ///
        /// 重扫每 500ms 跑一次,而一台笔记本上这类节点有十几个(电源键、合盖开关、
        /// 视频总线、HDMI 音频…)。不记住它们的话,每次重扫都要把它们全部
        /// `open` 一遍再判定一次,并且往日志里刷一屏 DEBUG —— 既浪费又淹没重点。
        ///
        /// 节点从 `/dev/input` 消失时会把对应条目清掉:拔掉再插上可能复用同一个
        /// 节点名而设备身份已经变了,那时必须重新判定。
        ignored: HashSet<PathBuf>,
        next_token: u64,
        /// 我们已经**上报为按下**的键码集合(含鼠标键)。
        /// 对账就是拿它减去内核真值,差集就是"我们以为按着、内核说没有"的键。
        delivered: HashSet<u16>,
        /// 对账时复用的设备句柄(同一节点第二次 open,只为读 EVIOCGKEY)。
        /// 缓存是为了避免每 400ms 重新 open 一遍;读失败就丢掉,下次重开。
        truth_devs: HashMap<PathBuf, evdev::Device>,
        /// 上次报告过的"漏掉的按下",用于抑制重复日志
        missed_seen: HashSet<u16>,
    }

    type Shared = Arc<Mutex<State>>;

    /// 带容错的加锁:某处 panic 让锁中毒之后,输入捕获必须还能继续工作
    fn lock(st: &Shared) -> std::sync::MutexGuard<'_, State> {
        st.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn start(
        grab: &Arc<AtomicBool>,
        mouse_grab: &Arc<AtomicBool>,
        _cursor_hide: &Arc<AtomicBool>,
        mouse_found: &Arc<AtomicBool>,
        stop: &Arc<AtomicBool>,
        tx: Sender<CaptureEvent>,
    ) -> Result<Option<Arc<std::sync::atomic::AtomicU32>>> {
        let st: Shared = Arc::new(Mutex::new(State::default()));

        // 首次扫描是**同步**的:必须在这一步就把权限问题暴露给调用方,
        // 而不是丢给后台线程慢慢发现(那时用户已经看到一个"看起来正常但按键全无反应"的界面)
        let (kb, mice, denied) = scan(&st, grab, mouse_grab, stop, &tx);
        mouse_found.store(mice > 0, Ordering::Relaxed);

        if kb == 0 && mice == 0 {
            if denied > 0 {
                // 第 16 条:先给"一次性安装"那条路(uaccess 规则),再给老路。
                // 顺序有讲究 —— 加组要注销重登,用户往往漏掉这一步,得到的是
                // "程序起来了但按键全无反应";uaccess 规则装上就生效,不用重登。
                anyhow::bail!(
                    "无权限读取输入设备(/dev/input/event*,共 {} 个节点打不开)。\n\
                     推荐(一次安装,不需注销重登):\n  \
                     sudo install -m 0644 /usr/share/scrcpy-pad/60-scrcpy-pad.rules /etc/udev/rules.d/\n  \
                     sudo udevadm control --reload-rules && sudo udevadm trigger --subsystem-match=input\n\
                     或者传统做法:sudo usermod -aG input $USER 后【注销重新登录】\
                     (本程序必须在重新登录之后启动)。详见诊断日志",
                    denied
                );
            }
            anyhow::bail!("未找到键盘设备(详见诊断日志里的设备清单)");
        }

        // 安装结果确认(2026-10-22,对齐 Windows 侧 W0-11 的做法):
        // Windows 那边会阻塞等钩子线程报回安装结果、超时就落 WARN 说"状态未知";
        // Linux 这边首次扫描是同步的,结果就在手上 —— 所以这里落一条 INFO,
        // 让日志能回答"捕获层到底起没起来、看到几个设备"。
        // 注意 Linux 下 diag_info! 只在 log_level >= info 时落盘(既有设计,不改级别)。
        // 位置刻意放在 kb == 0 && mice == 0 那两个 bail 之后:失败时由 bail 的错误
        // 文案说明原因(里面已带 denied 计数),这里只负责成功路径的确认。
        diag_info!(
            "capture",
            "捕获层已启动:首次扫描到键盘 {kb} 个、鼠标 {mice} 个、打不开的节点 {denied} 个"
        );

        // 后台:热插拔重扫 + 丢失 release 的自愈
        {
            let st = st.clone();
            let grab = grab.clone();
            let mouse_grab = mouse_grab.clone();
            let mouse_found = mouse_found.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                supervisor(st, grab, mouse_grab, mouse_found, stop, tx);
            });
        }
        Ok(None)
    }

    /// 扫描 `/dev/input` 并接管所有"新出现"的键盘/鼠标设备。
    ///
    /// 返回 (键盘数, 鼠标数, 打不开的节点数)。**三个数都要**,因为
    /// "一个都没打开"与"部分打不开"是两种完全不同的故障,日志要能区分。
    fn scan(
        st: &Shared,
        grab: &Arc<AtomicBool>,
        mouse_grab: &Arc<AtomicBool>,
        stop: &Arc<AtomicBool>,
        tx: &Sender<CaptureEvent>,
    ) -> (usize, usize, usize) {
        let mut keyboards = 0usize;
        let mut mice = 0usize;
        let mut denied = 0usize;

        // 排序已在 `event_paths` 里按稳定身份做完(第 15 条),这里不再按路径名排
        let paths = event_paths();
        // 先把"已经不存在的忽略项"清掉,再决定本轮的跳过集合
        {
            let present: HashSet<&PathBuf> = paths.iter().collect();
            lock(st).ignored.retain(|p| present.contains(p));
        }
        for path in paths {
            // 已接管:只统计(设备身份不变时不要重复开线程,否则一次按键会被上报两遍)
            let known = {
                let s = lock(st);
                if s.ignored.contains(&path) {
                    continue;
                }
                s.alive.get(&path).map(|slot| (slot.keyboard, slot.mouse))
            };
            if let Some((k, m)) = known {
                keyboards += usize::from(k);
                mice += usize::from(m);
                continue;
            }

            let device = match evdev::Device::open(&path) {
                Ok(d) => d,
                Err(e) => {
                    denied += 1;
                    // 权限与"设备已消失"要分开说:前者要用户去改组,后者是正常竞态
                    let hint = match e.kind() {
                        std::io::ErrorKind::PermissionDenied => {
                            " → 当前用户不在 input 组,或未重新登录"
                        }
                        _ => "",
                    };
                    diag_warn!(
                        "capture",
                        "打不开 {}: {e} (errno {:?}){hint}",
                        path.display(),
                        e.raw_os_error()
                    );
                    continue;
                }
            };

            let name = device.name().unwrap_or("(无名)").to_string();
            let id = device.input_id();
            let is_kb = is_keyboard(&device);
            let is_mouse_dev = is_mouse(&device);
            let key_count = device
                .supported_keys()
                .map(|k| k.iter().count())
                .unwrap_or(0);

            if !is_kb && !is_mouse_dev {
                // 只有排查时才需要看这些"路过"的设备;记进忽略集合,
                // 后续重扫不再重复 open 与记录
                diag_debug!(
                    "capture",
                    "跳过 {} 「{name}」{id:?} 键数={key_count} (既不像键盘也不像鼠标)",
                    path.display()
                );
                lock(st).ignored.insert(path.clone());
                continue;
            }
            if let Err(e) = device.set_nonblocking(true) {
                diag_warn!("capture", "设置非阻塞失败 {}: {e}", path.display());
                continue;
            }

            let token = {
                let mut s = lock(st);
                // 先把令牌算出来再借用 `alive`:直接在字段初始化式里读
                // `s.next_token` 会与外层的可变借用冲突
                let token = s.next_token + 1;
                s.next_token = token;
                s.alive.insert(
                    path.clone(),
                    Slot {
                        token,
                        keyboard: is_kb,
                        mouse: is_mouse_dev,
                    },
                );
                token
            };
            // 第 15 条:日志里同时给出**稳定身份**(by-id 链接名),这样两份日志
            // 比对时不会因为 eventN 换号而误判"设备换了"。
            let stable = by_id_map()
                .get(&path.file_name().map(|n| n.to_owned()).unwrap_or_default())
                .cloned()
                .unwrap_or_else(|| "(无 by-id)".to_string());
            diag_info!(
                "capture",
                "已接管 {} 「{name}」{id:?} 键数={key_count} 键盘={is_kb} 鼠标={is_mouse_dev} 身份={stable}",
                path.display()
            );
            if is_kb {
                keyboards += 1;
            }
            if is_mouse_dev {
                mice += 1;
                // 热插鼠标必须让 FPS 面板立刻知道,否则用户只能重启程序
                // (界面上的"检测到鼠标设备"直接读这个标志)
            }

            let grab_flag = if is_kb {
                grab.clone()
            } else {
                mouse_grab.clone()
            };
            let st2 = st.clone();
            let tx2 = tx.clone();
            let stop2 = stop.clone();
            std::thread::spawn(move || {
                // evdev 读取线程 = Linux 版"钩子线程":每台设备一条,事件即读即转。
                crate::priority::boost(crate::priority::Class::Highest);
                device_loop(
                    device,
                    path,
                    token,
                    tx2,
                    grab_flag,
                    stop2,
                    st2,
                    is_mouse_dev,
                );
            });
        }
        (keyboards, mice, denied)
    }

    /// `/dev/input` 下的所有 event 节点,按**稳定身份**排序(第 15 条)。
    ///
    /// 为什么不能直接按路径名排:`event10` 的字典序在 `event2` 前面,而
    /// `eventN` 里的 N 是**分配序号** —— 拔掉再插回来可能变成另一个号。
    /// 于是"先接管谁、后接管谁"每次热插拔都在变,日志里的设备顺序也跟着跳,
    /// 比对两份日志时会以为"设备换了"。
    ///
    /// 稳定身份取 `/dev/input/by-id/<link>` 这个 udev 符号链接的名字:它带
    /// 厂商/型号/序列号(如 `usb-Logitech_G502_12345678-event-mouse`),
    /// 换号也不变。**没有** by-id 链接的设备(虚拟设备、部分 I2C 设备,
    /// 例如触摸板)退回设备名 `EVIOCGNAME`,再退回路径 —— 三层都只是为了
    /// 排序与日志可读,不改变任何接管判定。
    fn event_paths() -> Vec<PathBuf> {
        let Ok(rd) = std::fs::read_dir("/dev/input") else {
            return Vec::new();
        };
        let mut paths: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().starts_with("event"))
                    .unwrap_or(false)
            })
            .collect();
        let ids = by_id_map();
        sort_by_identity(&mut paths, &ids);
        paths
    }

    /// 按稳定身份排序(第 15 条)。抽成独立函数是为了能被单测直接喂假数据 ——
    /// `/dev/input/by-id` 的内容随机器而变,测试不能依赖真实宿主。
    fn sort_by_identity(paths: &mut [PathBuf], ids: &HashMap<std::ffi::OsString, String>) {
        paths.sort_by_key(|p| {
            let name = p.file_name().map(|n| n.to_owned()).unwrap_or_default();
            ids.get(&name)
                .cloned()
                .unwrap_or_else(|| name.to_string_lossy().into_owned())
        });
    }

    /// `eventN -> by-id 链接名` 的反查表(一次 `read_dir`,失败就是空表)。
    ///
    /// by-id 里的链接指向 `../../eventN`,而 `fs::read_link` 给的正是这个相对
    /// 路径,所以只取它的文件名就能还原出 eventN —— 不需要 `canonicalize`
    /// (那会多一堆系统调用,而且在容器/权限收紧时会失败)。
    fn by_id_map() -> HashMap<std::ffi::OsString, String> {
        parse_by_id(std::path::Path::new("/dev/input/by-id"))
    }

    /// [`by_id_map`] 的可测内核:目录路径是参数,单测可以喂临时目录。
    ///
    /// 只负责**读目录**(顺序不可控),收敛逻辑全在 [`reduce_by_id`] 这个纯函数里。
    /// 这样"结果与读取顺序无关"这件事可以被单测直接证明,而不是靠祈祷。
    fn parse_by_id(dir: &std::path::Path) -> HashMap<std::ffi::OsString, String> {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return HashMap::new();
        };
        let mut entries: Vec<(String, String)> = Vec::new();
        for entry in rd.flatten() {
            let link_name = entry.file_name().to_string_lossy().into_owned();
            let Ok(target) = std::fs::read_link(entry.path()) else {
                continue;
            };
            let Some(node) = target.file_name() else {
                continue;
            };
            entries.push((link_name, node.to_string_lossy().into_owned()));
        }
        reduce_by_id(&entries)
    }

    /// 把 `(链接名, 目标节点名)` 列表收敛成 `eventN -> 链接名`(纯函数)。
    ///
    /// **结果与输入顺序无关** —— 这是"稳定身份"的全部意义:同一台机器每次启动
    /// 必须在日志里报同一个身份名,否则按身份排查热插拔就失去意义。
    ///
    /// 同一台设备常有 `-event-mouse` / `-event-kbd` / `-event-joystick` 多条链接,
    /// 它们**全都指向同一个 eventN**;此外 hidraw 别名也可能指到 eventN 上。
    /// 旧实现是"后者覆盖前者"⇒ 取哪条取决于 `read_dir` 顺序 ⇒ 每次启动可能不同。
    /// 2026-10-22 在 WSL Ubuntu-20.04 上首次真跑 Linux 全量测试时,正是这一条
    /// 被 T3 抓红(期望 `-event-kbd`,实际拿到另一条别名)。
    ///
    /// 现在的取舍是**确定性的**:链接名必须含 `event`(排除反常命名),同节点取
    /// 字典序最小。
    fn reduce_by_id(entries: &[(String, String)]) -> HashMap<std::ffi::OsString, String> {
        let mut out: HashMap<std::ffi::OsString, String> = HashMap::new();
        for (link_name, node) in entries {
            if !node.starts_with("event") || !link_name.contains("event") {
                continue;
            }
            let key = std::ffi::OsString::from(node);
            match out.get(&key) {
                Some(prev) if prev.as_str() <= link_name.as_str() => {}
                _ => {
                    out.insert(key, link_name.clone());
                }
            }
        }
        out
    }

    /// 判定"这个节点是键盘"。
    ///
    /// 主判据(字母区 A/Z + 回车)沿用旧实现:一把键盘会同时产生好几个
    /// input 节点(主键区、多媒体、系统控制…),而按键矩阵只落在其中一个上,
    /// 靠"有没有字母键"能把那个节点挑出来。
    ///
    /// 兜底判据是为少数可编程/宏键盘加的:它们可能把字母区拆到不同节点。
    /// 要求同时具备 A + 空格 + 回车 + 足够多的键 —— 遥控器、数字小键盘、
    /// 手柄(BTN_*)都不会同时满足。
    fn is_keyboard(device: &evdev::Device) -> bool {
        let Some(k) = device.supported_keys() else {
            return false;
        };
        if k.contains(evdev::KeyCode::KEY_A)
            && k.contains(evdev::KeyCode::KEY_Z)
            && k.contains(evdev::KeyCode::KEY_ENTER)
        {
            return true;
        }
        k.iter().count() >= 30
            && k.contains(evdev::KeyCode::KEY_A)
            && k.contains(evdev::KeyCode::KEY_SPACE)
            && k.contains(evdev::KeyCode::KEY_ENTER)
    }

    /// 具备 REL_X / REL_Y 相对轴的设备按鼠标处理。
    ///
    /// 注意:现代笔记本触摸板走 ABS 多点触控协议,**没有相对轴**,
    /// 因此不会被认作鼠标(FPS 瞄准用不了)。这是有意的:触摸板的绝对坐标
    /// 需要另一套"手指位移 -> 视角"的模型,不能直接套用相对位移的灵敏度。
    fn is_mouse(device: &evdev::Device) -> bool {
        device
            .supported_relative_axes()
            .map(|a| {
                a.contains(evdev::RelativeAxisCode::REL_X)
                    && a.contains(evdev::RelativeAxisCode::REL_Y)
            })
            .unwrap_or(false)
    }

    /// 把 evdev 事件自带的时间戳折算到进程单调时钟(W1-1)。
    ///
    /// evdev 的时间戳默认走 CLOCK_REALTIME(与 `SystemTime` 同基准),而引擎的
    /// 排程与归中判断用的是 `Instant`。这里以"**本批**事件的读取时刻"为锚做一次
    /// 减法:不需要额外的时钟系统调用,也不会跨批次积累漂移(每批都重新锚定,
    /// realtime 被 NTP 微调也被吸收在锚点里)。时间戳落到未来时(时钟被向后调整)
    /// 夹到锚点,不用负偏移 —— 宁可当作"刚刚"。
    fn event_time(ev: &evdev::InputEvent, sys_anchor: SystemTime, inst_anchor: Instant) -> Instant {
        let behind = sys_anchor
            .duration_since(ev.timestamp())
            .unwrap_or_default();
        inst_anchor.checked_sub(behind).unwrap_or(inst_anchor)
    }

    /// 单设备读取循环
    #[allow(clippy::too_many_arguments)]
    fn device_loop(
        mut device: evdev::Device,
        path: PathBuf,
        token: u64,
        tx: Sender<CaptureEvent>,
        grab_flag: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
        st: Shared,
        mouse: bool,
    ) {
        let mut grabbed = false;
        // 设备名先取成 owned 字符串:`fetch_events` 期间 `device` 是可变借用的,
        // 那时再调 `device.name()`(不可变借用)会冲突。
        let dev_name = device.name().unwrap_or("(无名)").to_string();
        // 连续多少次"非 WouldBlock"的失败后放弃这个设备。
        // 不能一失败就退出:USB 短暂复位后同一个 fd 有可能恢复;
        // 但也不能永远空转 —— 旧实现就是在这里 50ms 一次无限循环,
        // 设备早没了却还占着线程,而且一声不吭。
        let mut hard_errors = 0u32;
        // 第 19 条:把"每 2ms 醒一次问内核"换成"让内核在真有事件时叫醒我们"。
        // `poll` 需要原始 fd;`evdev::Device` 实现了 `AsRawFd`,拿走一次即可
        // (fd 的所有权仍在 `device` 手里)。
        let raw_fd = {
            use std::os::unix::io::AsRawFd;
            device.as_raw_fd()
        };
        while !stop.load(Ordering::Relaxed) {
            if token != lock(&st).alive.get(&path).map(|s| s.token).unwrap_or(0) {
                break; // 已被更新的线程接管
            }
            let want = grab_flag.load(Ordering::Relaxed);
            if want != grabbed {
                let r = if want { device.grab() } else { device.ungrab() };
                if r.is_ok() {
                    grabbed = want;
                    diag_info!(
                        "capture",
                        "{} {} 「{}」",
                        if want { "已独占" } else { "已放开" },
                        path.display(),
                        &dev_name
                    );
                }
            }
            // 先等可读(最多 POLL_TIMEOUT_MS),再做非阻塞读。
            // 这一句就是全部改动的核心:旧代码是"立刻非阻塞读 → WouldBlock →
            // sleep(2ms)",现在是"让内核在事件到达时把我叫醒"。
            if !wait_readable(raw_fd, POLL_TIMEOUT_MS) {
                continue; // 超时/被信号打断:回去重看停止标志与 grab 开关
            }
            match device.fetch_events() {
                Ok(events) => {
                    hard_errors = 0;
                    let mut dx = 0f32;
                    let mut dy = 0f32;
                    // W1-1:整批共用一个锚,把每条事件的时间戳折算成 Instant
                    let sys_anchor = SystemTime::now();
                    let inst_anchor = Instant::now();
                    // 位移是整批聚合后再发的,记下最后一条相对轴事件的时刻
                    let mut last_axis_at = inst_anchor;
                    for ev in events {
                        let at = event_time(&ev, sys_anchor, inst_anchor);
                        match ev.destructure() {
                            evdev::EventSummary::Key(_, key, value) => {
                                // value: 0=抬起 1=按下 2=自动重复
                                if value == 2 {
                                    // 自动重复不转发(引擎用上升沿收口),但它证明
                                    // "这个键此刻确实按着",是天然的心跳,留着无坏处
                                    continue;
                                }
                                let pressed = value == 1;
                                {
                                    let mut s = lock(&st);
                                    if pressed {
                                        s.delivered.insert(key.0);
                                    } else {
                                        s.delivered.remove(&key.0);
                                    }
                                }
                                let _ = tx.send(CaptureEvent::Button {
                                    code: key.0,
                                    pressed,
                                    at,
                                });
                            }
                            evdev::EventSummary::RelativeAxis(_, axis, value) if mouse => {
                                last_axis_at = at;
                                if axis == evdev::RelativeAxisCode::REL_X {
                                    dx += value as f32;
                                } else if axis == evdev::RelativeAxisCode::REL_Y {
                                    dy += value as f32;
                                } else if axis == evdev::RelativeAxisCode::REL_WHEEL
                                    || axis == evdev::RelativeAxisCode::REL_HWHEEL
                                {
                                    if value != 0 {
                                        let code = match (axis, value.is_positive()) {
                                            (evdev::RelativeAxisCode::REL_WHEEL, true) => {
                                                crate::keymap::BTN_WHEEL_UP
                                            }
                                            (evdev::RelativeAxisCode::REL_WHEEL, false) => {
                                                crate::keymap::BTN_WHEEL_DOWN
                                            }
                                            (evdev::RelativeAxisCode::REL_HWHEEL, true) => {
                                                crate::keymap::BTN_WHEEL_RIGHT
                                            }
                                            _ => crate::keymap::BTN_WHEEL_LEFT,
                                        };
                                        for _ in 0..value.unsigned_abs().min(10) {
                                            let _ = tx.send(CaptureEvent::Button {
                                                code,
                                                pressed: true,
                                                at,
                                            });
                                            let _ = tx.send(CaptureEvent::Button {
                                                code,
                                                pressed: false,
                                                at,
                                            });
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    if dx != 0.0 || dy != 0.0 {
                        let _ = tx.send(CaptureEvent::Motion {
                            dx,
                            dy,
                            at: last_axis_at,
                        });
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // 第 19 条之后这里已经不该发生:`poll` 说可读才进来读,
                    // 而 evdev 的读是整批语义。真出现了也不 sleep —— 立刻回
                    // 循环顶部重新 `poll`,否则一次竞态就白担 2ms 延迟。
                    // (旧的 `sleep(2ms)` 是整条链路里最大的一段可消除延迟。)
                }
                Err(e) => {
                    hard_errors += 1;
                    if hard_errors == 1 || hard_errors == 20 {
                        diag_warn!(
                            "capture",
                            "读取 {} 出错(第 {} 次): {e} (errno {:?})",
                            path.display(),
                            hard_errors,
                            e.raw_os_error()
                        );
                    }
                    if hard_errors >= 20 {
                        // 设备确实没了:登记注销,让重扫有机会重新接管。
                        // 注销时校验令牌,避免把复用同一节点名的新设备登记抹掉。
                        {
                            let mut s = lock(&st);
                            if s.alive.get(&path).map(|x| x.token) == Some(token) {
                                s.alive.remove(&path);
                                s.truth_devs.remove(&path);
                            }
                        }
                        diag_warn!(
                            "capture",
                            "放弃 {} 「{}」(连续 {hard_errors} 次读取失败;它被拔掉或复位了,重扫会重新接管)",
                            path.display(),
                            &dev_name
                        );
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
        if grabbed {
            let _ = device.ungrab();
        }
    }

    /// 后台监管线程:热插拔重扫 + 丢失 release 的自愈
    fn supervisor(
        st: Shared,
        grab: Arc<AtomicBool>,
        mouse_grab: Arc<AtomicBool>,
        mouse_found: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
        tx: Sender<CaptureEvent>,
    ) {
        let mut tick: u32 = 0;
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(HEARTBEAT_MS));
            if stop.load(Ordering::Relaxed) {
                break;
            }
            tick = tick.wrapping_add(1);
            if tick % RECONCILE_TICKS == 0 {
                reconcile_keys(&st, &tx);
            }
            if tick % RESCAN_TICKS == 0 {
                let (kb, mice, denied) = scan(&st, &grab, &mouse_grab, &stop, &tx);
                let had = mouse_found.load(Ordering::Relaxed);
                mouse_found.store(mice > 0, Ordering::Relaxed);
                if had && mice == 0 {
                    diag_warn!(
                        "capture",
                        "鼠标设备已全部消失(FPS 瞄准将不可用),等待重新插入"
                    );
                } else if !had && mice > 0 {
                    diag_info!("capture", "检测到鼠标设备({mice} 个),FPS 瞄准现在可用");
                }
                if denied > 0 && kb == 0 && mice == 0 {
                    diag_warn!("capture", "当前没有任何可用输入设备({denied} 个节点打不开)");
                }
            }
        }
        diag_info!("capture", "捕获层监管线程退出");
    }

    /// 与内核真值对账:把"我们以为按着、内核说已经松开"的键补发一次抬起。
    ///
    /// 为什么必须做:底层一旦漏掉一个 KeyRelease(设备被拔、USB 复位、
    /// 事件缓冲区溢出…),引擎侧那个键就会永远留在按下状态 ——
    /// 既占着一个永远抬不起来的幻影触点(设备端触点池一共只有 10 个),
    /// 又让"上升沿"永远不再产生(开关型点按/滑动彻底失效)。
    /// 用户看到的就是"过一会儿某个键就没反应了,非得重新开一次映射"。
    ///
    /// 只补发**抬起**,不补发按下:补发按下会让一次性动作(点按、滑动)
    /// 在用户没碰键盘的时候凭空触发。而"漏掉的按下"我们只报告不动作 ——
    /// 下一次真实按键会正常补上。
    fn reconcile_keys(st: &Shared, tx: &Sender<CaptureEvent>) {
        let (paths, delivered, missed_before) = {
            let s = lock(st);
            if s.delivered.is_empty() && s.missed_seen.is_empty() {
                return; // 没有任何"我们认为按着"的键,无需打扰内核
            }
            (
                s.alive.keys().cloned().collect::<Vec<_>>(),
                s.delivered.clone(),
                s.missed_seen.clone(),
            )
        };

        // 一个设备都没有 = 全被拔了:真值就是"什么都没按着"。
        // 注意"列表非空但都打不开"要区分对待 —— 那是暂时的读失败,不能据此清空。
        if paths.is_empty() {
            if !delivered.is_empty() {
                diag_warn!(
                    "capture",
                    "所有输入设备均已消失,补发 {} 个抬起以防卡键: {}",
                    delivered.len(),
                    names(delivered.iter().copied())
                );
                release_all(st, tx, delivered.iter().copied());
            }
            return;
        }

        let mut truth: HashSet<u16> = HashSet::new();
        let mut read_ok = false;
        {
            let mut s = lock(st);
            for path in &paths {
                // 先确保有句柄(不存在则开一个),再取不可变引用去读。
                // 不能边持有引用边 `entry().or_insert()` —— 同一字段的
                // 不可变借用与可变借用会冲突。
                if !s.truth_devs.contains_key(path) {
                    match evdev::Device::open(path) {
                        Ok(d) => {
                            s.truth_devs.insert(path.clone(), d);
                        }
                        Err(e) => {
                            diag_debug!("capture", "对账时打不开 {}: {e}", path.display());
                            continue;
                        }
                    }
                }
                let mut drop_handle = false;
                {
                    let Some(dev) = s.truth_devs.get(path) else {
                        continue;
                    };
                    match dev.get_key_state() {
                        Ok(keys) => {
                            read_ok = true;
                            for k in keys.iter() {
                                truth.insert(k.0);
                            }
                        }
                        Err(e) => {
                            diag_debug!(
                                "capture",
                                "读 {} 的按键状态失败({e}),丢弃句柄待下次重开",
                                path.display()
                            );
                            drop_handle = true;
                        }
                    }
                }
                if drop_handle {
                    s.truth_devs.remove(path);
                }
            }
        }
        if !read_ok {
            return; // 一个都读不到:宁可不动作,也不要凭空抬起一堆键
        }

        let lost: Vec<u16> = delivered
            .iter()
            .copied()
            .filter(|c| !truth.contains(c))
            .collect();
        if !lost.is_empty() {
            diag_warn!(
                "capture",
                "权威对账:内核认为这些键已抬起,但我们从未收到 release —— 补发抬起以防卡键: {}",
                names(lost.iter().copied())
            );
            release_all(st, tx, lost.iter().copied());
        }

        // 反向(内核说按着、我们没记录)只报告不动作,且只在集合变化时报告一次,
        // 否则会每 400ms 刷一行。
        let missed: HashSet<u16> = truth
            .iter()
            .copied()
            .filter(|c| !delivered.contains(c))
            .collect();
        if missed != missed_before {
            if !missed.is_empty() {
                diag_warn!(
                    "capture",
                    "权威对账:这些键内核认为正按着,但我们没收到按下事件(该次按下已丢失): {}",
                    names(missed.iter().copied())
                );
            }
            lock(st).missed_seen = missed;
        }
    }

    /// 补发抬起并同步本地"已上报"集合
    fn release_all<I: IntoIterator<Item = u16>>(st: &Shared, tx: &Sender<CaptureEvent>, codes: I) {
        for code in codes {
            // 合成事件:没有"事件时刻",按发出时刻计(W1-1)
            let _ = tx.send(CaptureEvent::Button {
                code,
                pressed: false,
                at: Instant::now(),
            });
            lock(st).delivered.remove(&code);
        }
    }

    /// 键码列表 -> 可读名(日志里"17 31"远不如"KEY_W KEY_S"好认)
    fn names<I: IntoIterator<Item = u16>>(codes: I) -> String {
        let mut v: Vec<String> = codes.into_iter().map(crate::keymap::key_name).collect();
        v.sort();
        v.join(" ")
    }

    #[cfg(test)]
    mod tests {
        use super::{POLL_TIMEOUT_MS, parse_by_id, reduce_by_id, sort_by_identity, wait_readable};

        /// T1(第 19 条):`wait_readable` 必须在"有数据"时立刻返回、在"没数据"时
        /// 按超时返回。用 pipe 造这两种状态 —— 不依赖任何真实输入设备,
        /// 因此这条测试在无 `/dev/input` 的 CI 上也能跑。
        ///
        /// 阳性对照:先往管子里写一个字节 ⇒ 必须返回 true;
        /// 阴性对照:空管子且超时 30ms ⇒ 必须返回 false,且**确实等过**
        /// (否则"立刻返回 false"的实现也能骗过这条断言,那就成了假绿测试)。
        #[test]
        fn poll_wait_reports_readiness_and_honours_the_timeout() {
            // 用真正的 pipe:`UnixStream::pair()` 是双向的,对它 `poll(POLLIN)`
            // 监听的是"对端写了什么",而自己写自己读是另一回事 —— 先前正是
            // 用错对象导致阳性对照红(libc 的 pipe2 在 Linux 上必然可用)。
            let mut fds = [0i32; 2];
            let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
            assert_eq!(rc, 0, "pipe2 失败: {}", std::io::Error::last_os_error());
            let (r, w) = (fds[0], fds[1]);

            // 空:必须等到超时,而且确实花了时间
            let t0 = std::time::Instant::now();
            let ready = wait_readable(r, 30);
            let waited = t0.elapsed();
            assert!(!ready, "空管子不该被判为可读");
            assert!(
                waited >= std::time::Duration::from_millis(25),
                "空管子必须等满超时才对(实际只用了 {waited:?})—— \
                 说明 `wait_readable` 没有真的阻塞,而是立刻返回了 false"
            );

            // 有数据:必须立刻返回 true
            let one = [b'x'; 1];
            let n = unsafe { libc::write(w, one.as_ptr().cast(), 1) };
            assert_eq!(n, 1, "写 pipe 失败");
            let t0 = std::time::Instant::now();
            let ready = wait_readable(r, 500);
            let waited = t0.elapsed();
            assert!(ready, "有数据时必须报告可读");
            assert!(
                waited < std::time::Duration::from_millis(100),
                "有数据时不该等到超时(实际等了 {waited:?})"
            );
            unsafe {
                libc::close(r);
                libc::close(w);
            }
        }

        /// T2(第 19 条):超时值必须远小于"按键可感知的延迟",同时又不能是 0
        /// (0 = 纯轮询,CPU 空转;旧实现就是每 2ms 一次)。
        ///
        /// 这条锁住的是取舍本身:有人把它改成 0 或改成 500ms 都会红。
        #[test]
        fn poll_timeout_is_a_poll_not_a_spin_and_bounded() {
            assert!(
                (5..=100).contains(&POLL_TIMEOUT_MS),
                "POLL_TIMEOUT_MS={POLL_TIMEOUT_MS} 超出 [5,100]:\
                 0~4 等于忙轮询(旧实现 2ms 一轮,每秒 500 次空醒),\
                 >100ms 会把'关映射后放开 grab'拖慢到用户能察觉"
            );
        }

        /// T3(第 15 条):by-id 反查表必须只收 event 节点、丢掉 hidraw/mouse 别名,
        /// 并且同一个节点有多条链接时只留一条。
        ///
        /// 用一个临时目录伪造 `/dev/input/by-id` 的**形状**(符号链接指向
        /// `../../eventN`),验证解析逻辑——不需要真实设备。
        #[test]
        fn by_id_map_only_keeps_event_nodes() {
            let dir = std::env::temp_dir().join(format!("spl-byid-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("mkdir");
            // 【真实形态】一台设备有多个能力时,udev 会给**同一个 eventN** 建多条
            // by-id 链接,例如同时支持鼠标+摇杆的设备:
            //   usb-X-event-mouse  -> ../../event7
            //   usb-X-event-joystick -> ../../event7
            // 旧实现"后者覆盖前者"⇒ 取哪条取决于 read_dir 顺序 ⇒ 日志里的"身份="
            // 每次启动可能变,与"稳定身份"的初衷正好相反。2026-10-22 在 WSL 20.04
            // 上实跑全量测试时被 T3 抓到(期望 -event-kbd,实际拿到另一条别名)。
            std::os::unix::fs::symlink("../../event7", dir.join("usb-Kbd_ABC-event-kbd"))
                .expect("symlink1");
            std::os::unix::fs::symlink("../../event7", dir.join("usb-Kbd_ABC-event-joystick"))
                .expect("symlink2");
            // 【合成形态,只作防御】链接名里没有 event 字样却指向 event 节点。
            // 真实 udev 不会这样命名(hidraw 别名指向 hidrawN),但既然判据是
            // "目标是不是 event 节点",就不该让一个名字反常的链接污染身份表。
            std::os::unix::fs::symlink("../../event9", dir.join("a-hidraw")).expect("symlink3");
            std::os::unix::fs::symlink("../../event9", dir.join("z-event")).expect("symlink4");
            // 目标不是 event 节点的,整条丢掉
            std::os::unix::fs::symlink("../../hidraw3", dir.join("usb-Other-hidraw"))
                .expect("symlink5");

            let map = parse_by_id(&dir);
            assert_eq!(
                map.len(),
                2,
                "只应该有 2 个 event 节点条目,实际 {map:?} —— \
                 hidraw 别名或重复别名漏进来了"
            );
            // 同一节点多条合法别名 ⇒ 必须取**字典序最小**(确定性),不是"最后读到的"
            assert_eq!(
                map.get(&std::ffi::OsString::from("event7"))
                    .map(String::as_str),
                Some("usb-Kbd_ABC-event-joystick"),
                "同一节点的多条别名没做确定性取舍(joystick < kbd,应取前者)"
            );
            // 名字反常的别名不许赢
            assert_eq!(
                map.get(&std::ffi::OsString::from("event9"))
                    .map(String::as_str),
                Some("z-event"),
                "选了名字里没有 event 的别名 ⇒ 没有按链接名过滤"
            );
            // 确定性:同一目录反复解析必须给出同一结果(read_dir 顺序不保证)
            for _ in 0..20 {
                assert_eq!(parse_by_id(&dir), map, "解析结果不稳定(read_dir 顺序敏感)");
            }
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// T3b:收敛逻辑必须**与输入顺序无关**(纯函数层直接证)。
        ///
        /// 为什么不能只靠上面那条:它在真实临时目录上跑,而 `read_dir` 在多数
        /// 文件系统上恰好按名字顺序返回 ⇒ "覆盖写"与"取字典序最小"会碰巧同解,
        /// 于是把实现改回覆盖写**测试不会变红**(2026-10-22 实测踩到:变异后仍
        /// 1 passed)。守护测试必须能对实现敏感,否则它保护的是文件系统而不是代码。
        ///
        /// 这里直接喂**两种相反顺序**的同一批条目,判据是"两次结果必须相同",
        /// 覆盖写实现无论顺序如何都会在某个顺序下给出另一条别名 ⇒ 必红。
        #[test]
        fn reduce_by_id_is_order_independent() {
            let fwd = vec![
                (
                    "usb-Kbd_ABC-event-joystick".to_string(),
                    "event7".to_string(),
                ),
                ("usb-Kbd_ABC-event-kbd".to_string(), "event7".to_string()),
                ("a-hidraw".to_string(), "event9".to_string()),
                ("z-event".to_string(), "event9".to_string()),
                ("usb-Other-hidraw".to_string(), "hidraw3".to_string()),
            ];
            let mut rev = fwd.clone();
            rev.reverse();
            let a = reduce_by_id(&fwd);
            let b = reduce_by_id(&rev);
            assert_eq!(
                a, b,
                "同一批条目换个读取顺序结果就变了 —— 身份名会随启动漂移"
            );
            // 两种顺序都必须收敛到同一个确定答案
            assert_eq!(
                a.get(&std::ffi::OsString::from("event7"))
                    .map(String::as_str),
                Some("usb-Kbd_ABC-event-joystick"),
                "同节点多别名没有取字典序最小"
            );
            assert_eq!(
                a.get(&std::ffi::OsString::from("event9"))
                    .map(String::as_str),
                Some("z-event"),
                "名字不含 event 的别名不该赢"
            );
            assert_eq!(a.len(), 2, "hidraw 目标节点漏进表了: {a:?}");
        }

        /// T4(第 15 条):排序必须按**稳定身份**而不是路径名。
        ///
        /// 这是"热插拔换号后日志顺序不乱跳"的可执行表述:event2/event10 的
        /// 字典序会给出 10 < 2(错),而按 by-id 名字排是稳定的。
        #[test]
        fn ordering_follows_stable_identity_not_node_number() {
            // event10 的 by-id 名以 "a-" 开头 ⇒ 它必须排在 event2("z-")之前,
            // 而按路径名字典序 event10 也确实在 event2 之前 —— 所以这里反过来
            // 构造,让两种排法给出不同结果,才能区分实现。
            let mut ids = std::collections::HashMap::new();
            ids.insert(std::ffi::OsString::from("event2"), "a-First".to_string());
            ids.insert(std::ffi::OsString::from("event10"), "z-Last".to_string());
            let mut paths = vec![
                std::path::PathBuf::from("/dev/input/event10"),
                std::path::PathBuf::from("/dev/input/event2"),
            ];
            sort_by_identity(&mut paths, &ids);
            assert_eq!(
                paths[0],
                std::path::PathBuf::from("/dev/input/event2"),
                "排序没按 by-id 身份走(按路径名字典序 event10 会在前面)"
            );
        }
    }
}

// ============================ Windows 实现 ============================

#[cfg(windows)]
mod windows {
    use super::{
        BoundKeys, CaptureEvent, FALLBACK_MAX, HB_MISS_LIMIT, HB_STALL_MS, HbVerdict,
        MotionCoalescer, ProbeStep, fallback_reinstall_due, hb_verdict, keyboard_swallows,
        mouse_shield_swallows, probe_step, stall_hit, swallow_bit, vktable, wheel_notches,
    };
    // B21-3/G-c:光标钉住的**决策**在模块顶层(纯函数,单测覆盖),这里只消费它 ——
    // 真正的 `ClipCursor` 调用集中在 `clip_cursor_to`/`clip_cursor_release`。
    use super::{ClipAction, clip_step, should_clip};
    use crate::keymap::{BTN_WHEEL_DOWN, BTN_WHEEL_LEFT, BTN_WHEEL_RIGHT, BTN_WHEEL_UP};
    use crate::{diag_debug, diag_error, diag_info, diag_warn};
    use anyhow::Result;
    use std::collections::HashSet;
    use std::sync::atomic::{
        AtomicBool, AtomicI32, AtomicPtr, AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering,
    };
    use std::sync::mpsc::{Sender, channel};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
    };
    use windows_sys::Win32::UI::Input::{
        GetRawInputData, HRAWINPUT, MOUSE_MOVE_ABSOLUTE, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER,
        RID_INPUT, RIDEV_INPUTSINK, RIDEV_REMOVE, RIM_TYPEMOUSE, RegisterRawInputDevices,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, ClipCursor, CreateCursor, CreateWindowExW, DefWindowProcW, DestroyWindow,
        DispatchMessageW, GetCursorPos, GetForegroundWindow, GetMessageW, GetSystemMetrics,
        GetWindowThreadProcessId, HC_ACTION, IDC_ARROW, KBDLLHOOKSTRUCT, LLKHF_EXTENDED,
        LLKHF_INJECTED, LoadCursorW, MSG, MSLLHOOKSTRUCT, OCR_APPSTARTING, OCR_CROSS, OCR_HAND,
        OCR_HELP, OCR_IBEAM, OCR_NO, OCR_NORMAL, OCR_SIZEALL, OCR_SIZENESW, OCR_SIZENS,
        OCR_SIZENWSE, OCR_SIZEWE, OCR_UP, OCR_WAIT, PostThreadMessageW, RegisterClassW,
        SM_CXSCREEN, SM_CYSCREEN, SPI_SETCURSORS, SPIF_SENDCHANGE, SYSTEM_CURSOR_ID, SetCursor,
        SetCursorPos, SetSystemCursor, SetWindowsHookExW, SystemParametersInfoW,
        UnhookWindowsHookEx, UnregisterClassW, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_APP, WM_INPUT,
        WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP,
        WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN,
        WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
        WS_POPUP, WindowFromPoint,
    };

    /// 抓取鼠标时,光标离屏幕中心超过该比例(相对显示器短边)才拉回中心。
    ///
    /// 旧实现是固定 64px —— 在 1080p 上,1600 DPI 的鼠标一次快速甩动就能走
    /// 50~100px,于是几乎**每个事件**都触发回中,而每次回中都会吃掉/反转
    /// 一次真实位移,表现就是"视角能动但幅度极小、一卡一卡"。
    const RECENTER_RATIO: i32 = 4; // 短边的 1/4
    /// 回中阈值的下限(小显示器/小窗口时别退化成"每帧回中")
    const RECENTER_MIN_PX: i32 = 96;

    /// 丢弃 release 的对账间隔。
    ///
    /// 2026-10-06 真机反馈后从 400ms 收紧到 200ms:一次触发、三两个键的
    /// `GetAsyncKeyState` 查询是纯本地调用(纳秒级),而"卡住的键"每多留
    /// 200ms,用户就多 200ms 的"方向卡在旧的一侧/技能一直按着"。代价可以忽略。
    const RECONCILE_INTERVAL: Duration = Duration::from_millis(200);
    /// 消费线程闲时的等待上限(W1-3:有攒着的位移时按窗口截止等,见 `wait_hint`)
    const RECV_IDLE: Duration = Duration::from_millis(50);
    /// 原始输入模式下的光标回中检查间隔(W1-2)。
    ///
    /// 这条路与老路径的回中**不是一回事**:位移完全不依赖光标,回中只为
    /// "点击落点"服务(见 `consumer` 里的说明),所以低频即可。
    const RAW_RECENTER_INTERVAL: Duration = Duration::from_millis(16);
    /// 松键误判的宽限期(判定条件见 `release_is_lost`:**系统说已抬起** 且
    /// 我们**记过按下**(`last_seen != 0`)且**距最后一次事件已超过宽限**)。
    ///
    /// 它**不是**给"release 在队列里排队晚到"用的 —— `last_seen` 由钩子回调在
    /// 收到事件的那一刻写,KEYUP 一到就清零(`mark`),而判定要求 `last_seen != 0`:
    /// 还在排队里的抬起根本到不了这个分支。真正需要宽限的是**系统本身报错**:
    /// UAC 安全桌面/锁屏时 `GetAsyncKeyState` 对非活动桌面一律报"未按下",此时
    /// 只有**不产生自动重复**的键(Shift/Ctrl/Alt、鼠标键)会被误判 —— 会自动
    /// 重复的键每 ~33ms 刷新一次 `last_seen`,对宽限期取值天然免疫。
    ///
    /// 2026-10-06 真机反馈后从 1500ms 收紧到 700ms:上面那类误判在 700 / 1500
    /// 两个取值下都只是"晚 0.8 秒才发生"的区别(给多少宽限都补不了安全桌面的
    /// 谎话),而**真的丢了抬起**时的恢复时间从 ≤1.9s 缩到 ≤0.9s(见
    /// `RECONCILE_INTERVAL`)—— 那个才是用户能感觉到的"卡键"。
    const RECONCILE_GRACE_MS: u64 = 700;
    /// 鼠标键在补发抬起前需要的**连续确认拍数**(用户 2026-10-11,取证见
    /// [`reconcile_with`] 的长注释)。键盘键不需要:它们的自动重复每 ~33ms
    /// 刷新一次 `last_seen`,对宽限期天然免疫。
    const MOUSE_RELEASE_STRIKES: u8 = 2;
    /// `last_seen` / 连续确认状态表的大小(键码上限,evdev 码 < 512)。
    const KEY_STATE_LEN: usize = 512;
    /// 回调耗时统计的汇总间隔
    const TIMING_REPORT_INTERVAL: Duration = Duration::from_secs(5);
    /// 回调耗时的预警线:p99 超过它就在界面点亮"捕获层延迟异常"。
    ///
    /// 5ms 远低于系统 300ms 的超时线 —— 它是**预警**不是红线:回调开始出现
    /// 毫秒级毛刺时,离"系统开始静默丢事件/摘钩子"就只剩一个数量级了。
    const HOOK_LAG_WARN_NANOS: u64 = 5_000_000;

    /// 回调耗时直方图(按 2 的幂分桶)。
    ///
    /// 为什么需要:Windows 对低级钩子有 `LowLevelHooksTimeout`(默认 300ms),
    /// 超时的事件会被静默丢弃、反复超时还会静默摘掉整个钩子。所以"回调里花了
    /// 多久"是这个平台最关键的指标 —— 有它才能判断"我到底离危险线还有多远"。
    struct Timing {
        buckets: [AtomicU64; 24],
        count: AtomicU64,
        max: AtomicU64,
    }

    impl Default for Timing {
        fn default() -> Self {
            Self {
                buckets: std::array::from_fn(|_| AtomicU64::new(0)),
                count: AtomicU64::new(0),
                max: AtomicU64::new(0),
            }
        }
    }

    impl Timing {
        fn record(&self, nanos: u64) {
            self.count.fetch_add(1, Ordering::Relaxed);
            let idx = if nanos == 0 {
                0
            } else {
                (u64::BITS - nanos.leading_zeros()) as usize
            }
            .min(self.buckets.len() - 1);
            self.buckets[idx].fetch_add(1, Ordering::Relaxed);
            self.max.fetch_max(nanos, Ordering::Relaxed);
        }

        /// 按分桶上界估算分位数(足够回答"有没有逼近超时")
        fn quantile_nanos(&self, q: f64) -> u64 {
            let total = self.count.load(Ordering::Relaxed);
            if total == 0 {
                return 0;
            }
            let want = (total as f64 * q).ceil() as u64;
            let mut acc = 0u64;
            for (i, b) in self.buckets.iter().enumerate() {
                acc += b.load(Ordering::Relaxed);
                if acc >= want {
                    return if i == 0 { 1 } else { 1u64 << i };
                }
            }
            1u64 << (self.buckets.len() - 1)
        }

        /// 取走本轮的统计并清零(用于周期性汇总)。
        ///
        /// 顺序至关重要:**先取分位数、再清零** —— 旧实现先 `swap(0)` 把
        /// count 归零,随后 `quantile_nanos` 读到 total==0 直接返回 0,
        /// p50/p99 永远打印成 0,健康告警(用同一份 p99 判定)永远不会触发。
        /// 返回的 `p99_nanos` 与文案取自同一份快照,消费方不必(也不能)再取一次。
        fn take_report(&self) -> TimingReport {
            let p50 = self.quantile_nanos(0.50);
            let p99 = self.quantile_nanos(0.99);
            let count = self.count.swap(0, Ordering::Relaxed);
            let max = self.max.swap(0, Ordering::Relaxed);
            for b in &self.buckets {
                b.store(0, Ordering::Relaxed);
            }
            TimingReport {
                text: format!(
                    "钩子回调 {} 次,耗时 p50≈{:.1}µs p99≈{:.1}µs 最大 {:.1}µs(超时线 300ms)",
                    count,
                    p50 as f64 / 1000.0,
                    p99 as f64 / 1000.0,
                    max as f64 / 1000.0
                ),
                p99_nanos: p99,
            }
        }
    }

    /// 一轮回调耗时汇总:文案 + 判定用的 p99(同一份快照)。
    struct TimingReport {
        text: String,
        p99_nanos: u64,
    }

    /// 队列里的事件:绝对坐标由消费线程做差分。
    ///
    /// 为什么不直接在回调里算位移:算位移要读/写"上一次位置"、还要在需要时
    /// `SetCursorPos` 回中 —— 那些都是**会阻塞的 Win32 调用**。低级钩子回调里
    /// 只允许做"读字段、查表、塞队列"这三件事。
    enum RawEvent {
        Key {
            code: u16,
            pressed: bool,
            /// 钩子回调进场时刻(W1-1):这才是"事件时刻",队列里排队的时间不算
            at: Instant,
        },
        Move {
            x: f64,
            y: f64,
            at: Instant,
        },
        /// Raw Input 的**相对位移**(设备计数,非像素;W1-2)。
        ///
        /// 与 `Move` 的绝对坐标不同:这里已经是位移本身,消费线程不必做差分,
        /// 因此也没有"光标撞到屏幕边缘就丢位移"的问题。
        MoveRel {
            dx: i32,
            dy: i32,
            at: Instant,
        },
    }

    struct WinShared {
        raw_tx: Sender<RawEvent>,
        /// 每个键码最后一次事件的时刻(ms since start);0 表示"该键不在按下状态"
        last_seen: Box<[AtomicU64]>,
        timing: Timing,
        start: Instant,
        /// Raw Input 通道的状态机(W1-2),取值见 `RAW_*` 常量。
        /// 钩子回调要按它决定"位移这条还要不要发",所以放在共享结构里。
        raw_state: AtomicU8,
        /// **开始持续丢位移**的时刻(ms since start;0 = 还没丢过)。
        ///
        /// 只由钩子线程在 `RAW_PENDING` 期间写(一次性,`compare_exchange`),
        /// 消费线程读:注册成功进入 `RAW_PENDING` 后,钩子路径的位移已经让位,
        /// 若**一边丢位移、一边一条 WM_INPUT 都等不到**,说明这条通道根本没在
        /// 跑,必须超时后退回去,否则鼠标彻底失效(见 `consumer`)。
        ///
        /// 为什么记"开始丢的时刻"而不是"注册的时刻":注册后用户一直没动鼠标
        /// (甚至只是坐在那里看界面)是完全正常的,那时根本没有位移可丢,拿注册
        /// 时刻去超时会天天误报"通道死了"。只有"真的在丢却收不到"才是证据。
        raw_drop_ms: AtomicU64,
        /// **连续丢位移**的起点(ms;0 = 当前没在丢)。与 `raw_drop_ms` 分工不同:
        /// 那个只服务"注册后一直在等第一条 `WM_INPUT`"的兜底;这个要覆盖
        /// **已经进入 `RAW_RELATIVE`** 之后的情形 —— 那时位移被 Raw Input 独占,
        /// 它一静默,钩子那条路即使还在收 `WM_MOUSEMOVE` 也不再转发,位移会
        /// **永久停摆**(真机证据 2026-10-11:锁死期间 3 秒零位移、键盘照常,
        /// 用户只能退出重进 FPS 才恢复)。
        raw_drop_streak_ms: AtomicU64,
        /// 最近一次收到 `WM_INPUT` 的时刻(ms;0 = 从没收到)。
        raw_input_ms: AtomicU64,
        /// `WM_INPUT` 累计条数(诊断:与 `raw_state` 一起回答"这条通道在不在跑")。
        raw_input_count: AtomicU64,
        /// 看门狗要求消费线程做一次"差分基准同步"(见 [`raw_stall_due`])。
        raw_resync: AtomicBool,
        /// 「拦截系统默认行为」掩码(位定义见 [`swallow_bit`])。界面线程按当前
        /// 配置/映射开关/独占键盘开关预先算好写进来;钩子回调只读一次原子量。
        swallow: Arc<AtomicU16>,
        /// 「已绑定键不再传给其它程序」位图(B21-2/G-b,见 [`BoundKeys`])。
        /// 界面线程每帧全量重算逐 word 写入;钩子回调只读一次位测试。
        /// 与 swallow 同一条纪律:重装时带着界面线程的 Arc 一起换进去。
        bound_keys: Arc<BoundKeys>,
        /// 独占模式总开关(B21-2/G-b):false(默认)⇒ 位图**不参与**判定,
        /// 行为与从前逐位一致。
        swallow_bound_keys: Arc<AtomicBool>,
        /// 「FPS 鼠标隔膜」开关(见 [`mouse_shield_swallows`]):true 时鼠标钩子
        /// 对所有鼠标消息返回 1(吞掉投递)。界面线程每帧重算写进来,
        /// 钩子回调/钩子重装/心跳都只是读这一枚原子量 —— 所以重装/自愈之后
        /// 开关语义原样保持。
        mouse_shield: Arc<AtomicBool>,
        /// 钩子心跳(W1-4)。数值都是 `now_ms()` 基准(同一时钟)。
        hb: Hb,
    }

    /// 钩子心跳状态(W1-4)。两个线程读写:
    /// - 消费线程:每秒 `PostThreadMessage(WM_APP_HB)` 给钩子线程,并读这里判定 UI 告警;
    /// - 钩子线程:处理心跳消息时写 `pump_ms`/`probe_sent_*`,自己的钩子回调里写 `probe_seen_*`。
    #[derive(Default)]
    struct Hb {
        /// 钩子线程最后一次处理心跳消息的时刻(0 = 还没跑过)
        pump_ms: AtomicU64,
        /// 钩子线程线程 id(消费线程 PostThreadMessage 用;0 = 还没起)
        tid: AtomicU32,
        /// 已经**发出**的探针**枚数**(0 = 还没发过;`SendInput` 被 UIPI 挡下时不加,
        /// 那不是钩子的问题)。
        ///
        /// **为什么是计数不是时间戳**(2026-10-12 修 F5):低级钩子回调会在
        /// `SendInput` 期间重入本线程,回调写"看到了"必然早于返回后写"发出了" ——
        /// 时间戳比较会**每一拍**都判"无回音",导致永久停用钩子自愈(真机稳定复现)。
        /// 计数语义下:重入时两者在返回后相等(健康);钩子真死时 `seen` 落后于 `sent`。
        probe_sent_kb: AtomicU64,
        /// 钩子回调**看到**自己探针的**枚数**(0 = 从没见过)。与 `probe_sent_kb`
        /// 同代比较,见上。
        probe_seen_kb: AtomicU64,
        /// "钩子死过一回(已自愈)"的事件式标记:重装触发时写下,消费线程取走后
        /// 才清。不这样就会漏报 —— 探针几毫秒后就会把钩子救回来,写-清之间的
        /// 窗口太短,消费侧下一拍未必看得到。
        dead_ms: AtomicU64,
        /// 消费线程自己测到的"上一圈循环到现在"的间隔(ms):正常 ≤50ms,
        /// 只有睡眠/被抢 CPU 才会到秒级。钩子线程判"刚才卡过"之前先读它 ——
        /// 卡顿若同时在消费侧,那是整个进程被挂起,不该记到钩子线程头上、
        /// 更不该触发重装(2026-10-06 复核 #4)。
        consumer_gap_ms: AtomicU64,
        /// **探针自愈已停用**(DF-10,2026-10-11):钩子线程走到
        /// `ProbeStep::GiveUp` 时置位,消费线程据此把界面告警置成 3。
        ///
        /// 为什么放在共享结构里而不是钩子线程局部:它是**持续**状态,要一直
        /// 摆在界面上("再失灵请重启"),不是只在那一拍有效。重启才清。
        probe_off: AtomicBool,
        /// 最近一次被钩子回调看到的**键盘/鼠标按键**事件时刻(ms since start;
        /// 0 = 还没有过)。由 `mark` 顺手写 —— 它本来就在热路径上做一枚 relaxed
        /// store,多这一枚可忽略;而兜底重装(判据①)只需要一个"多久没事件"的
        /// 粗粒度时间戳,不值得为一个只在心跳里跑的检查去新建一条热路径。
        ///
        /// 注意口径:只标**按键/滚轮**类事件(位移走 Raw Input,不经过 `mark`),
        /// 这与判据②(有键按着却收不到)是同一件事的两面。
        last_input_ms: AtomicU64,
    }

    /// 钩子回调无法捕获环境,共享状态必须放静态里。这里不能用 `OnceLock`:
    /// 主题切换会让 eframe 在**同一进程内关闭并重新打开窗口**,从而第二次
    /// 调用 Capture::start；OnceLock 会保留上一次已经断开的 raw_tx,新钩子
    /// 仍然把事件送进死队列,表现就是“映射完全失效”。
    /// 改成原子指针；每次启动换入新的 Box。旧 Box 故意不释放,避免退出中的
    /// 旧钩子线程仍可能读到它。
    static SHARED_PTR: AtomicPtr<WinShared> = AtomicPtr::new(std::ptr::null_mut());

    fn shared() -> Option<&'static WinShared> {
        let ptr = SHARED_PTR.load(Ordering::Acquire);
        if ptr.is_null() {
            None
        } else {
            Some(unsafe { &*ptr })
        }
    }

    fn install_shared(
        raw_tx: Sender<RawEvent>,
        swallow: Arc<AtomicU16>,
        bound_keys: Arc<BoundKeys>,
        swallow_bound_keys: Arc<AtomicBool>,
        mouse_shield: Arc<AtomicBool>,
    ) -> *mut WinShared {
        let shared = Box::into_raw(Box::new(WinShared {
            raw_tx,
            last_seen: (0..512).map(|_| AtomicU64::new(0)).collect(),
            timing: Timing::default(),
            start: Instant::now(),
            raw_state: AtomicU8::new(RAW_UNREGISTERED),
            raw_drop_ms: AtomicU64::new(0),
            raw_drop_streak_ms: AtomicU64::new(0),
            raw_input_ms: AtomicU64::new(0),
            raw_input_count: AtomicU64::new(0),
            raw_resync: AtomicBool::new(false),
            swallow,
            bound_keys,
            swallow_bound_keys,
            mouse_shield,
            hb: Hb::default(),
        }));
        SHARED_PTR.store(shared, Ordering::Release);
        shared
    }
    /// 两个钩子的句柄分开存 —— rdev 当年用一个 `static mut HOOK` 装了两把,
    /// 后装的把先装的覆盖掉,于是键盘钩子永远卸不下来。
    static HOOK_KB: AtomicU64 = AtomicU64::new(0);
    static HOOK_MS: AtomicU64 = AtomicU64::new(0);

    /// 滚轮齿累加器(W1-5):高精度滚轮的一齿可能拆成多个小 delta 分批到达,
    /// 不足一齿的余量跨事件保留。鼠标钩子回调(挂在本线程上)是唯一写者,
    /// 用 Relaxed 的 load/store 即可(没有别的读者)。
    static WHEEL_ACC_V: AtomicI32 = AtomicI32::new(0);
    static WHEEL_ACC_H: AtomicI32 = AtomicI32::new(0);

    // ================= Raw Input 位移通道(W1-2) =================
    //
    // 为什么要有它:WH_MOUSE_LL 只给得到**光标坐标**,位移只能靠两次坐标相减。
    // 那条路带三层失真:①系统指针加速(同样的手部位移,快速甩动与慢速拖动
    // 得到不同像素数);②DPI 缩放(125%/150% 下像素数又变一次);③光标顶到
    // 屏幕边缘后坐标不再变化,位移直接丢失(靠"回中 + 回声识别"打补丁)。
    // Raw Input 给的是鼠标自己上报的**设备计数**,三层失真都不存在。
    //
    // 按键与滚轮仍走低级钩子:按键在两条路上都拿得到(钩子全局、Raw Input
    // 带 RIDEV_INPUTSINK 也能后台收),同时开就得去重;而按键本来就没有
    // "加速/DPI/边缘"那三层失真,换路换不来精度 —— 索性一条路管位移、
    // 一条路管按键与滚轮。

    /// Raw Input 通道状态:未启用(建窗/注册失败)。位移走钩子的光标差路径。
    const RAW_UNREGISTERED: u8 = 0;
    /// 已注册、还没收到过鼠标事件:先按"原始输入会接手"算 ——
    /// 否则注册完成到第一条事件之间的钩子位移会与原始输入**重复计一次**。
    const RAW_PENDING: u8 = 1;
    /// 见过**相对**位移:原始输入接管,钩子的位移整体让位。
    const RAW_RELATIVE: u8 = 2;
    /// 只见过**绝对**坐标(数位板/远程桌面/部分触控板):Raw Input 对这类设备
    /// 给的是 0..65535 的归一化坐标,不是位移;退回钩子的光标差路径。
    const RAW_ABSOLUTE_ONLY: u8 = 3;

    /// **持续丢位移**多久还等不到一条 WM_INPUT 就判定通道没在跑(见
    /// [`WinShared::raw_drop_ms`]):远程桌面/虚拟指针设备/注册受限时,
    /// Raw Input 收不到鼠标,而钩子路径的位移已经让位 —— 两条路都不发。
    ///
    /// 取 700ms:判据本身已经排除"用户没动鼠标"(那时没有位移可丢),所以这里
    /// 只需覆盖"丢了几条之后 raw 事件才追上"的正常时序 —— 那是毫秒级的事。
    const RAW_PENDING_TIMEOUT_MS: u64 = 700;

    /// 一条 Raw Input 鼠标记录属于哪一类(纯判定,便于单测)
    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    enum RawKind {
        Relative,
        Absolute,
        Empty,
    }

    /// 状态机的裁决结果(纯逻辑,便于单测)
    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    enum RawAdmit {
        /// 相对位移:发出去
        Emit,
        /// 空位移或非鼠标记录:丢掉
        Drop,
        /// 绝对坐标设备:退回钩子路径
        Fallback,
        /// 已经由相对设备接管,又来一条绝对事件:忽略(极罕见,记一次日志)
        MixedAbsolute,
    }

    fn raw_kind(flags: u16, lx: i32, ly: i32) -> RawKind {
        if flags & MOUSE_MOVE_ABSOLUTE != 0 {
            return RawKind::Absolute;
        }
        if lx == 0 && ly == 0 {
            return RawKind::Empty;
        }
        RawKind::Relative
    }

    /// 把一条记录并入状态机,返回 (新状态, 本条怎么处理)。
    ///
    /// 抽成纯函数的原因:这几条转移是 W1-2 的核心判定,而"接没接鼠标、
    /// 是不是绝对坐标设备"在这台开发机上没法真机验证 —— 至少让状态机有单测。
    fn raw_admit(cur: u8, kind: RawKind) -> (u8, RawAdmit) {
        match kind {
            RawKind::Empty => (cur, RawAdmit::Drop),
            RawKind::Relative => (RAW_RELATIVE, RawAdmit::Emit),
            RawKind::Absolute => {
                if cur == RAW_RELATIVE {
                    (cur, RawAdmit::MixedAbsolute)
                } else {
                    (RAW_ABSOLUTE_ONLY, RawAdmit::Fallback)
                }
            }
        }
    }

    /// 位移该不该由原始输入来发(钩子回调按它决定是否让位)
    fn raw_takes_motion(state: u8) -> bool {
        matches!(state, RAW_PENDING | RAW_RELATIVE)
    }

    /// 当前共享状态里"原始输入是否接管位移"
    fn raw_takes_motion_now() -> bool {
        shared().is_some_and(|s| raw_takes_motion(s.raw_state.load(Ordering::Relaxed)))
    }

    /// **只读探针**:Raw Input 通道"此刻"的运行状态(悬浮窗显示用,不写任何状态)。
    ///
    /// 返回 `(raw_state, WM_INPUT 累计条数, 距最近一条 WM_INPUT 的毫秒数)`。
    /// `raw_state` 取值见 `RAW_*`:0=未注册 / 1=待定 / 2=相对接管 / 3=只有绝对设备。
    /// 捕获层还没起来(`shared()` 为 `None`)或从没收到过 WM_INPUT 时,后两项为 0。
    ///
    /// 为什么必须有它:用户在**游戏里**要能看见"位移到底有没有到捕获层" —— 这正是
    /// 排除"捕获层坏了"还是"引擎侧不认"的最小判据(与轨迹里的 `t` 行同源)。
    pub fn raw_probe() -> (u8, u64, u64) {
        match shared() {
            Some(s) => {
                let last = s.raw_input_ms.load(Ordering::Relaxed);
                let age_ms = if last == 0 {
                    0
                } else {
                    now_ms().saturating_sub(last)
                };
                (
                    s.raw_state.load(Ordering::Relaxed),
                    s.raw_input_count.load(Ordering::Relaxed),
                    age_ms,
                )
            }
            None => (0, 0, 0),
        }
    }

    /// 钩子这条路的 `WM_MOUSEMOVE` 要不要转发给引擎(纯判定,便于单测)。
    ///
    /// **与「FPS 鼠标隔膜」无关**:隔膜只决定"这条消息还能不能投递给别的窗口",
    /// 位移的采集与转发在隔膜打开时原样进行 —— 用户 2026-10-11 的根因修复必须
    /// 满足的不变量:吞掉投递不能把手感一起吞掉。所以这里的输入只有 Raw Input
    /// 状态机一个,隔膜开关连参数都不是(单测据此锁住这条不变量)。
    fn hook_forwards_motion(state: u8) -> bool {
        !raw_takes_motion(state)
    }

    /// "相对与绝对坐标混用"这条提示只记一次(它是 per-event 分支,见 `handle_raw_input`)
    static MIXED_ABSOLUTE_LOGGED: AtomicBool = AtomicBool::new(false);

    /// "原始输入恢复接管"这条提示同理只记一次(见 `handle_raw_input` 的 `Emit` 分支)
    static RAW_RECOVERY_LOGGED: AtomicBool = AtomicBool::new(false);

    /// "这次恢复要不要记一条日志"的判定缝(R4/W1-B1)。
    ///
    /// 语义与内联版逐字一致:只有**从兜底态(`RAW_ABSOLUTE_ONLY`)恢复**才记,
    /// 且进程内只记一次(`RAW_RECOVERY_LOGGED`)—— 绝对坐标设备交替发相对事件时
    /// 这里会被反复走到,不能让诊断反过来变成钩子回调里的刷屏源。
    /// 抽出来接受 `log` 闭包的原因:测试可以用内存计数驱动"0→1→1"三拍,
    /// 而不必碰 diag 文件(§3.5:测试绝不写用户配置/日志)。
    /// "只记一次"的门做成 `logged` 参数(R4 扩缝):生产调用点传
    /// [`RAW_RECOVERY_LOGGED`],与原先直接读静态的行为逐字一致;测试传本地
    /// `AtomicBool` —— 否则直接驱动两拍会污染进程级静态,产生测试序依赖。
    fn note_raw_recovery(prev: u8, logged: &AtomicBool, log: impl FnOnce()) {
        if prev == RAW_ABSOLUTE_ONLY && !logged.swap(true, Ordering::Relaxed) {
            log();
        }
    }

    /// 隐藏的接收窗口句柄(`RIDEV_INPUTSINK` 要求 hwndTarget 是**有效窗口**;
    /// 消息专用窗口 HWND_MESSAGE 收 Raw Input 不可靠,所以建的是普通顶层窗口,
    /// 只是永远不显示)。0 = 没建起来。
    static RAW_HWND: AtomicU64 = AtomicU64::new(0);

    /// 接收窗口的类名(注册与注销都要用同一份)
    fn raw_class_name() -> Vec<u16> {
        "scrcpy_pad_raw_input\0".encode_utf16().collect()
    }

    /// Raw Input 报上来的**鼠标按键位图**(独立于 `GetAsyncKeyState` 的第二来源)。
    ///
    /// # 为什么必须另找一个来源(用户 2026-10-11 的真机日志)
    ///
    /// 同一轮对账里出现了矛盾读数:移动键读得到"按下",而**鼠标左键读不到** ——
    /// 于是 `channel_healthy` 被判成 true(有别的键按着),而左键被判成"抬起丢了",
    /// 对账**补发一次假释放**:扫射中途停火。`GetAsyncKeyState` 在前台窗口属于
    /// 别的进程(玩的时候前台就是 scrcpy)时对鼠标键会谎报 0,这不是"真的松手"。
    ///
    /// Raw Input 走的是另一条管道,而且我们注册的是 `RIDEV_INPUTSINK` —— **前台是
    /// 别的进程也照样收得到**。拿它的 `usButtonFlags` 维护位图,就能把
    /// "真丢了抬起"与"读数失明"分开:位图说按着,就绝不补发抬起。
    ///
    /// 位序与 [`raw_btn_bit`] 一一对应;0 = 五个键都没按。
    static RAW_BTN_DOWN: AtomicU32 = AtomicU32::new(0);

    /// evdev 鼠标键码 → [`RAW_BTN_DOWN`] 的位(非鼠标键返回 `None`)。
    fn raw_btn_bit(code: u16) -> Option<u32> {
        match code {
            272 => Some(1 << 0), // BTN_LEFT
            273 => Some(1 << 1), // BTN_RIGHT
            274 => Some(1 << 2), // BTN_MIDDLE
            275 => Some(1 << 3), // BTN_SIDE
            276 => Some(1 << 4), // BTN_EXTRA
            _ => None,
        }
    }

    /// 由 Raw Input 的 `usButtonFlags` 推进位图。
    ///
    /// 常量用字面量而不是 `RI_MOUSE_*`:SDK 里这五对 DOWN/UP 就是 0x0001/0x0002
    /// 起、每对隔两位的固定布局,写死反而少一层版本耦合(注释即出处)。
    fn note_raw_buttons(flags: u16) {
        let mut cur = RAW_BTN_DOWN.load(Ordering::Relaxed);
        for (i, (down, up)) in [
            (0x0001u16, 0x0002u16), // 左 / 右
            (0x0004, 0x0008),
            (0x0010, 0x0020), // 中
            (0x0040, 0x0080), // 侧键 1
            (0x0100, 0x0200), // 侧键 2
        ]
        .into_iter()
        .enumerate()
        {
            if flags & down != 0 {
                cur |= 1 << i;
            }
            if flags & up != 0 {
                cur &= !(1 << i);
            }
        }
        RAW_BTN_DOWN.store(cur, Ordering::Relaxed);
    }

    /// 鼠标键的**独立判据**:位图说"按着"就直接返回按下(交给对账当"确认在按")。
    /// 位图里没有这个键(非鼠标键、或从未见过它的翻转)→ `None`,回落 `GetAsyncKeyState`。
    fn raw_button_oracle(code: u16) -> Option<i16> {
        let bit = raw_btn_bit(code)?;
        (RAW_BTN_DOWN.load(Ordering::Relaxed) & bit != 0).then_some(-1)
    }

    /// 接收窗口的消息过程:只处理 WM_INPUT,其余交回系统默认。
    unsafe extern "system" fn raw_wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_INPUT {
            unsafe { handle_raw_input(lparam as HRAWINPUT) };
            // 不调 DefWindowProc:GetRawInputData 已经把这条记录取走,
            // 沿用的正是 MSDN"Using Raw Input"样例的写法。
            return 0;
        }
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    /// 处理一条 WM_INPUT:相对位移入队,绝对坐标设备记录一次并让位。
    unsafe fn handle_raw_input(hrawinput: HRAWINPUT) {
        let Some(shared) = shared() else {
            return;
        };
        let began = Instant::now();
        // DF-9(可见化 + 看门狗证据):收到一条 `WM_INPUT` 说明这条链在跑 ——
        // 记时刻与条数,并清掉"连续丢位移"的起点(见 `raw_drop_streak_ms`)。
        shared
            .raw_input_ms
            .store(now_ms().max(1), Ordering::Relaxed);
        shared.raw_input_count.fetch_add(1, Ordering::Relaxed);
        shared.raw_drop_streak_ms.store(0, Ordering::Relaxed);
        let mut data: RAWINPUT = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of::<RAWINPUT>() as u32;
        let got = unsafe {
            GetRawInputData(
                hrawinput,
                RID_INPUT,
                (&raw mut data).cast(),
                &mut size,
                std::mem::size_of::<RAWINPUTHEADER>() as u32,
            )
        };
        if got == 0 || got == u32::MAX {
            return;
        }
        if data.header.dwType != RIM_TYPEMOUSE {
            return;
        }
        let mouse = unsafe { data.data.mouse };
        // 鼠标按键位图(独立判据,见 [`RAW_BTN_DOWN`]):这里只登记翻转,不做判定 ——
        // 判定发生在对账那一步(它才是"要不要补发抬起"的决策点)。
        note_raw_buttons(unsafe { mouse.Anonymous.Anonymous.usButtonFlags });
        let kind = raw_kind(mouse.usFlags, mouse.lLastX, mouse.lLastY);
        let prev = shared.raw_state.load(Ordering::Relaxed);
        let (next, admit) = raw_admit(prev, kind);
        shared.raw_state.store(next, Ordering::Relaxed);
        match admit {
            RawAdmit::Emit => {
                // 兜底退回光标差路径之后又收到相对事件 = 通道其实还在,只是刚才
                // 哑了一段。有一条 "退回…" 的 warn 就必须有一条 "恢复" —— 否则
                // 日志会永远停在"通道死了"上,诊断时被带偏。只记一次:绝对坐标
                // 设备交替发相对事件时这里会被反复走到,不能让诊断反过来变成
                // 钩子回调里的刷屏源(同 `MIXED_ABSOLUTE_LOGGED`)。
                // 判定缝见 `note_raw_recovery`(R4/W1-B1:生产语义逐字不变)。
                note_raw_recovery(prev, &RAW_RECOVERY_LOGGED, || {
                    diag_info!(
                        "capture",
                        "原始输入重新收到相对位移事件:位移改由原始输入接管"
                    )
                });
                let _ = shared.raw_tx.send(RawEvent::MoveRel {
                    dx: mouse.lLastX,
                    dy: mouse.lLastY,
                    at: began,
                });
                shared.timing.record(began.elapsed().as_nanos() as u64);
            }
            RawAdmit::Fallback => {
                // **只在状态跳变那一条**记日志。绝对坐标设备(数位板/远程桌面/
                // 部分触控板)的每一条事件都会落到这个分支,而 diag 的 warn 要
                // 加锁 + 格式化 + 立即落盘 —— 逐条记就等于在钩子线程上按事件
                // 频率做磁盘刷新,正是系统 300ms 钩子超时(超时即静默丢事件)
                // 的温床。状态没变说明"同一台绝对坐标设备仍在原路上",不必重复说。
                if prev != next {
                    diag_warn!(
                        "capture",
                        "检测到绝对坐标鼠标(数位板/远程桌面/部分触控板):原始输入对它不适用,位移退回光标差路径"
                    );
                }
            }
            RawAdmit::MixedAbsolute => {
                // 同理:混合设备下每条绝对事件都进这里。本来就在 debug 档
                // (默认级别下 `diag::log` 头部就返回),这里再收成"只记一次"。
                if !MIXED_ABSOLUTE_LOGGED.swap(true, Ordering::Relaxed) {
                    diag_debug!(
                        "capture",
                        "相对与绝对坐标鼠标混用:绝对坐标设备继续走光标差路径"
                    );
                }
            }
            RawAdmit::Drop => {}
        }
    }

    /// 建接收窗口并注册 Raw Input。返回窗口句柄(0 = 失败,调用方保持钩子路径)。
    fn setup_raw_input() -> u64 {
        unsafe {
            let hinst = GetModuleHandleW(std::ptr::null());
            let class = raw_class_name();
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(raw_wnd_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinst,
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: class.as_ptr(),
            };
            // 失败可能是"类已注册"(主题切换会在同进程里重启捕获层),
            // 那不是错误 —— 继续建窗即可。
            if RegisterClassW(&wc) == 0 {
                diag_debug!("capture", "Raw Input 窗口类已存在(同进程重启),继续");
            }
            let name: Vec<u16> = "scrcpy-pad raw input\0".encode_utf16().collect();
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                class.as_ptr(),
                name.as_ptr(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                hinst,
                std::ptr::null(),
            );
            if hwnd.is_null() {
                diag_warn!(
                    "capture",
                    "创建 Raw Input 接收窗口失败(GetLastError={});位移继续走光标差路径",
                    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
                );
                return 0;
            }
            let rid = RAWINPUTDEVICE {
                usUsagePage: 0x01, // Generic Desktop Controls
                usUsage: 0x02,     // Mouse
                dwFlags: RIDEV_INPUTSINK,
                hwndTarget: hwnd,
            };
            if RegisterRawInputDevices(&rid, 1, std::mem::size_of::<RAWINPUTDEVICE>() as u32) == 0 {
                diag_warn!(
                    "capture",
                    "注册 Raw Input 失败(GetLastError={});位移继续走光标差路径",
                    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
                );
                DestroyWindow(hwnd);
                return 0;
            }
            RAW_HWND.store(hwnd as u64, Ordering::Relaxed);
            diag_info!(
                "capture",
                "原始输入(Raw Input)已启用:鼠标位移按设备计数上报(无指针加速/DPI 失真)"
            );
            hwnd as u64
        }
    }

    /// 撤销注册并销毁接收窗口(退出时调用)
    fn teardown_raw_input() {
        let hwnd = RAW_HWND.swap(0, Ordering::Relaxed);
        if hwnd == 0 {
            return;
        }
        unsafe {
            let rid = RAWINPUTDEVICE {
                usUsagePage: 0x01,
                usUsage: 0x02,
                dwFlags: RIDEV_REMOVE,
                hwndTarget: std::ptr::null_mut(),
            };
            RegisterRawInputDevices(&rid, 1, std::mem::size_of::<RAWINPUTDEVICE>() as u32);
            DestroyWindow(hwnd as HWND);
            let class = raw_class_name();
            UnregisterClassW(class.as_ptr(), GetModuleHandleW(std::ptr::null()));
        }
    }

    fn now_ms() -> u64 {
        shared()
            .map(|s| s.start.elapsed().as_millis() as u64)
            .unwrap_or(0)
    }

    // 共享原子量都是平台侧的"旋钮",逐个传比打包成一个结构体更直白
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        grab: &Arc<AtomicBool>,
        mouse_grab: &Arc<AtomicBool>,
        swallow: &Arc<AtomicU16>,
        bound_keys: &Arc<BoundKeys>,
        swallow_bound_keys: &Arc<AtomicBool>,
        clip_cursor: &Arc<AtomicBool>,
        mouse_shield: &Arc<AtomicBool>,
        cursor_hide: &Arc<AtomicBool>,
        mouse_found: &Arc<AtomicBool>,
        hook_lag: &Arc<AtomicBool>,
        hb_state: &Arc<AtomicU8>,
        hook_ok: &Arc<AtomicU8>,
        stop: &Arc<AtomicBool>,
        tx: Sender<CaptureEvent>,
        ready_tx: Sender<u8>,
    ) -> Result<(Option<Arc<AtomicU32>>, std::thread::JoinHandle<()>)> {
        // 键盘**整把**抓取在 Windows 上无法实现(低级钩子只能观察,不能拦截),
        // 但"候选键拦截"可以:钩子回调对 `swallow` 掩码里置位的键返回非零、
        // 把事件从系统输入流里吞掉(见 keyboard_proc / mouse_proc 与 swallow_bit)。
        // 鼠标整把抓取走"隐藏光标 + 回中",下面由消费线程执行。
        //
        // 2026-10-11 追加:`mouse_shield`(FPS 鼠标隔膜)—— mouse_proc 里对所有
        // 鼠标消息返回非零,让投屏窗口收不到鼠标(见 `mouse_shield_swallows`)。
        let _ = grab;
        let (raw_tx, raw_rx) = channel::<RawEvent>();
        let _ = install_shared(
            raw_tx,
            swallow.clone(),
            bound_keys.clone(),
            swallow_bound_keys.clone(),
            mouse_shield.clone(),
        );

        let wake = Arc::new(AtomicU32::new(0));

        // 消费线程:算位移、维护光标、定期对账
        {
            let mouse_grab = mouse_grab.clone();
            let clip_cursor = clip_cursor.clone();
            let cursor_hide = cursor_hide.clone();
            let hb_state = hb_state.clone();
            let hook_lag = hook_lag.clone();
            let stop = stop.clone();
            let txt = tx.clone();
            std::thread::spawn(move || {
                // 钩子→引擎的中转线程:轻量但要及时。
                crate::priority::boost(crate::priority::Class::AboveNormal);
                consumer(
                    raw_rx,
                    txt,
                    mouse_grab,
                    clip_cursor,
                    cursor_hide,
                    hb_state,
                    hook_lag,
                    stop,
                );
            });
        }

        // 钩子线程:装钩子 + 消息循环。低级钩子必须装在跑消息循环的那个线程上。
        let hook_thread = {
            let wake = wake.clone();
            let stop = stop.clone();
            let mouse_found = mouse_found.clone();
            let hook_ok = hook_ok.clone();
            std::thread::spawn(move || {
                // 低级钩子回调跑在这个线程上,系统对钩子投递有低延迟预期:
                // 提到最高优先级,满载机器上也能第一时间处理输入。
                crate::priority::boost(crate::priority::Class::Highest);
                install_and_pump(wake, stop, mouse_found, hook_ok, ready_tx);
            })
        };

        Ok((Some(wake), hook_thread))
    }

    /// 唤醒钩子线程(退出时用;`GetMessage` 阻塞,光置标志它看不见)
    pub fn post_quit(tid: u32) {
        const WM_QUIT: u32 = 0x0012;
        unsafe {
            PostThreadMessageW(tid, WM_QUIT, 0, 0);
        }
    }

    // ================= 钩子心跳自愈(W1-4) =================
    //
    // 两件在真机上会发生、但过去**完全没有提示**的事:
    // ①消息泵卡死:钩子回调就跑在本线程的消息循环里,任何一次卡顿超过
    //   `LowLevelHooksTimeout`(默认 300ms),系统就开始丢事件,反复超时则
    //   静默摘掉整个钩子;②钩子被静默摘除:系统摘钩子**不通知**(见
    //   `HOOK_LAG_WARN_NANOS` 处的说明),用户看到的是"按键突然全部失灵",
    //   除了重启程序没有别的办法。
    //
    // 做法(与方案一致,只把 SetTimer 换成 PostThreadMessage,原因见下):
    // - 消费线程每秒 `PostThreadMessage(WM_APP_HB)`;钩子线程处理到它就写一次
    //   `pump_ms`。3 秒没被处理 = 泵卡死。
    // - 每次心跳顺带 `SendInput` 一枚带魔数标记的**合成按键**(VK 0xFF,不映射
    //   任何功能)。自己的钩子回调看到它 ⇒ 记一笔 `probe_seen_kb` 并**吞掉**
    //   (不进系统输入流)。连续 3 枚看不到 ⇒ 钩子已被摘 ⇒ 就地重装。
    // - 重装会在 `dead_ms` 写下一个"死过一回"的**事件**;消费线程取走后按
    //   `HB_DEAD_DWELL_MS` 展示几秒再回正常,界面据此说"曾失效,已自动重装"。
    //   判定是每秒一次读几个原子量(不是只在心跳时刻看),几十毫秒内就能上报。
    //
    // 为什么不用 SetTimer:WM_TIMER 是**最低优先级**消息,只在消息队列空时才
    // 生成 —— Raw Input 在快甩鼠标时每秒往队列灌几千条 WM_INPUT,定时器会被
    // 饿住,把"正在高速移动鼠标"误报成"泵卡死"。PostThreadMessage 的投递
    // 优先级高于输入消息,不存在这个饿死问题(方案原文也给了这个备选:
    // "或主循环向自己 PostThreadMessage(WM_APP)")。

    /// 消费线程发心跳的节拍
    const HB_INTERVAL: Duration = Duration::from_millis(1_000);
    /// 两拍之间的最小间隔:泵恢复后积压的多枚心跳会一次涌进来,只处理第一枚
    /// (由它发现"刚才卡过"),其余丢弃 —— 免得连发一串探针。
    const HB_MIN_GAP_MS: u64 = 500;
    /// "钩子死过(已自愈)"在界面上展示多久 —— 探针几毫秒就把钩子救回来,
    /// 不 dwell 一下消费侧根本来不及上报,用户也来不及看见
    const HB_DEAD_DWELL_MS: u64 = 5_000;
    /// 心跳消息(消费线程 → 钩子线程)
    const WM_APP_HB: u32 = WM_APP + 1;
    /// 探针的魔数标记:`dwExtraInfo` 里带上它,和别的注入事件区分开
    const PROBE_MAGIC: usize = 0x5343_5041_4431; // "SCPAD1"
    /// 探针虚拟键:0xFF(未定义键)不在 `vktable` 表里,不映射任何功能。
    /// 只有"钩子已经死了"时它才会漏进系统输入流 —— 一秒最多两下、最多漏 3 次。
    const PROBE_VK: u16 = 0xFF;

    /// 发一枚键盘探针(按下+抬起)。返回是否真的发出去了:SendInput 在前台是
    /// 高完整性进程(UIPI)时会被拒绝 —— 那不是钩子的问题,不能记成"探针丢了"。
    fn send_probe_kb() -> bool {
        let mk = |flags: u32| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: PROBE_VK,
                    wScan: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: PROBE_MAGIC,
                },
            },
        };
        let ins = [mk(0), mk(KEYEVENTF_KEYUP)];
        unsafe {
            SendInput(
                ins.len() as u32,
                ins.as_ptr(),
                std::mem::size_of::<INPUT>() as i32,
            ) == ins.len() as u32
        }
    }

    /// 心跳的逐拍状态(钩子线程局部)
    struct HbTick {
        /// 连续没回音的探针数
        miss: u32,
        /// 连续"重装后仍无回音"的轮数(环境吞注入的判据;看到回音即清零)
        fruitless: u32,
        /// DF-10:上一次"探针停用后的兜底重装"的时刻(ms;0 = 还没做过)
        last_fallback_ms: u64,
        /// DF-10:兜底重装的累计次数(上限 `FALLBACK_MAX`)
        fallback_count: u32,
        /// 上一次真正处理心跳的时刻(ms;0 = 还没处理过)
        last_ms: u64,
        /// 是否期望键盘钩子在收事件(重装后跟随新结果)
        expect_kb: bool,
    }

    /// 此刻系统里是否有**我们键盘钩子认得**的键按着(DF-10 兜底判据②)。
    ///
    /// 只问 `vktable::map_vk_ex` 认得的 VK:认不得的键(媒体键、厂商自定义键)
    /// 钩子本来就不会产生事件,拿它们当"用户在手按"会平白触发兜底重装。
    /// 只在心跳这一拍(≤1 次/秒、且还要 probe_off 才会走到)调用 ——
    /// **不在任何输入事件热路径上**。
    fn any_mapped_key_down() -> bool {
        (0x01u16..=0xFE).any(|vk| {
            vktable::map_vk_ex(vk, false).is_some() && unsafe { GetAsyncKeyState(vk as i32) } < 0
        })
    }

    /// 处理一次心跳(在钩子线程上,由 `WM_APP_HB` 触发)。
    fn heartbeat_tick(hb: &mut HbTick, mouse_found: &Arc<AtomicBool>, hook_ok: &Arc<AtomicU8>) {
        let now = now_ms();
        if hb.last_ms != 0 && now.saturating_sub(hb.last_ms) < HB_MIN_GAP_MS {
            return; // 泵恢复后涌进来的积压心跳,丢掉
        }
        hb.last_ms = now;
        let Some(s) = shared() else { return };
        let prev = s.hb.pump_ms.swap(now, Ordering::Relaxed);

        // ① 刚才卡过:上一拍离现在超过门限 ⇒ 卡了这么久,系统多半已把超时的
        //    钩子摘掉 —— 不等探针,直接重装。
        //    例外(复核 #4):消费侧自己也卡了(睡眠/被抢 CPU)—— 那种情况两边
        //    都没有心跳,不是钩子线程的账,连日志都不该刷。
        if stall_hit(now, prev, s.hb.consumer_gap_ms.load(Ordering::Relaxed)) {
            diag_warn!(
                "capture",
                "钩子线程心跳中断 {}ms 后恢复:钩子可能已被系统摘除,就地重装",
                now - prev
            );
            let bits = reinstall_hooks(mouse_found, hook_ok, "消息泵卡顿后恢复");
            // 复核 #1:重装会把"键盘钩子装没装上"的结果刷新(可能这次才补上,
            // 也可能这把反而失败)—— 探针的"该不该期望回音"必须跟着走,
            // 否则补装成功后探针永远不再发,静默摘钩再也检测不到。
            hb.expect_kb = crate::capture::hook_bits_keyboard_ok(bits);
            hb.miss = 0;
        }

        // ②③ 探针策略在 `probe_step`(纯函数,单测锁住):连续 HB_MISS_LIMIT
        //     拍没回音 ⇒ 重装;重装后仍无回音连续 PROBE_FRUITLESS_LIMIT 轮 ⇒
        //     认定环境吞掉了注入事件,停用探针自愈(复核 #3:否则每 3 秒重装
        //     一轮、日志刷屏,还连累好着的鼠标钩子一起被卸装)。
        if s.hb.probe_off.load(Ordering::Relaxed) {
            // DF-10(2026-10-11):探针这条路已经放弃,而当初的结论未必对 ——
            // 停用之后**没有任何人**能发现"钩子又被系统静默摘了"(残余的
            // `stall_hit` 只覆盖"消息泵也卡住",系统静默摘钩时泵并不卡),
            // 键盘+鼠标钩子会一起失灵到重启。这里补一条**不依赖探针回音**的
            // 兜底:长时间收不到任何钩子事件 + 系统说此刻有键按着 ⇒ 再重装
            // 一次。判据是纯函数 `fallback_reinstall_due`(单测锁住),
            // 只挂在这条 1s 一拍的周期路径上,**不进任何输入事件热路径**。
            let last_event = s.hb.last_input_ms.load(Ordering::Relaxed);
            if fallback_reinstall_due(
                true,
                now,
                last_event,
                any_mapped_key_down(),
                hb.last_fallback_ms,
                hb.fallback_count,
            ) {
                hb.last_fallback_ms = now;
                hb.fallback_count += 1;
                diag_warn!(
                    "capture",
                    "探针自愈停用后的兜底重装(第 {}/{} 次):已 {}ms 收不到钩子事件且系统显示有键按着",
                    hb.fallback_count,
                    FALLBACK_MAX,
                    now.saturating_sub(last_event)
                );
                let bits = reinstall_hooks(mouse_found, hook_ok, "探针停用后的兜底");
                hb.expect_kb = crate::capture::hook_bits_keyboard_ok(bits);
                hb.miss = 0;
            }
            return;
        }
        let (miss, fruitless, step) = probe_step(
            hb.miss,
            s.hb.probe_sent_kb.load(Ordering::Relaxed),
            s.hb.probe_seen_kb.load(Ordering::Relaxed),
            hb.expect_kb,
            hb.fruitless,
        );
        hb.miss = miss;
        hb.fruitless = fruitless;
        match step {
            ProbeStep::Idle => {}
            ProbeStep::Reinstall => {
                diag_warn!(
                    "capture",
                    "键盘钩子探针连续 {} 次无回音:钩子已被系统摘除,自动重装",
                    HB_MISS_LIMIT
                );
                let bits = reinstall_hooks(mouse_found, hook_ok, "探针无回音");
                hb.expect_kb = crate::capture::hook_bits_keyboard_ok(bits);
            }
            ProbeStep::GiveUp => {
                // DF-10:置的是**共享**标志(界面据此常驻红字),不再是线程局部 ——
                // 以前这里只有一行 diag 日志,用户在界面上什么也看不到,于是
                // "钩子被静默摘掉"表现为"按键突然全部失灵、只能重启"。
                s.hb.probe_off.store(true, Ordering::Relaxed);
                diag_warn!(
                    "capture",
                    "键盘钩子探针连续 {} 轮重装后仍无回音:疑似环境吞掉注入事件,已停用探针自愈(消息泵心跳检测保留)。\
                     界面会常驻告警:本进程内不再自动重装钩子,按键/视角失灵请重启程序",
                    hb.fruitless
                );
            }
        }
        // ④ 发下一枚探针(重装上后立刻补一发,让"还活着"的证据尽早回来)。
        //    计数语义:`sent` 与 `seen` 同代比较(见 `probe_missed`)。
        if hb.expect_kb && send_probe_kb() {
            s.hb.probe_sent_kb.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// 装两把低级钩子(初始安装与自愈重装共用)。返回位标志(见 `hook_bits_encode`)。
    /// 必须在钩子线程上调用(消息循环所在线程)。`mouse_found` 只在鼠标钩子
    /// **真的装上**时置位(W0-11)。
    fn install_hooks(mouse_found: &Arc<AtomicBool>) -> u8 {
        let mut kb_ok = false;
        let mut ms_ok = false;
        unsafe {
            let h = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), std::ptr::null_mut(), 0);
            if h.is_null() {
                diag_error!(
                    "capture",
                    "安装键盘钩子失败(GetLastError={});按键映射将完全不可用",
                    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
                );
            } else {
                HOOK_KB.store(h as u64, Ordering::Relaxed);
                kb_ok = true;
                diag_info!("capture", "键盘低级钩子已安装");
            }
            let h = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), std::ptr::null_mut(), 0);
            if h.is_null() {
                diag_error!(
                    "capture",
                    "安装鼠标钩子失败(GetLastError={});鼠标按键与 FPS 瞄准将不可用",
                    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
                );
            } else {
                HOOK_MS.store(h as u64, Ordering::Relaxed);
                ms_ok = true;
                // 鼠标钩子真的装上了才敢说"检测到鼠标"(W0-11):
                // 以前消费线程无条件置 true,钩子装失败时界面照样显示 ✓,
                // 用户拿着"检测到鼠标设备"的绿灯却怎么按都没反应。
                mouse_found.store(true, Ordering::Relaxed);
                diag_info!("capture", "鼠标低级钩子已安装");
            }
        }
        crate::capture::hook_bits_encode(kb_ok, ms_ok)
    }

    /// 重装两把钩子(W1-4 自愈):先卸后装、更新 `hook_ok`、给心跳换新一代探针。
    /// 只允许在钩子线程上调用 —— 低级钩子的回调就投递到安装它的线程。
    /// 返回新安装的位标志(调用方要用它决定探针是否还应期望回音,复核 #1)。
    fn reinstall_hooks(mouse_found: &Arc<AtomicBool>, hook_ok: &Arc<AtomicU8>, reason: &str) -> u8 {
        let old = hook_ok.load(Ordering::Relaxed);
        unsafe {
            let kb = HOOK_KB.swap(0, Ordering::Relaxed);
            if kb != 0 {
                UnhookWindowsHookEx(kb as _);
            }
            let ms = HOOK_MS.swap(0, Ordering::Relaxed);
            if ms != 0 {
                UnhookWindowsHookEx(ms as _);
            }
        }
        let bits = install_hooks(mouse_found);
        hook_ok.store(bits, Ordering::Relaxed);
        if let Some(s) = shared() {
            // 探针计数换新一代(都归零):上一代的 seen 会让下一拍判定立刻又报死。
            // 归零后两者相等 ⇒ 健康,等新一代探针的回音。
            s.hb.probe_sent_kb.store(0, Ordering::Relaxed);
            s.hb.probe_seen_kb.store(0, Ordering::Relaxed);
            // dead 标志("死过一回"的事件):键盘钩子装上了才写 —— 没装上就没
            // 什么可判的,那种情况由 hook_ok 的自检红灯负责。它由**消费侧取走**后
            // 才清(见 consumer):写在这是为了即便探针几毫秒后就回来,界面也能看到。
            // 复核 #6:失败时**不写 0** —— 那会把上一次还没被消费的事件抹掉,
            // 界面上那次真实的"死过"就永远看不到了。
            if crate::capture::hook_bits_keyboard_ok(bits) {
                s.hb.dead_ms.store(now_ms().max(1), Ordering::Relaxed);
            }
        }
        diag_warn!(
            "capture",
            "钩子自愈:已重装({reason});键盘{} 鼠标{}(此前位标志 {:#04x})",
            if crate::capture::hook_bits_keyboard_ok(bits) {
                "✓"
            } else {
                "✗"
            },
            if crate::capture::hook_bits_mouse_ok(bits) {
                "✓"
            } else {
                "✗"
            },
            old
        );
        bits
    }

    fn install_and_pump(
        wake: Arc<AtomicU32>,
        stop: Arc<AtomicBool>,
        mouse_found: Arc<AtomicBool>,
        hook_ok: Arc<AtomicU8>,
        ready_tx: Sender<u8>,
    ) {
        let tid = unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() };
        wake.store(tid, Ordering::Relaxed);

        // 位标志:bit0=键盘钩子装上了,bit1=鼠标钩子装上了(编解码见 hook_bits_encode)
        let bits = install_hooks(&mouse_found);
        // 结果对外可见 + 通知 start() 等待方;即便无人等待也要存下来(界面自检读它)
        hook_ok.store(bits, Ordering::Relaxed);
        let _ = ready_tx.send(bits);

        // 心跳(W1-4):消费线程每秒往本线程投一枚 WM_APP_HB —— 见上方小节
        let mut hb = HbTick {
            miss: 0,
            fruitless: 0,
            // DF-10:兜底重装的限流计数(探针停用后才用得上)
            last_fallback_ms: 0,
            fallback_count: 0,
            last_ms: 0,
            expect_kb: crate::capture::hook_bits_keyboard_ok(bits),
        };
        // 真机自愈冒烟:设 `SCRCPY_PAD_UNHOOK_AFTER_MS=<ms>` 时,到点在钩子线程上
        // **主动摘掉两把钩子一次**,模拟"系统静默摘钩"——不设这个变量零开销。
        let mut test_unhook_at: Option<u64> = std::env::var("SCRCPY_PAD_UNHOOK_AFTER_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(|ms| now_ms() + ms);
        if let Some(s) = shared() {
            s.hb.tid.store(tid, Ordering::Relaxed);
            // 复核 #2:立刻种下"第一拍"的时间。否则线程若在进入消息循环之前
            // 就卡死(排队/建接收窗口),pump 会一直是 0,而 0 的语义是
            // "还没跑起来不判" —— 界面会一路显示正常,恰恰漏掉最该报的情况。
            s.hb.pump_ms.store(now_ms().max(1), Ordering::Relaxed);
        }

        // Raw Input(位移通道,W1-2):建好了就把状态推到"待定" ——
        // 在收到第一条鼠标事件之前,位移按"原始输入会接手"算,避免两条路重复计。
        if setup_raw_input() != 0 {
            if let Some(s) = shared() {
                s.raw_state.store(RAW_PENDING, Ordering::Relaxed);
                // 这里**不**记时刻:兜底要的判据是"在丢位移却收不到"(见
                // `raw_drop_ms`),由钩子回调在真的丢一条位移时写下。
            }
        }

        // 消息循环。注意:`GetMessageW` 在没有消息时阻塞,这是**必须**的
        // (低级钩子靠系统调用宿主线程来处理事件),不能改成轮询。
        // WM_INPUT 也进这个队列(接收窗口建在本线程上),要派发给窗口过程处理。
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        loop {
            let r = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
            if r <= 0 {
                break; // 0 = WM_QUIT,-1 = 出错
            }
            if stop.load(Ordering::Relaxed) {
                break;
            }
            // 线程消息(hwnd 为空)不走 DispatchMessage:心跳就地处理(W1-4);
            // 冒烟用的"模拟摘钩"也在这里
            if msg.hwnd.is_null() {
                if msg.message == WM_APP_HB {
                    if let Some(t) = test_unhook_at {
                        if now_ms() >= t {
                            test_unhook_at = None;
                            unsafe {
                                let kb = HOOK_KB.swap(0, Ordering::Relaxed);
                                if kb != 0 {
                                    UnhookWindowsHookEx(kb as _);
                                }
                                let ms = HOOK_MS.swap(0, Ordering::Relaxed);
                                if ms != 0 {
                                    UnhookWindowsHookEx(ms as _);
                                }
                            }
                            diag_warn!(
                                "capture",
                                "冒烟自检:已按 SCRCPY_PAD_UNHOOK_AFTER_MS 摘掉两把钩子(模拟系统静默摘钩)"
                            );
                        }
                    }
                    heartbeat_tick(&mut hb, &mouse_found, &hook_ok);
                    continue;
                }
            }
            unsafe {
                DispatchMessageW(&msg);
            }
        }

        teardown_raw_input();
        unsafe {
            let kb = HOOK_KB.swap(0, Ordering::Relaxed);
            if kb != 0 {
                UnhookWindowsHookEx(kb as _);
            }
            let ms = HOOK_MS.swap(0, Ordering::Relaxed);
            if ms != 0 {
                UnhookWindowsHookEx(ms as _);
            }
        }
        diag_info!("capture", "Windows 钩子线程已退出(钩子已卸载)");
    }

    /// 记一次事件时刻(供丢失 release 的对账使用),并记录回调耗时。
    ///
    /// 这里只有两次 relaxed 原子操作 —— 相对"附着到前台线程"那种开销,
    /// 可以认为不花时间。绝不在此加锁、绝不调用任何可能阻塞的 Win32 API。
    fn mark(shared: &WinShared, code: u16, pressed: bool, began: Instant) {
        let seen = now_ms().max(1);
        // DF-10:顺手记"钩子还看得到按键事件"——兜底重装(判据①)按它判断
        // "这么久一条事件都没有"。多一枚 relaxed store,热路径开销不变。
        shared.hb.last_input_ms.store(seen, Ordering::Relaxed);
        if (code as usize) < shared.last_seen.len() {
            shared.last_seen[code as usize]
                .store(if pressed { seen } else { 0 }, Ordering::Relaxed);
        }
        shared.timing.record(began.elapsed().as_nanos() as u64);
    }

    /// 光标(屏幕坐标)下面最上层那个窗口是否**属于本进程**。
    ///
    /// 用户 2026-10-09(第 3 条"滚动"):滚轮压在界面自己身上时不该被吞 ——
    /// 吞掉就等于"界面里的清单/弹窗只能用鼠标拖滚动条"。
    ///
    /// 游戏窗口是独立的 `scrcpy.exe` 进程,所以这个判据刚好把两种情况分开:
    /// 本进程窗口 = 在操作界面(放行给 egui 滚);别的进程 = 在打游戏(照旧吞,交给映射)。
    ///
    /// 只在滚轮事件里调用(不是每个鼠标事件),两次 user32 查询,量级可忽略;
    /// 这里**不取任何锁**(`WindowFromPoint` 是同步的窗口管理器调用,不会回调我们的钩子)。
    unsafe fn wheel_over_own_window(pt: &POINT) -> bool {
        let hwnd = unsafe { WindowFromPoint(*pt) };
        if hwnd.is_null() {
            return false;
        }
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        pid != 0 && pid == unsafe { GetCurrentProcessId() }
    }

    /// 鼠标钩子回调的**统一收尾**:这条消息还要不要放行给系统。
    ///
    /// 「FPS 鼠标隔膜」(2026-10-11 用户根因定位)打开时返回 1 —— 消息不再投递给
    /// **任何**窗口(含投屏窗口 scrcpy.exe),于是它不会在我们已经注入手机触摸的
    /// 同时自己再转发一套鼠标输入(用户实测的抖动/跳变/不流畅来源)。
    /// 覆盖面与语义见 [`mouse_shield_swallows`];关掉时本函数与我们原来的
    /// `CallNextHookEx` 逐字等价(同样的参数),所以非 FPS 模式行为不变。
    ///
    /// **关键不变量**:收尾只影响"消息投递",与我们的采集无关 ——
    /// 所有调用点都排在转发分支**之后**,位移/按键/滚轮早已进 `raw_tx`;
    /// 而 Raw Input 那条位移通道走的是 `WM_INPUT`(独立消息、不经过钩子,
    /// 别的进程自己的 Raw Input 注册也拦不住,见 `mouse_shield_swallows` 注释)。
    /// 所以吞掉投递不会让 FPS 里的视角采集少一条位移。
    ///
    /// **安全性**:本函数只影响消息投递;钩子在进程退出/崩溃时由系统自动摘除
    /// (`Capture::drop` 里也会显式 `UnhookWindowsHookEx` 并先把开关置回 false),
    /// 因此不存在"用户鼠标被永久锁死"的路径 —— 最坏情况杀掉本进程即恢复。
    ///
    /// **有意为之**:隔膜开着时本程序自己的界面同样点不了(消息不投递给任何窗口)。
    /// 这是用户拍板的取舍:靠开关键(键盘路径,照旧转发给引擎)退出 FPS 模式即恢复。
    unsafe fn mouse_tail(shared: &WinShared, code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        // 第 4 条(2026-10-11):未绑定的鼠标键**直接转发原生事件给 scrcpy** ——
        // 判定要看界面预算好的绑定掩码,不能像以前那样"隔膜开着就全吞"。
        let swallow = shared.swallow.load(Ordering::Relaxed);
        if mouse_shield_swallows(
            shared.mouse_shield.load(Ordering::Relaxed),
            wparam as u32,
            swallow,
        ) {
            return 1;
        }
        unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
    }

    unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        let began = Instant::now();
        if code == HC_ACTION as i32 {
            if let Some(shared) = shared() {
                let kb = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
                // 心跳探针(W1-4):自己用 SendInput 打的 VK 0xFF + 魔数记号,
                // 走到这里说明钩子还活着 —— 记下证据、再吞掉它,不打扰系统输入流。
                // (dead 标志不在这里清:它是给消费侧的"死过一回"事件,由消费侧取走)
                if kb.vkCode as u16 == PROBE_VK
                    && kb.dwExtraInfo == PROBE_MAGIC
                    && kb.flags & LLKHF_INJECTED != 0
                {
                    // 计数 +1(不是时间戳):本回调会在 `SendInput` 期间重入本线程,
                    // 用时刻比较会恒判"无回音"(见 `probe_missed` 的说明)。
                    shared.hb.probe_seen_kb.fetch_add(1, Ordering::Relaxed);
                    return 1;
                }
                let pressed = matches!(wparam as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
                let released = matches!(wparam as u32, WM_KEYUP | WM_SYSKEYUP);
                if pressed || released {
                    // W1-5:带上 LLKHF_EXTENDED —— 小键盘 Enter 与主 Enter、
                    // NumLock 关闭时的小键盘与真导航区,只有这个标志能分开
                    match vktable::map_vk_ex(kb.vkCode as u16, kb.flags & LLKHF_EXTENDED != 0) {
                        Some(ev) => {
                            mark(shared, ev, pressed, began);
                            let _ = shared.raw_tx.send(RawEvent::Key {
                                code: ev,
                                pressed,
                                at: began,
                            });
                            // 被映射的"系统默认功能"候选键(swallow_bit):吞掉,
                            // 不再放行给系统 —— 事件已进我们自己的管线,前台程序
                            // 却收不到(Esc 不再退出全屏)。按下/抬起都吞,只吞一半
                            // 会让前台收到孤儿抬起。掩码是预计算的原子量,这里
                            // 只做一次位测试,钩子回调效率不变。
                            if let Some(bit) = swallow_bit(ev) {
                                if shared.swallow.load(Ordering::Relaxed) & bit != 0 {
                                    return 1;
                                }
                            }
                            // G-b(独占模式,默认关):开关开着且这个 vk 在"已绑定键"
                            // 位图里 ⇒ 按下与抬起**都**吞(与上面的候选键分支同一条
                            // 纪律:只吞一半会让前台收到孤儿抬起)。实测已证钩子
                            // return 1 是链式吞 —— 别的进程也收不到;但拦不住通过
                            // Raw Input 读输入的程序(原理边界,见 keyboard_swallows)。
                            // 判据是纯函数,回调只做两次原子读 + 一次位测试。
                            if keyboard_swallows(
                                shared.swallow_bound_keys.load(Ordering::Relaxed),
                                pressed,
                                kb.vkCode as u16,
                                &shared.bound_keys,
                            ) {
                                return 1;
                            }
                        }
                        None => {
                            // 首次遇到未映射的键码时记一条:用户报"这个键绑不上"时,
                            // 日志里直接就有答案,补一行表即可
                            if vktable::note_unknown_vk(kb.vkCode as u16) {
                                diag_debug!(
                                    "capture",
                                    "未映射的虚拟键码 {} (0x{:02X}),该键当前无法绑定",
                                    kb.vkCode,
                                    kb.vkCode
                                );
                            }
                        }
                    }
                }
            }
        }
        // 默认放行。低级钩子返回非零确实能拦截(旧注释"无法屏蔽"是错的)——
        // 但我们只对少数候选键这么做(上面的 swallow 分支与心跳探针,均提前
        // 返回);整把抓取不做,代价与副作用都不值得。
        unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
    }

    unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        let began = Instant::now();
        if code == HC_ACTION as i32 {
            if let Some(shared) = shared() {
                // 「FPS 鼠标隔膜」(2026-10-11,用户根因定位):开关打开时这里的
                // 每一条鼠标消息最终都会走到下面的 `mouse_tail` 并 `return 1`
                // (吞掉投递)。**转发在我们的采集之前/之中照旧进行** ——
                // 按键/滚轮进 `raw_tx`、位移进 `raw_tx` 或由 Raw Input 送,
                // 隔膜只掐掉"系统把消息投递给窗口"这一步。详见 `mouse_tail`。
                let msg = wparam as u32;
                if msg == WM_MOUSEWHEEL || msg == WM_MOUSEHWHEEL {
                    let ms = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
                    let delta = ((ms.mouseData >> 16) & 0xFFFF) as u16 as i16 as i32;
                    let vertical = msg == WM_MOUSEWHEEL;
                    if delta != 0 {
                        // W1-5:按齿累加 —— 高精度滚轮会把一齿拆成多个小 delta,
                        // 旧的"只看符号"会把一次多齿滚动压成 1 齿、把小 delta 也
                        // 凑成 1 齿。不足一齿的余量留在累加器里等下一枚事件;
                        // 单次事件最多补 WHEEL_NOTCH_CAP 齿(与 Linux 侧同口径)。
                        let acc = if vertical { &WHEEL_ACC_V } else { &WHEEL_ACC_H };
                        let mut a = acc.load(Ordering::Relaxed);
                        let notches = wheel_notches(&mut a, delta);
                        acc.store(a, Ordering::Relaxed);
                        if notches != 0 {
                            let code = if vertical {
                                if notches > 0 {
                                    BTN_WHEEL_UP
                                } else {
                                    BTN_WHEEL_DOWN
                                }
                            } else if notches > 0 {
                                BTN_WHEEL_RIGHT
                            } else {
                                BTN_WHEEL_LEFT
                            };
                            for _ in 0..notches.unsigned_abs() {
                                mark(shared, code, true, began);
                                let _ = shared.raw_tx.send(RawEvent::Key {
                                    code,
                                    pressed: true,
                                    at: began,
                                });
                                mark(shared, code, false, began);
                                let _ = shared.raw_tx.send(RawEvent::Key {
                                    code,
                                    pressed: false,
                                    at: began,
                                });
                            }
                        }
                    }
                    // 滚轮候选键(上/下)被映射(或正被 FPS 滚轮缩放接管)时吞掉
                    // 系统滚动。**残齿也吞**:还没凑够一齿的余量如果放行,系统侧
                    // 会照滚,等于拦截漏了半拍。
                    //
                    // 2026-10-09(用户第 3 条"滚动"):**光标压在本程序自己的窗口上时不吞**。
                    // 只要配置里绑了滚轮(上一轮新增的[鼠标映射]就是干这个的),这个位就
                    // 一直置着,于是界面里所有可滚动控件(宏弹窗、截图小窗、各种清单)都
                    // 只能拖滚动条 —— 用户报的就是这个。判据用 OS 现成的:
                    // 光标下最上层的那个窗口属于本进程 = 用户在操作界面,滚轮该去滚动界面;
                    // 属于别的进程(游戏窗口是独立的 scrcpy.exe)= 照旧吞,交给映射,打游戏不变。
                    let cand = if vertical {
                        if delta > 0 {
                            BTN_WHEEL_UP
                        } else {
                            BTN_WHEEL_DOWN
                        }
                    } else if delta > 0 {
                        BTN_WHEEL_RIGHT
                    } else {
                        BTN_WHEEL_LEFT
                    };
                    if let Some(bit) = swallow_bit(cand) {
                        if shared.swallow.load(Ordering::Relaxed) & bit != 0
                            && !unsafe { wheel_over_own_window(&ms.pt) }
                        {
                            return 1;
                        }
                    }
                    // 隔膜优先于上面那条"光标压在自己窗口上就放行"的例外:
                    // FPS 模式里连本界面的滚轮也不放行(用户拍板的取舍,见 `mouse_tail`)。
                    return unsafe { mouse_tail(shared, code, wparam, lparam) };
                }
                let ev_code = match wparam as u32 {
                    WM_LBUTTONDOWN | WM_LBUTTONUP => Some(272u16), // BTN_LEFT
                    WM_RBUTTONDOWN | WM_RBUTTONUP => Some(273u16), // BTN_RIGHT
                    WM_MBUTTONDOWN | WM_MBUTTONUP => Some(274u16), // BTN_MIDDLE
                    WM_XBUTTONDOWN | WM_XBUTTONUP => {
                        // XBUTTON1/2 -> BTN_SIDE/BTN_EXTRA。
                        // 旧实现写成 275+n(得到 276/277),与 Linux 的
                        // BTN_SIDE=275/BTN_EXTRA=276 差一位,同一颗物理侧键
                        // 在两个平台上会绑成不同的键码,配置无法互通。
                        let ms = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
                        let which = ((ms.mouseData >> 16) & 0xFFFF) as u16;
                        Some(if which == 1 { 275 } else { 276 })
                    }
                    WM_MOUSEMOVE => None,
                    _ => None,
                };
                if let Some(c) = ev_code {
                    let pressed = matches!(
                        wparam as u32,
                        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
                    );
                    mark(shared, c, pressed, began);
                    let _ = shared.raw_tx.send(RawEvent::Key {
                        code: c,
                        pressed,
                        at: began,
                    });
                    // 被映射的候选鼠标键(右键/中键/侧键)吞掉系统默认行为:
                    // 右键不再弹前台菜单、侧键不再让浏览器后退。按下/抬起都吞。
                    if let Some(bit) = swallow_bit(c) {
                        if shared.swallow.load(Ordering::Relaxed) & bit != 0 {
                            return 1;
                        }
                    }
                } else if wparam as u32 == WM_MOUSEMOVE {
                    // W1-2:原始输入一旦接手位移,这条路的坐标差就整体让位 ——
                    // 两条都发会把同一次手部位移记两遍(而且钩子这条还带
                    // 指针加速与边缘丢失)。绝对坐标设备则相反:状态机会停在
                    // RAW_ABSOLUTE_ONLY,这里照发,等于自动回退。
                    //
                    // 注意:这个"让位"只看 Raw Input 状态机,**与隔膜无关** ——
                    // 隔膜开着时这条分支照旧把位移送进 `raw_tx`(原始输入没接管时),
                    // 或被原始输入接管(那条走 `WM_INPUT`,钩子拦不住);判据是纯函数
                    // [`hook_forwards_motion`],单测锁着这条不变量。
                    let st = shared.raw_state.load(Ordering::Relaxed);
                    if hook_forwards_motion(st) {
                        let ms = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
                        let _ = shared.raw_tx.send(RawEvent::Move {
                            x: ms.pt.x as f64,
                            y: ms.pt.y as f64,
                            at: began,
                        });
                        shared.timing.record(began.elapsed().as_nanos() as u64);
                    } else {
                        // 位移被 Raw Input 独占:这是一条**真实被丢掉的位移**
                        // (钩子收到了,却因为原始输入接管而不转发)。
                        let now = now_ms().max(1);
                        if st == RAW_PENDING {
                            // 注册后迟迟等不到第一条 WM_INPUT:记下"开始丢"的时刻
                            // (只记第一次),消费线程据此判"通道根本没在跑"。
                            let _ = shared.raw_drop_ms.compare_exchange(
                                0,
                                now,
                                Ordering::Relaxed,
                                Ordering::Relaxed,
                            );
                        }
                        // DF-9:维护"连续丢、且期间一条 WM_INPUT 都没有"的起点。
                        // 起点之后只要收到过一条 WM_INPUT(`raw_input_ms >= streak`),
                        // 就重新起算 —— 于是通道活着时这个计时被不停重置,
                        // 只有真的静默才会攒满(`raw_stall_due`)。
                        let streak = shared.raw_drop_streak_ms.load(Ordering::Relaxed);
                        let last_input = shared.raw_input_ms.load(Ordering::Relaxed);
                        if streak == 0 || last_input >= streak {
                            shared.raw_drop_streak_ms.store(now, Ordering::Relaxed);
                        }
                    }
                }
                // 收尾:转发已经全部做完,这里只决定"这条消息还要不要放行给系统"。
                // 隔膜开着就 `return 1`(含 `WM_MOUSEMOVE`),关着等于原来的
                // `CallNextHookEx` —— 非 FPS 模式行为逐字不变。
                return unsafe { mouse_tail(shared, code, wparam, lparam) };
            }
        }
        unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
    }

    /// 光标所在显示器的中心与回中阈值
    fn monitor_center() -> ((i32, i32), i32) {
        unsafe {
            let mut pt: POINT = std::mem::zeroed();
            let mon = if GetCursorPos(&mut pt) != 0 {
                MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST)
            } else {
                std::ptr::null_mut()
            };
            let mut mi: MONITORINFO = std::mem::zeroed();
            mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            if !mon.is_null() && GetMonitorInfoW(mon, &mut mi) != 0 {
                let r = mi.rcMonitor;
                let (w, h) = (r.right - r.left, r.bottom - r.top);
                let thr = (w.min(h) / RECENTER_RATIO).max(RECENTER_MIN_PX);
                return (((r.left + r.right) / 2, (r.top + r.bottom) / 2), thr);
            }
            // 兜底:主屏中心(多显示器下不理想,但好过不用)
            (
                (
                    GetSystemMetrics(SM_CXSCREEN) / 2,
                    GetSystemMetrics(SM_CYSCREEN) / 2,
                ),
                RECENTER_MIN_PX,
            )
        }
    }

    /// B21-3/G-c:把光标钉在 `pt` 周围的一个极小矩形里(见 [`super::clip_rect_for`])。
    ///
    /// 返回**系统是否接受**(`ClipCursor` 的返回值)。失败不 panic:最常见的原因是
    /// 调用瞬间焦点窗口变了/桌面在切换 —— 这种情况钉不住就算了,下一圈再试,
    /// 绝不能因为一个"锦上添花"的优化把捕获线程打死。
    ///
    /// 为什么坐标必须由调用方从 `GetCursorPos` 取:多显示器布局、DPI 缩放、屏幕
    /// 旋转都只有系统知道;手搓屏幕坐标必然在某些布局下把光标拽走。
    pub(super) fn clip_cursor_to(pt: (i32, i32)) -> bool {
        let (l, t, r, b) = super::clip_rect_for(pt);
        let rect = RECT {
            left: l,
            top: t,
            right: r,
            bottom: b,
        };
        unsafe { ClipCursor(&rect) != 0 }
    }

    /// 解除光标裁剪(`ClipCursor(NULL)`)。**幂等**:没钉住时调用等于什么都没做。
    ///
    /// 这是全部释放路径的唯一出口:① 抓取翻转 ② 消费线程退出 ③ 线程 panic
    /// (RAII 守卫)④ 捕获层 Drop。第 ⑤ 条(进程被强杀)由 OS 归还 —— Windows
    /// 会在这个进程消失时把它设的裁剪矩形一并清掉,不需要代码。
    pub(super) fn clip_cursor_release() {
        unsafe {
            ClipCursor(std::ptr::null());
        }
    }

    /// G-c 的 RAII 守卫:不管这个线程是正常退出、提前 `break` 还是 **panic**,
    /// 只要守卫被 drop,光标就一定还回去。
    ///
    /// 为什么必须要它:`ClipCursor` 是**进程级全局**状态。捕获线程里任何一个
    /// panic(哪怕与本功能无关)都会跳过收尾代码 —— 没有这条守卫,用户的光标会被
    /// 一直钉在屏幕上,而且**关了程序也没用**(要好到系统察觉进程没了才归还)。
    /// 这是"必须保证任何路径都能恢复"的兜底,不是可选项。
    ///
    /// `active` 只是"我记住自己钉过"的快速判断,不是正确性前提:
    /// drop 时**无条件**调用释放 —— 它幂等,而漏放一次的代价远大于多放一次。
    pub(super) struct ClipGuard {
        active: bool,
    }

    impl ClipGuard {
        /// 建立一个"还没钉"的守卫(线程启动时建,**在循环之前**)。
        pub(super) fn new() -> Self {
            Self { active: false }
        }

        /// 钉住 `pt` 并记住状态;返回本次调用是否被系统接受。
        pub(super) fn pin(&mut self, pt: (i32, i32)) -> bool {
            let ok = clip_cursor_to(pt);
            self.active = ok;
            ok
        }

        /// 主动释放(正常路径)。释放后守卫再被 drop 也是幂等的。
        pub(super) fn release(&mut self) {
            if self.active {
                clip_cursor_release();
                self.active = false;
            }
        }
    }

    impl Drop for ClipGuard {
        fn drop(&mut self) {
            // 无条件放:见结构体注释("多放一次"零代价,"漏放一次"钉死用户光标)。
            clip_cursor_release();
            self.active = false;
        }
    }

    static CURSOR_HIDDEN: AtomicBool = AtomicBool::new(false);
    static CURSOR_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    pub(super) fn set_cursor_visible(visible: bool) {
        let _guard = CURSOR_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        if visible {
            // Always restore the user's configured cursor scheme.  This also
            // repairs a stale blank cursor left by an abruptly killed run.
            let restored = unsafe {
                let ok =
                    SystemParametersInfoW(SPI_SETCURSORS, 0, std::ptr::null_mut(), SPIF_SENDCHANGE);
                SetCursor(LoadCursorW(std::ptr::null_mut(), IDC_ARROW));
                ok
            };
            if restored == 0 {
                diag_warn!(
                    "capture",
                    "恢复系统光标方案失败: {}",
                    std::io::Error::last_os_error()
                );
            }
            CURSOR_HIDDEN.store(false, Ordering::Release);
            return;
        }
        if CURSOR_HIDDEN.load(Ordering::Acquire) {
            return;
        }

        // ShowCursor only controls the calling thread's visible window, which
        // is why scrcpy's window kept painting its own arrow.  Replace the
        // system cursor images with a transparent cursor instead; this is
        // global and does not depend on which application has focus.
        let ids: [SYSTEM_CURSOR_ID; 14] = [
            OCR_NORMAL,
            OCR_IBEAM,
            OCR_WAIT,
            OCR_CROSS,
            OCR_UP,
            OCR_SIZENWSE,
            OCR_SIZENESW,
            OCR_SIZEWE,
            OCR_SIZENS,
            OCR_SIZEALL,
            OCR_NO,
            OCR_HAND,
            OCR_APPSTARTING,
            OCR_HELP,
        ];
        let and_mask = [0xFF_u8, 0xFF];
        let xor_mask = [0_u8, 0];
        let mut installed = false;
        unsafe {
            for id in ids {
                let cursor = CreateCursor(
                    std::ptr::null_mut(),
                    0,
                    0,
                    1,
                    1,
                    and_mask.as_ptr().cast(),
                    xor_mask.as_ptr().cast(),
                );
                if !cursor.is_null() && SetSystemCursor(cursor, id) != 0 {
                    installed = true;
                }
            }
            SetCursor(std::ptr::null_mut());
        }
        CURSOR_HIDDEN.store(installed, Ordering::Release);
        if !installed {
            diag_warn!("capture", "无法安装透明系统光标，指针消隐可能无效");
        }
    }

    fn move_cursor(x: i32, y: i32) {
        unsafe {
            SetCursorPos(x, y);
        }
    }

    /// 鼠标位移与光标状态(只被消费线程碰,因此无需加锁)
    struct Motion {
        last: Option<(f64, f64)>,
        center: (i32, i32),
        threshold: i32,
        /// "我们自己刚把光标挪到过这里"的预期回声。
        ///
        /// 旧实现用一个布尔 `skip_next`:只要置了它就无条件丢弃**下一条**
        /// 鼠标事件。问题在于回声到达的时机没有保证 —— 真实移动可能抢在回声
        /// 之前到达,于是那条真实位移被丢掉;随后回声到达,反而被当成
        /// "从新位置回到中心"的反向位移发出去。快速甩动时每个事件都回中,
        /// 于是每条位移都要经历一次"丢一条 + 发一条反向"。
        /// 改成记录**预期的坐标**:只有位置真的落在那里才判定为回声。
        /// 真位移一来就把预期清掉(那条位移绝不丢失),回声没来也不再赖着。
        echo: Option<(i32, i32)>,
        hiding: bool,
    }

    impl Motion {
        fn new() -> Self {
            let (c, t) = monitor_center();
            Self {
                last: None,
                center: c,
                threshold: t,
                echo: None,
                hiding: false,
            }
        }
    }

    fn consumer(
        rx: std::sync::mpsc::Receiver<RawEvent>,
        tx: Sender<CaptureEvent>,
        mouse_grab: Arc<AtomicBool>,
        // G-c:用户开关(`Settings::clip_cursor_in_aim`),界面每帧写。
        // 与 `mouse_grab` 分开放:那个说"此刻在不在瞄准"(引擎写),这个说
        // "允许不允许钉"(界面写)。决策全在 [`super::should_clip`] 里,
        // 这里只负责把两个值取出来喂给同一个状态机。
        clip_cursor: Arc<AtomicBool>,
        cursor_hide: Arc<AtomicBool>,
        hb_state: Arc<AtomicU8>,
        hook_lag: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
    ) {
        // 注:`mouse_found` 由钩子线程在**鼠标钩子真的装上之后**置位(W0-11),
        // 消费线程不碰它 —— 两个线程是并行的,在这里无条件置 true 会在钩子装
        // 失败时骗过界面自检:用户拿着"检测到鼠标"的绿灯却怎么按都没反应。

        let mut motion = Motion::new();
        let mut coalescer = MotionCoalescer::new();
        let mut pressed: HashSet<u16> = HashSet::new();
        // 鼠标键"补发抬起"的连续确认状态(见 `reconcile_with`):固定数组、
        // 只在消费线程里,不分配堆。
        let mut release_strikes = ReleaseStrikes::default();
        let mut last_reconcile = Instant::now();
        let mut last_report = Instant::now();
        let mut last_raw_recenter = Instant::now();
        let mut last_fg: isize = 0; // 前台窗口句柄:变化即"切换了焦点"
        // G-c:裁剪**已经施加过没有**。与 `motion.hiding` 分开是刻意的:
        // `cursor_hide`(全局消隐)单独为真时不该裁剪,两者翻转点不同。
        let mut clip_applied = false;
        // G-c:被系统拒绝的**累计**次数。用户日志级别下"钉没钉住"只有一条 warn,
        // 而"没钉住"可能是四种原因(开关关 / 没在瞄准 / 被系统拒绝 / 取不到光标位置)。
        // 这条计数器让界面状态行能区分后两种,不必让用户数日志行。
        let mut clip_reject_count: u32 = 0;
        // G-c 的兜底:线程 panic 也必须把光标还回去(见 `ClipGuard`)。
        let mut clip_guard = ClipGuard::new();
        let mut last_coalesce = (0u64, 0u64); // 上次 5s 汇总时的 (events, batches)
        let mut last_hb = Instant::now(); // W1-4:上次发心跳的时刻
        let mut last_loop = Instant::now(); // W1-4:上一圈循环的起点(复核 #4:区分"谁卡了")
        let mut hb_dead_until_ms: u64 = 0; // W1-4:"钩子死过"的展示截止(0=没有)
        let mut hb_last: u8 = 0; // W1-4:镜像给界面的上一次结论(只记跳变)

        while !stop.load(Ordering::Relaxed) {
            // W1-4 复核 #4:把消费侧自己的节拍写进共享 —— 钩子线程判"刚才卡过"
            // 之前先看它:系统睡眠/被抢 CPU 时两边都不会有心跳,那种 pump 变旧
            // 不是钩子线程的账(正常一圈 ≤50ms,只有真被挂起才会到秒级)。
            let loop_gap_ms = last_loop.elapsed().as_millis() as u64;
            last_loop = Instant::now();
            if let Some(s) = shared() {
                s.hb.consumer_gap_ms.store(loop_gap_ms, Ordering::Relaxed);
            }
            // W1-3:窗口到点的位移先发;recv 只等到窗口截止(有攒着的)或 50ms
            let now = Instant::now();
            coalescer.flush_due(&tx, now);
            let wait = coalescer.wait_hint(now, RECV_IDLE);
            match rx.recv_timeout(wait) {
                Ok(RawEvent::Key {
                    code,
                    pressed: down,
                    at,
                }) => {
                    // 顺序点:先把攒着的位移发出去,再发按键
                    coalescer.flush(&tx);
                    // 维护"本地认为按着"的键集合:键按下入表、抬起出表。
                    // 它是 `reconcile` 对账的唯一依据 —— 一次丢失的 KeyUp 只有
                    // 先被这里记着"仍按着",对账才有机会补发抬起。
                    note_key(&mut pressed, code, down);
                    let _ = tx.send(CaptureEvent::Button {
                        code,
                        pressed: down,
                        at,
                    });
                }
                Ok(RawEvent::Move { x, y, at }) => {
                    // 老路径的位移也进同一个窗口(W1-3)——它同样可能到 1000Hz
                    coalescer.flush(&tx);
                    handle_move(
                        &mut motion,
                        &mouse_grab,
                        &cursor_hide,
                        x,
                        y,
                        at,
                        &mut coalescer,
                    );
                }
                Ok(RawEvent::MoveRel { dx, dy, at }) => {
                    // Raw Input 的设备计数就是瞄准要的位移:不做差分、不做回声
                    // 识别、不碰光标(W1-2),只做 1ms 窗口合并(W1-3)。
                    coalescer.push(dx as f32, dy as f32, at);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
            let now = Instant::now();
            coalescer.flush_due(&tx, now);

            // 焦点切换后光标可能落在别的显示器上:重新取一次中心,
            // 否则"回中"会把光标反复拽向一个错误的点,位移几乎全被吃掉。
            let fg = foreground_handle();
            if fg != last_fg {
                last_fg = fg;
                let (c, t) = monitor_center();
                motion.center = c;
                motion.threshold = t;
                motion.last = None;
                motion.echo = None;
                diag_debug!("capture", "前台窗口变化,重新取显示器中心 {:?}", c);
            }

            let grabbing = mouse_grab.load(Ordering::Relaxed);
            let hide_cursor = grabbing || cursor_hide.load(Ordering::Relaxed);
            if hide_cursor != motion.hiding {
                set_cursor_visible(!hide_cursor);
                motion.hiding = hide_cursor;
                if grabbing {
                    let (c, t) = monitor_center();
                    motion.center = c;
                    motion.threshold = t;
                    move_cursor(c.0, c.1);
                    motion.last = Some((c.0 as f64, c.1 as f64));
                    motion.echo = Some(c);
                    diag_info!("capture", "鼠标已捕获(光标隐去并按中心回中)");
                } else if hide_cursor {
                    diag_info!("capture", "系统光标已由全局消隐开关隐藏");
                } else {
                    diag_info!("capture", "系统光标已恢复显示");
                }
            }

            // ---- G-c(钉住光标)施加/释放 ----
            // 三件事分开看,这是整段的核心:
            //   * **什么时候钉** = 既有的 `mouse_grab`(engine.rs:
            //     `enabled && pointer_should_be_hidden`)。本批**不另造判据**;
            //   * **允不允许钉** = `Settings::clip_cursor_in_aim`(界面每帧写);
            //   * **`cursor_hide` 单独为真不钉** —— 那只是"让光标消失",不是瞄准。
            // 决策全在纯函数 `should_clip` / `clip_step` 里(单测见
            // `capture::clip_cursor_tests`),这里只负责调用 Win32。
            //
            // 为什么**每圈都算**而不是只在上面的翻转分支里算:开关(`clip_cursor`)
            // 可以从界面实时翻转,而 `hide_cursor` 那个分支只在状态**变化**时进 ——
            // 挂在里面就会漏掉"用户中途取消了勾选"。每圈两次原子读 + 一次整数比较,
            // 代价可以忽略;真正的 Win32 调用由 `clip_step` 保证只在翻转时发生。
            {
                let want_clip = should_clip(
                    grabbing,
                    cursor_hide.load(Ordering::Relaxed),
                    clip_cursor.load(Ordering::Relaxed),
                );
                match clip_step(clip_applied, want_clip) {
                    ClipAction::Apply => {
                        // 坐标**必须**来自 GetCursorPos(多显示器 / DPI / 旋转只有
                        // 系统知道)。拿不到就保守地不钉 —— 宁可没有这个优化,
                        // 也绝不猜一个坐标把用户的光标拽走。
                        let mut pt: POINT = unsafe { std::mem::zeroed() };
                        if unsafe { GetCursorPos(&mut pt) } != 0 {
                            if clip_guard.pin((pt.x, pt.y)) {
                                clip_applied = true;
                                // WARN 级(2026-10-18 用户实测反馈):这条是"钉住到底成没成"的
                                // **唯一证据**,而用户的 settings.json 是 log_level=warn ⇒
                                // info/debug 全被压掉,导致该功能**无法验收**。G-a 已踩过一次,不许再犯。
                                diag_warn!(
                                    "capture",
                                    "已钉住光标(瞄准期间不再满屏乱跑,位置 {},{};关掉开关或退出瞄准即释放)",
                                    pt.x,
                                    pt.y
                                );
                            } else {
                                // 系统拒绝(桌面切换/焦点窗口正在变):不重试、不刷屏,
                                // 下一圈 want 仍为真时会再来一次。错误码一并落盘 ——
                                // "被拒绝"与"没生效"是两种完全不同的失效,没有码就分不开。
                                clip_reject_count = clip_reject_count.saturating_add(1);
                                diag_warn!(
                                    "capture",
                                    "ClipCursor 被系统拒绝(GetLastError={}),下一圈重试;累计被拒 {} 次",
                                    unsafe { windows_sys::Win32::Foundation::GetLastError() },
                                    clip_reject_count
                                );
                            }
                        } else {
                            clip_reject_count = clip_reject_count.saturating_add(1);
                            diag_warn!("capture", "取不到光标位置,本圈不钉住(保守)");
                        }
                    }
                    ClipAction::Release => {
                        clip_guard.release();
                        clip_applied = false;
                        // WARN 级:与"已钉住"配对,同样是用户日志级别下必须可见的证据。
                        diag_warn!("capture", "已释放光标裁剪(光标可以自由移动了)");
                    }
                    ClipAction::Nothing => {}
                }
            }

            // W1-2:原始输入模式下位移不再来自光标位置,老路径那套"回中 + 回声
            // 识别"就不再是**精度**需要了;但它还是**点击落点**需要 —— 光标飘到
            // 别的显示器/别的窗口上之后,钩子发出去的每一次点击都会落到那个窗口
            // 上(点一下就切走焦点)。所以这里保留一个低频检查:不跟踪回声、
            // 不算差分,只在光标飘远时把它拉回当前显示器中心。
            //
            // **G-c 起这段与 ClipCursor 并存,不是被它取代**:裁剪生效时这条
            // 检查通常不再触发(光标根本出不了那 1x1 像素),但**保留作兜底** ——
            // 钉子可能被拿到前台的其它程序用`ClipCursor(NULL)`覆盖掉、也可能被
            // 系统在某些桌面切换瞬间作废;那时这段仍能把光标拉回来。
            if mouse_grab.load(Ordering::Relaxed)
                && raw_takes_motion_now()
                && last_raw_recenter.elapsed() >= RAW_RECENTER_INTERVAL
            {
                last_raw_recenter = Instant::now();
                unsafe {
                    let mut pt: POINT = std::mem::zeroed();
                    if GetCursorPos(&mut pt) != 0
                        && ((pt.x - motion.center.0).abs() > motion.threshold
                            || (pt.y - motion.center.1).abs() > motion.threshold)
                    {
                        move_cursor(motion.center.0, motion.center.1);
                    }
                }
            }

            if last_reconcile.elapsed() >= RECONCILE_INTERVAL {
                last_reconcile = Instant::now();
                if let Some(s) = shared() {
                    // 轨迹状态心跳:与对账同频(200ms)。**必须放在任何可能卡住的
                    // 东西之前** —— 它同时充当"消费线程还活着"的证据:这些行一停,
                    // 就说明位移断在消费线程,而按键走钩子直发、所以照样能用。
                    crate::trace::state(
                        s.raw_state.load(Ordering::Relaxed),
                        s.raw_input_count.load(Ordering::Relaxed),
                        s.raw_drop_streak_ms.load(Ordering::Relaxed),
                        now_ms(),
                    );
                    reconcile(&mut pressed, &tx, &s.last_seen, &mut release_strikes);
                    // W1-2 兜底:注册成功进入 `RAW_PENDING` 后,钩子回调的位移
                    // 已经让位(见 `raw_takes_motion`),但若一条 WM_INPUT 都没
                    // 等到,位移**两条路都不发** —— 表现是"鼠标彻底不动",而且
                    // 不会自己恢复。判据见 `raw_fallback_due`:一边在丢位移、
                    // 一边收不到 —— 而不是"注册后过了多久"。超时后主动退回光标差
                    // 路径;真来事件时 `raw_admit` 仍会把状态切回 RELATIVE
                    // (自我纠正,并留一行"恢复"日志)。
                    let since = s.raw_drop_ms.load(Ordering::Relaxed);
                    // 单步逻辑见 `watchdog_fallback_step`(R4/W1-B1:生产语义逐字
                    // 不变;`now_ms` 单次采样后判定与告警共用同一时刻)。
                    watchdog_fallback_step(&s.raw_state, since, now_ms(), |dropped| {
                        diag_warn!(
                            "capture",
                            "原始输入注册后持续丢弃位移 {}ms 仍未收到任何鼠标事件:位移退回光标差路径",
                            dropped
                        );
                    });
                    // DF-9:进了 `RAW_RELATIVE` 之后原本**没有任何兜底** —— 位移被
                    // Raw Input 独占,它一静默,钩子那条路即使还在收 WM_MOUSEMOVE
                    // 也不再转发,位移**永久停摆**(真机证据 2026-10-11:t=27736 起
                    // 3 秒零位移、键盘照常,用户只能退出重进 FPS 才恢复)。
                    let streak = s.raw_drop_streak_ms.load(Ordering::Relaxed);
                    if raw_stall_due(s.raw_state.load(Ordering::Relaxed), streak, now_ms()) {
                        s.raw_drop_streak_ms.store(0, Ordering::Relaxed);
                        s.raw_state.store(RAW_PENDING, Ordering::Relaxed);
                        // 让消费线程把差分基准对齐到当前光标(否则回退后会猛跳一下)。
                        s.raw_resync.store(true, Ordering::Relaxed);
                        diag_warn!(
                            "capture",
                            "原始输入静默 {}ms(期间未收到任何 WM_INPUT,而位移一直在丢):\
                             位移退回光标差路径自愈;累计收到 {} 条 WM_INPUT",
                            now_ms().saturating_sub(streak),
                            s.raw_input_count.load(Ordering::Relaxed)
                        );
                    }
                }
            }

            // DF-9:看门狗刚把位移路径退回钩子时,把差分基准对齐到**当前光标** ——
            // 否则钩子路径的第一条位移会把"接管期间漂走的整段坐标差"当成真实位移
            // (文档里的 W2:先卡住一下、再猛跳一下)。
            if let Some(s) = shared()
                && s.raw_resync.swap(false, Ordering::Relaxed)
            {
                let (c, t) = monitor_center();
                motion.center = c;
                motion.threshold = t;
                let mut p = POINT { x: 0, y: 0 };
                if unsafe { GetCursorPos(&mut p) } != 0 {
                    motion.last = Some((p.x as f64, p.y as f64));
                }
                motion.echo = None;
                diag_info!("capture", "位移已退回光标差路径:差分基准已对齐到当前光标");
            }

            // W1-4:每秒给钩子线程投一枚心跳;判定则每轮都看(只是读几个原子量),
            // 这样"探针无回音 → 重装"的 dead 事件在几十毫秒内就能报给界面,
            // 而不是等到下一个整拍。探针、重装都在钩子线程上发生。
            if last_hb.elapsed() >= HB_INTERVAL {
                last_hb = Instant::now();
                if let Some(s) = shared() {
                    let tid = s.hb.tid.load(Ordering::Relaxed);
                    if tid != 0 {
                        unsafe {
                            PostThreadMessageW(tid, WM_APP_HB, 0, 0);
                        }
                    }
                }
            }
            if let Some(s) = shared() {
                // dead 是**事件**(重装探针无回音的钩子时写下):由消费侧取走,
                // 取走才清 —— 探针几毫秒后就会把钩子救回来,不取走就永远看不到。
                // 取走后按 dwell 展示几秒,用户才来得及看见这次"失灵已自愈"。
                let now = now_ms();
                let dead = s.hb.dead_ms.load(Ordering::Relaxed);
                if dead != 0
                    && s.hb
                        .dead_ms
                        .compare_exchange(dead, 0, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                {
                    hb_dead_until_ms = now + HB_DEAD_DWELL_MS;
                }
                let pump = s.hb.pump_ms.load(Ordering::Relaxed);
                let probe_off = s.hb.probe_off.load(Ordering::Relaxed);
                let v = match hb_verdict(now, pump, dead) {
                    // 复核 #5:泵卡死优先于 dwell —— 真卡住了必须说"正在丢事件",
                    // 不能被"已自愈"的 5 秒展示盖住。
                    // 但若卡的是消费侧自己(睡眠/被抢,loop_gap_ms 到秒级),那是
                    // 我们的表停了,不是钩子线程死了:这一轮不下结论,下一轮
                    // (≈50ms 后)pump 若仍不前进,自然会给准话(复核 #4)。
                    HbVerdict::PumpStall if loop_gap_ms < HB_STALL_MS => 1,
                    HbVerdict::PumpStall => 0,
                    // dead 是刚取到(或还没取走)的"死过一回"事件:直接进 2,
                    // 并靠上面取走时种下的 dwell 延续
                    HbVerdict::HookDead => 2,
                    HbVerdict::Ok if now < hb_dead_until_ms => 2,
                    // DF-10(2026-10-11):探针自愈已**永久停用** —— 这是持续状态,
                    // 不是事件。以前停用之后界面一切正常,用户只能靠"按键全部失灵"
                    // 自己发现;现在常驻红字说清"本进程内不再自动重装钩子"。
                    // 泵卡死(1)仍然优先:那是在丢事件,更急。
                    HbVerdict::Ok if probe_off => 3,
                    HbVerdict::Ok => 0,
                };
                if v != hb_last {
                    hb_last = v;
                    match v {
                        0 => diag_info!("capture", "捕获层心跳恢复正常"),
                        1 => diag_warn!(
                            "capture",
                            "捕获层心跳中断:钩子线程 ≥3s 无响应,事件可能正在丢失"
                        ),
                        2 => diag_warn!(
                            "capture",
                            "捕获层钩子失效:探针连续无回音,已触发自动重装(事件可能丢失过)"
                        ),
                        _ => diag_warn!(
                            "capture",
                            "输入捕获自愈已停用:本进程内不再自动重装钩子;若出现按键/视角失灵,请重启程序"
                        ),
                    }
                }
                hb_state.store(v, Ordering::Relaxed);
            }
            if last_report.elapsed() >= TIMING_REPORT_INTERVAL {
                last_report = Instant::now();
                if let Some(s) = shared() {
                    let r = s.timing.take_report();
                    // W1-3:这 5s 里真的收到过位移,就附上合并率 —— bench 用它
                    // 验证 8000Hz 注入被压到 ≤1000 条/s(条:批)。
                    let cx = (coalescer.events, coalescer.batches);
                    let cx_note = if cx.0 > last_coalesce.0 {
                        format!(
                            " · 位移合并 {}(条)→{}(批)",
                            cx.0 - last_coalesce.0,
                            cx.1 - last_coalesce.1
                        )
                    } else {
                        String::new()
                    };
                    last_coalesce = cx;
                    // 判定与文案来自同一份快照(旧实现清零后再取 p99,读到的
                    // 永远是 0,这条健康告警从未真正触发过)。
                    if r.p99_nanos > HOOK_LAG_WARN_NANOS {
                        hook_lag.store(true, Ordering::Relaxed);
                        diag_warn!("capture", "{}{}", r.text, cx_note);
                    } else {
                        hook_lag.store(false, Ordering::Relaxed);
                        diag_debug!("capture", "{}{}", r.text, cx_note);
                    }
                }
            }
        }
        // 退出前把窗口里最后一条位移发掉:那多半就是鼠标停下的最终位置
        coalescer.flush(&tx);
        // G-c 释放路径②(消费线程**正常**退出):先把钉子拔掉,再把光标露出来。
        // 顺序是有意的 —— 如果先恢复光标再解裁剪,中间那一瞬光标是被钉着的,
        // 恢复出来的箭头会被拽回钉点,观感上像"光标自己跳了一下"。
        if clip_applied {
            clip_guard.release();
            diag_warn!("capture", "捕获消费线程退出:已释放光标裁剪");
        }
        if motion.hiding {
            set_cursor_visible(true);
        }
        // 释放路径③(线程 panic)由 `clip_guard` 的 Drop 兜底 —— 它在这里、
        // 也在任何提前 `break`/`return`/`panic` 的路径上自动执行。
        diag_info!("capture", "Windows 捕获消费线程退出");
    }

    /// 处理一次绝对坐标移动:差分 -> 回声识别 -> 必要时回中。
    /// 产出不直接发,而是进 `MotionCoalescer`(W1-3),与 Raw Input 路径同一窗口。
    fn handle_move(
        motion: &mut Motion,
        mouse_grab: &Arc<AtomicBool>,
        cursor_hide: &Arc<AtomicBool>,
        x: f64,
        y: f64,
        at: Instant,
        coalescer: &mut MotionCoalescer,
    ) {
        if mouse_grab.load(Ordering::Relaxed) || cursor_hide.load(Ordering::Relaxed) {
            unsafe {
                SetCursor(std::ptr::null_mut());
            }
        }
        let pos = (x, y);
        let mut suppress = false;
        if let Some(e) = motion.echo {
            if (x.round() as i32, y.round() as i32) == e {
                // 这正是我们自己 SetCursorPos 触发的回声:丢弃,但把 last 对齐到它
                motion.echo = None;
                suppress = true;
            } else {
                // 回声没来(被系统合并/丢弃),或抢在了真位移后面:
                // 无论哪种,这条都是真实位移,必须发出去。
                motion.echo = None;
            }
        }
        let d = match motion.last {
            Some((px, py)) => (x - px, y - py),
            None => (0.0, 0.0),
        };
        motion.last = Some(pos);
        if !suppress && (d.0 != 0.0 || d.1 != 0.0) {
            coalescer.push(d.0 as f32, d.1 as f32, at);
        }

        if mouse_grab.load(Ordering::Relaxed)
            && ((x - motion.center.0 as f64).abs() > motion.threshold as f64
                || (y - motion.center.1 as f64).abs() > motion.threshold as f64)
        {
            let c = motion.center;
            move_cursor(c.0, c.1);
            motion.last = Some((c.0 as f64, c.1 as f64));
            motion.echo = Some(c);
        }
    }

    /// 当前前台窗口的句柄(转成 isize 便于比较)。
    ///
    /// 句柄变化 ≈ 用户切换了程序/窗口焦点。为什么要盯着它:切换之后光标很可能
    /// 落在**另一个显示器**上,而回中用的中心点还是旧的那个 —— 于是回中条件
    /// 几乎每个事件都成立,表现就是"视角能动但幅度极小、一卡一卡"。
    /// 用窗口句柄而不是桌面会话 id 是为了少依赖一个 Win32 API(会话 id 只在
    /// 多用户/远程桌面场景才有区别,对焦点切换这个用途毫无增益)。
    fn foreground_handle() -> isize {
        use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
        unsafe { GetForegroundWindow() as isize }
    }

    /// 维护"本地认为按着"的键集合:按下入表、抬起出表。
    ///
    /// Windows 的键盘自动重复会连发多次 KEYDOWN,而 Set 的插入天然幂等,
    /// 无需额外去重;抬起必须真正移除,否则对账会把已松开的键永远当成"卡住"。
    fn note_key(pressed: &mut HashSet<u16>, code: u16, down: bool) {
        if down {
            pressed.insert(code);
        } else {
            pressed.remove(&code);
        }
    }

    /// 与系统对账:只有"键还在本地按下表 + 系统也明确抬起 + 距最后事件已过宽限期"
    /// 三条同时成立,才认定 release 真丢了。
    ///
    /// 旧实现只看 `GetAsyncKeyState`,会在真实 KeyRelease 还在队列里排队时就
    /// 抢先把键判死:随后向引擎补发一个 UP,而键盘自动重复的下一帧 DOWN 又被
    /// 当成全新按下,于是 Hold 触点会被反复重按。A+U+K 这类"一个持续方向 +
    /// 多个 Hold 技能"的组合正好能持续供给重复 DOWN,所以会卡成死循环。
    ///
    /// **鼠标键另加两道闸**(用户 2026-10-11:"开火键按不下去"):见
    /// [`reconcile_with`] 与 [`mouse_release_confirmed`]。
    fn reconcile(
        pressed: &mut HashSet<u16>,
        tx: &Sender<CaptureEvent>,
        last_seen: &[AtomicU64],
        strikes: &mut ReleaseStrikes,
    ) {
        let out = reconcile_with(
            pressed,
            tx,
            last_seen,
            now_ms(),
            |code| {
                // (B) 鼠标键**先问 Raw Input 位图**:它说按着就直接算按着 ——
                // `GetAsyncKeyState` 在前台是别的进程(玩的时候就是 scrcpy)时对
                // 鼠标键会谎报 0,那种"读数失明"绝不能被当成"抬起丢了"。
                if let Some(down) = raw_button_oracle(code) {
                    return Some(down);
                }
                // W1-5:把同一物理键的全部别名 VK 都问一遍(小键盘在 NumLock 开/关
                // 时报不同 VK)—— 任一按着就算还按着。宁可少报一次"丢了抬起"
                // (宽限期 + 下一个真事件兜底),也不能把还按着的键误判成已释放
                // —— 那正是卡键与 Hold 连环重按的来源。没映射的码按"问不着"跳过。
                let vks = vktable::vks_for_evdev(code);
                if vks.is_empty() {
                    return None;
                }
                // 高位为 1(负数)表示此刻按着。⚠ 返回 0 有两种含义:
                // "确实没按下" 与 "这次调用失败了"(见 [`reconcile_with`] 的长注释),
                // 两者在 API 上**无法区分** —— 所以下面的判据不能只看一次读数。
                let mut raw = 0i16;
                for &vk in &vks {
                    let v = unsafe { GetAsyncKeyState(vk as i32) };
                    if v < 0 {
                        raw = v;
                        break;
                    }
                }
                Some(raw)
            },
            strikes,
        );
        if !out.lost.is_empty() {
            diag_warn!(
                "capture",
                "权威对账:宽限期内没有任何后续事件且系统确认已抬起,补发 KeyRelease 以防卡键: {}(依据: {};{})",
                out.lost
                    .iter()
                    .map(|&c| crate::keymap::key_name(c))
                    .collect::<Vec<_>>()
                    .join(" "),
                out.evidence(),
                foreground_owner(),
            );
        }
        // 鼠标键"有嫌疑但被扣下"的取证:只在**每一段嫌疑的第一拍**记一行
        // (读通道一直不可信时这条会一直成立,不能每 200ms 刷一行)。
        for &(code, raw, since, n) in &out.suspects {
            if n != 1 {
                continue;
            }
            let why = if out.measured_down {
                "读通道正常:这只是第一拍,等下一拍确认"
            } else {
                "本轮没有任何键被读到按下 ⇒ 读通道本身可能失效(整次调用失败也返回 0)"
            };
            diag_warn!(
                "capture",
                "权威对账:{} 读到\"未按下\"但本次**没有**补发抬起({why});raw={raw},距最后一次事件 {since}ms;{}。\
                 若下一次日志里它反复出现,说明 GetAsyncKeyState 在这个环境下不可信,而不是真的丢了抬起。",
                crate::keymap::key_name(code),
                foreground_owner(),
            );
        }
    }

    /// 判定"补发抬起"时,前台窗口属于谁(取证用)。
    ///
    /// 为什么值得记:MSDN 在 `GetAsyncKeyState` 的"返回 0"里列出了失败原因 ——
    /// ①当前桌面不是活动桌面;②**UIPI 阻止调用线程访问前台线程**;
    /// ③**前台线程属于别的进程**,而调用线程没有该桌面的 DESKTOP_HOOKCONTROL /
    /// DESKTOP_JOURNALRECORD 权限。②③在"别的进程(游戏)在前台"时正好成立,
    /// 而返回值 0 与"键确实抬起了"**无法区分**。这一行就是为了让下一次真机日志
    /// 能一眼定案:读数 0 + 前台是别的进程 = 说谎的是 API,不是用户。
    fn foreground_owner() -> String {
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.is_null() {
                return "前台窗口: 无".to_string();
            }
            let mut pid = 0u32;
            let tid = GetWindowThreadProcessId(hwnd, &mut pid);
            format!(
                "前台线程 tid={tid} pid={pid}(本进程 pid={})",
                std::process::id()
            )
        }
    }

    /// `reconcile` 的可测版本:把"问系统这个键是否按着"抽成闭包
    /// (返回 `None` = 该键码没有 VK 映射、无法询问,按"不动"处理;
    /// 返回 `Some(raw)` = `GetAsyncKeyState` 的原始读数,< 0 才算按下)。
    /// 判定主体与真实现共用同一份代码,单测因此能覆盖真实行为。
    ///
    /// # 鼠标键为什么不能"一次读数就动手"(用户 2026-10-11,取证结论)
    ///
    /// 1. **鼠标键永远不会自动重复**,所以 `last_seen` 只在该键自己的事件里刷新。
    ///    用户按住开火键不动时,过了 [`RECONCILE_GRACE_MS`] 宽限期,这个判定就
    ///    退化成"只看一次 `GetAsyncKeyState`" —— 键盘还有 ~33ms 的重复 DOWN
    ///    兜底,鼠标键没有。
    /// 2. **`GetAsyncKeyState` 返回 0 有两种含义**,MSDN 写在"Return value"里:
    ///    "The return value is zero if the call fails. Reasons for failure include
    ///    the current desktop is not the active desktop; UIPI prevents the calling
    ///    thread from accessing the foreground thread; the foreground thread belongs
    ///    to another process and the calling thread does not have DESKTOP_HOOKCONTROL
    ///    or DESKTOP_JOURNALRECORD access to its desktop."
    ///    也就是说:**打游戏时前台是别的进程(游戏),这次调用完全可能失败并返回 0,
    ///    而这与"键已经抬起"逐位相同**。2026-10-11 的真机日志里,BTN_LEFT 在
    ///    开局两分钟内被"补发 KeyRelease"三次 —— 每一次都发生在"用户按住开火"
    ///    期间,且我们**从未收到过 WM_LBUTTONUP**(`last_seen` 非 0 才可能进这个
    ///    分支)。两条权威互相矛盾时,旧实现选择相信那个**可能根本不可用**的读数,
    ///    于是把正在按着的开火触点抬掉了。
    ///
    /// 所以鼠标键要求两条同时成立(见 [`mouse_release_confirmed`]):
    /// ①**读通道自证在工作**:本轮至少要有一个被询问的键读到了"按下"
    ///   (`measured_down`)。API 失败是**整次调用**的失败(不是单键),所以只要
    ///   有一个键读到了非 0,就说明这次调用没失败,那么某个鼠标键的 0 就是真的"没按下"。
    ///   ⚠ 只有鼠标键在按下表里时(用户只按着开火、一个键盘键都没按)这一条**永远
    ///   不可能成立** —— 那时我们**故意不猜**:宁可不补发,也不拿一条零信息的读数
    ///   去抬正在按着的开火触点。
    /// ②**连续两拍确认**且**期间没有新事件**(`last_seen` 没变过)。
    ///
    /// 为什么这样不会重新引入"卡键":键盘键的判据**一个字都没改**(自动重复天然
    /// 免疫);鼠标键只多等一拍(≤ [`RECONCILE_INTERVAL`] 200ms,而原来也要先等
    /// 700ms 宽限),**真的丢了抬起**迟早会被判出来并补发。唯独"读通道整体不可信"
    /// 时我们**拒绝猜**:此时那条 0 读数不携带任何信息,凭它去抬一个可能正被按住
    /// 的触点,代价是"开火/瞄准按不住";而不抬的代价只是"可能晚一次点击才恢复"
    /// (用户松开再按一次,或钩子探针重装机制都会收口)。宁可少抬,不可错抬。
    fn reconcile_with(
        pressed: &mut HashSet<u16>,
        tx: &Sender<CaptureEvent>,
        last_seen: &[AtomicU64],
        now: u64,
        system_down: impl Fn(u16) -> Option<i16>,
        strikes: &mut ReleaseStrikes,
    ) -> ReconcileOutcome {
        let mut out = ReconcileOutcome::default();
        if pressed.is_empty() {
            return out;
        }
        // 先统一问一遍:读通道是否"自证在工作"必须看**本轮所有读数的整体**
        // (API 失败是整次调用的失败),不能只看有嫌疑的那一个键。
        let mut readings: Vec<(u16, i16, u64)> = Vec::new();
        for &code in pressed.iter() {
            let Some(raw) = system_down(code) else {
                continue;
            };
            out.measured_down |= raw < 0;
            let seen = last_seen
                .get(code as usize)
                .map(|v| v.load(Ordering::Relaxed))
                .unwrap_or(0);
            readings.push((code, raw, seen));
        }
        for &(code, raw, seen) in &readings {
            let down = raw < 0;
            let since = now.saturating_sub(seen);
            let lost_now = release_is_lost(down, seen, now, RECONCILE_GRACE_MS);
            let n = strikes.next(code, seen, lost_now);
            if !lost_now {
                continue;
            }
            if is_reconcile_mouse_button(code) {
                if mouse_release_confirmed(out.measured_down, n) {
                    out.lost.push(code);
                    out.lost_evidence.push((code, raw, since));
                } else {
                    out.suspects.push((code, raw, since, n));
                }
            } else {
                out.lost.push(code);
                out.lost_evidence.push((code, raw, since));
            }
        }
        for &code in &out.lost {
            pressed.remove(&code);
            // 合成事件:按补发时刻计(W1-1)
            let _ = tx.send(CaptureEvent::Button {
                code,
                pressed: false,
                at: Instant::now(),
            });
        }
        out
    }

    /// 一次对账的取证结果 —— 日志与单测都读它。
    #[derive(Default)]
    pub(super) struct ReconcileOutcome {
        /// 真的补发了抬起的键
        pub(super) lost: Vec<u16>,
        /// 补发依据:(键码, 原始读数, 距最后事件毫秒)
        pub(super) lost_evidence: Vec<(u16, i16, u64)>,
        /// 鼠标键"有嫌疑但被扣下"的记录:(键码, 原始读数, 距最后事件毫秒, 连续命中拍数)
        pub(super) suspects: Vec<(u16, i16, u64, u8)>,
        /// 本轮是否至少有一个键被读到"按下"(读通道是否自证在工作)
        pub(super) measured_down: bool,
    }

    impl ReconcileOutcome {
        /// 日志用的原始依据:每个被补发的键的读数与静默时长
        fn evidence(&self) -> String {
            self.lost_evidence
                .iter()
                .map(|&(c, raw, since)| {
                    format!(
                        "{}(raw={raw}, 距最后事件 {since}ms)",
                        crate::keymap::key_name(c)
                    )
                })
                .collect::<Vec<_>>()
                .join(" ")
        }
    }

    /// 「补发抬起」的连续确认状态(鼠标键专用,见 [`reconcile_with`])。
    ///
    /// 每个键记两样:上一轮核对时的 `last_seen`(判断"这期间是不是来了新事件")
    /// 与"已经连续命中几拍"。固定 [`KEY_STATE_LEN`] 项、只在捕获消费线程里存在
    /// 一份,不分配堆、不跨线程共享。
    pub(super) struct ReleaseStrikes {
        seen_at_check: [u64; KEY_STATE_LEN],
        strikes: [u8; KEY_STATE_LEN],
    }

    impl Default for ReleaseStrikes {
        fn default() -> Self {
            Self {
                seen_at_check: [0; KEY_STATE_LEN],
                strikes: [0; KEY_STATE_LEN],
            }
        }
    }

    impl ReleaseStrikes {
        /// 推进 `code` 的连续确认计数并返回新值(状态更新见 [`next_strike`])。
        fn next(&mut self, code: u16, seen: u64, lost_now: bool) -> u8 {
            let i = code as usize;
            if i >= KEY_STATE_LEN {
                // 越界只可能来自"没有 VK 映射的怪码",直接当作无需确认(永不补发)。
                return 0;
            }
            let changed = self.seen_at_check[i] != seen;
            self.seen_at_check[i] = seen;
            self.strikes[i] = next_strike(self.strikes[i], lost_now, changed);
            self.strikes[i]
        }
    }

    /// 连续确认计数的推进(纯函数,便于单测)。
    /// - 这一轮不成立(系统说按下 / 还没过宽限 / 没有按下心跳)⇒ 归零
    /// - 成立且这期间**没有新事件**(`last_seen` 没变)⇒ +1
    /// - 成立但这期间来过新事件 ⇒ 从 1 重新数(刚有动静,不算"一直没消息")
    pub(super) fn next_strike(prev: u8, lost_now: bool, seen_changed: bool) -> u8 {
        if !lost_now {
            0
        } else if seen_changed {
            1
        } else {
            prev.saturating_add(1)
        }
    }

    /// 本模块"鼠标键"的口径:**左 / 右 / 中 / 侧键**(evdev 272..=276)。
    ///
    /// 与 `keymap::is_mouse_button`(272..=274,界面"这一下按在按钮上"的判定)
    /// 刻意分开:侧键同样没有自动重复、同样会被这次误判抬掉;在对账这条路上把它们
    /// 一并纳入只多一层确认,不牵动界面语义。
    fn is_reconcile_mouse_button(code: u16) -> bool {
        (272..=276).contains(&code)
    }

    /// 鼠标键这一轮能不能真的补发抬起(见 [`reconcile_with`] 的取证结论):
    /// 读通道自证在工作 **且** 连续命中满 [`MOUSE_RELEASE_STRIKES`] 拍。
    pub(super) fn mouse_release_confirmed(channel_healthy: bool, strikes: u8) -> bool {
        channel_healthy && strikes >= MOUSE_RELEASE_STRIKES
    }

    /// `seen == 0` 表示我们从未记录到按下心跳,宁可不动,避免凭空补抬。
    /// 判定公式抽出来便于单元测试:系统抬起、且最后一次事件已超过宽限期。
    pub(super) fn release_is_lost(
        system_down: bool,
        seen_ms: u64,
        now_ms: u64,
        grace_ms: u64,
    ) -> bool {
        !system_down && seen_ms != 0 && now_ms.saturating_sub(seen_ms) >= grace_ms
    }

    /// Raw Input 兜底是否到点(纯判定,便于单测)。
    ///
    /// 三个条件缺一不可:①还在等第一条 WM_INPUT(`RAW_PENDING`);②**真的丢过**
    /// 位移(`drop_ms != 0`)—— 用户注册后就是没动鼠标时没有位移可丢,那种情况
    /// 不是故障,拿"注册时刻"去超时会天天误报;③从开始丢起已满
    /// [`RAW_PENDING_TIMEOUT_MS`] —— "丢了几条 raw 就追上来"是毫秒级的事,700ms
    /// 足够排除这种正常时序。
    ///
    /// 时钟异常(`drop_ms` 落在未来)经 `saturating_sub` 得 0 → 判不到期:
    /// 宁可不退回,也不能误判。
    pub(super) fn raw_fallback_due(state: u8, drop_ms: u64, now_ms: u64) -> bool {
        state == RAW_PENDING
            && drop_ms != 0
            && now_ms.saturating_sub(drop_ms) >= RAW_PENDING_TIMEOUT_MS
    }

    /// 消费线程看门狗的"判定→切状态→记一次退回告警"单步缝(R4/W1-B1)。
    ///
    /// 与内联版逐字一致:[`raw_fallback_due`] 判到点才把状态切到
    /// `RAW_ABSOLUTE_ONLY` 并调一次 `warn`(参数 = 已丢弃时长 ms);判不到点
    /// 什么都不动。"告警只记一次"由状态机自己保证:切走后 `raw_fallback_due`
    /// 的 `RAW_PENDING` 门不再满足,后续步天然静默。`now_ms` 与 `warn` 都可注入,
    /// 测试得以在内存里驱动 699/700/再一步三拍(§3.5:不碰 diag 文件)。
    fn watchdog_fallback_step(state: &AtomicU8, drop_ms: u64, now_ms: u64, warn: impl FnOnce(u64)) {
        if raw_fallback_due(state.load(Ordering::Relaxed), drop_ms, now_ms) {
            state.store(RAW_ABSOLUTE_ONLY, Ordering::Relaxed);
            warn(now_ms.saturating_sub(drop_ms));
        }
    }

    /// DF-9:Raw Input 在接管之后静默多久就判"这条通道哑了"(毫秒)。
    ///
    /// 500ms:比"丢几条 raw 事件之后才追上"的正常时序(毫秒级)宽得多,又短到
    /// 用户只会觉得"卡了一下",而不是"视角死了、只能退出重进 FPS"。
    const RAW_STALL_TIMEOUT_MS: u64 = 500;

    /// DF-9:Raw Input 在 **`RAW_RELATIVE` 期间静默**时是否该退回钩子路径。
    ///
    /// 与 [`raw_fallback_due`] 的分工:那个管"注册后一直没等到第一条事件",
    /// 这个管"已经接管了、然后哑了" —— 后者原本**完全没有兜底**,位移会永久停摆。
    /// `streak_ms` 是"连续丢位移、且期间没有 WM_INPUT"的起点(钩子回调维护,
    /// 收到一条事件就清零),所以这条判据不会因为"用户暂时没动鼠标"而误报。
    pub(super) fn raw_stall_due(state: u8, streak_ms: u64, now_ms: u64) -> bool {
        state == RAW_RELATIVE
            && streak_ms != 0
            && now_ms.saturating_sub(streak_ms) >= RAW_STALL_TIMEOUT_MS
    }

    #[cfg(test)]
    mod tests {
        use super::{
            CaptureEvent, HC_ACTION, HOOK_LAG_WARN_NANOS, Instant, LPARAM, MSLLHOOKSTRUCT, POINT,
            RAW_ABSOLUTE_ONLY, RAW_PENDING, RAW_PENDING_TIMEOUT_MS, RAW_RELATIVE, RAW_UNREGISTERED,
            RawAdmit, RawEvent, RawKind, ReleaseStrikes, Timing, WM_MOUSEMOVE, WPARAM, WinShared,
            hook_forwards_motion, install_shared, is_reconcile_mouse_button, mouse_proc,
            mouse_release_confirmed, mouse_shield_swallows, next_strike, note_key, raw_admit,
            raw_fallback_due, raw_kind, raw_takes_motion, reconcile_with, release_is_lost, shared,
        };
        use std::collections::HashSet;
        use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
        use std::sync::mpsc::channel;
        use std::sync::{Arc, Mutex, MutexGuard};

        /// `install_shared` 写的是**进程级全局** `SHARED_PTR`,而测试是并行跑的:
        /// 凡是要动这个全局的测试都先拿这把锁,免得互相把对方装的共享结构换掉
        /// (否则是偶发红灯)。中毒也照拿(`unwrap_or_else`):一次失败不该连累
        /// 后面每个测试都 panic。
        static SHARED_LOCK: Mutex<()> = Mutex::new(());

        fn lock_shared_global() -> MutexGuard<'static, ()> {
            SHARED_LOCK.lock().unwrap_or_else(|e| e.into_inner())
        }

        /// 造一条鼠标钩子事件(测试用:钩子回调只读 `pt` 与 `mouseData`)
        fn mouse_event(x: i32, y: i32, mouse_data: u32) -> MSLLHOOKSTRUCT {
            MSLLHOOKSTRUCT {
                pt: POINT { x, y },
                mouseData: mouse_data,
                flags: 0,
                time: 0,
                dwExtraInfo: 0,
            }
        }

        /// (B) 鼠标键的独立判据:Raw Input 位图说"按着"时,`GetAsyncKeyState` 的
        /// 谎报(0)不许被判成"抬起丢了"。
        ///
        /// 真机证据(2026-10-11):同一轮对账里**键盘读得到按下、鼠标左键读不到**,
        /// 于是开火键被补发了一次假释放 —— 扫射中途停火。位图来自另一条管道
        /// (`RIDEV_INPUTSINK`,前台是别的进程也收得到),所以它才是可信的那一方。
        /// DF-9:**接管之后静默**才该退回;没在丢位移、或还没接管,都不许动。
        ///
        /// 这条守护的是真机实证的"视角锁死":位移被 Raw Input 独占后它哑了,
        /// 钩子那条路即使还在收消息也不再转发 —— 3 秒零位移、键盘照常,
        /// 只能退出重进 FPS。
        #[test]
        fn raw_stall_needs_dropped_motion_after_takeover() {
            use super::{RAW_PENDING, RAW_RELATIVE, RAW_STALL_TIMEOUT_MS, raw_stall_due};
            let t = RAW_STALL_TIMEOUT_MS;
            assert!(
                !raw_stall_due(RAW_RELATIVE, 0, 10_000),
                "没在丢位移 = 用户没动鼠标,不是故障"
            );
            assert!(
                !raw_stall_due(RAW_RELATIVE, 10_000 - (t - 1), 10_000),
                "刚开始丢,还不够久"
            );
            assert!(
                raw_stall_due(RAW_RELATIVE, 10_000 - t, 10_000),
                "恰好满一个窗口"
            );
            assert!(
                !raw_stall_due(RAW_PENDING, 1, 10_000),
                "那个状态由 raw_fallback_due 管,别重复动手"
            );
            assert!(
                !raw_stall_due(RAW_RELATIVE, 20_000, 10_000),
                "时钟异常(起点在未来):宁可不退"
            );
        }

        #[test]
        fn raw_button_bitmap_vetoes_a_synthesised_mouse_release() {
            use super::{
                RAW_BTN_DOWN, RECONCILE_GRACE_MS, note_raw_buttons, raw_btn_bit, raw_button_oracle,
                release_is_lost,
            };
            use std::sync::atomic::Ordering;
            // 位图是进程级全局,先清干净(只有本测试写它)
            RAW_BTN_DOWN.store(0, Ordering::Relaxed);
            assert_eq!(raw_btn_bit(272), Some(1 << 0));
            assert_eq!(raw_btn_bit(17), None, "键盘键不进位图");
            // 左键按下 ⇒ 位图说按着:即便读通道谎报 0,也不许补发抬起
            note_raw_buttons(0x0001);
            assert_eq!(raw_button_oracle(272), Some(-1));
            let down = raw_button_oracle(272).unwrap_or(0) < 0;
            assert!(!release_is_lost(down, 1, 5000, RECONCILE_GRACE_MS));
            // 左键抬起 ⇒ 位图**不下结论**(回落 GetAsyncKeyState):它只负责"否决补发",
            // 不负责宣布按下 —— 这样"注册 Raw Input 之前就已经按着"的设备也不会被骗。
            note_raw_buttons(0x0002);
            assert_eq!(raw_button_oracle(272), None);
            let down = raw_button_oracle(272).unwrap_or(0) < 0;
            assert!(release_is_lost(down, 1, 5000, RECONCILE_GRACE_MS));
            // 五个键各自独立:右/中/侧键抬起不影响左键
            note_raw_buttons(0x0001);
            note_raw_buttons(0x0008);
            note_raw_buttons(0x0020);
            note_raw_buttons(0x0200);
            assert_eq!(raw_button_oracle(272), Some(-1), "左键仍然按着");
            assert_eq!(raw_button_oracle(273), None);
            assert_eq!(raw_button_oracle(274), None);
            assert_eq!(raw_button_oracle(276), None);
            RAW_BTN_DOWN.store(0, Ordering::Relaxed);
        }

        /// W1-2:相对/绝对/空位移的分类。绝对坐标判定优先于"零位移"——
        /// 数位板悬停时会给出 lLastX==lLastY==0 的绝对事件,若先判零位移
        /// 就会把它当空事件丢掉、状态机也停在原地,而它其实是"这是绝对设备"
        /// 的唯一线索。
        #[test]
        fn raw_kind_prefers_absolute_over_empty() {
            use super::MOUSE_MOVE_ABSOLUTE;
            assert_eq!(raw_kind(0, 10, -4), RawKind::Relative);
            assert_eq!(raw_kind(0, 0, 0), RawKind::Empty);
            assert_eq!(raw_kind(MOUSE_MOVE_ABSOLUTE, 100, 200), RawKind::Absolute);
            assert_eq!(raw_kind(MOUSE_MOVE_ABSOLUTE, 0, 0), RawKind::Absolute);
        }

        /// W1-2:状态机的四条转移。核心不变量:相对设备一经出现就由原始输入
        /// 接管(RAW_RELATIVE 不会退回),绝对设备在没被接管时退回钩子路径。
        #[test]
        fn raw_state_machine_admits_relative_and_falls_back_for_absolute() {
            // 相对位移:接管并发出
            assert_eq!(
                raw_admit(RAW_PENDING, RawKind::Relative),
                (RAW_RELATIVE, RawAdmit::Emit)
            );
            // 绝对坐标设备:退回钩子路径(状态固定为"只有绝对设备")
            assert_eq!(
                raw_admit(RAW_PENDING, RawKind::Absolute),
                (RAW_ABSOLUTE_ONLY, RawAdmit::Fallback)
            );
            assert_eq!(
                raw_admit(RAW_UNREGISTERED, RawKind::Absolute),
                (RAW_ABSOLUTE_ONLY, RawAdmit::Fallback)
            );
            // 已被相对设备接管后又来绝对事件:保持接管,只记一条
            assert_eq!(
                raw_admit(RAW_RELATIVE, RawKind::Absolute),
                (RAW_RELATIVE, RawAdmit::MixedAbsolute)
            );
            // 空位移不改变状态
            assert_eq!(
                raw_admit(RAW_PENDING, RawKind::Empty),
                (RAW_PENDING, RawAdmit::Drop)
            );
            assert_eq!(
                raw_admit(RAW_ABSOLUTE_ONLY, RawKind::Empty),
                (RAW_ABSOLUTE_ONLY, RawAdmit::Drop)
            );
            // 混合场景下相对设备后来居上
            assert_eq!(
                raw_admit(RAW_ABSOLUTE_ONLY, RawKind::Relative),
                (RAW_RELATIVE, RawAdmit::Emit)
            );
        }

        /// R2:逐态×事件转移表补全缺失格。4 态 × 3 事件 = 12 格,
        /// 邻居 `raw_state_machine_admits_relative_and_falls_back_for_absolute`
        /// 已锁 7 格(含 ABSOLUTE_ONLY+Empty),这里补上其余 5 格:
        /// - UNREGISTERED+Relative:相对设备一到就接管,不必先经过 PENDING;
        /// - UNREGISTERED+Empty:未注册时空事件不许把状态机挪出 UNREGISTERED;
        /// - RELATIVE+Relative:接管后自循环,后续每条相对位移照发;
        /// - RELATIVE+Empty:接管后空位移既不退出接管也不发(Drop 而非 MixedAbsolute);
        /// - ABSOLUTE_ONLY+Absolute:绝对设备自循环,重复绝对事件反复退回钩子,
        ///   状态不漂移、也不许被误判成 MixedAbsolute(那格只属于 RELATIVE)。
        #[test]
        fn raw_state_machine_transition_table_is_total() {
            assert_eq!(
                raw_admit(RAW_UNREGISTERED, RawKind::Relative),
                (RAW_RELATIVE, RawAdmit::Emit)
            );
            assert_eq!(
                raw_admit(RAW_UNREGISTERED, RawKind::Empty),
                (RAW_UNREGISTERED, RawAdmit::Drop)
            );
            assert_eq!(
                raw_admit(RAW_RELATIVE, RawKind::Relative),
                (RAW_RELATIVE, RawAdmit::Emit)
            );
            assert_eq!(
                raw_admit(RAW_RELATIVE, RawKind::Empty),
                (RAW_RELATIVE, RawAdmit::Drop)
            );
            assert_eq!(
                raw_admit(RAW_ABSOLUTE_ONLY, RawKind::Absolute),
                (RAW_ABSOLUTE_ONLY, RawAdmit::Fallback)
            );
        }

        /// W1-2:钩子只在"原始输入会发位移"时才让位 ——
        /// 未启用与"只有绝对设备"两种状态下钩子必须继续发。
        #[test]
        fn hook_yields_motion_only_while_raw_will_deliver() {
            assert!(!raw_takes_motion(RAW_UNREGISTERED));
            assert!(raw_takes_motion(RAW_PENDING));
            assert!(raw_takes_motion(RAW_RELATIVE));
            assert!(!raw_takes_motion(RAW_ABSOLUTE_ONLY));
        }

        /// 用户 2026-10-11 根因修复的**不变量**:「FPS 鼠标隔膜」只掐"消息投递",
        /// 不能连"位移采集"一起掐掉 —— 否则 FPS 里视角直接不动。
        ///
        /// 判据是纯函数 `hook_forwards_motion`,它连隔膜开关都不是参数:钩子这条
        /// 路的转发只由 Raw Input 状态机决定。而原始输入接管时位移走 `WM_INPUT`,
        /// 那条消息不经过钩子、也无法被钩子拦住 —— 两条路都与隔膜无关。
        #[test]
        fn shield_never_stops_motion_capture() {
            // 原始输入没接手(未注册 / 只有绝对设备):钩子**必须**照发位移,
            // 隔膜开着也一样(否则绝对坐标设备在 FPS 里彻底没视角)
            assert!(hook_forwards_motion(RAW_UNREGISTERED));
            assert!(hook_forwards_motion(RAW_ABSOLUTE_ONLY));
            // 原始输入会发位移:钩子让位 —— 这是去重,不是"隔膜吞掉了",
            // 与隔膜开关无关(邻居 `hook_yields_motion_only_while_raw_will_deliver` 锁同一件事)
            assert!(!hook_forwards_motion(RAW_PENDING));
            assert!(!hook_forwards_motion(RAW_RELATIVE));
            assert_eq!(
                hook_forwards_motion(RAW_RELATIVE),
                !raw_takes_motion(RAW_RELATIVE)
            );

            // 同一时刻,隔膜只对"投递"做决定:开着 → 连移动消息也要吞
            assert!(mouse_shield_swallows(
                true,
                0x0200, /*WM_MOUSEMOVE*/
                u16::MAX
            ));
            // 关着 → 移动消息照放(回到"我们的采集照旧 + 系统照旧投递")
            assert!(!mouse_shield_swallows(false, 0x0200, u16::MAX));
        }

        /// 「FPS 鼠标隔膜」的**端到端**口径(直接调钩子回调,不只看纯函数):
        /// 同一条 `WM_MOUSEMOVE`,隔膜开着时**照样进 `raw_tx`**(引擎收得到位移),
        /// 但回调返回 1 —— 系统不再把它投递给任何窗口(投屏窗口 scrcpy 收不到);
        /// 关掉隔膜时同一条消息照旧转发,且返回 0(放行,与原来的行为逐字一致)。
        #[test]
        fn shield_on_swallows_delivery_but_still_forwards_motion() {
            let _guard = lock_shared_global();
            let shield = Arc::new(AtomicBool::new(true));
            let (tx, rx) = channel::<RawEvent>();
            install_shared(
                tx,
                Arc::new(AtomicU16::new(0)),
                Arc::new(super::BoundKeys::new()),
                Arc::new(AtomicBool::new(false)),
                shield.clone(),
            );

            let ms = mouse_event(4321, 8765, 0);
            // 共享结构刚装好,状态是 RAW_UNREGISTERED:钩子这条路还没被原始输入
            // 接管 → 位移由钩子发(正是"隔膜开着也得有视角"的那条路)
            let swallowed = unsafe {
                mouse_proc(
                    HC_ACTION as i32,
                    WM_MOUSEMOVE as WPARAM,
                    &ms as *const MSLLHOOKSTRUCT as LPARAM,
                )
            };
            assert_eq!(swallowed, 1, "隔膜开启:移动消息不能再投递给任何窗口");
            assert!(
                matches!(
                    rx.recv_timeout(std::time::Duration::from_millis(100)),
                    Ok(RawEvent::Move { .. })
                ),
                "隔膜开启:位移必须照旧送进引擎(吞投递不能连采集一起吞)"
            );

            // 关掉隔膜:同一条消息照旧转发,并且放行(返回 0 = CallNextHookEx)
            shield.store(false, Ordering::Relaxed);
            let passed = unsafe {
                mouse_proc(
                    HC_ACTION as i32,
                    WM_MOUSEMOVE as WPARAM,
                    &ms as *const MSLLHOOKSTRUCT as LPARAM,
                )
            };
            assert_eq!(passed, 0, "隔膜关闭:必须放行给系统(非 FPS 行为不变)");
            assert!(matches!(
                rx.recv_timeout(std::time::Duration::from_millis(100)),
                Ok(RawEvent::Move { .. })
            ));
        }

        /// **G-b 缝测试(§3.7):独占模式吞键的端到端口径** —— 直接调键盘钩子
        /// 回调,不只测纯函数。同一条 `WM_KEYDOWN`/`WM_KEYUP`:
        /// - 开关开 + vk 在位图 ⇒ 返回 1(吞掉投递),但事件**照样进 raw_tx**
        ///   (映射管线照常工作,吞的只是"传给其它程序"这一步);
        /// - 按下与抬起**都吞**(只吞一半会让前台收到孤儿抬起);
        /// - 开关关(默认)⇒ 同一条消息返回 0 放行,行为与从前逐位一致;
        /// - 开关开但 vk 不在位图(没绑的键)⇒ 照样放行。
        #[test]
        fn bound_key_swallow_swallows_down_and_up_but_keeps_pipeline_fed() {
            use super::{KBDLLHOOKSTRUCT, WM_KEYDOWN, WM_KEYUP, keyboard_proc};
            let _guard = lock_shared_global();
            let (tx, rx) = channel::<RawEvent>();
            // 位图:绑定 evdev 30(KEY_A)⇒ vk 0x41;开关:开
            let bound = Arc::new(super::BoundKeys::new());
            bound.store_words(super::super::bound_key_words(
                std::iter::once((30u16, false)),
                false,
            ));
            let on = Arc::new(AtomicBool::new(true));
            install_shared(
                tx,
                Arc::new(AtomicU16::new(0)),
                bound,
                on.clone(),
                Arc::new(AtomicBool::new(false)),
            );
            let kb = KBDLLHOOKSTRUCT {
                vkCode: 0x41, // VK_A
                scanCode: 0,
                flags: 0,
                time: 0,
                dwExtraInfo: 0,
            };
            let lp = &kb as *const KBDLLHOOKSTRUCT as LPARAM;

            // 按下:吞(返回 1),但映射管线照收
            let down = unsafe { keyboard_proc(HC_ACTION as i32, WM_KEYDOWN as WPARAM, lp) };
            assert_eq!(down, 1, "独占模式:已绑定键的按下必须被吞");
            assert!(
                matches!(
                    rx.recv_timeout(std::time::Duration::from_millis(100)),
                    Ok(RawEvent::Key {
                        code: 30,
                        pressed: true,
                        ..
                    })
                ),
                "吞投递不能连采集一起吞:按下事件必须照旧送进引擎"
            );
            // 抬起:同判(孤儿抬起防护)
            let up = unsafe { keyboard_proc(HC_ACTION as i32, WM_KEYUP as WPARAM, lp) };
            assert_eq!(up, 1, "独占模式:已绑定键的抬起也必须被吞(孤儿抬起防护)");
            assert!(matches!(
                rx.recv_timeout(std::time::Duration::from_millis(100)),
                Ok(RawEvent::Key {
                    code: 30,
                    pressed: false,
                    ..
                })
            ));

            // 开关关(默认档):同一条消息放行,管线照收 —— 与从前逐位一致
            on.store(false, Ordering::Relaxed);
            let passed = unsafe { keyboard_proc(HC_ACTION as i32, WM_KEYDOWN as WPARAM, lp) };
            assert_eq!(passed, 0, "开关关(默认):必须放行给系统");
            assert!(
                rx.recv_timeout(std::time::Duration::from_millis(100))
                    .is_ok()
            );
            assert!(unsafe { keyboard_proc(HC_ACTION as i32, WM_KEYUP as WPARAM, lp) } == 0);
            assert!(
                rx.recv_timeout(std::time::Duration::from_millis(100))
                    .is_ok()
            );

            // 开关开但**没绑定**的键(0x53 = S ⇒ evdev 31):照样放行
            on.store(true, Ordering::Relaxed);
            let kb_other = KBDLLHOOKSTRUCT { vkCode: 0x53, ..kb };
            let lp_other = &kb_other as *const KBDLLHOOKSTRUCT as LPARAM;
            let other = unsafe { keyboard_proc(HC_ACTION as i32, WM_KEYDOWN as WPARAM, lp_other) };
            assert_eq!(other, 0, "没绑定的键不在位图里:必须放行");
            assert!(matches!(
                rx.recv_timeout(std::time::Duration::from_millis(100)),
                Ok(RawEvent::Key { code: 31, .. })
            ));
        }

        /// 钩子重装 / 心跳自愈之后,「隔膜」开关必须**依然生效**:它就是共享结构
        /// 里的一枚原子量,重装时带着界面线程的 Arc 一起换进去(与 swallow 同一条
        /// 纪律)。这里顺手也锁住 `swallow` 的同一件事。
        #[test]
        fn restart_replaces_windows_hook_shared_transport() {
            let _guard = lock_shared_global();
            let (tx1, _rx1) = channel::<RawEvent>();
            let shield1 = Arc::new(AtomicBool::new(false));
            let first = install_shared(
                tx1,
                Arc::new(AtomicU16::new(0)),
                Arc::new(super::BoundKeys::new()),
                Arc::new(AtomicBool::new(false)),
                shield1.clone(),
            );
            let (tx2, _rx2) = channel::<RawEvent>();
            let shield2 = Arc::new(AtomicBool::new(false));
            let second = install_shared(
                tx2,
                Arc::new(AtomicU16::new(0xFF)),
                Arc::new(super::BoundKeys::new()),
                Arc::new(AtomicBool::new(false)),
                shield2.clone(),
            );
            assert_ne!(first, second);
            // 重装后的共享结构必须带着**新的**拦截掩码 Arc,而不是沿用旧的
            assert_eq!(
                shared().unwrap().swallow.load(Ordering::Relaxed),
                0xFF,
                "重启后 WinShared.swallow 应指向本次安装传入的 Arc"
            );
            // 隔膜同理:界面线程置位旧 Arc 不影响新钩子,置位新 Arc 才生效
            shield1.store(true, Ordering::Relaxed);
            assert!(
                !shared().unwrap().mouse_shield.load(Ordering::Relaxed),
                "重启后 WinShared.mouse_shield 应指向本次安装传入的 Arc(旧 Arc 不再算数)"
            );
            shield2.store(true, Ordering::Relaxed);
            assert!(
                shared().unwrap().mouse_shield.load(Ordering::Relaxed),
                "界面线程下一帧照常写新 Arc → 隔膜照常生效"
            );
            assert_eq!(
                shared().map(|s| s as *const WinShared),
                Some(second as *const WinShared)
            );
            shared()
                .unwrap()
                .raw_tx
                .send(RawEvent::Key {
                    code: 17,
                    pressed: true,
                    at: Instant::now(),
                })
                .unwrap();
            assert!(matches!(
                _rx2.recv_timeout(std::time::Duration::from_millis(100)),
                Ok(RawEvent::Key {
                    code: 17,
                    pressed: true,
                    ..
                })
            ));
        }

        /// R3(计划:110)端到端口径:**绝对坐标设备 ⇒ 状态机停在 `RAW_ABSOLUTE_ONLY`
        /// ⇒ 钩子这条路的坐标差照发**。整条链在一个测试里走通:
        /// ① `raw_admit` 对 `MOUSE_MOVE_ABSOLUTE` 事件给出 `(RAW_ABSOLUTE_ONLY, Fallback)`,
        ///    且在该状态下自循环(0..65535 归一化坐标只是设备口径,**绝不当像素解码**);
        /// ② 共享结构置为 `RAW_ABSOLUTE_ONLY` 后 `raw_probe` 如实报出该状态(通道在);
        /// ③ `hook_forwards_motion(RAW_ABSOLUTE_ONLY)` = 转发
        ///    (`RAW_PENDING` 的让位由邻居 `shield_never_stops_motion_capture` 锁,不重复);
        /// ④ 直接调钩子回调喂两条 `WM_MOUSEMOVE`(屏幕像素坐标 4321,8765 → 4400,8800),
        ///    `raw_tx` 收到逐字对应的两条 `RawEvent::Move`;
        /// ⑤ 同两个坐标喂 `handle_move`(mouse_grab=false、cursor_hide=false 分支,
        ///    全程不触 `GetCursorPos`/`SetCursorPos`),产出的**MotionCoalescer 增量**
        ///    等于屏幕坐标差 (79, 35) —— 断言落在消费线程向下游发送的位移增量这一层
        ///    (`handle_move` 的产出本就进 coalescer,不直接发消息)。
        #[test]
        fn absolute_only_device_keeps_hook_motion_and_deltas_reach_coalescer() {
            use super::{MOUSE_MOVE_ABSOLUTE, Motion, MotionCoalescer, handle_move, raw_probe};

            // ① 状态机:绝对坐标事件 → RAW_ABSOLUTE_ONLY + Fallback,且自循环。
            //    坐标用 0 与 65535(归一化口径的两端),只作为 raw_kind 的输入 —
            //    它们决定"这是绝对设备",数值本身不进任何像素路径。
            assert_eq!(
                raw_admit(RAW_PENDING, raw_kind(MOUSE_MOVE_ABSOLUTE, 0, 0)),
                (RAW_ABSOLUTE_ONLY, RawAdmit::Fallback),
                "绝对坐标设备:等第一条 WM_INPUT 时也必须退回钩子路径"
            );
            assert_eq!(
                raw_admit(
                    RAW_ABSOLUTE_ONLY,
                    raw_kind(MOUSE_MOVE_ABSOLUTE, 65535, 65535)
                ),
                (RAW_ABSOLUTE_ONLY, RawAdmit::Fallback),
                "已在兜底态又来绝对事件:留在 RAW_ABSOLUTE_ONLY(自循环)"
            );

            // ②+③+④ 钩子链:装共享结构、置兜底态、探针确认、直调回调。
            let _guard = lock_shared_global();
            let (tx, rx) = channel::<RawEvent>();
            install_shared(
                tx,
                Arc::new(AtomicU16::new(0)),
                Arc::new(super::BoundKeys::new()),
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicBool::new(false)),
            );
            shared()
                .unwrap()
                .raw_state
                .store(RAW_ABSOLUTE_ONLY, Ordering::Relaxed);
            assert_eq!(
                raw_probe().0,
                RAW_ABSOLUTE_ONLY,
                "探针必须如实报出兜底态(悬浮窗据此显示'位移走钩子路径')"
            );
            assert!(
                hook_forwards_motion(RAW_ABSOLUTE_ONLY),
                "兜底态:钩子这条路必须照发位移(绝对坐标设备唯一的视角来源)"
            );

            let ms1 = mouse_event(4321, 8765, 0);
            let r1 = unsafe {
                mouse_proc(
                    HC_ACTION as i32,
                    WM_MOUSEMOVE as WPARAM,
                    &ms1 as *const MSLLHOOKSTRUCT as LPARAM,
                )
            };
            assert_eq!(r1, 0, "隔膜关闭:照旧放行给系统");
            match rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(RawEvent::Move { x, y, .. }) => {
                    assert_eq!((x, y), (4321.0, 8765.0), "第一条:屏幕坐标逐字转发");
                }
                other => panic!("兜底态钩子必须转发 Move,实际 {:?}", other.is_ok()),
            }

            let ms2 = mouse_event(4400, 8800, 0);
            let _r2 = unsafe {
                mouse_proc(
                    HC_ACTION as i32,
                    WM_MOUSEMOVE as WPARAM,
                    &ms2 as *const MSLLHOOKSTRUCT as LPARAM,
                )
            };
            match rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(RawEvent::Move { x, y, .. }) => {
                    assert_eq!((x, y), (4400.0, 8800.0), "第二条:屏幕坐标逐字转发");
                }
                other => panic!("兜底态钩子必须转发第二条 Move,实际 {:?}", other.is_ok()),
            }

            // ⑤ 消费侧差分:同两个屏幕坐标喂 handle_move,coalescer 增量 = 坐标差。
            //    mouse_grab/cursor_hide 都关 → 不触 SetCursor/move_cursor(SetCursorPos);
            //    Motion 用字面量构造(不走 Motion::new 的 monitor_center→GetCursorPos)。
            let grab = Arc::new(AtomicBool::new(false));
            let hide = Arc::new(AtomicBool::new(false));
            let mut motion = Motion {
                last: None,
                center: (0, 0),
                threshold: i32::MAX,
                echo: None,
                hiding: false,
            };
            let mut coalescer = MotionCoalescer::new();
            let at = Instant::now();
            handle_move(
                &mut motion,
                &grab,
                &hide,
                4321.0,
                8765.0,
                at,
                &mut coalescer,
            );
            assert!(
                !coalescer.pending(),
                "第一条只建基准(last=None 时差分为 0),不得发出增量"
            );
            handle_move(
                &mut motion,
                &grab,
                &hide,
                4400.0,
                8800.0,
                at,
                &mut coalescer,
            );
            match coalescer.take() {
                Some((dx, dy, _)) => {
                    assert_eq!(
                        (dx, dy),
                        ((4400.0 - 4321.0) as f32, (8800.0 - 8765.0) as f32),
                        "下游收到的位移增量必须等于屏幕坐标差 (79, 35)"
                    );
                }
                None => panic!("第二条必须产出非零增量"),
            }
        }

        #[test]
        fn lost_release_requires_system_up_and_grace_period() {
            assert!(
                !release_is_lost(true, 100, 10_000, 1_500),
                "系统仍按住时不能补抬"
            );
            assert!(
                !release_is_lost(false, 9_500, 10_000, 1_500),
                "宽限期内不能抢在真实事件前补抬"
            );
            assert!(
                release_is_lost(false, 8_000, 10_000, 1_500),
                "系统抬起且超过宽限期才可补抬"
            );
            assert!(
                !release_is_lost(false, 0, 10_000, 1_500),
                "没有按下心跳时不能凭空补抬"
            );
        }

        /// 2026-10-06 复核修复:Raw Input 兜底只在"**在丢位移**却收不到"时触发。
        /// 曾经的判据是"注册后过了 700ms",于是"程序开着、用户就是没动鼠标"
        /// (最常见的启动场景)会天天误报一条"位移退回光标差路径"的 warn ——
        /// 而那条通道其实好好的,第一条鼠标事件一到就又接管了。
        #[test]
        fn raw_fallback_needs_evidence_of_dropped_motion() {
            let t = RAW_PENDING_TIMEOUT_MS;
            // 没丢过位移 = 用户没动鼠标,不是故障
            assert!(!raw_fallback_due(RAW_PENDING, 0, 10_000));
            // 刚开始丢,还不够久
            assert!(!raw_fallback_due(RAW_PENDING, 10_000 - (t - 1), 10_000));
            // 恰好丢满一个超时窗口
            assert!(raw_fallback_due(RAW_PENDING, 10_000 - t, 10_000));
            assert!(raw_fallback_due(RAW_PENDING, 1, 10_000));
            // 已经不在"等第一条事件"的状态:不归这里管
            assert!(!raw_fallback_due(RAW_RELATIVE, 1, 10_000));
            assert!(!raw_fallback_due(RAW_ABSOLUTE_ONLY, 1, 10_000));
            assert!(!raw_fallback_due(RAW_UNREGISTERED, 1, 10_000));
            // 时间戳异常(未来时刻):宁可不退回,也不能误判
            assert!(!raw_fallback_due(RAW_PENDING, 20_000, 10_000));
        }

        /// R4/W1-B1:看门狗单步缝 `watchdog_fallback_step` 的三拍锁定 ——
        /// 699ms 不动(未丢满窗口不误报)、700ms 恰好切 `RAW_ABSOLUTE_ONLY`
        /// 且告警一次、再走一步仍只有一次(切走后 `RAW_PENDING` 门不再满足,
        /// "只记一次"由状态机自己保证)。告警计数走内存闭包(§3.5:不碰 diag)。
        #[test]
        fn watchdog_fallback_step_switches_once_at_exactly_the_timeout() {
            use super::watchdog_fallback_step;
            use std::cell::Cell;
            use std::sync::atomic::AtomicU8;
            let t = RAW_PENDING_TIMEOUT_MS; // 700
            let state = AtomicU8::new(RAW_PENDING);
            let warns = Cell::new(0u32);
            let last_dropped = Cell::new(0u64);
            let drop_ms = 10_000 - t; // 开始丢位移的时刻:9_300

            // 第一拍:now=9_999,丢了 699ms —— 还差 1ms,状态与告警都不许动
            watchdog_fallback_step(&state, drop_ms, 10_000 - 1, |d| {
                warns.set(warns.get() + 1);
                last_dropped.set(d);
            });
            assert_eq!(state.load(Ordering::Relaxed), RAW_PENDING, "699ms 不许退");
            assert_eq!(warns.get(), 0, "未触发就不许记告警");

            // 第二拍:恰好丢满 700ms —— 切兜底态、告警恰好一次、参数 = 已丢时长
            watchdog_fallback_step(&state, drop_ms, 10_000, |d| {
                warns.set(warns.get() + 1);
                last_dropped.set(d);
            });
            assert_eq!(
                state.load(Ordering::Relaxed),
                RAW_ABSOLUTE_ONLY,
                "满 700ms 必须退回光标差路径"
            );
            assert_eq!(warns.get(), 1, "退回时必须记且只记一次告警");
            assert_eq!(last_dropped.get(), t, "告警参数 = 已丢弃时长 ms");

            // 第三拍:再走一步 —— 状态已切走,告警计数不许再涨
            watchdog_fallback_step(&state, drop_ms, 10_500, |d| {
                warns.set(warns.get() + 1);
                last_dropped.set(d);
            });
            assert_eq!(state.load(Ordering::Relaxed), RAW_ABSOLUTE_ONLY);
            assert_eq!(warns.get(), 1, "只记一次:后续步必须静默");
        }

        /// R4/W1-B1:恢复缝 `note_raw_recovery` 的计数锁定 —— 从兜底态恢复
        /// 第一拍记 1 条、第二拍仍 1 条(只记一次);`prev` 不是
        /// `RAW_ABSOLUTE_ONLY` 时一条都不记(如 RELATIVE→Relative 的正常路径)。
        /// 门旗标做成参数后,测试用本地 `AtomicBool` 驱动,不污染进程级静态
        /// `RAW_RECOVERY_LOGGED`(否则产生测试序依赖)。
        #[test]
        fn note_raw_recovery_logs_once_only_from_fallback_state() {
            use super::note_raw_recovery;
            use std::cell::Cell;
            let logged = AtomicBool::new(false);
            let calls = Cell::new(0u32);

            // 第一拍:ABS_ONLY → 收到相对事件(恢复)⇒ 记 1 条
            note_raw_recovery(RAW_ABSOLUTE_ONLY, &logged, || calls.set(calls.get() + 1));
            assert_eq!(calls.get(), 1, "从兜底态恢复必须记一条");

            // 第二拍:同一面旗标已置位 ⇒ 仍 1 条(绝对坐标设备交替发相对事件
            // 时这里会被反复走到,不许刷屏)
            note_raw_recovery(RAW_ABSOLUTE_ONLY, &logged, || calls.set(calls.get() + 1));
            assert_eq!(calls.get(), 1, "只记一次:第二拍必须静默");

            // prev 不是兜底态 ⇒ 不记,也不消耗旗标
            let fresh = AtomicBool::new(false);
            note_raw_recovery(RAW_RELATIVE, &fresh, || calls.set(calls.get() + 1));
            assert_eq!(calls.get(), 1, "非兜底态的恢复不归这条日志管");
            assert!(!fresh.load(Ordering::Relaxed), "没记就不许置位旗标");
        }

        /// W0-2 回归:汇总必须"先取分位数、再清零"。
        /// 旧实现先 swap(0) 再取分位数,p50/p99 恒为 0,健康告警从未触发。
        #[test]
        fn take_report_reads_quantiles_before_resetting() {
            let t = Timing::default();
            for _ in 0..100 {
                t.record(100); // 快速样本:100ns
            }
            for _ in 0..3 {
                t.record(6_000_000); // 毛刺:6ms,占 3/103 ≈ 2.9% > 1%
            }
            let r = t.take_report();
            assert!(
                r.text.contains("钩子回调 103 次"),
                "文案必须包含本轮样本数: {}",
                r.text
            );
            assert!(
                r.p99_nanos > HOOK_LAG_WARN_NANOS,
                "p99 必须反映 6ms 毛刺(旧实现此处恒为 0): {}",
                r.p99_nanos
            );
            // 清零后:没有新样本的一轮,分位数回到 0
            let r2 = t.take_report();
            assert_eq!(r2.p99_nanos, 0);
            assert!(r2.text.contains("钩子回调 0 次"), "{}", r2.text);
        }

        /// W0-1 回归:`pressed` 集合必须"按下入表、抬起出表"。
        /// 旧实现按下时反而 remove,集合恒空,对账形同虚设 —— 丢一次 KeyUp
        /// 就会让键永远卡在按下状态。
        #[test]
        fn note_key_tracks_held_keys_across_repeat_and_release() {
            let mut pressed = HashSet::new();
            note_key(&mut pressed, 17, true);
            assert!(pressed.contains(&17), "按下后必须记录为按着");
            // Windows 键盘自动重复会连发 KEYDOWN:集合状态不变
            note_key(&mut pressed, 17, true);
            assert!(pressed.contains(&17), "自动重复不应改变按下状态");
            note_key(&mut pressed, 17, false);
            assert!(!pressed.contains(&17), "抬起后必须移出集合");
        }

        /// W0-1 验收场景:按着 + 系统已抬起 + 心跳过期 → 必须补发一条
        /// `Button{pressed:false}`;按着 + 系统仍按着 → 一条都不发。
        #[test]
        fn reconcile_releases_only_lost_keys_and_keeps_held_ones() {
            let (tx, rx) = channel::<CaptureEvent>();
            let mut pressed: HashSet<u16> = HashSet::new();
            note_key(&mut pressed, 30, true); // 丢抬起的键:系统确认已抬起
            note_key(&mut pressed, 31, true); // 正按着的键:系统确认仍按下
            note_key(&mut pressed, 32, true); // 刚有事件、宽限期内
            note_key(&mut pressed, 250, true); // 无 VK 映射,无法询问系统
            let last_seen: Vec<AtomicU64> = (0..512).map(|_| AtomicU64::new(0)).collect();
            last_seen[30].store(1_000, Ordering::Relaxed); // 老心跳(已过期)
            last_seen[31].store(9_999, Ordering::Relaxed); // 新心跳
            last_seen[32].store(9_900, Ordering::Relaxed); // 新心跳(宽限期内)
            last_seen[250].store(1_000, Ordering::Relaxed);

            let mut strikes = ReleaseStrikes::default();
            reconcile_with(
                &mut pressed,
                &tx,
                &last_seen,
                10_000,
                |code| match code {
                    30 => Some(0),      // 系统明确抬起
                    31 => Some(-32767), // 系统仍按着(高位为 1 ⇒ 负数)
                    32 => Some(0),      // 系统抬起但宽限期没过
                    _ => None,          // 问不了(无映射)
                },
                &mut strikes,
            );

            // 只有 30 被补抬,且恰好一条消息
            match rx.try_recv() {
                Ok(CaptureEvent::Button {
                    code: 30,
                    pressed: false,
                    ..
                }) => {}
                other => panic!("应补发且仅补发 code=30 的抬起,实际 {:?}", other.is_ok()),
            }
            assert!(rx.try_recv().is_err(), "不得补发第二条");
            assert!(!pressed.contains(&30), "补抬后必须移出按下表");
            assert!(pressed.contains(&31), "系统仍按着时不得误补抬");
            assert!(pressed.contains(&32), "宽限期内不得抢发");
            assert!(pressed.contains(&250), "无法询问系统的键不得动");
        }

        /// 用户 2026-10-11(问题 3)的核心回归:**鼠标键不许凭一次读数就被抬掉**。
        ///
        /// 真机现场:按住开火(左键)期间,`GetAsyncKeyState` 返回 0(MSDN 明说
        /// 调用失败也返回 0:前台线程属于别的进程 / UIPI / 非活动桌面),而我们的
        /// 钩子从未收到过 `WM_LBUTTONUP` —— 旧实现据此补发 KeyRelease,把正在按着的
        /// 开火触点抬掉,用户看到的就是"开火键按不下去"。
        ///
        /// 新口径(见 [`reconcile_with`]):**读通道整体不可信时拒绝猜** ——
        /// 所有读数都是 0 时,一条鼠标键的抬起都不补发。
        #[test]
        fn reconcile_never_synthesizes_a_mouse_release_when_the_oracle_is_blind() {
            let (tx, rx) = channel::<CaptureEvent>();
            let mut pressed: HashSet<u16> = HashSet::new();
            note_key(&mut pressed, 272, true); // BTN_LEFT:用户正按住开火
            note_key(&mut pressed, 17, true); // W:同时在按着(自动重复刷心跳)
            let last_seen: Vec<AtomicU64> = (0..512).map(|_| AtomicU64::new(0)).collect();
            last_seen[272].store(1_000, Ordering::Relaxed); // 开局按下,之后再没事件
            last_seen[17].store(9_999, Ordering::Relaxed); // 自动重复:心跳是新的
            let mut strikes = ReleaseStrikes::default();

            // 连续 5 拍(≈1 秒)都读到"没按下"(整次调用失败的样子)
            for round in 0..5 {
                let out = reconcile_with(
                    &mut pressed,
                    &tx,
                    &last_seen,
                    10_000,
                    |_| Some(0),
                    &mut strikes,
                );
                assert!(out.lost.is_empty(), "第 {round} 拍不得补发鼠标键的抬起");
                assert!(out.suspects.iter().any(|&(c, ..)| c == 272), "必须记下取证");
                assert!(!out.measured_down, "整轮读数全 0 = 读通道不可信");
                assert!(rx.try_recv().is_err(), "第 {round} 拍不得发任何消息");
                assert!(pressed.contains(&272), "正在按着的开火键不许被抬掉");
            }

            // 读通道恢复:因为嫌疑拍数是从第一拍起**连续累加**的,累计已经有 5 拍,
            // 这一拍就该按"真的丢了抬起"补发(而不是重新数两拍)。
            let out = reconcile_with(
                &mut pressed,
                &tx,
                &last_seen,
                10_000,
                |code| if code == 272 { Some(0) } else { Some(-32767) },
                &mut strikes,
            );
            assert!(out.measured_down, "有键读到按下 ⇒ 读通道自证在工作");
            assert_eq!(out.lost, vec![272], "读通道恢复后才允许补发");
            match rx.try_recv() {
                Ok(CaptureEvent::Button {
                    code: 272,
                    pressed: false,
                    ..
                }) => {}
                other => panic!("应补发 BTN_LEFT 的抬起,实际 {:?}", other.is_ok()),
            }
            assert!(!pressed.contains(&272), "补抬后必须移出按下表");

            // ---- 键侧:读通道正常时,鼠标键仍然要**连续两拍**才动手 ----
            let (tx, rx) = channel::<CaptureEvent>();
            let mut pressed: HashSet<u16> = HashSet::new();
            note_key(&mut pressed, 272, true);
            note_key(&mut pressed, 17, true);
            let last_seen: Vec<AtomicU64> = (0..512).map(|_| AtomicU64::new(0)).collect();
            last_seen[272].store(1_000, Ordering::Relaxed);
            last_seen[17].store(9_999, Ordering::Relaxed);
            let mut strikes = ReleaseStrikes::default();
            let oracle = |code: u16| if code == 272 { Some(0) } else { Some(-32767) };
            let out = reconcile_with(&mut pressed, &tx, &last_seen, 10_000, oracle, &mut strikes);
            assert!(out.measured_down && out.lost.is_empty(), "第一拍只记嫌疑");
            assert!(rx.try_recv().is_err(), "第一拍不得发消息");
            let out = reconcile_with(&mut pressed, &tx, &last_seen, 10_000, oracle, &mut strikes);
            assert_eq!(out.lost, vec![272], "第二拍才补发");
            assert!(!pressed.contains(&272));
        }

        /// 键侧:键盘键的判据一个字都没改(自动重复天然免疫宽限期),
        /// 而且宽限期内/系统仍报按下时一律不动。
        #[test]
        fn reconcile_keeps_keyboard_keys_on_the_old_single_strike_rule() {
            assert_eq!(next_strike(0, true, true), 1, "新事件 ⇒ 从 1 起数");
            assert_eq!(next_strike(1, true, false), 2, "无新事件 ⇒ 连续 +1");
            assert_eq!(next_strike(5, false, false), 0, "这一轮不成立 ⇒ 归零");
            // 键盘键只需要一次命中;鼠标键要求读通道可信 + 两拍
            assert!(mouse_release_confirmed(true, 2));
            assert!(!mouse_release_confirmed(true, 1), "一拍不够");
            assert!(
                !mouse_release_confirmed(false, 99),
                "读通道失效时无论多少拍都不许动手"
            );
            assert!(is_reconcile_mouse_button(272));
            assert!(is_reconcile_mouse_button(276), "侧键也算");
            assert!(!is_reconcile_mouse_button(17), "W 不是鼠标键");
        }
    }
}
