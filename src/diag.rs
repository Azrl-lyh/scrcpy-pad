//! 底层诊断日志(`diagnostics.log`):把"只在别人机器上才复现"的问题变成可回读的现场。
//!
//! 为什么必须有它:外接键盘失灵、FPS 瞄准在老系统上没反应、偶发断触这几类问题
//! 全是**偶发 + 跨设备 + 平台相关**的。而 Windows GUI 版根本没有控制台,
//! 原先散落各处的那几处 `eprintln!` 全部进黑洞;线程 panic 也没人接
//! (从未安装 panic hook),崩溃之后一无所获 —— 于是任何修复都只能靠猜。
//!
//! 三条设计约束,违反任何一条都会让它失去意义:
//!
//! 1. **绝不 panic**。日志写失败不能拖垮程序:所有 IO 一律 `let _ =`,
//!    目录只读就退化成"只写 stderr",绝不 `unwrap`。
//! 2. **崩溃现场的最后一句话必须已经落盘**。`ERROR`/`WARN` 立即 flush;
//!    panic hook 里也强制 flush —— 不能等 `BufWriter` 自然溢出,
//!    否则最关键的那几行会随进程一起消失。
//! 3. **高频事件不得逐条写盘**。鼠标位移在 1000Hz 鼠标上每秒上千条,
//!    调用方必须先聚合成计数/最近值(见 `engine` 的 `AimLive`/`EngineLive`),
//!    只在**状态跃迁**时才记一行。这里靠级别过滤与 flush 节流兜底。
//!
//! 级别由环境变量 `SCRCPY_PAD_LOG`(error/warn/info/debug/trace)控制,默认 `info`;
//! 也可由 `settings.json` 的 `log_level` 覆盖(见 [`set_level_from_str`]),
//! 后者是给 GUI 用户用的 —— Windows 上设环境变量并不方便。
//!
//! 文件名与位置:`app::config_dir()/diagnostics.log`,与 `profile.yaml`
//! 同级。**每次启动截断重写**(用户要求"每次运行后完全更新");
//! 因此崩溃现场会一直保留到你下次启动程序为止。

use std::collections::VecDeque;
use std::fmt::Display;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// 日志级别。数值越小越严重,`enabled()` 按"小于等于当前级别"判定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
    Trace = 4,
}

impl Level {
    fn short(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN ",
            Level::Info => "INFO ",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        }
    }

    /// 解析级别名(大小写不敏感,容错常见写法)
    pub fn parse(s: &str) -> Option<Level> {
        match s.trim().to_ascii_lowercase().as_str() {
            "error" | "err" | "e" => Some(Level::Error),
            "warn" | "warning" | "w" => Some(Level::Warn),
            "info" | "i" => Some(Level::Info),
            "debug" | "d" => Some(Level::Debug),
            "trace" | "t" | "verbose" => Some(Level::Trace),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warn => "warn",
            Level::Info => "info",
            Level::Debug => "debug",
            Level::Trace => "trace",
        }
    }
}

/// 日志文件名(与配置同目录)
pub const FILE_NAME: &str = "diagnostics.log";
/// 控制级别的环境变量名(优先级高于 settings.json)
pub const ENV_LEVEL: &str = "SCRCPY_PAD_LOG";
/// 命令行 `--log-level` 定下级别之后的"来源"文案。
///
/// 它同时是一块**禁区标记**:级别来源等于它时,别的来源(settings.json 的
/// log_level、环境变量)一律不许再改级别 —— 用户在同一条命令里写了参数,
/// 就是要**这一次**生效(命令行 > 环境变量 > 配置文件)。
pub const CLI_LEVEL_SOURCE: &str = "命令行 --log-level";
/// 命令行显式指定的级别(由 `main` 在 [`init`] **之前**调用 [`set_cli_level`] 设置)。
///
/// 用 `OnceLock` 而不是普通原子整数:级别是枚举,放这里能保住类型,读的时候
/// 不必每次做数值转换;而"命令行最多定一次"正好是 OnceLock 的语义。
static CLI_LEVEL: OnceLock<Level> = OnceLock::new();
/// 命令行是否已经定过级别(定过之后 [`set_level`] 拒绝来路不同的覆盖)。
static CLI_PINNED: AtomicBool = AtomicBool::new(false);
/// 正常退出的标记行。下次启动会检查上一次是否以它结尾 ——
/// 没有它就意味着上次是崩溃/被强杀,这本身就是一个重要线索。
const EXIT_MARKER: &str = "===== 正常退出 =====";
/// `INFO` 及以下级别的最大落盘间隔。
///
/// 为什么要节流而不是每条都 flush:TRACE 级别下日志量很可观,每条一次 write
/// 系统调用会明显拖慢引擎线程(它每 4ms 就转一圈)。500ms 把"崩溃时最多丢多少"
/// 限制在一个可控的量,同时把系统调用次数降三个数量级。
const FLUSH_INTERVAL: Duration = Duration::from_millis(500);

struct Sink {
    file: Option<BufWriter<File>>,
    /// 上一次写失败是否已经提示过(避免刷屏)
    write_failed_reported: bool,
}

struct Diag {
    sink: Option<Sink>,
    path: PathBuf,
    level: Level,
    /// 级别是谁定的(写进日志头,便于确认用户的设置到底生效没有)
    level_source: String,
    start: Instant,
}

static STATE: OnceLock<Mutex<Diag>> = OnceLock::new();
/// 启动快照的文本副本:界面"导出诊断包"要用,避免重新推导一遍
static SNAPSHOT: OnceLock<String> = OnceLock::new();

/// 诊断面板"最近日志"环形缓冲(容量 [`RECENT_CAP`] 行,满了挤掉最早的)。
///
/// 为什么必须有它:界面上的日志列表(`PadApp::logs`)只接 `PadApp::log()`,
/// 而捕获层那三条 Raw Input 关键行走的是 `diag_info!`/`diag_warn!` —— 它们
/// **只落文件,一行都不进界面**。于是"出问题时打开诊断面板"这条路径上,
/// 用户看不到"原始输入到底启用没有 / 为什么退回光标差",而那正是
/// "视角不动、手感不对"的第一现场。这条缓冲就是那座缺失的桥。
///
/// **语义 = "真正写进文件的那一行"**:推入点在 [`write_raw`] 内部,即已经过
/// `enabled` 级别过滤、且 `writeln!` 成功之后的最终落盘行。取舍说明:面板因此
/// **随级别变化**(级别压到 `error` 时面板也只剩 error 行)—— 这正是想要的:
/// 面板里出现的每一行,用户都能在同目录的 `diagnostics.log` 里逐字找到;
/// 反之,面板里绝不会出现"文件里其实没有"的行。
static RECENT: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());
/// 环形缓冲容量。界面一次只显示最近 8 行,留 64 行是为了让"刚滚过去的那几行"
/// 仍然可读(用户点一下"刷新预览"之前就能看到现场)。
const RECENT_CAP: usize = 64;

