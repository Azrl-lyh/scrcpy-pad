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
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU32, Ordering};
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

#[cfg(test)]
mod swallow_tests {
    use super::swallow_bit;

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

#[cfg(not(windows))]
pub fn set_cursor_visible_from_ui(_visible: bool) {}

/// Re-apply a NULL cursor shape after egui/window painting.  The transparent
/// system cursor remains the global fallback across other applications.
#[cfg(windows)]
pub fn hide_cursor_shape_from_ui() {
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::SetCursor(std::ptr::null_mut());
    }
}

#[cfg(not(windows))]
pub fn hide_cursor_shape_from_ui() {}

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
    /// 重扫 `/dev/input` 的间隔(心跳数):≈2s。插上键盘到能用之间最多等这么久。
    const RESCAN_TICKS: u32 = 20;
    /// 与内核对账按键状态的间隔(心跳数):≈400ms。
    ///
    /// 为什么不需要更快:对账只处理"release 丢了"这种异常,而异常多等 400ms
    /// 没有任何影响;但它会 open/ioctl 设备,太快反而增加无谓的系统调用。
    const RECONCILE_TICKS: u32 = 4;

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
        /// 重扫每 2 秒跑一次,而一台笔记本上这类节点有十几个(电源键、合盖开关、
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
                anyhow::bail!(
                    "无权限读取输入设备(/dev/input/event*)。\
                     请确认已执行 sudo usermod -aG input $USER 并【重新登录】,\
                     且本程序是在重新登录后启动的(共 {} 个节点打不开,详见诊断日志)",
                    denied
                );
            }
            anyhow::bail!("未找到键盘设备(详见诊断日志里的设备清单)");
        }

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

        let mut paths = event_paths();
        paths.sort();
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
            diag_info!(
                "capture",
                "已接管 {} 「{name}」{id:?} 键数={key_count} 键盘={is_kb} 鼠标={is_mouse_dev}",
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

    /// `/dev/input` 下的所有 event 节点
    fn event_paths() -> Vec<PathBuf> {
        let Ok(rd) = std::fs::read_dir("/dev/input") else {
            return Vec::new();
        };
        rd.flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().starts_with("event"))
                    .unwrap_or(false)
            })
            .collect()
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
                    std::thread::sleep(Duration::from_millis(2));
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
}

// ============================ Windows 实现 ============================

#[cfg(windows)]
mod windows {
    use super::{
        CaptureEvent, HB_MISS_LIMIT, HB_STALL_MS, HbVerdict, MotionCoalescer, ProbeStep,
        hb_verdict, probe_step, stall_hit, swallow_bit, vktable, wheel_notches,
    };
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

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
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
        CallNextHookEx, CreateCursor, CreateWindowExW, DefWindowProcW, DestroyWindow,
        DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics, GetWindowThreadProcessId,
        HC_ACTION, IDC_ARROW, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED, LoadCursorW, MSG,
        MSLLHOOKSTRUCT, OCR_APPSTARTING, OCR_CROSS, OCR_HAND, OCR_HELP, OCR_IBEAM, OCR_NO,
        OCR_NORMAL, OCR_SIZEALL, OCR_SIZENESW, OCR_SIZENS, OCR_SIZENWSE, OCR_SIZEWE, OCR_UP,
        OCR_WAIT, PostThreadMessageW, RegisterClassW, SM_CXSCREEN, SM_CYSCREEN, SPI_SETCURSORS,
        SPIF_SENDCHANGE, SYSTEM_CURSOR_ID, SetCursor, SetCursorPos, SetSystemCursor,
        SetWindowsHookExW, SystemParametersInfoW, UnhookWindowsHookEx, UnregisterClassW,
        WH_KEYBOARD_LL, WH_MOUSE_LL, WM_APP, WM_INPUT, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN,
        WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL,
        WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
        WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP, WindowFromPoint,
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
        /// 「拦截系统默认行为」掩码(位定义见 [`swallow_bit`])。界面线程按当前
        /// 配置/映射开关/独占键盘开关预先算好写进来;钩子回调只读一次原子量。
        swallow: Arc<AtomicU16>,
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
        /// 最后一枚探针**发出**的时刻(0 = 没发过;SendInput 被 UIPI 挡下时不记,
        /// 那不是钩子的问题)
        probe_sent_kb: AtomicU64,
        /// 钩子回调**看到**自己探针的时刻(0 = 从没见过)
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

