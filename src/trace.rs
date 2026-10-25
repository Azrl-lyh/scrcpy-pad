//! 注入轨迹(trace):把"引擎收到了什么输入"与"往设备发了什么命令"落成 **同一根时间轴** 上的 jsonl。
//!
//! # 为什么需要它(用户 2026-10-11 的决定)
//!
//! 上一批"只有真机能暴露"的 bug,根因不是缺方案,而是**缺可回放的现场**:
//! 真机上出现了、日志里只有几个计数、复现全靠手感。有了这条轨迹,真机复现一次
//! 就得到一份**回归资产** —— 由 [`crate::sim`] 的假设备回放输入、断言注入序列
//! 的不变量(位移守恒 / 触点不泄漏 / 坐标合法),同一个问题不必再靠真机碰运气。
//!
//! # 开关与开销
//!
//! 入口优先级:`--trace [路径]` > `SCRCPY_PAD_TRACE=<路径.jsonl>` > **默认落盘**
//! (配置目录 `trace-<时间戳>.jsonl`;同目录最多保留 3 份,最旧的先删)。
//!
//! 为什么"什么都不给"也必须写:环境变量在**双击图标启动**时不会被继承,用户按
//! 说明复现一整局却全盘搜不到文件(2026-10-11 实测)。排查工具只有"默认就在"
//! 才有意义。于是 `enabled() == false` 现在只剩一个来源:[`init_from_env`] ——
//! 仿真回放器(`sim::Harness`)走它,"没设就是不开"在那里是刻意的(否则每跑一个
//! 测试场景都会往用户配置目录里留文件)。
//!
//! 开着的时候,每一条记录都是一次 `write` + `flush`(逐行落盘,见 [`write_line`])。
//! 这是刻意的取舍:宁可慢一点,也不能"整局打完发现文件是空的"。
//!
//! 记录格式(每行一条,`t` = 自打开轨迹起的毫秒):
//!
//! ```text
//! {"t":12,"k":"in","e":"btn","code":67,"down":true}
//! {"t":13,"k":"in","e":"motion","dx":12.0,"dy":-3.0}
//! {"t":14,"k":"out","e":"touch","a":0,"pid":3000,"x":640,"y":1386}
//! {"t":15,"k":"out","e":"key","a":0,"code":4}
//! ```

#![allow(dead_code)]

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::control::{ACTION_DOWN, ACTION_MOVE, ACTION_UP, ControlCmd};

/// 一个进程只开一条轨迹(与诊断日志同一纪律:不设就该是零开销)。
struct Sink {
    start: Instant,
    w: Mutex<BufWriter<File>>,
}

static ON: AtomicBool = AtomicBool::new(false);
static SINK: OnceLock<Sink> = OnceLock::new();

/// 打开一条轨迹。
///
/// **失败必须留下痕迹**:用户是"打完整整一局才发现文件没生成"的(2026-10-11),
/// 所以这里既写 stderr,也写诊断日志 —— 双击启动时看不到 stderr,但日志看得到。
fn open(path: &Path) -> std::io::Result<()> {
    let f = File::create(path)?;
    let _ = SINK.set(Sink {
        start: Instant::now(),
        w: Mutex::new(BufWriter::new(f)),
    });
    ON.store(true, Ordering::Relaxed);
    crate::diag_info!("trace", "注入轨迹已开启: {}", path.display());
    println!("[trace] 注入轨迹已开启: {}", path.display());
    Ok(())
}

/// 默认轨迹最多保留几份(**含本次**这一份,见 [`prune_old_traces`])。
const MAX_TRACE_FILES: usize = 3;

/// 没给路径时的落盘位置:配置目录下带时间戳的文件(与其它配置同目录,好找)。
fn default_path() -> PathBuf {
    let dir = crate::app::config_dir();
    let _ = std::fs::create_dir_all(&dir);
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    dir.join(format!("trace-{secs}.jsonl"))
}