/// 取全局状态。**中毒也要能用** —— 日志系统一旦罢工,排查就彻底瞎了。
fn state() -> &'static Mutex<Diag> {
    STATE.get_or_init(|| {
        let (level, source) = initial_level();
        Mutex::new(Diag {
            sink: None,
            path: PathBuf::new(),
            level,
            level_source: source.to_string(),
            start: Instant::now(),
        })
    })
}

/// 带容错的加锁:锁被 panic 中毒时仍取出内部数据继续跑。
fn lock() -> std::sync::MutexGuard<'static, Diag> {
    state().lock().unwrap_or_else(|e| e.into_inner())
}

/// 级别裁决(纯函数,便于单测):命令行 > 环境变量 > settings.json > 内置默认。
///
/// 为什么必须按这个顺序:用户在同一条命令里写了 --log-level,就是要**这一次**
/// 生效;而 settings.json 是持久化产物,GUI 启动后必然拿它调一次
/// [`set_level_from_str`](见 PadApp 的启动流程)。若不把命令行放在最前面,
/// 一次普通启动就能把它静默冲掉 —— 用户看到的是"我加的参数好像没起作用"。
fn pick_level(
    cli: Option<Level>,
    env: Option<&str>,
    settings: Option<&str>,
) -> (Level, &'static str) {
    if let Some(l) = cli {
        return (l, CLI_LEVEL_SOURCE);
    }
    if let Some(l) = env.and_then(Level::parse) {
        return (l, "环境变量 SCRCPY_PAD_LOG");
    }
    if let Some(l) = settings.and_then(Level::parse) {
        return (l, "settings.json");
    }
    (Level::Info, "内置默认")
}

/// 初始级别:命令行 > 环境变量 > 内置默认。
///
/// settings.json 不参与这一步 —— 它在启动之后才被读到(GUI 侧),
/// 那时走的是 [`set_level_from_str`],而它会尊重命令行钉住的级别。
fn initial_level() -> (Level, &'static str) {
    pick_level(cli_level(), std::env::var(ENV_LEVEL).ok().as_deref(), None)
}

/// 记下命令行给的级别。**必须在 [`init`] 之前调用**。
///
/// 位置有讲究:日志头部第 3 行写的是"级别 X (来源)",用户正是靠这一行确认
/// "我加的 --log-level 生效没有";晚于 init 调用的话头部展示的还是旧级别,
/// 自证链条就断了。
pub fn set_cli_level(level: Level) {
    // 顺序:先钉住,再写值。两个线程同时进来时最终值一致,不会出现
    // "钉住了但值是空的"这种中间态被 initial_level 读到。
    CLI_PINNED.store(true, Ordering::Relaxed);
    let _ = CLI_LEVEL.set(level);
}

/// 命令行给的级别(没给则为 None)
pub fn cli_level() -> Option<Level> {
    CLI_LEVEL.get().copied()
}

/// 打开(截断)日志文件,**父目录不存在时先建**。
///
/// Linux 首次启动时 `config_dir()`(如 `~/.config/scrcpy-pad`)往往还不存在。
/// `trace.rs` / `settings.rs` / `instance.rs` 各自都会 `create_dir_all`,
/// 只有这里原本是裸 `File::create` —— 于是诊断日志整份丢失。2026-10-19 在
/// WSL 实测到的原文:
/// `[diag] 无法创建 /home/xxx/.config/scrcpy-pad/diagnostics.log: No such file
/// or directory (os error 2)`。诊断日志是取证链第一环,丢它等于用户报障时无
/// 现场可看,所以这里必须补建。
///
/// 仍然遵守模块头约束 1:绝不 panic;建目录失败也只返回 `Err`,由调用方退化成
/// "只写 stderr"。
pub fn open_log_file(path: &Path) -> std::io::Result<File> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    File::create(path)
}

/// 初始化:在配置目录下**截断重写**日志文件,并写入启动头部。
///
/// 必须在 `app::PadApp::new()` 之前调用 —— 那里会立刻开始写设备清单,
/// 晚一步那些最关键的第一手信息就会丢。
pub fn init() {
    // **测试构建绝不许碰生产日志路径**(2026-10-12 事故):
    // `cargo test` 跑在**用户机器**上,而 `init()` 会对
    // `config_dir()/diagnostics.log` 做 `File::create` —— 一次 `cargo test`
    // 就把用户当时那场会话的日志整份覆盖掉(真实发生过:用户让我看的会话日志
    // 因此丢失,而日志正是唯一的取证介质)。测试一律落到临时目录,名字带 pid
    // 以便并发/多进程互不干扰。
    #[cfg(test)]
    let path = {
        let mut p = std::env::temp_dir();
        p.push(format!("scrcpy-pad-test-diag-{}.log", std::process::id()));
        p
    };
    #[cfg(not(test))]
    let path = crate::app::config_dir().join(FILE_NAME);
    // 先看上一份日志的结尾(Create 会把内容冲掉,必须在此之前读)
    let prev_abnormal = previous_run_was_abnormal(&path);

    let mut file = None;
    match open_log_file(&path) {
        Ok(f) => file = Some(BufWriter::new(f)),
        Err(e) => eprintln!(
            "[diag] 无法创建 {}: {e} —— 诊断日志将只输出到 stderr(便携模式 + 只读介质?)",
            path.display()
        ),
    }

    {
        let mut st = lock();
        st.start = Instant::now();
        st.path = path.clone();
        st.sink = file.map(|f| Sink {
            file: Some(f),
            write_failed_reported: false,
        });
    }

    for line in header_lines() {
        write_raw(&line);
    }
    if let Some(secs) = stale_seconds(&path) {
        write_raw(&format!(
            "# 上一份日志的最后修改时间: {} ({secs} 秒前)",
            fmt_time(now_secs())
        ));
    }
    if prev_abnormal {
        write_raw(
            "# 上一份日志**没有**以正常退出标记结尾 —— 上次运行是崩溃或被强杀,请检查其中是否有 ERROR。",
        );
    }
    flush();
    // 落盘线程必须在头部写完、且任何 `log()` 之前起好,否则最早的几行
    // (恰恰是最有价值的启动现场)会等到下一次写日志才落地
    spawn_flusher();
}

/// 启动头部:程序版本、构建目标、级别来源、日志自身位置
fn header_lines() -> Vec<String> {
    let (level, source) = {
        let st = lock();
        (st.level, st.level_source.clone())
    };
    vec![
        "# ============================================================".to_string(),
        format!(
            "# scrcpy-pad 诊断日志 v{} ({}, {})",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH
        ),
        format!(
            "# 级别 {} ({source});改成更详细:设环境变量 {ENV_LEVEL}=debug/trace,",
            level.name()
        ),
        format!("#   或在 settings.json 里写 \"log_level\": \"debug\"。文件每次启动整体重写。"),
        format!("# 文件: {}", lock().path.display()),
        format!("# 起始时间: {}", fmt_time(now_secs())),
        "# 时间戳: [+相对启动毫秒][绝对本地时间]  —— 相对时间用于分析延迟/竞态,".to_string(),
        "#   绝对时间可直接与 journalctl / 事件查看器对齐。".to_string(),
        "# ============================================================".to_string(),
    ]
}