    fn install_shared(raw_tx: Sender<RawEvent>, swallow: Arc<AtomicU16>) -> *mut WinShared {
        let shared = Box::into_raw(Box::new(WinShared {
            raw_tx,
            last_seen: (0..512).map(|_| AtomicU64::new(0)).collect(),
            timing: Timing::default(),
            start: Instant::now(),
            raw_state: AtomicU8::new(RAW_UNREGISTERED),
            raw_drop_ms: AtomicU64::new(0),
            swallow,
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

    /// "相对与绝对坐标混用"这条提示只记一次(它是 per-event 分支,见 `handle_raw_input`)
    static MIXED_ABSOLUTE_LOGGED: AtomicBool = AtomicBool::new(false);

    /// "原始输入恢复接管"这条提示同理只记一次(见 `handle_raw_input` 的 `Emit` 分支)
    static RAW_RECOVERY_LOGGED: AtomicBool = AtomicBool::new(false);

    /// 隐藏的接收窗口句柄(`RIDEV_INPUTSINK` 要求 hwndTarget 是**有效窗口**;
    /// 消息专用窗口 HWND_MESSAGE 收 Raw Input 不可靠,所以建的是普通顶层窗口,
    /// 只是永远不显示)。0 = 没建起来。
    static RAW_HWND: AtomicU64 = AtomicU64::new(0);

    /// 接收窗口的类名(注册与注销都要用同一份)
    fn raw_class_name() -> Vec<u16> {
        "scrcpy_pad_raw_input\0".encode_utf16().collect()
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
                if prev == RAW_ABSOLUTE_ONLY && !RAW_RECOVERY_LOGGED.swap(true, Ordering::Relaxed) {
                    diag_info!(
                        "capture",
                        "原始输入重新收到相对位移事件:位移改由原始输入接管"
                    );
                }
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
        let _ = grab;
        let (raw_tx, raw_rx) = channel::<RawEvent>();
        let _ = install_shared(raw_tx, swallow.clone());

        let wake = Arc::new(AtomicU32::new(0));

        // 消费线程:算位移、维护光标、定期对账
        {
            let mouse_grab = mouse_grab.clone();
            let cursor_hide = cursor_hide.clone();
            let hb_state = hb_state.clone();
            let hook_lag = hook_lag.clone();
            let stop = stop.clone();
            let start = Instant::now();
            let txt = tx.clone();
            std::thread::spawn(move || {
                // 钩子→引擎的中转线程:轻量但要及时。
                crate::priority::boost(crate::priority::Class::AboveNormal);
                consumer(
                    raw_rx,
                    txt,
                    mouse_grab,
                    cursor_hide,
                    hb_state,
                    hook_lag,
                    stop,
                    start,
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
        /// 探针自愈已停用(复核 #3):不再发探针、不再因探针重装
        probe_off: bool,
        /// 上一次真正处理心跳的时刻(ms;0 = 还没处理过)
        last_ms: u64,
        /// 是否期望键盘钩子在收事件(重装后跟随新结果)
        expect_kb: bool,
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
        if hb.probe_off {
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
                hb.probe_off = true;
                diag_warn!(
                    "capture",
                    "键盘钩子探针连续 {} 轮重装后仍无回音:疑似环境吞掉注入事件,已停用探针自愈(消息泵心跳检测保留)",
                    hb.fruitless
                );
            }
        }
        // ④ 发下一枚探针(重装上后立刻补一发,让"还活着"的证据尽早回来)。
        //    `max(1)`:0 在这套标记里表示"没发过",别让开机的第 0ms 冒充它。
        if hb.expect_kb && !hb.probe_off && send_probe_kb() {
            s.hb.probe_sent_kb.store(now.max(1), Ordering::Relaxed);
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
            // 探针时间戳换新一代:上一代的 seen 会让下一拍判定立刻又报死。
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
            probe_off: false,
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
        if (code as usize) < shared.last_seen.len() {
            let v = if pressed { now_ms().max(1) } else { 0 };
            shared.last_seen[code as usize].store(v, Ordering::Relaxed);
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
                    shared
                        .hb
                        .probe_seen_kb
                        .store(now_ms().max(1), Ordering::Relaxed);
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
                    return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
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
                    let st = shared.raw_state.load(Ordering::Relaxed);
                    if !raw_takes_motion(st) {
                        let ms = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
                        let _ = shared.raw_tx.send(RawEvent::Move {
                            x: ms.pt.x as f64,
                            y: ms.pt.y as f64,
                            at: began,
                        });
                        shared.timing.record(began.elapsed().as_nanos() as u64);
                    } else if st == RAW_PENDING {
                        // 还在"等第一条 WM_INPUT"的状态,却又在丢一条真实位移:
                        // 记下**开始丢**的时刻(只记第一次),消费线程据此判定
                        // 这条通道是不是根本没在跑(见 WinShared::raw_drop_ms)。
                        let _ = shared.raw_drop_ms.compare_exchange(
                            0,
                            now_ms().max(1),
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        );
                    }
                }
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
        cursor_hide: Arc<AtomicBool>,
        hb_state: Arc<AtomicU8>,
        hook_lag: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
        _start: Instant,
    ) {
        // 注:`mouse_found` 由钩子线程在**鼠标钩子真的装上之后**置位(W0-11),
        // 消费线程不碰它 —— 两个线程是并行的,在这里无条件置 true 会在钩子装
        // 失败时骗过界面自检:用户拿着"检测到鼠标"的绿灯却怎么按都没反应。

        let mut motion = Motion::new();
        let mut coalescer = MotionCoalescer::new();
        let mut pressed: HashSet<u16> = HashSet::new();
        let mut last_reconcile = Instant::now();
        let mut last_report = Instant::now();
        let mut last_raw_recenter = Instant::now();
        let mut last_fg: isize = 0; // 前台窗口句柄:变化即"切换了焦点"
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

            // W1-2:原始输入模式下位移不再来自光标位置,老路径那套"回中 + 回声
            // 识别"就不再是**精度**需要了;但它还是**点击落点**需要 —— 光标飘到
            // 别的显示器/别的窗口上之后,钩子发出去的每一次点击都会落到那个窗口
            // 上(点一下就切走焦点)。所以这里保留一个低频检查:不跟踪回声、
            // 不算差分,只在光标飘远时把它拉回当前显示器中心。
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
                    reconcile(&mut pressed, &tx, &s.last_seen);
                    // W1-2 兜底:注册成功进入 `RAW_PENDING` 后,钩子回调的位移
                    // 已经让位(见 `raw_takes_motion`),但若一条 WM_INPUT 都没
                    // 等到,位移**两条路都不发** —— 表现是"鼠标彻底不动",而且
                    // 不会自己恢复。判据见 `raw_fallback_due`:一边在丢位移、
                    // 一边收不到 —— 而不是"注册后过了多久"。超时后主动退回光标差
                    // 路径;真来事件时 `raw_admit` 仍会把状态切回 RELATIVE
                    // (自我纠正,并留一行"恢复"日志)。
                    let since = s.raw_drop_ms.load(Ordering::Relaxed);
                    if raw_fallback_due(s.raw_state.load(Ordering::Relaxed), since, now_ms()) {
                        s.raw_state.store(RAW_ABSOLUTE_ONLY, Ordering::Relaxed);
                        diag_warn!(
                            "capture",
                            "原始输入注册后持续丢弃位移 {}ms 仍未收到任何鼠标事件:位移退回光标差路径",
                            now_ms().saturating_sub(since)
                        );
                    }
                }
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
                        _ => diag_warn!(
                            "capture",
                            "捕获层钩子失效:探针连续无回音,已触发自动重装(事件可能丢失过)"
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
        if motion.hiding {
            set_cursor_visible(true);
        }
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
    fn reconcile(pressed: &mut HashSet<u16>, tx: &Sender<CaptureEvent>, last_seen: &[AtomicU64]) {
        reconcile_with(pressed, tx, last_seen, now_ms(), |code| {
            // W1-5:把同一物理键的全部别名 VK 都问一遍(小键盘在 NumLock 开/关
            // 时报不同 VK)—— 任一按着就算还按着。宁可少报一次"丢了抬起"
            // (宽限期 + 下一个真事件兜底),也不能把还按着的键误判成已释放
            // —— 那正是卡键与 Hold 连环重按的来源。没映射的码按"问不着"跳过。
            let vks = vktable::vks_for_evdev(code);
            if vks.is_empty() {
                return None;
            }
            // 高位为 1 表示此刻按着
            Some(
                vks.iter()
                    .any(|&vk| unsafe { GetAsyncKeyState(vk as i32) } < 0),
            )
        });
    }

    /// `reconcile` 的可测版本:把"问系统这个键是否按着"抽成闭包
    /// (返回 `None` = 该键码没有 VK 映射、无法询问,按"不动"处理)。
    /// 判定主体与真实现共用同一份代码,单测因此能覆盖真实行为。
    fn reconcile_with(
        pressed: &mut HashSet<u16>,
        tx: &Sender<CaptureEvent>,
        last_seen: &[AtomicU64],
        now: u64,
        system_down: impl Fn(u16) -> Option<bool>,
    ) {
        if pressed.is_empty() {
            return;
        }
        let mut lost: Vec<u16> = Vec::new();
        for &code in pressed.iter() {
            let Some(down) = system_down(code) else {
                continue;
            };
            let seen = last_seen
                .get(code as usize)
                .map(|v| v.load(Ordering::Relaxed))
                .unwrap_or(0);
            if release_is_lost(down, seen, now, RECONCILE_GRACE_MS) {
                lost.push(code);
            }
        }
        if lost.is_empty() {
            return;
        }
        let names: Vec<String> = lost.iter().map(|&c| crate::keymap::key_name(c)).collect();
        diag_warn!(
            "capture",
            "权威对账:宽限期内没有任何后续事件且系统确认已抬起,补发 KeyRelease 以防卡键: {}",
            names.join(" ")
        );
        for code in lost {
            pressed.remove(&code);
            // 合成事件:按补发时刻计(W1-1)
            let _ = tx.send(CaptureEvent::Button {
                code,
                pressed: false,
                at: Instant::now(),
            });
        }
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

    #[cfg(test)]
    mod tests {
        use super::{
            CaptureEvent, HOOK_LAG_WARN_NANOS, Instant, RAW_ABSOLUTE_ONLY, RAW_PENDING,
            RAW_PENDING_TIMEOUT_MS, RAW_RELATIVE, RAW_UNREGISTERED, RawAdmit, RawEvent, RawKind,
            Timing, WinShared, install_shared, note_key, raw_admit, raw_fallback_due, raw_kind,
            raw_takes_motion, reconcile_with, release_is_lost, shared,
        };
        use std::collections::HashSet;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
        use std::sync::mpsc::channel;

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

        /// W1-2:钩子只在"原始输入会发位移"时才让位 ——
        /// 未启用与"只有绝对设备"两种状态下钩子必须继续发。
        #[test]
        fn hook_yields_motion_only_while_raw_will_deliver() {
            assert!(!raw_takes_motion(RAW_UNREGISTERED));
            assert!(raw_takes_motion(RAW_PENDING));
            assert!(raw_takes_motion(RAW_RELATIVE));
            assert!(!raw_takes_motion(RAW_ABSOLUTE_ONLY));
        }

        #[test]
        fn restart_replaces_windows_hook_shared_transport() {
            let (tx1, _rx1) = channel::<RawEvent>();
            let first = install_shared(tx1, Arc::new(AtomicU16::new(0)));
            let (tx2, _rx2) = channel::<RawEvent>();
            let second = install_shared(tx2, Arc::new(AtomicU16::new(0xFF)));
            assert_ne!(first, second);
            // 重装后的共享结构必须带着**新的**拦截掩码 Arc,而不是沿用旧的
            assert_eq!(
                shared().unwrap().swallow.load(Ordering::Relaxed),
                0xFF,
                "重启后 WinShared.swallow 应指向本次安装传入的 Arc"
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

            reconcile_with(&mut pressed, &tx, &last_seen, 10_000, |code| match code {
                30 => Some(false), // 系统明确抬起
                31 => Some(true),  // 系统仍按着
                32 => Some(false), // 系统抬起但宽限期没过
                _ => None,         // 问不了(无映射)
            });

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
    }
}
