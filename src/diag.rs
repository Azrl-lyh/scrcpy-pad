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

use std::fmt::Display;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
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

/// 初始级别:环境变量优先,否则内置默认 `info`
fn initial_level() -> (Level, &'static str) {
    match std::env::var(ENV_LEVEL).ok().and_then(|v| Level::parse(&v)) {
        Some(l) => (l, "环境变量 SCRCPY_PAD_LOG"),
        None => (Level::Info, "内置默认"),
    }
}

/// 初始化:在配置目录下**截断重写**日志文件,并写入启动头部。
///
/// 必须在 `app::PadApp::new()` 之前调用 —— 那里会立刻开始写设备清单,
/// 晚一步那些最关键的第一手信息就会丢。
pub fn init() {
    let path = crate::app::config_dir().join(FILE_NAME);
    // 先看上一份日志的结尾(Create 会把内容冲掉,必须在此之前读)
    let prev_abnormal = previous_run_was_abnormal(&path);

    let mut file = None;
    match File::create(&path) {
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
    write_raw(&line);
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
    let mut guard = lock();
    // 一次性把字段拆开:`st.sink.as_mut()` 会借用整个 `*guard`(Deref 之后
    // 借用检查看不到字段互不相交),那样后面再用别的字段就会冲突。
    let Diag { sink, .. } = &mut *guard;
    let Some(sink) = sink.as_mut() else { return };
    let Some(f) = sink.file.as_mut() else { return };
    if writeln!(f, "{line}").is_err() && !sink.write_failed_reported {
        sink.write_failed_reported = true;
        // 此处不能调 log(会把锁再借一次),直接打 stderr
        eprintln!("[diag] 诊断日志写入失败(后续不再提示)");
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
            "否 —— 读不到 /dev/input,必须 sudo usermod -aG input $USER 后重新登录"
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