/// 上一次运行是否异常结束(日志文件末尾没有正常退出标记)
fn previous_run_was_abnormal(path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false; // 没有上一份,不算异常
    };
    if bytes.is_empty() {
        return false;
    }
    let tail = &bytes[bytes.len().saturating_sub(512)..];
    !String::from_utf8_lossy(tail).contains(EXIT_MARKER)
}

/// 日志文件的最后修改时间距现在多少秒(用于"上一份日志有多旧")
fn stale_seconds(path: &Path) -> Option<u64> {
    let m = std::fs::metadata(path).ok()?;
    let t = m.modified().ok()?;
    let secs = t.duration_since(UNIX_EPOCH).ok()?.as_secs();
    now_secs().checked_sub(secs)
}

/// 当前级别够不够详细
pub fn enabled(level: Level) -> bool {
    level <= lock().level
}

/// 当前级别(界面显示用)
pub fn level() -> Level {
    lock().level
}

/// 级别来源说明(界面显示用,回答"我设的到底生效没有")
pub fn level_source() -> String {
    lock().level_source.clone()
}

/// 运行期改级别(settings.json 的 log_level 会在启动后调用它)
pub fn set_level(level: Level, source: &str) {
    // 命令行优先:写了 --log-level 之后,其他来源一律不许改级别。
    // 但**留一条 DEBUG 记录**(而不是完全无声):用户若在设置页里改了
    // log_level 却看不到日志变化,这行就是答案 —— 命令行钉住了它。
    if !should_apply(source, CLI_PINNED.load(Ordering::Relaxed)) {
        log(
            Level::Debug,
            "diag",
            format_args!(
                "忽略来自 {source} 的日志级别 {}(命令行 --log-level 优先)",
                level.name()
            ),
        );
        return;
    }
    {
        let mut st = lock();
        st.level = level;
        st.level_source = source.to_string();
    }
    log(
        Level::Info,
        "diag",
        format_args!("日志级别已设为 {} ({source})", level.name()),
    );
}

/// 按字符串设置级别(来自 settings.json);无法识别时保持原样并说明
pub fn set_level_from_str(s: &str, source: &str) {
    match Level::parse(s) {
        Some(l) => set_level(l, source),
        None if s.trim().is_empty() => {}
        None => log(
            Level::Warn,
            "diag",
            format_args!("无法识别的日志级别 {s:?}(可用 error/warn/info/debug/trace),保持原级别"),
        ),
    }
}

/// 日志文件路径(界面"打开日志目录"用)
pub fn path() -> PathBuf {
    lock().path.clone()
}

/// 这次改级别该不该生效(纯函数,便于单测)。
///
/// 规则只有一条:命令行钉住之后,只有"命令行"这个来源还能改级别。
/// 抽成纯函数是为了能直接喂 "pinned=true + 各种 source" 的组合,
/// 不必真的去动进程级全局(那会和并行跑的其它 diag 测试互踩)。
fn should_apply(source: &str, pinned: bool) -> bool {
    !pinned || source == CLI_LEVEL_SOURCE
}

/// 记一行(受级别过滤)。`tag` 是模块名,便于 `grep '\[capture\]'` 这类过滤。
pub fn log(level: Level, tag: &str, msg: impl Display) {
    if !enabled(level) {
        return;
    }
    let line = format_line(level, tag, &format!("{msg}"));
    // WARN/ERROR 一律同时打到 stderr(journalctl / 终端能直接看到);
    // 文件不可用时退化成"全部级别都打 stderr",否则就什么都看不到了。
    let file_broken = {
        let st = lock();
        st.sink.as_ref().map(|s| s.file.is_none()).unwrap_or(true)
    };
    if level <= Level::Warn || file_broken {
        eprintln!("{line}");
    }
    write_raw_meta(&line, Some(level), tag);
    if level <= Level::Warn {
        flush();
    }
}

/// 写一行,**不过滤级别**也不打 stderr(头部、快照这类程序自己产出的内容用它)
///
/// 这里只写进 `BufWriter`,**不做节流落盘** —— 落盘交给 [`spawn_flusher`] 的
/// 定时线程。原因:如果把"该不该 flush"挂在"有没有新日志"上,那么一旦程序
/// 忙起来或者干脆不再写日志(典型场景:主线程在解码一张很大的背景图、
/// 或者用户只是没碰键盘),缓冲区里的尾巴就会一直留着,进程被强杀时直接丢光。
/// 定时线程与主线程是否忙无关,能把"崩溃时最多丢多久"稳定限制在 500ms 以内。
fn write_raw(line: &str) {
    write_raw_meta(line, None, "");
}

/// [`write_raw`] 的带元信息版本:额外带上"级别 + 模块",供 --trace-diag 镜像进轨迹。
///
/// 为什么不从格式化后的行文本里反解级别:行里的 [WARN ] 只是 5 字符定宽的
/// 展示形式,反解既脆弱,又会把"行首恰好长得像级别"的普通内容误认。
/// 把元信息一路传下来,成本只是一次函数调用。
fn write_raw_meta(line: &str, level: Option<Level>, tag: &str) {
    // 镜像点必须在一切早退**之前**,两条理由:
    //  1. 日志文件写不进去时(目录只读 / 句柄失效 / 本进程已 shutdown),
    //     诊断内容仍然要进轨迹 —— 那正是最需要现场的时刻;
    //  2. 轨迹与日志是两条独立汇流:开了 --trace-diag,即使日志只落到 stderr,
    //     轨迹里也照样应该有这些行。
    if crate::trace::diag_mirror_enabled() {
        crate::trace::diag(level.map(|l| l.name()).unwrap_or("-"), tag, line);
    }
    // 先落盘,成功后才推入界面缓冲(缓冲语义 = "真正写进文件的那一行")。
    // 两块状态用两把锁,且顺序固定为 Diag → RECENT(反向不存在),
    // 所以不会死锁;作用域分开只是为了不在持 Diag 锁时再做别的加锁。
    let wrote = {
        let mut guard = lock();
        // 一次性把字段拆开:`st.sink.as_mut()` 会借用整个 `*guard`(Deref 之后
        // 借用检查看不到字段互不相交),那样后面再用别的字段就会冲突。
        let Diag { sink, .. } = &mut *guard;
        let Some(sink) = sink.as_mut() else { return };
        let Some(f) = sink.file.as_mut() else { return };
        if writeln!(f, "{line}").is_ok() {
            true
        } else {
            if !sink.write_failed_reported {
                sink.write_failed_reported = true;
                // 此处不能调 log(会把锁再借一次),直接打 stderr
                eprintln!("[diag] 诊断日志写入失败(后续不再提示)");
            }
            false
        }
    };
    if wrote {
        push_recent(line);
    }
}