/// 只认默认命名的轨迹文件:`trace-<纯数字>.jsonl` -> 那个时间戳。
///
/// 判名字而不是判扩展名:同目录里还有 `profile.yaml` / `look.json` /
/// `diagnostics.log`,而**用户自己的文件绝不能被我们删掉**。
fn trace_stamp(name: &str) -> Option<u64> {
    let mid = name.strip_prefix("trace-")?.strip_suffix(".jsonl")?;
    if mid.is_empty() || !mid.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    mid.parse().ok()
}

/// 删掉最旧的默认轨迹文件,让**含本次**的总数不超过 [`MAX_TRACE_FILES`]。
///
/// 只碰 [`trace_stamp`] 认得的文件,且本次要写的 `current` 永不删除(同一秒内
/// 重启时它可能已经存在)。清理失败(被占用/只读)只记一行 debug —— 轨迹本身照写:
/// 这条规则是"别无限增长",不是"清理不了就别开"。
fn prune_old_traces(current: &Path) {
    let dir = current.parent().unwrap_or_else(|| Path::new("."));
    let Ok(entries) = std::fs::read_dir(dir) else {
        return; // 目录读不到就算了:轨迹写到 open() 那一步自然会给出真实错误
    };
    let mut old: Vec<(u64, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path == current {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(stamp) = trace_stamp(name) else {
            continue;
        };
        old.push((stamp, path));
    }
    // 旧的在前。时间戳相同时按路径定序 —— 删哪一份必须是确定的,不能看目录顺序。
    old.sort();
    let keep = MAX_TRACE_FILES.saturating_sub(1); // 空位留给本次这一份
    if old.len() <= keep {
        return;
    }
    let removed = old.len() - keep;
    for (_, path) in old.drain(..removed) {
        if let Err(e) = std::fs::remove_file(&path) {
            crate::diag_debug!("trace", "旧轨迹删不掉(不影响本次): {} {e}", path.display());
        }
    }
    crate::diag_debug!(
        "trace",
        "轨迹总量限制:最多 {MAX_TRACE_FILES} 份,本次启动清掉 {removed} 份旧轨迹"
    );
}

/// 命令行开关(推荐用法):`--trace [路径]`。
///
/// 为什么必须有它:`SCRCPY_PAD_TRACE` 这类环境变量在**双击图标启动**时不会被
/// 继承 —— 用户按说明复现了一整局,文件根本没生成(2026-10-11 实测:全盘搜不到)。
/// 命令行参数一定跟着进程走,是唯一不会丢现场的方式。
///
/// **什么都没给时也落盘**(默认位置 + 总量限制):这就是"默认可用"的那一档,
/// 见模块文档。返回值恒为本次实际使用的路径(开不出来时也会留 stderr + 日志)。
pub fn init_from_args(args: &[String]) -> PathBuf {
    let path = match decide(args, std::env::var_os("SCRCPY_PAD_TRACE").as_deref()) {
        Choice::Explicit(path) => path,
        Choice::Default => {
            let path = default_path();
            prune_old_traces(&path);
            path
        }
    };
    open_reporting(&path);
    path
}

/// 这次启动该往哪儿写轨迹。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Choice {
    /// 显式路径(`--trace <路径>` 或 `SCRCPY_PAD_TRACE`)
    Explicit(PathBuf),
    /// 默认位置:配置目录里的 `trace-<时间戳>.jsonl`
    Default,
}

/// 决策:命令行优先,其次环境变量,**最后默认落盘**(而不是"关闭")。
///
/// 纯决策、不碰文件系统 —— 所以"什么都不给也落盘"这条能直接单测(真去开文件
/// 会污染用户的配置目录)。`env` 也当参数传:这样能单测"双击启动"那种
/// "环境变量根本不存在"的场景。
fn decide(args: &[String], env: Option<&std::ffi::OsStr>) -> Choice {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a != "--trace" {
            continue;
        }
        // 紧跟其后、不以 `--` 开头的参数当作路径;否则用默认名。
        return match it.next() {
            Some(p) if !p.starts_with("--") => Choice::Explicit(PathBuf::from(p)),
            _ => Choice::Default,
        };
    }
    match env {
        Some(v) if !v.is_empty() => Choice::Explicit(PathBuf::from(v)),
        // 没给命令行、也没有环境变量 —— 默认落盘,不再有"关掉"这一档
        _ => Choice::Default,
    }
}