/// 把一行已落盘的内容推进 [`RECENT`] 环形缓冲。
///
/// **任何失败都静默忽略**(中毒锁、容量问题都当作"面板少一行"处理):
/// 诊断日志落盘是取证生命线,绝不允许界面侧的内存缓冲反过来影响它 ——
/// 与模块头部"三条设计约束"第 1 条(绝不 panic)同一口径。
fn push_recent(line: &str) {
    let Ok(mut q) = RECENT.lock() else { return };
    if q.len() >= RECENT_CAP {
        q.pop_front();
    }
    q.push_back(line.to_string());
}

/// [`RECENT`] 的快照(最近若干条已落盘行,由旧到新)。
///
/// 给诊断面板用:面板只负责**显示**,不做任何过滤/排序,免得面板里出现
/// 文件里没有的行(那会让"面板看到什么 = 日志里有什么"这条对应关系失效)。
pub fn recent_lines() -> Vec<String> {
    match RECENT.lock() {
        Ok(q) => q.iter().cloned().collect(),
        Err(_) => Vec::new(),
    }
}

/// 起一个只看时间的落盘线程。
///
/// 这是"日志可信"的最后一道保险:`ERROR`/`WARN` 仍然即时落盘(见 [`log`]),
/// 而 `INFO` 及以下靠它每 [`FLUSH_INTERVAL`] 落一次。没有它,日志的尾巴
/// 就取决于"程序接下来还写不写日志",这是不可接受的 —— 最需要日志的时刻
/// 恰恰是程序忙死或已经不动的时候。
fn spawn_flusher() {
    std::thread::Builder::new()
        .name("diag-flush".into())
        .spawn(|| {
            loop {
                std::thread::sleep(FLUSH_INTERVAL);
                // 缓冲区为空时 BufWriter::flush 不会发起系统调用,所以空转几乎不花钱
                flush();
            }
        })
        .ok();
}

/// 强制落盘。退出前、panic 时、以及界面点了"导出"之后必须调用。
pub fn flush() {
    let mut st = lock();
    if let Some(sink) = st.sink.as_mut() {
        if let Some(f) = sink.file.as_mut() {
            let _ = f.flush();
        }
    }
}

/// 正常退出:打标记并落盘。下次启动靠这个标记判断上次是否异常结束。
pub fn shutdown() {
    log(Level::Info, "diag", format_args!("{EXIT_MARKER}"));
    flush();
}

/// 组装一行:`[+相对ms][绝对时间][级别][tag][线程] 消息`
fn format_line(level: Level, tag: &str, msg: &str) -> String {
    let ms = lock().start.elapsed().as_millis();
    let thread = std::thread::current();
    let tname = thread.name().unwrap_or("unnamed");
    // 消息里带换行时缩进后续行,避免把日志的"一行一事件"结构冲垮
    let msg = msg.replace('\n', "\n    ");
    format!(
        "[+{ms:>7}ms][{}][{}][{tag}][{tname}] {msg}",
        fmt_time_ms(),
        level.short()
    )
}

/// 安装 panic hook:把"哪个线程、哪一行、什么原因、什么调用栈"完整落盘。
///
/// 为什么必须装:引擎线程一旦 panic,映射就整体失效,而用户侧只看到
/// "程序还在,但按什么都没反应" —— 没有这份记录,这种问题无法定位。
/// 另外线程 panicking 时会 drop 掉 `Receiver`,捕获层的每次 `send` 从此静默失败,
/// 表现同样是"彻底失去输入"。
pub fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "(无法读取 panic 信息)".to_string());
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "(位置未知)".to_string());
        let tname = std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_string();

        // 这里必须用 try_lock:若当前线程正持有日志锁(例如在写日志的过程中 panic),
        // 再 lock() 就会自己把自己锁死 —— 那样连崩溃信息都留不下。
        let text = format!(
            "[+?][{}][ERROR][panic][{tname}] 线程崩溃于 {location}: {payload}",
            fmt_time_ms()
        );
        if let Ok(mut st) = state().try_lock() {
            if let Some(f) = st.sink.as_mut().and_then(|s| s.file.as_mut()) {
                let _ = writeln!(f, "{text}");
                let _ = writeln!(
                    f,
                    "[+?][{}][ERROR][panic][{tname}] 回溯(开发版带符号,release 版可能只有地址):\n{}",
                    fmt_time_ms(),
                    std::backtrace::Backtrace::force_capture()
                );
                let _ = f.flush();
            }
        }
        eprintln!("{text}");
        default_hook(info);
    }));
}

/// 启动快照:把"跨设备排错真正需要的环境事实"一次性写进去。
///
/// 为什么值得单独一整块:几乎所有"我这里不行你那里行"的问题,
/// 答案都在这几行里 —— 发行版/内核、会话类型(Wayland 与 X11 的 grab 行为不同)、
/// 用户所属组(有没有 input 组)、配置文件在不在、显示器与缩放。
pub fn snapshot_environment() {
    let mut lines: Vec<String> = Vec::new();
    lines.push("===== 启动环境快照 =====".to_string());
    lines.push(format!(
        "程序: scrcpy-pad {} ({}/{}, {} 位)",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        usize::BITS
    ));
    lines.push(format!("系统: {}", os_description()));
    lines.push(format!("会话: {}", session_description()));
    lines.push(format!("用户: {}", user_and_groups()));
    lines.push(format!("显示器: {}", display_description()));
    lines.push(format!("进程: pid={}", std::process::id()));
    lines.push(format!(
        "程序路径: {}",
        std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "(未知)".into())
    ));
    lines.push(format!(
        "工作目录: {}",
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "(未知)".into())
    ));
    lines.push(format!("配置目录: {}", crate::app::config_dir().display()));
    lines.push(format!("配置文件: {}", config_files()));
    lines.push(format!("日志级别: {}", level_source()));
    lines.push(format!("===== 快照结束 ({}) =====", fmt_time(now_secs())));

    let text = lines.join("\n");
    let _ = SNAPSHOT.set(text.clone());
    for line in lines {
        write_raw(&line);
    }
    flush();
}

/// 启动快照文本(界面导出用)
pub fn snapshot() -> Option<String> {
    SNAPSHOT.get().cloned()
}

/// 读取日志尾部若干字节(界面预览与导出用;不解析,原样返回)
pub fn tail(max_bytes: usize) -> String {
    let p = path();
    let Ok(bytes) = std::fs::read(&p) else {
        return String::new();
    };
    let start = bytes.len().saturating_sub(max_bytes);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

// ============================ 环境信息采集 ============================

#[cfg(target_os = "linux")]
fn os_description() -> String {
    let mut out = String::new();
    if let Ok(text) = std::fs::read_to_string("/etc/os-release") {
        for line in text.lines() {
            if let Some(v) = line.strip_prefix("PRETTY_NAME=") {
                out.push_str(v.trim_matches('"'));
                break;
            }
        }
    }
    if let Ok(k) = std::fs::read_to_string("/proc/sys/kernel/osrelease") {
        if !out.is_empty() {
            out.push_str(" · ");
        }
        out.push_str("内核 ");
        out.push_str(k.trim());
    }
    if out.is_empty() {
        out.push_str("(未能识别发行版)");
    }
    out
}

#[cfg(not(target_os = "linux"))]
fn os_description() -> String {
    // Windows:OS 变量固定为 "Windows_NT";构建号要额外的系统调用,收益不大
    let os = std::env::var("OS").unwrap_or_else(|_| "(未知)".into());
    let arch = std::env::var("PROCESSOR_ARCHITECTURE").unwrap_or_default();
    format!("{os} {arch}")
}

/// 会话类型。Wayland 与 X11 对 evdev 抓取、光标隐藏的行为并不一致,
/// 很多"我这儿抓不到按键"的差异都出在这里。
fn session_description() -> String {
    let mut parts = Vec::new();
    for key in [
        "XDG_SESSION_TYPE",
        "WAYLAND_DISPLAY",
        "DISPLAY",
        "DESKTOP_SESSION",
    ] {
        if let Ok(v) = std::env::var(key) {
            if !v.trim().is_empty() {
                parts.push(format!("{key}={v}"));
            }
        }
    }
    if parts.is_empty() {
        "(无图形会话相关环境变量)".into()
    } else {
        parts.join(" ")
    }
}

/// 当前用户与所属组(是否在 input 组直接决定能否读 /dev/input)
#[cfg(target_os = "linux")]
fn user_and_groups() -> String {
    let user = std::env::var("USER").unwrap_or_else(|_| "(未知用户)".into());
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return user;
    };
    let mut uid = None;
    let mut gids: Vec<u32> = Vec::new();
    for line in status.lines() {
        if let Some(v) = line.strip_prefix("Uid:") {
            uid = v
                .split_whitespace()
                .next()
                .and_then(|s| s.parse::<u32>().ok());
        } else if let Some(v) = line.strip_prefix("Groups:") {
            gids = v
                .split_whitespace()
                .filter_map(|s| s.parse::<u32>().ok())
                .collect();
        }
    }
    let names = group_names(&gids);
    format!(
        "{user} uid={} gid=[{}] (含 input 组? {})",
        uid.map(|u| u.to_string()).unwrap_or_else(|| "?".into()),
        names.join(","),
        if names.iter().any(|n| n == "input") {
            "是"
        } else {
            "否 —— 读不到 /dev/input;装 60-scrcpy-pad.rules(不用重登)或 sudo usermod -aG input $USER 后重新登录"
        }
    )
}

/// gid -> 组名(查 /etc/group);查不到就退回数字
#[cfg(target_os = "linux")]
fn group_names(gids: &[u32]) -> Vec<String> {
    let text = std::fs::read_to_string("/etc/group").unwrap_or_default();
    gids.iter()
        .map(|gid| {
            text.lines()
                .find_map(|line| {
                    let mut it = line.split(':');
                    let name = it.next()?;
                    it.next()?; // 口令字段
                    let g = it.next()?;
                    (g.parse::<u32>().ok() == Some(*gid)).then(|| name.to_string())
                })
                .unwrap_or_else(|| gid.to_string())
        })
        .collect()
}

#[cfg(not(target_os = "linux"))]
fn user_and_groups() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "(未知用户)".into())
}

/// 显示器分辨率。多显示器与 DPI 缩放是 Windows 侧 FPS 出问题的头号嫌疑,
/// 所以这两个值要留在快照里。
#[cfg(windows)]
fn display_description() -> String {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN,
    };
    unsafe {
        format!(
            "主屏 {}x{}, 虚拟桌面(含副屏) {}x{}",
            GetSystemMetrics(SM_CXSCREEN),
            GetSystemMetrics(SM_CYSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN)
        )
    }
}

#[cfg(not(windows))]
fn display_description() -> String {
    // Linux/macOS 走 X11 或 Wayland,取值要额外接口且与注入无关(我们的坐标空间
    // 以手机分辨率为准),这里只记环境变量给出的提示。
    //
    // 顺序很重要:Wayland 桌面上 DISPLAY 通常**也**有值(XWayland 兼容层),
    // 先看 DISPLAY 会把所有 Wayland 会话都误报成 X11 —— 而这两者的
    // 输入抓取行为并不相同,正是排查时要区分的。
    let wayland = std::env::var("WAYLAND_DISPLAY").unwrap_or_default();
    if !wayland.is_empty() {
        return format!("Wayland {wayland}");
    }
    match std::env::var("DISPLAY") {
        Ok(d) if !d.is_empty() => format!("X11 DISPLAY={d}"),
        _ => "(未知)".into(),
    }
}

/// 三个配置文件的在否与大小 —— 一眼看出"配置根本没被读"这类问题
fn config_files() -> String {
    let dir = crate::app::config_dir();
    ["profile.yaml", "look.json", "settings.json"]
        .iter()
        .map(|name| match std::fs::metadata(dir.join(name)) {
            Ok(m) => format!("{name}={}B", m.len()),
            Err(_) => format!("{name}=不存在"),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ============================ 时间格式化 ============================

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `epoch 秒 -> 本地时间 "2026-09-27 06:31:02"`
///
/// 用系统时区而不是 UTC:诊断日志的价值有一大半在于和 `journalctl` /
/// Windows 事件查看器对齐时间线,而那两个工具显示的是本地时间。
fn fmt_time(secs: u64) -> String {
    let (y, mo, d, h, mi, s, _) = local_parts(secs);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

/// 带毫秒的当前本地时间
fn fmt_time_ms() -> String {
    let st = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let (y, mo, d, h, mi, s, ms) = local_parts(st.as_secs());
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}.{ms:03}")
}

/// 当前本地时间,精确到毫秒。
///
/// 给**人读的**日志文本用(例如"放弃某设备时是几点"):日志行本身已经有时间戳,
/// 但一句话里同时给出"哪台设备、哪些键、什么时刻",用户才方便把消息记录对回
/// 自己当时在做什么 —— 用户能贴过来的往往只有日志片段,贴不了时间轴。
// 目前只有 Linux 捕获层用(设备消失的 WARN);Windows 构建里它是空闲的,
// 但函数本身不该因此被删 —— 那些 WARN 只在 Linux 才存在。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn clock_now() -> String {
    fmt_time_ms()
}

/// epoch 秒 -> 本地时间各分量 (年,月,日,时,分,秒,毫秒)
#[cfg(unix)]
fn local_parts(secs: u64) -> (i64, i64, i64, i64, i64, i64, u32) {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_millis())
        .unwrap_or(0);
    let t: libc::time_t = secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // localtime_r 失败(极端时区库缺失)时退回纯 Rust 的 UTC 民用历算法,
    // 宁可时间差几个时区,也不能让整行日志丢掉时间戳。
    let ok = unsafe { !libc::localtime_r(&t, &mut tm).is_null() };
    if !ok {
        let (y, mo, d) = civil_from_days((secs as i64).div_euclid(86400));
        let rem = (secs as i64).rem_euclid(86400);
        return (y, mo, d, rem / 3600, (rem % 3600) / 60, rem % 60, ms);
    }
    (
        tm.tm_year as i64 + 1900,
        tm.tm_mon as i64 + 1,
        tm.tm_mday as i64,
        tm.tm_hour as i64,
        tm.tm_min as i64,
        tm.tm_sec as i64,
        ms,
    )
}

#[cfg(windows)]
fn local_parts(_secs: u64) -> (i64, i64, i64, i64, i64, i64, u32) {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    let mut st: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut st) };
    (
        st.wYear as i64,
        st.wMonth as i64,
        st.wDay as i64,
        st.wHour as i64,
        st.wMinute as i64,
        st.wSecond as i64,
        st.wMilliseconds as u32,
    )
}