/// 环境变量开关(`SCRCPY_PAD_TRACE=<路径>`);**只认环境变量**,不隐式开默认轨迹。
///
/// 为什么与 [`init_from_args`] 分开:仿真回放器(`sim::Harness`)用它 ——
/// 那里"没设就是不开"是刻意的,否则每跑一个测试场景都会在用户配置目录里留文件。
pub fn init_from_env() {
    let Some(path) = std::env::var_os("SCRCPY_PAD_TRACE").filter(|v| !v.is_empty()) else {
        return;
    };
    let path = PathBuf::from(path);
    open_reporting(&path);
}

/// 开一条轨迹,并把失败**明确**报出去(stderr + 诊断日志)。
///
/// 双击启动看不到 stderr,但日志看得到 —— 这正是"用户整局没留下文件"的直接教训。
fn open_reporting(path: &Path) {
    if let Err(e) = open(path) {
        eprintln!("trace: 无法创建 {}: {e}", path.display());
        crate::diag_warn!("trace", "无法创建轨迹文件 {}: {e}", path.display());
    }
}

pub fn enabled() -> bool {
    ON.load(Ordering::Relaxed)
}

fn write_line(line: String) {
    let Some(sink) = SINK.get() else { return };
    // 锁中毒不放弃:轨迹是排查工具,不能因为它自己出问题就拖垮输入线程。
    let mut w = sink.w.lock().unwrap_or_else(|e| e.into_inner());
    let _ = writeln!(w, "{line}");
    // **逐行落盘**,不靠用户态缓冲。第一版用纯 `BufWriter` 的结果是:一次场景
    // 跑完、文件是**空的** —— 静态 `SINK` 永远不会被 drop,`BufWriter` 里那点
    // 尾巴(小轨迹就是全部)再没机会写出去。真机上"复现完发现文件没内容"
    // 比慢一点严重得多,所以这里每行一次 write 系统调用。
    let _ = w.flush();
}

fn now_ms() -> u128 {
    SINK.get()
        .map(|s| s.start.elapsed().as_millis())
        .unwrap_or(0)
}

/// 捕获层的一个按键事件(键盘与鼠标键同码空间)。
pub fn button(code: u16, pressed: bool) {
    if !enabled() {
        return;
    }
    write_line(format!(
        r#"{{"t":{},"k":"in","e":"btn","code":{},"down":{}}}"#,
        now_ms(),
        code,
        pressed
    ));
}

/// 捕获层的一次鼠标相对位移(设备计数,非像素)。
pub fn motion(dx: f32, dy: f32) {
    if !enabled() {
        return;
    }
    write_line(format!(
        r#"{{"t":{},"k":"in","e":"motion","dx":{},"dy":{}}}"#,
        now_ms(),
        dx,
        dy
    ));
}