/// 自 epoch 起的天数 -> (年, 月, 日)(Howard Hinnant 民用历算法)。
/// 仅在 `localtime_r` 不可用时作为兜底。
#[cfg(unix)]
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

// ============================ 便捷宏 ============================

/// `error` 级一行:故障必须留下痕迹,且立即落盘
#[macro_export]
macro_rules! diag_error {
    ($tag:expr, $($arg:tt)*) => {
        $crate::diag::log($crate::diag::Level::Error, $tag, format_args!($($arg)*))
    };
}

/// `warn` 级一行:可疑但不致命(丢事件、被拒绝、降级运行)
#[macro_export]
macro_rules! diag_warn {
    ($tag:expr, $($arg:tt)*) => {
        $crate::diag::log($crate::diag::Level::Warn, $tag, format_args!($($arg)*))
    };
}

/// `info` 级一行:状态跃迁(连接/断开/开关映射/设备增减)
#[macro_export]
macro_rules! diag_info {
    ($tag:expr, $($arg:tt)*) => {
        $crate::diag::log($crate::diag::Level::Info, $tag, format_args!($($arg)*))
    };
}

/// `debug` 级一行:排查时才需要的过程细节
#[macro_export]
macro_rules! diag_debug {
    ($tag:expr, $($arg:tt)*) => {
        $crate::diag::log($crate::diag::Level::Debug, $tag, format_args!($($arg)*))
    };
}

/// `trace` 级一行:**只允许低频调用**。高频事件(鼠标位移等)必须先聚合成计数,
/// 否则 TRACE 级别会把引擎线程拖慢 —— 日志本身不能成为故障源。
#[macro_export]
macro_rules! diag_trace {
    ($tag:expr, $($arg:tt)*) => {
        $crate::diag::log($crate::diag::Level::Trace, $tag, format_args!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 级别比较语义:`enabled` 是"不高于当前级别",即设 info 时 error/warn/info 都出
    #[test]
    fn level_ordering_matches_severity() {
        assert!(Level::Error < Level::Warn);
        assert!(Level::Warn < Level::Info);
        assert!(Level::Info < Level::Debug);
        assert!(Level::Debug < Level::Trace);
    }

    /// 级别名解析要容错常见写法(用户手写 settings.json 时不会那么规范)
    #[test]
    fn level_parsing_is_forgiving() {
        assert_eq!(Level::parse("DEBUG"), Some(Level::Debug));
        assert_eq!(Level::parse(" debug "), Some(Level::Debug));
        assert_eq!(Level::parse("Warn"), Some(Level::Warn));
        assert_eq!(Level::parse("warning"), Some(Level::Warn));
        assert_eq!(Level::parse("trace"), Some(Level::Trace));
        assert_eq!(Level::parse(""), None);
        assert_eq!(Level::parse("verbose!"), None);
    }

    /// 守护 `initial_level` 的那一行 `pick_level(cli_level(), 环境变量, None)`
    /// 以及 `set_level` 开头的 `should_apply(...)` 判断:
    /// 级别顺序必须是 **命令行 > 环境变量 > settings.json > 内置默认**。
    ///
    /// 阳性对照:命令行给了值时必须压过环境变量与配置;
    /// 阴性对照:命令行没给时,环境变量必须压过配置,两者都没给才落到内置默认。
    /// 任何把顺序写反、或把"命令行"当成"最后一个才看"的改动都会让这里的断言红。
    #[test]
    fn level_priority_is_cli_then_env_then_settings() {
        // 命令行给了:其余两个来源再详细也不许翻盘
        assert_eq!(
            pick_level(Some(Level::Warn), Some("trace"), Some("debug")),
            (Level::Warn, CLI_LEVEL_SOURCE)
        );
        // 命令行没给:环境变量 > 配置
        assert_eq!(
            pick_level(None, Some("trace"), Some("debug")),
            (Level::Trace, "环境变量 SCRCPY_PAD_LOG")
        );
        // 只有配置
        assert_eq!(
            pick_level(None, None, Some("debug")),
            (Level::Debug, "settings.json")
        );
        // 什么都没有:内置默认 info
        assert_eq!(pick_level(None, None, None), (Level::Info, "内置默认"));
        // 非法值当没写:环境变量写错了不该把配置一起拖下水
        assert_eq!(
            pick_level(None, Some("verbo se"), Some("warn")),
            (Level::Warn, "settings.json")
        );
        assert_eq!(
            pick_level(None, Some("nope"), None),
            (Level::Info, "内置默认")
        );
    }

    /// 守护 `set_level` 里那句 `if !should_apply(source, CLI_PINNED.load(...))`:
    /// 命令行钉住级别之后,settings.json / 环境变量都不许再覆盖它。
    ///
    /// 阳性对照:没钉住时任何来源都能改(否则运行期设级别就失效了);
    /// 阴性对照:钉住之后非命令行来源必须被拒,而命令行来源自己仍可再改。
    #[test]
    fn cli_level_pin_rejects_other_sources() {
        assert!(
            should_apply("settings.json", false),
            "未钉住时必须允许配置改级别"
        );
        assert!(should_apply("环境变量 SCRCPY_PAD_LOG", false));
        assert!(
            should_apply(CLI_LEVEL_SOURCE, true),
            "命令行来源本身不受自己的钉子限制"
        );
        assert!(
            !should_apply("settings.json", true),
            "命令行钉住之后 settings.json 不许覆盖"
        );
        assert!(
            !should_apply("环境变量 SCRCPY_PAD_LOG", true),
            "命令行钉住之后环境变量不许覆盖"
        );
    }

    /// **测试串行锁**(2026-10-13):`diag` 的 sink / path 是**进程级全局**,
    /// 而 `cargo test` 默认**并行**跑 —— 任何改这两个全局的测试都必须先拿这把锁,
    /// 否则会互相踩(实测:`interface_log_reaches_the_diagnostics_file` 单跑绿、
    /// 全量跑红,根因就是它把 `sink` 换成临时文件后又被别的测试 `init()` 覆盖)。
    ///
    /// 中毒也照拿(`unwrap_or_else`):一次失败不该连累后面每个测试都 panic
    /// —— 与 `capture.rs` 的 `SHARED_LOCK` 同一套写法。
    static DIAG_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn diag_test_guard() -> std::sync::MutexGuard<'static, ()> {
        DIAG_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// **事故回归**(2026-10-12):测试**绝不许**碰生产诊断日志路径。
    ///
    /// 真实事故:`cargo test` 里一条测试调了 `init()`,而 `init()` 会对
    /// `config_dir()/diagnostics.log` 做 `File::create` —— 用户机器上跑一次测试,
    /// 用户当时那场会话的日志就被整份覆盖(而那正是唯一的取证介质)。
    ///
    /// 这条测试锁住修法:`init()` 在 `cfg(test)` 下必须指向**临时目录**。
    #[test]
    fn tests_never_touch_the_production_log_path() {
        let _g = diag_test_guard();
        let prod = crate::app::config_dir().join(FILE_NAME);
        init();
        let in_use = path();
        assert_ne!(
            in_use,
            prod,
            "测试构建下 init() 必须落到临时目录,不许指向生产日志 {}",
            prod.display()
        );
        assert!(
            in_use.starts_with(std::env::temp_dir()),
            "测试日志必须在临时目录里:{:?}",
            in_use
        );
    }

    /// **F1 接线层回归**(2026-10-12,协议 §6.3 必测缝①):
    /// 界面日志必须真的落进**文件**,而不是只进内存环形缓冲。
    ///
    /// 为什么必须有一条**落盘**断言,而不是只看实现:`PadApp::log()` 曾经只把
    /// 消息推进 `self.logs` 环形缓冲,于是"手机被手触摸 / 手离开手机 / 设备端
    /// 输入账本"这些自愈层消息在用户能看到的 diagnostics.log 里**一行都没有** ——
    /// 承诺的取证链是空的,而所有门禁照样全绿(门禁绿 ≠ 问题解决)。
    ///
    /// **绝不调用 `init()`**(2026-10-12 事故):`init()` 会对
    /// `config_dir()/diagnostics.log` 做 `File::create` —— 测试跑在**用户机器**上,
    /// 一调就把用户当时那场会话的日志整份覆盖掉(真实发生过:用户让我看的
    /// 21:47 会话日志因此丢失)。所以这里绕开 `init`,直接把 `Sink` 指向**临时
    /// 目录**里的文件,验证同一条写入通路(`log()` → `write_raw()` → 文件),
    /// 并在末尾断言"生产路径没有被本次测试动过"。
    #[test]
    fn interface_log_reaches_the_diagnostics_file() {
        use std::io::Write as _;
        // 串行:这条测试会**替换全局 sink**,必须与其它动 diag 全局的测试互斥
        let _g = diag_test_guard();

        // ① 记录生产路径的当前状态(绝不修改它)
        let prod = crate::app::config_dir().join(FILE_NAME);
        let prod_before = std::fs::read(&prod).ok();

        // ② 把 sink 指向临时文件(与生产同一条 `write_raw` 通路,只是换了文件)
        let tmp = std::env::temp_dir().join(format!(
            "diag-seam-{}-{:?}.log",
            std::process::id(),
            std::thread::current().id()
        ));
        let f = std::fs::File::create(&tmp).expect("临时日志文件应可创建");
        {
            let mut st = lock();
            st.sink = Some(Sink {
                file: Some(std::io::BufWriter::new(f)),
                write_failed_reported: false,
            });
        }
        let marker = format!("F1-SEAM-PROBE-{}", std::process::id());
        // 直接走 `write_raw`(生产 `log()` 的落盘那一步):级别过滤与 stderr 行为
        // 不属于这条缝,缝是"写下去到底进没进文件"。
        write_raw(&format!("[INFO ][app][test] {marker}"));
        flush();
        // 关掉临时 sink,免得影响其它测试
        {
            let mut st = lock();
            if let Some(s) = st.sink.as_mut() {
                if let Some(f) = s.file.as_mut() {
                    let _ = f.flush();
                }
            }
            st.sink = None;
        }
        let text = std::fs::read_to_string(&tmp).expect("临时日志文件应可读回");
        assert!(
            text.contains(&marker),
            "界面日志必须真的落进文件(F1 的回归点);文件里没有 {marker}"
        );
        assert!(
            text.contains("[app]"),
            "落盘行必须带来源标签,便于按模块过滤"
        );
        let _ = std::fs::remove_file(&tmp);

        // ③ **不许碰生产日志**:这条断言是本事故(测试覆盖用户现场)的回归点
        assert_eq!(
            std::fs::read(&prod).ok(),
            prod_before,
            "测试绝不许改动生产诊断日志 {} —— 用户现场就是取证证据",
            prod.display()
        );
    }

    /// 把全局 sink 临时指向一个临时文件(与生产同一条 `write_raw` 通路,只是换了文件)。
    /// 调用方必须已持有 [`diag_test_guard`],并在结束时调 [`restore_sink`]。
    fn install_temp_sink(tag: &str) -> std::path::PathBuf {
        let tmp = std::env::temp_dir().join(format!(
            "diag-{tag}-{}-{:?}.log",
            std::process::id(),
            std::thread::current().id()
        ));
        let f = std::fs::File::create(&tmp).expect("临时日志文件应可创建");
        let mut st = lock();
        st.sink = Some(Sink {
            file: Some(std::io::BufWriter::new(f)),
            write_failed_reported: false,
        });
        tmp
    }

    /// 收掉临时 sink,免得影响其它测试。
    fn restore_sink() {
        let mut st = lock();
        if let Some(s) = st.sink.as_mut() {
            if let Some(f) = s.file.as_mut() {
                let _ = f.flush();
            }
        }
        st.sink = None;
    }

    /// **R15 缝①(协议 §3.7)**:Raw Input 通道那三条关键日志必须**真的落盘**,
    /// 并且**真的进得了诊断面板的环形缓冲**(界面可见性就靠这条桥)。
    ///
    /// 为什么值得单独一条:这三行是"原始输入到底启用没有 / 为什么退回光标差"的
    /// 第一现场,而它们走的是 `diag_info!`/`diag_warn!` → `log()` → `write_raw()`,
    /// **完全不经过 `PadApp::log()` 与界面的 `self.logs`**。改之前它们只落文件,
    /// 面板上什么都没有(用户只能自己去翻文件)。这条测试同时钉住两头:
    /// 文件里有(取证)、`recent_lines()` 里也有(面板可见)。
    ///
    /// **级别口径**(不许假绿):三条里有一条是 `INFO`。默认级别虽是 `info`,但
    /// 环境变量 `SCRCPY_PAD_LOG` 可以把它压到 `error`,那样这条测试会因为
    /// "级别被过滤"而**假绿/假红**。所以这里显式 `set_level(Info)` 并复原。
    ///
    /// **绝不调用 `init()`**(2026-10-12 事故):`init()` 会对
    /// `config_dir()/diagnostics.log` 做 `File::create`,测试跑在用户机器上,
    /// 一调就把用户当时那场会话的日志整份覆盖。末尾仍断言生产路径未被改动。
    #[test]
    fn raw_channel_logs_reach_the_diagnostics_file() {
        // 串行:这条测试会替换全局 sink + 改全局级别,必须与其它动 diag 全局的测试互斥
        let _g = diag_test_guard();

        // ① 记录生产路径的当前状态(绝不修改它)
        let prod = crate::app::config_dir().join(FILE_NAME);
        let prod_before = std::fs::read(&prod).ok();

        // ② 显式把级别抬到 info 并保存原值 —— 环境变量把它压到 error 时,
        //    `log(Info, ..)` 会被 `enabled()` 静默吞掉,这条测试就会假绿。
        let saved_level = level();
        let saved_source = level_source();
        set_level(Level::Info, "R15 测试");

        // ③ 把 sink 指向临时文件(与生产同一条 `write_raw` 通路,只是换了文件)
        let tmp = install_temp_sink("rawseam");

        // ④ 逐条写 Raw Input 通道的三条**生产原文**(capture.rs 逐字引用)
        log(
            Level::Info,
            "capture",
            format_args!("原始输入(Raw Input)已启用:鼠标位移按设备计数上报(无指针加速/DPI 失真)"),
        );
        log(
            Level::Warn,
            "capture",
            format_args!(
                "检测到绝对坐标鼠标(数位板/远程桌面/部分触控板):原始输入对它不适用,位移退回光标差路径"
            ),
        );
        log(
            Level::Warn,
            "capture",
            format_args!(
                "原始输入注册后持续丢弃位移 {}ms 仍未收到任何鼠标事件:位移退回光标差路径",
                700
            ),
        );
        flush();

        // 先取缓冲快照、再读文件:缓冲是**进程级全局**,别的测试也会往里推行,
        // 先取快照能把"被别的测试挤出去"的时间窗压到最小(§3.9 flaky = bug)。
        let recent = recent_lines();
        let text = std::fs::read_to_string(&tmp).expect("临时日志文件应可读回");

        // ⑤ 三条都必须落盘,且带 `[capture]` 标签(便于 `grep '[capture]'` 过滤)
        for (what, needle) in [
            ("info 启用行", "原始输入(Raw Input)已启用"),
            ("warn 绝对坐标行", "检测到绝对坐标鼠标"),
            ("warn 丢弃位移行", "持续丢弃位移 700ms"),
        ] {
            assert!(
                text.contains(needle),
                "Raw 通道的{what}必须落进文件(缝①的回归点);文件里没有 {needle:?}\n--- 文件全文 ---\n{text}"
            );
            assert!(
                text.contains("[capture]"),
                "落盘行必须带 [capture] 标签,便于按模块过滤;文件里没有"
            );
            assert!(
                recent.iter().any(|l| l.contains(needle)),
                "Raw 通道的{what}必须同时进诊断面板的环形缓冲(否则面板上看不到 \
                 『原始输入』的任何一行 —— 这正是 R15 要补的桥);缓冲里没有 {needle:?}\n\
                 --- 缓冲全文 ---\n{recent:#?}"
            );
        }
        // 带参数那条的参数必须被真的渲染进去(不是留下 `{}`)
        assert!(
            text.contains("位移退回光标差路径") && !text.contains("{}ms"),
            "带参数的那条 warn 必须渲染出具体毫秒数,不能留下未展开的占位符"
        );

        // ⑥ 收尾:还原 sink 与级别
        restore_sink();
        set_level(saved_level, &saved_source);
        let _ = std::fs::remove_file(&tmp);

        // ⑦ **不许碰生产日志**:这条断言是"测试覆盖用户现场"事故的回归点
        assert_eq!(
            std::fs::read(&prod).ok(),
            prod_before,
            "测试绝不许改动生产诊断日志 {} —— 用户现场就是取证证据",
            prod.display()
        );
    }

    /// **R15:环形缓冲容量**。界面只显示最近 8 行,但缓冲留 [`RECENT_CAP`] 行;
    /// 超容量时必须**挤掉最早的**,而不是无限增长(那是内存泄漏),也不是丢掉最新的
    /// (那会让面板永远停在旧内容上)。
    ///
    /// 走 `write_raw` 而不是 `push_recent`:推入点必须在**落盘那一步** ——
    /// 变异验证③(删掉 `write_raw` 里的推入行)就是要让这条测试变红。
    #[test]
    fn recent_log_lines_are_capped() {
        let _g = diag_test_guard();
        let prod = crate::app::config_dir().join(FILE_NAME);
        let prod_before = std::fs::read(&prod).ok();

        let tmp = install_temp_sink("recentcap");
        // 多写 6 行,确保"最早那行"一定被挤出去(与缓冲里此前的残留无关)
        let total = RECENT_CAP + 6;
        for i in 0..total {
            write_raw(&format!("[+{:>7}ms][R15-CAP] 第 {i} 行", i));
        }
        let lines = recent_lines();
        restore_sink();
        let _ = std::fs::remove_file(&tmp);

        assert!(
            lines.len() <= RECENT_CAP,
            "环形缓冲不得超过容量 {RECENT_CAP},实为 {}",
            lines.len()
        );
        assert_eq!(
            lines.len(),
            RECENT_CAP,
            "写满 {total} 行后缓冲应恰好是容量 {RECENT_CAP} 行,实为 {}",
            lines.len()
        );
        assert!(
            !lines.iter().any(|l| l.contains("第 0 行")),
            "最早的『第 0 行』必须已被挤掉(否则是无限增长)"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains(&format!("第 {} 行", total - 1))),
            "最后写的『第 {} 行』必须在缓冲里(否则会丢最新)",
            total - 1
        );
        assert_eq!(
            std::fs::read(&prod).ok(),
            prod_before,
            "测试绝不许改动生产诊断日志 {}",
            prod.display()
        );
    }

    /// 没有日志文件(或空文件)时不能误判成"上次异常结束" ——
    /// 否则每次全新安装都会警告一次,狼来了之后用户就不看了
    #[test]
    fn first_run_is_not_flagged_as_abnormal() {
        let p = std::env::temp_dir().join(format!("diag-none-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&p);
        assert!(!previous_run_was_abnormal(&p), "文件不存在不应算异常");
        std::fs::write(&p, b"").unwrap();
        assert!(!previous_run_was_abnormal(&p), "空文件不应算异常");
        std::fs::write(&p, format!("...\n{EXIT_MARKER}\n")).unwrap();
        assert!(!previous_run_was_abnormal(&p), "有退出标记不应算异常");
        std::fs::write(&p, "[INFO] 跑到一半就被 kill 了").unwrap();
        assert!(previous_run_was_abnormal(&p), "没有退出标记就算异常");
        let _ = std::fs::remove_file(&p);
    }

    /// 时间格式化必须产出可读且形状固定的字符串:
    /// `grep` 与人工对齐时间线都依赖这个形状
    #[test]
    fn time_format_is_stable() {
        let s = fmt_time(1_700_000_000);
        assert_eq!(s.len(), 19, "应为 YYYY-MM-DD HH:MM:SS: {s}");
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[10..11], " ");
        assert_eq!(&s[13..14], ":");
    }
}