/// 真正写进 socket 的那一条控制命令(在写线程里、序列化之前调用)。
///
/// 位置很关键:这里是**唯一的真相** —— 队列可能丢旧保新、写线程可能失败,
/// 只有这一步是"确实要发给设备的东西"。
pub fn out(cmd: &ControlCmd) {
    if !enabled() {
        return;
    }
    let t = now_ms();
    let line = match cmd {
        ControlCmd::Touch {
            action,
            pointer_id,
            x,
            y,
        } => format!(
            r#"{{"t":{t},"k":"out","e":"touch","a":{action},"pid":{pointer_id},"x":{x},"y":{y}}}"#
        ),
        ControlCmd::Key { action, keycode } => {
            format!(r#"{{"t":{t},"k":"out","e":"key","a":{action},"code":{keycode}}}"#)
        }
        ControlCmd::UhidCreate { id, .. } => {
            format!(r#"{{"t":{t},"k":"out","e":"uhid_create","id":{id}}}"#)
        }
        ControlCmd::UhidInput { id, .. } => {
            format!(r#"{{"t":{t},"k":"out","e":"uhid_input","id":{id}}}"#)
        }
        ControlCmd::UhidDestroy { id } => {
            format!(r#"{{"t":{t},"k":"out","e":"uhid_destroy","id":{id}}}"#)
        }
    };
    write_line(line);
}

/// 把缓冲区刷出去(退出前调用)。
///
/// `write_line` 已经逐行落盘,所以这里**不是**正确性依赖 —— 保留它有两个理由:
/// 调用点的语义是完整的("要退出了,把轨迹收干净"),以及将来若改成批量写,
/// 这一处不必再回头补。
/// **链路状态心跳**(每 200ms 一拍,由捕获层消费线程写)。
///
/// 为什么必须有它:只靠 in/out 两条流分不清"没人发"与"发了却被某一环吞掉" ——
/// 真机那个"视角锁死"就卡在这个歧义上(2026-10-11:锁死期间 motion=0、按键照常)。
/// 有了这一拍,轨迹自己就能回答三件事:
/// * `raw` = 位移状态机(`0` 未注册 / `1` 待定 / `2` 已接管 / `3` 仅绝对设备);
/// * `wmin` = 累计收到的 `WM_INPUT` 条数(不涨 = 这条通道没在跑);
/// * **这些行本身还在不在**(停 = 消费线程卡死/退出,那才是"位移断了、按键还在"
///   的直接解释)。
///
/// `now` 是捕获层自己的毫秒时钟,用来与 `t`(轨迹时钟)对齐。
pub fn state(raw_state: u8, wm_input: u64, drop_streak_ms: u64, capture_ms: u64) {
    if !enabled() {
        return;
    }
    write_line(format!(
        r#"{{"t":{},"k":"st","raw":{raw_state},"wmin":{wm_input},"streak":{drop_streak_ms},"now":{capture_ms}}}"#,
        now_ms()
    ));
}

/// `aim` 心跳的最小写出间隔(节流)。
///
/// 为什么必须节流:这行是**引擎每轮**写的,真机实测 ~205 条/s、占整份轨迹的 84%
/// (2026-10-15:602s 的 11.9MB 里 120,638 条是 aim、~10MB)。它的用途是"事后看
/// 七判据在症状那一刻是什么",**不需要逐轮精度** —— 10/s 已足够分辨
/// "mode 掉了 / gate 掉了 / 池子满了"这三类完全不同的病因(它们都以百毫秒计
/// 保持),却把体积压到 1/20。
///
/// 取舍:同一时刻的**跳变**可能被压掉一两条(最坏晚 100ms 才出现在轨迹里)。
/// 症状排查不需要那 100ms —— 而"文件大到打不开/传不动"会让轨迹直接失去价值。
const AIM_STATE_MIN_INTERVAL_MS: u64 = 100;

/// 上次写 `aim` 心跳的时刻(轨迹时钟毫秒;0 = 还没写过)。
static AIM_LAST_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `aim` 心跳这一拍该不该写(纯函数,便于单测与变异验证)。
///
/// `last == 0` 表示"本次运行还没写过" ⇒ 第一条立刻写(否则开局 100ms 内
/// 完全看不到引擎状态,而"刚进 FPS 模式那一刻"正是最该有快照的时点)。
///
/// `now < last`(时钟回拨/截断异常)也判**到期**:宁可多写一条,也不能因为
/// 一次异常的时间戳让这行**永久沉默** —— 它是排查"引擎侧到底怎么了"的唯一
/// 可见面(见本函数上方那段"为什么必须有它")。
fn aim_state_due(last: u64, now: u64) -> bool {
    last == 0 || now < last || now - last >= AIM_STATE_MIN_INTERVAL_MS
}
/// **瞄准/模式状态心跳**(引擎每轮维护写一次,**已节流到 ≤10 条/s**)。
///
/// 为什么必须有它:真机那个"鼠标在动、视角却转不动"已经排除了捕获层(位移确实
/// 到了引擎、触点无泄漏、通道没断),剩下的全在引擎这一侧,而引擎的内部状态
/// **此前完全不可见**。这一行把七个判据摊开:
/// `en`=总映射开 / `mode`=FPS 模式锁存 / `susp`=按住才退出 / `gate`=门控键 /
/// `anchor`=锚点已设 / `down`=瞄准指正按着 / `others`=其它触点数。
///
/// 症状出现时只要看这一行:是 `mode` 掉了、还是 `gate` 掉了、还是 `others`
/// 把池子占满导致落不下指 —— 三者对应三种完全不同的修法。
#[allow(clippy::too_many_arguments)]
pub fn aim_state(
    mapping_on: bool,
    mode_active: bool,
    suspended: bool,
    gate_down: bool,
    anchor_set: bool,
    down: bool,
    others: usize,
) {
    if !enabled() {
        return;
    }
    // 节流(见 `AIM_STATE_MIN_INTERVAL_MS`):不足间隔就整条不写,不做"攒着后补" ——
    // 这行是状态快照而非事件,补写旧快照只会让时间轴更难读。
    // `now_ms()` 是 u128(轨迹时钟),这里压到 u64 存原子量:进程存活毫秒数远超
    // u64 上限需要 5.8 亿年,截断不可能发生(用 min 显式表达这个上界)。
    let now = now_ms();
    let now_u64 = now.min(u64::MAX as u128) as u64;
    let last = AIM_LAST_MS.load(Ordering::Relaxed);
    if !aim_state_due(last, now_u64) {
        return;
    }
    AIM_LAST_MS.store(now_u64, Ordering::Relaxed);
    write_line(format!(
        r#"{{"t":{},"k":"aim","en":{},"mode":{},"susp":{},"gate":{},"anchor":{},"down":{},"others":{}}}"#,
        now,
        mapping_on as u8,
        mode_active as u8,
        suspended as u8,
        gate_down as u8,
        anchor_set as u8,
        down as u8,
        others
    ));
}

pub fn flush() {
    if let Some(sink) = SINK.get() {
        let mut w = sink.w.lock().unwrap_or_else(|e| e.into_inner());
        let _ = w.flush();
    }
}

// ============================ 读取(回放用) ============================

/// 一条记录。回放器([`crate::sim::replay`])只认这三种。
#[derive(Debug, Clone, PartialEq)]
pub enum Rec {
    Button { t: u64, code: u16, down: bool },
    Motion { t: u64, dx: f32, dy: f32 },
    Out(OutRec),
}

/// 一条"往设备发出去"的记录。
#[derive(Debug, Clone, PartialEq)]
pub enum OutRec {
    Touch {
        t: u64,
        action: u8,
        pid: u64,
        x: u32,
        y: u32,
    },
    Key {
        t: u64,
        action: u8,
        keycode: u32,
    },
    Other {
        t: u64,
        kind: String,
    },
}

impl Rec {
    pub fn t(&self) -> u64 {
        match self {
            Rec::Button { t, .. } | Rec::Motion { t, .. } => *t,
            Rec::Out(o) => match o {
                OutRec::Touch { t, .. } | OutRec::Key { t, .. } | OutRec::Other { t, .. } => *t,
            },
        }
    }
}

/// 读一份轨迹。无法识别的行跳过(轨迹是排查工具:半截文件也该能读多少算多少)。
pub fn read(path: &Path) -> std::io::Result<Vec<Rec>> {
    let text = std::fs::read_to_string(path)?;
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let t = v.get("t").and_then(|t| t.as_u64()).unwrap_or(0);
        let k = v.get("k").and_then(|k| k.as_str()).unwrap_or("");
        let e = v.get("e").and_then(|e| e.as_str()).unwrap_or("");
        match (k, e) {
            ("in", "btn") => {
                let (Some(code), Some(down)) = (
                    v.get("code").and_then(|c| c.as_u64()),
                    v.get("down").and_then(|d| d.as_bool()),
                ) else {
                    continue;
                };
                out.push(Rec::Button {
                    t,
                    code: code as u16,
                    down,
                });
            }
            ("in", "motion") => {
                let (Some(dx), Some(dy)) = (
                    v.get("dx").and_then(|d| d.as_f64()),
                    v.get("dy").and_then(|d| d.as_f64()),
                ) else {
                    continue;
                };
                out.push(Rec::Motion {
                    t,
                    dx: dx as f32,
                    dy: dy as f32,
                });
            }
            ("out", "touch") => {
                let (Some(a), Some(pid), Some(x), Some(y)) = (
                    v.get("a").and_then(|a| a.as_u64()),
                    v.get("pid").and_then(|p| p.as_u64()),
                    v.get("x").and_then(|x| x.as_u64()),
                    v.get("y").and_then(|y| y.as_u64()),
                ) else {
                    continue;
                };
                out.push(Rec::Out(OutRec::Touch {
                    t,
                    action: a as u8,
                    pid,
                    x: x as u32,
                    y: y as u32,
                }));
            }
            ("out", "key") => {
                let (Some(a), Some(code)) = (
                    v.get("a").and_then(|a| a.as_u64()),
                    v.get("code").and_then(|c| c.as_u64()),
                ) else {
                    continue;
                };
                out.push(Rec::Out(OutRec::Key {
                    t,
                    action: a as u8,
                    keycode: code as u32,
                }));
            }
            ("out", other) => out.push(Rec::Out(OutRec::Other {
                t,
                kind: other.to_string(),
            })),
            _ => {}
        }
    }
    Ok(out)
}

/// 轨迹里的人话摘要(手工排查与 `--scenario` 的报告都用它)。
pub fn summarize(recs: &[Rec]) -> String {
    let mut buttons = 0usize;
    let mut motions = 0usize;
    let mut down = 0usize;
    let mut up = 0usize;
    let mut moves = 0usize;
    let mut keys = 0usize;
    for r in recs {
        match r {
            Rec::Button { .. } => buttons += 1,
            Rec::Motion { .. } => motions += 1,
            Rec::Out(OutRec::Touch { action, .. }) => match *action {
                ACTION_DOWN => down += 1,
                ACTION_UP => up += 1,
                ACTION_MOVE => moves += 1,
                _ => {}
            },
            Rec::Out(OutRec::Key { .. }) => keys += 1,
            Rec::Out(OutRec::Other { .. }) => {}
        }
    }
    format!(
        "输入 {buttons} 键 / {motions} 位移;注入 down {down} / move {moves} / up {up} / 系统键 {keys}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 常量必须与协议一致 —— 轨迹里的 `a` 字段就是它们,读错了会让整份回放失真。
    #[test]
    fn action_constants_match_protocol() {
        assert_eq!((ACTION_DOWN, ACTION_UP, ACTION_MOVE), (0, 1, 2));
    }

    /// 写→读 往返:四种记录都能原样回来。
    #[test]
    fn round_trip_records() {
        let dir = std::env::temp_dir().join("scrcpy-pad-trace-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("round-trip.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"t":1,"k":"in","e":"btn","code":67,"down":true}"#,
                "\n",
                r#"{"t":2,"k":"in","e":"motion","dx":12.5,"dy":-3.25}"#,
                "\n",
                r#"{"t":3,"k":"out","e":"touch","a":0,"pid":3000,"x":640,"y":1386}"#,
                "\n",
                r#"{"t":4,"k":"out","e":"key","a":1,"code":4}"#,
                "\n",
                // 坏行与未知类型都不许让整份文件读不出来
                "这不是 json",
                "\n",
                r#"{"t":5,"k":"out","e":"uhid_input","id":1}"#,
                "\n",
            ),
        )
        .unwrap();
        let recs = read(&path).unwrap();
        assert_eq!(recs.len(), 5, "{recs:?}");
        assert_eq!(
            recs[0],
            Rec::Button {
                t: 1,
                code: 67,
                down: true
            }
        );
        assert_eq!(
            recs[1],
            Rec::Motion {
                t: 2,
                dx: 12.5,
                dy: -3.25
            }
        );
        assert_eq!(
            recs[2],
            Rec::Out(OutRec::Touch {
                t: 3,
                action: ACTION_DOWN,
                pid: 3000,
                x: 640,
                y: 1386
            })
        );
        assert_eq!(
            recs[3],
            Rec::Out(OutRec::Key {
                t: 4,
                action: 1,
                keycode: 4
            })
        );
        assert!(matches!(recs[4], Rec::Out(OutRec::Other { .. })));
        assert_eq!(recs[4].t(), 5);
    }

    /// 摘要必须把三类注入分开数 —— 排查"只按下没抬起"时第一眼看的就是它。
    #[test]
    fn summarize_counts_by_action() {
        let recs = vec![
            Rec::Button {
                t: 0,
                code: 1,
                down: true,
            },
            Rec::Motion {
                t: 1,
                dx: 1.0,
                dy: 0.0,
            },
            Rec::Out(OutRec::Touch {
                t: 2,
                action: ACTION_DOWN,
                pid: 1,
                x: 1,
                y: 1,
            }),
            Rec::Out(OutRec::Touch {
                t: 3,
                action: ACTION_MOVE,
                pid: 1,
                x: 2,
                y: 1,
            }),
            Rec::Out(OutRec::Touch {
                t: 4,
                action: ACTION_UP,
                pid: 1,
                x: 2,
                y: 1,
            }),
        ];
        let s = summarize(&recs);
        assert!(s.contains("down 1"), "{s}");
        assert!(s.contains("move 1"), "{s}");
        assert!(s.contains("up 1"), "{s}");
    }

    /// **默认落盘**(用户 2026-10-11 第 5 条):双击启动既没有 `--trace`、
    /// 也没有环境变量(环境变量在双击时根本不被继承),这时必须仍然有轨迹文件。
    #[test]
    fn trace_is_on_by_default_without_any_switch() {
        let argv = ["scrcpy-pad".to_string()];
        assert_eq!(
            decide(&argv, None),
            Choice::Default,
            "什么都没给时必须默认落盘"
        );
        assert_eq!(
            decide(&argv, Some(std::ffi::OsStr::new(""))),
            Choice::Default,
            "环境变量为空 = 没给"
        );
        assert_eq!(
            decide(&["scrcpy-pad".into(), "--trace".into()], None),
            Choice::Default,
            "--trace 后面没跟路径 -> 默认位置"
        );
    }

    /// 显式开关仍然优先(语义不变):命令行 > 环境变量;`--trace` 后面跟着的
    /// 另一个开关不能被当成路径。
    #[test]
    fn explicit_switches_still_win() {
        assert_eq!(
            decide(
                &["scrcpy-pad".into(), "--trace".into(), "x.jsonl".into()],
                Some(std::ffi::OsStr::new("env.jsonl"))
            ),
            Choice::Explicit(PathBuf::from("x.jsonl")),
            "命令行给了路径,环境变量不该抢"
        );
        assert_eq!(
            decide(
                &["scrcpy-pad".into()],
                Some(std::ffi::OsStr::new("env.jsonl"))
            ),
            Choice::Explicit(PathBuf::from("env.jsonl")),
            "没给 --trace 时才轮到环境变量"
        );
        assert_eq!(
            decide(
                &["scrcpy-pad".into(), "--trace".into(), "--selftest".into()],
                None
            ),
            Choice::Default,
            "--trace 后面是另一个开关,不能当成路径"
        );
    }

    /// 清理只认默认命名:`profile.yaml`、`trace-abc.jsonl`、`trace-1.txt` 这些
    /// 都不是我们的文件,用户的东西一个都不许删。
    #[test]
    fn only_default_trace_names_are_recognized() {
        assert_eq!(trace_stamp("trace-1750000000.jsonl"), Some(1_750_000_000));
        assert_eq!(trace_stamp("trace-0007.jsonl"), Some(7));
        assert_eq!(trace_stamp("trace-.jsonl"), None);
        assert_eq!(trace_stamp("trace-abc.jsonl"), None);
        assert_eq!(trace_stamp("trace-1.txt"), None);
        assert_eq!(trace_stamp("xtrace-1.jsonl"), None);
        assert_eq!(trace_stamp("diagnostics.log"), None);
        assert_eq!(trace_stamp("trace-1.jsonl.bak"), None);
    }

    /// 总量限制:同目录最多留下 [`MAX_TRACE_FILES`] 份(**含本次**),删的是最旧的;
    /// 名字不合规的文件与本次要写的那一份都动不得。
    #[test]
    fn default_trace_prunes_oldest_and_keeps_three_total() {
        let dir =
            std::env::temp_dir().join(format!("scrcpy-pad-trace-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for secs in [10u64, 20, 30, 40, 50] {
            std::fs::write(dir.join(format!("trace-{secs}.jsonl")), b"x").unwrap();
        }
        // 干扰项:都不该被删(不是"我们"的文件)
        for name in ["profile.yaml", "trace-abc.jsonl", "trace-60.txt"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        let current = dir.join("trace-60.jsonl");
        prune_old_traces(&current);

        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            vec![
                "profile.yaml",
                "trace-40.jsonl",
                "trace-50.jsonl",
                // 这两个是干扰项,按字节序排在 40/50 之后,但**都在**:一个都不许删
                "trace-60.txt",
                "trace-abc.jsonl",
            ],
            "应删掉最旧的 3 份(10/20/30),其余原样;本次这份尚未创建也应留出空位"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `aim` 心跳节流(2026-10-15):这行原本是引擎**每轮**写,真机实测 ~205 条/s、
    /// 占整份轨迹 84%(602s 的 11.9MB 里 120,638 条 / ~10MB)。
    ///
    /// 三条断言各管一件事:
    /// ① 第一条立刻写(`last == 0`)—— 否则开局那 100ms 看不到引擎状态;
    /// ② 间隔不足 100ms 不写 —— 这就是节流本身,也是本测试的存在理由;
    /// ③ 恰好 100ms 与超过 100ms 都写 —— 边界属"到点即写",不是"必须超过"。
    #[test]
    fn aim_state_heartbeat_is_throttled_to_ten_per_second() {
        // ① 首拍
        assert!(aim_state_due(0, 0), "首拍必须写(否则开局看不到引擎状态)");
        assert!(aim_state_due(0, 12_345), "首拍与 now 无关,恒写");
        // ② 间隔不足
        assert!(!aim_state_due(1000, 1000), "同一毫秒不得连写");
        assert!(!aim_state_due(1000, 1099), "99ms 仍不足间隔");
        assert!(
            !aim_state_due(1000, 1000 + AIM_STATE_MIN_INTERVAL_MS - 1),
            "间隔差 1ms 也不写(边界属下一拍)"
        );
        // ③ 到点/超过
        assert!(
            aim_state_due(1000, 1000 + AIM_STATE_MIN_INTERVAL_MS),
            "恰好 100ms 必须写"
        );
        assert!(aim_state_due(1000, 5000), "超过间隔必须写");
        // 时钟回拨(u128→u64 截断/异常)不许 panic,也不许永久沉默
        assert!(
            aim_state_due(9000, 1000),
            "now < last 时应视作到期,不能卡死不再写"
        );
    }
}
