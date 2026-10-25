use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

/// 进程内选定的 adb 可执行文件(通常为绝对路径;未设置时回退 PATH 中的 "adb")
static ADB_BIN: Mutex<Option<String>> = Mutex::new(None);

/// 设置 adb 可执行文件路径;传 None 表示恢复为使用 PATH 中的 "adb"
pub fn set_adb_bin(path: Option<&Path>) {
    *ADB_BIN.lock().unwrap() = path.map(|p| p.display().to_string());
}

/// 当前生效的 adb 可执行文件(绝对路径或 "adb")
pub fn adb_bin_now() -> Option<String> {
    ADB_BIN.lock().unwrap().clone()
}

fn adb_bin() -> String {
    ADB_BIN
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| "adb".to_string())
}

/// 各平台 adb 可执行文件名(Windows 必须带 .exe 才能在 PATH 中命中)
pub fn adb_exe_name() -> &'static str {
    if cfg!(windows) { "adb.exe" } else { "adb" }
}

fn adb_cmd(serial: Option<&str>) -> Command {
    let mut c = Command::new(adb_bin());
    if let Some(s) = serial {
        if !s.is_empty() {
            c.args(["-s", s]);
        }
    }
    c
}

/// 列出已连接设备序列号
pub fn list_devices() -> Vec<String> {
    let out = Command::new(adb_bin()).arg("devices").output();
    let Ok(out) = out else { return Vec::new() };
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .skip(1)
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            match (it.next(), it.next()) {
                (Some(id), Some("device")) => Some(id.to_string()),
                _ => None,
            }
        })
        .collect()
}

/// 运行 adb --version 解析版本(失败返回 None,可据此判断 adb 是否可用)
pub fn adb_version_at(exe: &str) -> Option<String> {
    let out = Command::new(exe).arg("version").output().ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .find(|l| l.contains("version"))
        .map(|l| l.trim().to_string())
}

/// 设备物理分辨率 (w, h)
pub fn screen_size(serial: &str) -> Result<(u32, u32)> {
    let out = adb_cmd(Some(serial))
        .args(["shell", "wm", "size"])
        .output()
        .context("执行 adb shell wm size 失败")?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    // "Physical size: 1080x2400"
    let size_part = stdout
        .trim()
        .rsplit(|c| c == ' ' || c == ':')
        .next()
        .context("无法解析 wm size 输出")?;
    let mut it = size_part.split('x');
    let w: u32 = it.next().context("无宽度")?.trim().parse()?;
    let h: u32 = it.next().context("无高度")?.trim().parse()?;
    Ok((w, h))
}

/// 当前显示器尺寸 (w, h) —— 随屏幕方向变化,即触摸注入使用的坐标空间。
///
/// `wm size` 返回的是设备物理分辨率(始终是自然方向),横屏时会与真实坐标空间
/// 不一致(例如 wm size=1280x2772 而横屏实际为 2772x1280),导致按自然方向钳制的
/// 坐标落到屏幕外被系统丢弃,因此这里优先取 `dumpsys window displays` 的 cur=WxH。
pub fn display_size(serial: &str) -> Result<(u32, u32)> {
    if let Ok(out) = adb_cmd(Some(serial))
        .args(["shell", "dumpsys", "window", "displays"])
        .output()
    {
        let stdout = String::from_utf8_lossy(&out.stdout);
        if let Some(wh) = parse_display_size(&stdout) {
            return Ok(wh);
        }
    }
    screen_size(serial)
}

/// 一次「设备侧巡检」的结果(周期性坐标空间重查与设备注入自检共用这一份)。
///
/// 每个字段都允许 `None` = **这一轮没读到**,调用方必须把它与"读到了某个值"
/// 分开处理 —— 尤其是 [`Self::size`]:一次 adb 抖动绝不能被当成"坐标空间变了",
/// 否则会平白无故抬起玩家此刻正按着的所有触点。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceProbe {
    /// 当前触摸坐标空间(`dumpsys window displays` 的 `cur=WxH`)。
    pub size: Option<(u32, u32)>,
    /// [`Self::size`] 是不是真的来自 `dumpsys` 的 `cur=`(即**当前方向**的空间)。
    ///
    /// 解析不到时 `size` 会回退成 `wm size` —— 那是**物理分辨率、永远自然方向**:
    /// 横屏设备上它与真实坐标空间正好差 90°,拿它跟缓存比对必然误判"空间变了"。
    /// 所以回退值只能"看",不能用来判定变化。
    pub size_is_display_space: bool,
    /// `mCurrentFocus=`(真正拿到输入焦点的窗口)。`null` 原样保留 ——
    /// "当前没有焦点窗口"本身就是"注入被系统丢掉"的一条线索。
    pub focus: Option<String>,
    /// `mFocusedApp=`(前台 ActivityRecord):与 `focus` 一起看才能分清
    /// "游戏在前台,但焦点被别的窗口(弹窗/输入法)抢走"。
    pub focused_app: Option<String>,
    /// 屏幕是否亮着(`dumpsys display` 的 `mScreenState=ON`);读不到为 None。
    pub screen_on: Option<bool>,
}

impl DeviceProbe {
    /// 屏幕亮灭的人话表示(读不到就说"未知",不瞎猜)
    pub fn screen_on_text(&self) -> &'static str {
        match self.screen_on {
            Some(true) => "亮",
            Some(false) => "灭",
            None => "未知",
        }
    }

    /// 一句话摘要(日志与悬浮窗共用同一份口径)
    pub fn summary(&self) -> String {
        format!(
            "空间={} 焦点={} 前台={} 屏幕={}",
            match self.size {
                Some((w, h)) => format!("{w}x{h}"),
                None => "未知".to_string(),
            },
            self.focus.as_deref().unwrap_or("未知"),
            self.focused_app.as_deref().unwrap_or("未知"),
            self.screen_on_text(),
        )
    }
}

/// 一次设备侧巡检:坐标空间 + 焦点窗口 + 屏幕亮灭。
///
/// 坐标空间沿用 [`display_size`] 的口径(`dumpsys window displays` 的 `cur=` 优先,
/// 解析不到回退 `wm size`);焦点窗口/前台 Activity 从同一条 `dumpsys window displays`
/// 输出里取(拿不到时补一条完整 `dumpsys window`);屏幕亮灭另取 `dumpsys display`。
/// **每条查询都失败也照样返回**:巡检的价值之一是"如实说出这一轮什么都没读到",
/// 由调用方决定不动手。
pub fn device_probe(serial: &str) -> DeviceProbe {
    let mut probe = DeviceProbe::default();
    if let Ok(out) = adb_cmd(Some(serial))
        .args(["shell", "dumpsys", "window", "displays"])
        .output()
    {
        let text = String::from_utf8_lossy(&out.stdout);
        probe.size = parse_display_size(&text);
        probe.size_is_display_space = probe.size.is_some();
        probe.focus = parse_window_field(&text, "mCurrentFocus=");
        probe.focused_app = parse_window_field(&text, "mFocusedApp=");
        // 少数 ROM 的 `displays` 变体不打印焦点(只在默认 dump 里)。补一条,
        // 别让遥测永远是空的 —— 只有在真的没拿到时才多这一次 adb 调用。
        if probe.focus.is_none() || probe.focused_app.is_none() {
            if let Ok(out) = adb_cmd(Some(serial))
                .args(["shell", "dumpsys", "window"])
                .output()
            {
                let text = String::from_utf8_lossy(&out.stdout);
                if probe.focus.is_none() {
                    probe.focus = parse_window_field(&text, "mCurrentFocus=");
                }
                if probe.focused_app.is_none() {
                    probe.focused_app = parse_window_field(&text, "mFocusedApp=");
                }
            }
        }
    }
    if probe.size.is_none() {
        // 回退口径与 `display_size` 一致。注意 `size_is_display_space` 保持 false:
        // 这是自然方向的物理分辨率,只能当参考,不能拿来判定"空间变了"。
        probe.size = screen_size(serial).ok();
    }
    if let Ok(out) = adb_cmd(Some(serial))
        .args(["shell", "dumpsys", "display"])
        .output()
    {
        probe.screen_on = parse_screen_on(&String::from_utf8_lossy(&out.stdout));
    }
    probe
}

/// 在设备上**用 shell 的 input 命令**点一下 (x, y)。
///
/// 这条路**完全绕过本程序的控制通道**(不走 scrcpy 的注入协议、不用我们缓存的
/// 坐标空间),所以它是"设备注入自检"里的对照器械:同一下点击,shell 点得动而
/// 我们的注入没反应 ⇒ 问题在我们这条通道;两个都没反应 ⇒ 问题在设备/系统层
/// (灭屏、焦点被别的窗口抢走、系统挡了注入)。
///
/// 成功时返回设备端输出(通常为空),失败返回人话原因 —— 自检要把这句话原样写进
/// 日志,不能把"命令没跑成"悄悄吞掉。
pub fn input_tap(serial: &str, x: u32, y: u32) -> Result<String> {
    let (sx, sy) = (x.to_string(), y.to_string());
    let out = adb_cmd(Some(serial))
        .args(["shell", "input", "tap", &sx, &sy])
        .output()
        .context("执行 adb shell input tap 失败")?;
    let mut text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !err.is_empty() {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&err);
    }
    if !out.status.success() {
        bail!("input tap 退出码非 0: {text}");
    }
    Ok(text)
}

/// 当前显示器刷新率（Hz），用于“其他功能 -> 调试信息”。
///
/// Android 没有统一公开的“游戏实时 FPS”查询接口。这里优先读取
/// SurfaceFlinger 的刷新周期；失败时回退到 `dumpsys display` 的 refreshRate。
pub fn display_refresh_rate(serial: &str) -> Result<f32> {
    if let Ok(out) = adb_cmd(Some(serial))
        .args(["shell", "dumpsys", "SurfaceFlinger", "--latency"])
        .output()
    {
        let stdout = String::from_utf8_lossy(&out.stdout);
        if let Some(fps) = parse_refresh_rate_latency(&stdout) {
            return Ok(fps);
        }
    }
    let out = adb_cmd(Some(serial))
        .args(["shell", "dumpsys", "display"])
        .output()
        .context("执行 adb shell dumpsys display 失败")?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    parse_refresh_rate_text(&stdout).context("无法从设备输出解析刷新率")
}

fn parse_refresh_rate_latency(text: &str) -> Option<f32> {
    let first = text
        .lines()
        .find_map(|line| line.trim().parse::<f64>().ok());
    if let Some(period) = first {
        if (1_000.0..=100_000_000.0).contains(&period) {
            let fps = 1_000_000_000.0 / period;
            if (1.0..=1000.0).contains(&fps) {
                return Some(fps as f32);
            }
        }
    }
    let timestamps: Vec<u64> = text
        .lines()
        .filter_map(|line| line.split(',').next()?.trim().parse().ok())
        .filter(|v: &u64| *v > 1_000_000)
        .collect();
    if timestamps.len() >= 2 {
        let first = timestamps[0];
        let last = *timestamps.last().unwrap();
        if last > first {
            let fps = (timestamps.len() - 1) as f64 * 1_000_000_000.0 / (last - first) as f64;
            if (1.0..=1000.0).contains(&fps) {
                return Some(fps as f32);
            }
        }
    }
    None
}

fn parse_refresh_rate_text(text: &str) -> Option<f32> {
    for key in ["refreshRate=", "fps="] {
        if let Some(value) = parse_float_after(text, key) {
            if (1.0..=1000.0).contains(&value) {
                return Some(value as f32);
            }
        }
    }
    None
}

fn parse_float_after(text: &str, key: &str) -> Option<f32> {
    let start = text.find(key)? + key.len();
    let rest = text[start..].trim_start();
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// 从 `dumpsys window displays` 输出中取当前显示尺寸。
/// 每行形如 "... init=1280x2772 520dpi ... cur=2772x1280 app=2772x1280 rng=..." ,
/// 取最后一个 cur=(物理显示器,即真正的触摸坐标空间)。
fn parse_display_size(dumpsys: &str) -> Option<(u32, u32)> {
    dumpsys
        .split("cur=")
        .skip(1)
        .filter_map(parse_size_token)
        .last()
}

/// 从 dumpsys 文本里取一个 `key=value` 字段的值(取第一处出现;值到行尾)。
///
/// `mCurrentFocus=` / `mFocusedApp=` 的行形如:
///
/// ```text
///   mCurrentFocus=Window{8b0f3a1 u0 com.tencent.tmgp.dfm/com.epicgames.ue4.GameActivity}
///   mFocusedApp=ActivityRecord{1a2b3c u0 com.tencent.tmgp.dfm/.MainActivity t42}
/// ```
///
/// 值可能是 `null`(真的没有焦点窗口)—— **原样返回**,调用方据此说"当前没有焦点
/// 窗口",而不是当成"没查到"。只有字段不存在或值为空时才是 `None`。
fn parse_window_field(text: &str, key: &str) -> Option<String> {
    let line = text.lines().find(|l| l.contains(key))?;
    let value = line.split_once(key)?.1.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// 从 `dumpsys display` 里取"屏幕亮着吗"。
///
/// 各 Android 版本的关键字不同:优先 `mScreenState=`(DisplayContent),
/// 退而用 `mGlobalDisplayState=`(DisplayManagerService)。**只认 ON/OFF 两种明确
/// 取值** —— DOZE / UNKNOWN 之类一律返回 None:遥测宁可不报,也不能把不确定的
/// 状态说成"灭屏"(那会把排查方向整个带偏)。
fn parse_screen_on(text: &str) -> Option<bool> {
    for key in ["mScreenState=", "mGlobalDisplayState="] {
        let Some((_, rest)) = text.split_once(key) else {
            continue;
        };
        let token: String = rest
            .trim_start()
            .chars()
            .take_while(|c| c.is_ascii_uppercase() || *c == '_')
            .collect();
        match token.as_str() {
            "ON" => return Some(true),
            "OFF" => return Some(false),
            _ => continue,
        }
    }
    None
}

/// 从 "2772x1280 rest..." 这样的片段里取出前导的 WxH
fn parse_size_token(s: &str) -> Option<(u32, u32)> {
    let tok: String = s
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == 'x')
        .collect();
    let mut it = tok.split('x');
    let w: u32 = it.next()?.parse().ok()?;
    let h: u32 = it.next()?.parse().ok()?;
    if w > 0 && h > 0 { Some((w, h)) } else { None }
}

/// 抓取一帧 PNG 截图
pub fn screencap_png(serial: &str) -> Result<Vec<u8>> {
    let out = adb_cmd(Some(serial))
        .args(["exec-out", "screencap", "-p"])
        .output()
        .context("执行 adb exec-out screencap 失败")?;
    if !out.status.success() || out.stdout.is_empty() {
        bail!("screencap 返回空或失败");
    }
    Ok(out.stdout)
}

// ===================== scrcpy 定位与测试 =====================

/// 在 PATH 环境变量中查找可执行文件
fn find_in_path(name: &str) -> Option<PathBuf> {
    let path_env = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_env) {
        let p = dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// 自动寻找 scrcpy 可执行文件:PATH 优先,其次各平台常见安装位置,
/// 最后回退到"程序旁边的便携 scrcpy 目录"。
pub fn find_scrcpy() -> Option<PathBuf> {
    let path_names: &[&str] = if cfg!(windows) {
        &["scrcpy.exe"]
    } else {
        &["scrcpy"]
    };
    for name in path_names {
        if let Some(p) = find_in_path(name) {
            return Some(p);
        }
    }

    let candidates: Vec<PathBuf> = if cfg!(windows) {
        let mut v = vec![
            PathBuf::from(r"C:\Program Files\scrcpy\scrcpy.exe"),
            PathBuf::from(r"C:\Program Files (x86)\scrcpy\scrcpy.exe"),
        ];
        if let Some(lo) = std::env::var_os("LOCALAPPDATA") {
            v.push(PathBuf::from(&lo).join(r"Programs\scrcpy\scrcpy.exe"));
        }
        if let Some(home) = std::env::var_os("USERPROFILE") {
            v.push(PathBuf::from(&home).join(r"scoop\shims\scrcpy.exe"));
        }
        v.push(PathBuf::from(r"C:\ProgramData\chocolatey\bin\scrcpy.exe"));
        v
    } else {
        let mut v = vec![
            PathBuf::from("/usr/bin/scrcpy"),
            PathBuf::from("/usr/local/bin/scrcpy"),
            PathBuf::from("/opt/scrcpy/scrcpy"),
            PathBuf::from("/snap/bin/scrcpy"),
        ];
        if let Some(home) = std::env::var_os("HOME") {
            v.push(PathBuf::from(&home).join(".local/bin/scrcpy"));
        }
        v
    };
    if let Some(p) = candidates.into_iter().find(|p| p.is_file()) {
        return Some(p);
    }

    // 系统里没装 scrcpy 时,再看程序旁边的便携目录
    find_scrcpy_portable()
}

/// 用户(或上次记住的设置)明确给出的 scrcpy 位置。
///
/// **既接受可执行文件,也接受目录** —— 用户嘴里的"scrcpy 目录"就是发行包解压出来的
/// 那个文件夹(scrcpy-win64-vX.Y)。只认 exe 会让"我设置好 scrcpy 目录后,
/// 再次启动依旧找不到"这种体验问题反复出现;目录里再多套一层子目录也能命中。
pub fn find_scrcpy_explicit(path: &str) -> Option<PathBuf> {
    let t = path.trim();
    if t.is_empty() {
        return None;
    }
    let p = PathBuf::from(t);
    if p.is_file() {
        return Some(p);
    }
    if p.is_dir() {
        return find_scrcpy_in_dir(&p, 3);
    }
    None
}

/// 在给定目录(含至多 depth 层子目录)里查 scrcpy 可执行文件。
/// 文件名精确匹配 `scrcpy.exe`(Windows)/`scrcpy`,因此不会把 `scrcpy-pad.exe` 认错。
pub fn find_scrcpy_in_dir(dir: &Path, depth: usize) -> Option<PathBuf> {
    find_file_recursive(dir, scrcpy_exe_name(), depth)
}

/// 在基准目录"一带"找 scrcpy。与 [`find_scrcpy_in_dir`] 的区别:
/// 这里只往**名字像 scrcpy 的子目录**里下探(允许外面再套一层任意名字的文件夹,
/// 因为"解压到一个文件夹里"很常见)。
///
/// 因此它可以安全地拿桌面、下载目录、用户主目录这类大目录当基准 ——
/// 代价只有每层一次 read_dir,不会递归整棵目录树。用户记住的 scrcpy 目录
/// 被换版本/改名之后,就是靠它从"上一级"把新版本找回来的。
pub fn find_scrcpy_under(base: &Path, depth: usize) -> Option<PathBuf> {
    // 1) 基准目录本身就是发行包目录
    if let Some(p) = find_scrcpy_in_dir(base, 0) {
        return Some(p);
    }
    // 2) 基准目录下以 scrcpy 开头的子目录(官方发行包解压出来的样子)
    for d in scrcpy_named_subdirs(base) {
        if let Some(p) = find_scrcpy_in_dir(&d, depth.saturating_sub(1)) {
            return Some(p);
        }
    }
    // 3) 再套一层任意名字的文件夹,例如 "D:\下载\一个文件夹\scrcpy-win64-v3.3"
    if depth > 0 {
        for outer in subdirs(base) {
            for d in scrcpy_named_subdirs(&outer) {
                if let Some(p) = find_scrcpy_in_dir(&d, 0) {
                    return Some(p);
                }
            }
        }
    }
    None
}

/// 目录下的子目录(排序,保证结果稳定可测)
fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut v: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .collect();
    v.sort();
    v
}

/// 目录下名字以 "scrcpy" 开头的子目录
fn scrcpy_named_subdirs(dir: &Path) -> Vec<PathBuf> {
    subdirs(dir)
        .into_iter()
        .filter(|p| is_scrcpy_dir_name(p))
        .collect()
}

/// 在"程序目录附近"寻找便携部署的 scrcpy。
///
/// 扫描程序所在目录及其父目录下、名字以 "scrcpy" 开头的文件夹
/// (对应"把官方 scrcpy 发行包解压到主程序旁边"的用法),
/// 并在其中(至多 3 层子目录)查找 scrcpy 可执行文件。
/// 找到后,同目录的 scrcpy-server 与 adb 会由既有联动逻辑自动补齐。
pub fn find_scrcpy_portable() -> Option<PathBuf> {
    for base in program_base_dirs() {
        if let Some(exe) = find_scrcpy_portable_under(&base) {
            return Some(exe);
        }
    }
    None
}

/// 在给定基准目录附近寻找便携 scrcpy:
/// 先看基准目录内部,再看其父目录(即"同级"位置)下的 scrcpy* 子目录。
fn find_scrcpy_portable_under(base: &Path) -> Option<PathBuf> {
    // 候选扫描根:程序目录本身;以及程序目录的父目录(即"同级"位置)
    let mut roots = vec![base.to_path_buf()];
    if let Some(parent) = base.parent() {
        roots.push(parent.to_path_buf());
    }
    roots
        .into_iter()
        .find_map(|root| find_scrcpy_under(&root, 3))
}

/// 目录名是否以 "scrcpy" 开头(不区分大小写)
fn is_scrcpy_dir_name(p: &Path) -> bool {
    p.file_name()
        .map(|n| {
            n.to_string_lossy()
                .to_ascii_lowercase()
                .starts_with("scrcpy")
        })
        .unwrap_or(false)
}

/// 程序所在目录:优先可执行文件所在目录,其次当前工作目录
fn program_base_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            v.push(dir.to_path_buf());
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        if !v.contains(&cwd) {
            v.push(cwd);
        }
    }
    v
}

/// 在目录树中按文件名查找(深度受限,避免无谓的大范围遍历)
fn find_file_recursive(dir: &Path, name: &str, depth: usize) -> Option<PathBuf> {
    let direct = dir.join(name);
    if direct.is_file() {
        return Some(direct);
    }
    if depth == 0 {
        return None;
    }
    let rd = std::fs::read_dir(dir).ok()?;
    let mut subs: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .collect();
    subs.sort();
    for sub in subs {
        if let Some(found) = find_file_recursive(&sub, name, depth - 1) {
            return Some(found);
        }
    }
    None
}

/// 各平台 scrcpy 可执行文件名(Windows 必须带 .exe 才能在 PATH 中命中)
pub fn scrcpy_exe_name() -> &'static str {
    if cfg!(windows) {
        "scrcpy.exe"
    } else {
        "scrcpy"
    }
}

/// 手动指定的可执行文件:非空且确实存在时返回它。
/// 用于"用户(或上次记住的配置)明确给了路径"的场合 —— 此时优先于任何自动寻找。
pub fn find_adb_explicit(path: &str) -> Option<PathBuf> {
    let t = path.trim();
    if t.is_empty() {
        return None;
    }
    let p = PathBuf::from(t);
    p.is_file().then_some(p)
}

/// 给定目录,查找该目录下的 adb.exe/adb(官方 scrcpy Windows 发行包将 adb.exe 与 scrcpy.exe 同目录)
pub fn find_adb_in_dir(dir: &Path) -> Option<PathBuf> {
    let p = dir.join(adb_exe_name());
    p.is_file().then_some(p)
}

/// 自动寻找 adb:手动指定的 scrcpy 同目录优先,其次 PATH,最后平台常见安装位置
/// (Android SDK platform-tools)。scrcpy_exe 传 None 时跳过"同目录"阶段。
pub fn find_adb(scrcpy_exe: Option<&Path>) -> Option<PathBuf> {
    // 1. scrcpy 同目录(Windows 官方发行包布局:scrcpy.exe / scrcpy-server / adb.exe 三者同目录)
    if let Some(exe) = scrcpy_exe {
        if let Some(dir) = exe.parent() {
            if let Some(p) = find_adb_in_dir(dir) {
                return Some(p);
            }
        }
    }
    // 2. PATH
    if let Some(p) = find_in_path(adb_exe_name()) {
        return Some(p);
    }
    // 3. 平台常见安装位置
    let candidates: Vec<PathBuf> = if cfg!(windows) {
        let mut v = Vec::new();
        if let Some(lo) = std::env::var_os("LOCALAPPDATA") {
            v.push(PathBuf::from(&lo).join(r"Android\Sdk\platform-tools\adb.exe"));
        }
        if let Some(home) = std::env::var_os("USERPROFILE") {
            v.push(PathBuf::from(&home).join(r"AppData\Local\Android\Sdk\platform-tools\adb.exe"));
        }
        v
    } else {
        Vec::new()
    };
    candidates.into_iter().find(|p| p.is_file())
}

/// 给定 scrcpy 可执行文件位置,寻找配套 scrcpy-server:
/// 优先与可执行文件同目录(官方发行包布局),其次系统共享目录
pub fn find_server(scrcpy_path: Option<&Path>) -> Option<PathBuf> {
    if let Some(exe) = scrcpy_path {
        if let Some(dir) = exe.parent() {
            let p = dir.join("scrcpy-server");
            if p.is_file() {
                return Some(p);
            }
        }
    }
    if cfg!(target_os = "linux") {
        for s in [
            "/usr/share/scrcpy/scrcpy-server",
            "/usr/local/share/scrcpy/scrcpy-server",
        ] {
            let p = PathBuf::from(s);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// 运行 scrcpy --version 解析版本号(如 "4.1")。
/// exe 为空时视为 PATH 中的 "scrcpy"。
pub fn scrcpy_version_at(exe: &str) -> Option<String> {
    let exe = if exe.trim().is_empty() { "scrcpy" } else { exe };
    let out = Command::new(exe).arg("--version").output().ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    // 第一行: "scrcpy 4.1 <https://...>"
    stdout
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

/// 设备信息(用于日志)
pub struct DeviceInfo {
    pub serial: String,
    pub brand: String,
    pub model: String,
    pub android: String,
    pub screen: String,
    pub host_os: String,
    pub scrcpy: String,
}

/// 收集设备与环境信息(全部为只读 adb 查询)
pub fn device_info(serial: &str, scrcpy_ver: &str) -> DeviceInfo {
    let getprop = |prop: &str| {
        adb_cmd(Some(serial))
            .args(["shell", "getprop", prop])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    };
    let screen = screen_size(serial)
        .map(|(w, h)| format!("{w}x{h}"))
        .unwrap_or_else(|_| "未知".into());
    DeviceInfo {
        serial: serial.to_string(),
        brand: getprop("ro.product.brand"),
        model: getprop("ro.product.model"),
        android: getprop("ro.build.version.release"),
        screen,
        host_os: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        scrcpy: scrcpy_ver.to_string(),
    }
}

// ===================== 控制通道 =====================

/// 启动一个 control-only 的 scrcpy-server(无视频无音频,仅控制通道),
/// 返回 (server 子进程, 本地转发端口)。
pub fn start_control_server(
    serial: &str,
    server_path: &str,
    version: &str,
    scid: u32,
    port: u16,
) -> Result<ControlServer> {
    let socket_name = format!("scrcpy_{scid:08x}");
    const DEVICE_JAR: &str = "/data/local/tmp/scrcpy-server-pad.jar";

    // server 必须推送到设备侧路径(scrcpy 官方行为相同,推入 /data/local/tmp)
    // 路径直接作为参数传递,不经 shell,空格/中文路径安全
    let st = adb_cmd(Some(serial))
        .args(["push", server_path, DEVICE_JAR])
        .output()
        .context("adb push scrcpy-server 失败")?;
    if !st.status.success() {
        bail!("adb push 失败: {}", String::from_utf8_lossy(&st.stderr));
    }

    // 先建立 forward,再启动 server(server 会 listen 并等待 accept)
    let st = adb_cmd(Some(serial))
        .args([
            "forward",
            &format!("tcp:{port}"),
            &format!("localabstract:{socket_name}"),
        ])
        .output()
        .context("adb forward 失败")?;
    if !st.status.success() {
        bail!("adb forward 失败: {}", String::from_utf8_lossy(&st.stderr));
    }

    let args = format!(
        "CLASSPATH={DEVICE_JAR} app_process / com.genymobile.scrcpy.Server {version} \
         scid={scid:08x} tunnel_forward=true video=false audio=false control=true \
         send_device_meta=false send_frame_meta=false send_stream_meta=false \
         send_dummy_byte=true cleanup=true power_on=false log_level=warn"
    );
    let mut child = adb_cmd(Some(serial))
        .args(["shell", &args])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("启动 scrcpy-server 失败")?;
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                // 设备端 server 的 stderr 是我们**唯一**能看见它内部事故的地方
                // (`Exception` / 反序列化失败 / 读线程炸掉)。所以:
                //   * 疑似错误的行升级到 ERROR —— 即便用户把日志级别压到 `error`
                //     也不会被过滤掉,并记进"最近一条 server 侧错误"给界面显示;
                //   * 其余行保持 WARN(本来就是 `log_level=warn`)。
                if server_stderr_is_error(&line) {
                    note_server_error(&line);
                    crate::diag_error!("scrcpy-server", "{line}");
                } else {
                    crate::diag_warn!("scrcpy-server", "{line}");
                }
            }
        });
    }

    Ok(ControlServer {
        child,
        serial: serial.to_string(),
        port,
    })
}

pub struct ControlServer {
    child: Child,
    serial: String,
    port: u16,
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        // 退出清理必须是有界的：kill 后只轮询 try_wait，绝不无条件 wait。
        let deadline = Instant::now() + Duration::from_millis(800);
        let _ = self.child.kill();
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(_) => break,
            }
        }
        // adb forward 清理同样设置超时，避免 adb server 异常时把 GUI 退出卡住。
        if let Ok(mut child) = adb_cmd(Some(&self.serial))
            .args(["forward", "--remove", &format!("tcp:{}", self.port)])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            let deadline = Instant::now() + Duration::from_millis(600);
            while Instant::now() < deadline {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                    Err(_) => break,
                }
            }
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
            }
        }
    }
}

// ===================== 控制通道存活自检 =====================
//
// 为什么需要它(2026-10-11 用户真机现场,FPS 模式内锁死):
// 我们的 control-only server 实例死了/不再接收时,`adb forward` 这一层**照样
// 把写入吞掉** —— 本地 write 成功、`ControlClient::is_connected()` 一直为真、
// 日志零错误,而设备端一个触点都收不到。所以"我们这条通道还活着吗"必须去
// **设备侧**取证,不能只看本地 socket。
//
// 两条证据都要能区分"**没查到**"与"**查到没有**":前者(adb 抖动、权限被拒、
// 输出不可信)必须报 [`ChannelVerdict::Unknown`],绝不能当成"死" —— 误判的
// 代价是无缘无故重启一条本来好的通道。

/// 一次控制通道存活自检的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelVerdict {
    /// 设备端确认在:abstract socket 在,或(退路)进程表里见到我们的 server
    Alive,
    /// 设备端确认不在:socket 消失,或进程表里连我们的 server 都没有
    Dead,
    /// 这次**没取到可信证据** —— 不下结论,也绝不触发重连
    Unknown,
}

impl ChannelVerdict {
    /// 界面用的一字标签
    pub fn label(self) -> &'static str {
        match self {
            ChannelVerdict::Alive => "活",
            ChannelVerdict::Dead => "死",
            ChannelVerdict::Unknown => "未知",
        }
    }

    /// 这个结论允不允许走"重启通道"这一步。
    ///
    /// **只有"确认死"允许**:`Unknown`(查询没成/输出不可信)必须什么都不做 ——
    /// 这是整条自检最容易写错、后果也最重的一处(误判会把一条好通道重启掉),
    /// 所以单独做成一个带单测的纯函数,而不是散在调用点上的 `== Dead`。
    pub fn may_restart(self) -> bool {
        matches!(self, ChannelVerdict::Dead)
    }
}

/// 一次存活自检的结果:结论 + 人话证据(日志与界面直接照抄)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelProbe {
    pub verdict: ChannelVerdict,
    /// 证词:两条查询各自查到了什么,判死/判活/判未知分别靠哪一条
    pub evidence: String,
}

/// 只读问一次设备端:我们自己那个 server 的 abstract socket 还在不在。
///
/// 返回 `None` = 这次查询本身没成(`adb` 失败 / 输出不像 `/proc/net/unix`),
/// 由调用方按"未知"处理。
pub fn control_socket_liveness(serial: &str, scid: u32) -> Option<bool> {
    let out = adb_cmd(Some(serial))
        .args(["shell", "cat", "/proc/net/unix"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_unix_has_socket(
        &String::from_utf8_lossy(&out.stdout),
        &format!("scrcpy_{scid:08x}"),
    )
}

/// 退路证据:进程表里还有没有**我们**那个 server。
///
/// 返回 `None` = 查不到可信证据(adb 失败 / 输出为空 / 只见到别的 scrcpy 实例
/// —— 用户自己那条投屏通道也叫 `com.genymobile.scrcpy.Server`)。
pub fn control_server_liveness(serial: &str, scid: u32) -> Option<bool> {
    // `-o PID,NAME,ARGS` 才看得到完整启动参数(jar 名与 scid 都在里面);
    // 老 toybox 不认 `-o` 时退回朴素的 `ps -A`。
    let with_o = adb_cmd(Some(serial))
        .args(["shell", "ps", "-A", "-o", "PID,NAME,ARGS"])
        .output()
        .ok()?;
    if with_o.status.success() {
        return parse_ps_pad_server(&String::from_utf8_lossy(&with_o.stdout), scid);
    }
    let plain = adb_cmd(Some(serial))
        .args(["shell", "ps", "-A"])
        .output()
        .ok()?;
    if !plain.status.success() {
        return None;
    }
    parse_ps_pad_server(&String::from_utf8_lossy(&plain.stdout), scid)
}

/// 存活自检主入口(工作线程里调用;全部是只读 adb 查询)。
///
/// 判定顺序:先看 abstract socket(最直接:这条通道就是它),socket 查不到时
/// 用进程表兜底。socket **确认消失**即判死 —— `tunnel_forward=true` 下 server
/// 自己 listen 在这个名字上,名字没了就没有任何东西会收我们的写入。
pub fn channel_probe(serial: &str, scid: u32) -> ChannelProbe {
    let socket_name = format!("scrcpy_{scid:08x}");
    match control_socket_liveness(serial, scid) {
        Some(true) => ChannelProbe {
            verdict: ChannelVerdict::Alive,
            evidence: format!("设备端 socket {socket_name} 在"),
        },
        Some(false) => {
            // 名字没了。顺带记一句进程表的结果当旁证:进程还在而名字没了,
            // 说明坏在我们这条通道而不是整个 adb/设备。
            let ps = match control_server_liveness(serial, scid) {
                Some(true) => "ps 仍见我们的 server 进程",
                Some(false) => "ps 也见不到我们的 server 进程",
                None => "ps 未能交叉确认",
            };
            ChannelProbe {
                verdict: ChannelVerdict::Dead,
                evidence: format!("设备端 socket {socket_name} 已消失({ps})"),
            }
        }
        None => match control_server_liveness(serial, scid) {
            Some(true) => ChannelProbe {
                verdict: ChannelVerdict::Alive,
                evidence: format!(
                    "/proc/net/unix 这次没取到,但 ps 见到我们的 server 进程({socket_name})"
                ),
            },
            Some(false) => ChannelProbe {
                verdict: ChannelVerdict::Dead,
                evidence: format!(
                    "/proc/net/unix 这次没取到,ps 也不见我们的 server 进程({socket_name})"
                ),
            },
            None => ChannelProbe {
                verdict: ChannelVerdict::Unknown,
                evidence: format!(
                    "/proc/net/unix 与 ps 两条路都没取到可信结果({socket_name});按未知处理,不重连"
                ),
            },
        },
    }
}

/// 解析 `/proc/net/unix`,看有没有我们那个 abstract socket(纯函数,单测覆盖)。
///
/// 返回 `None` ⇔ **这份输出不可信**(空输出 / 没有 `Num` 表头 / 只有表头),
/// 必须是"未知"而不是"死"。真机输出长这样(小米 HyperOS,Android 15):
///
/// ```text
/// Num       RefCount Protocol Flags    Type St Inode Path
/// 0000000000000000: 00000003 00000000 00000000 0001 03 276288363 @scrcpy_1a2b3c4d
/// ```
///
/// abstract socket 在路径列带 `@` 前缀(老内核不带),两种都认。
pub(crate) fn parse_unix_has_socket(text: &str, socket_name: &str) -> Option<bool> {
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let header = lines.next()?;
    if !header.trim_start().starts_with("Num") {
        return None;
    }
    let mut rows = 0usize;
    for line in lines {
        rows += 1;
        if line
            .split_whitespace()
            .any(|f| f.trim_start_matches('@') == socket_name)
        {
            return Some(true);
        }
    }
    if rows == 0 {
        // 只有表头:这份输出不完整,不能当成"设备上一个 socket 都没有"
        return None;
    }
    Some(false)
}

/// 解析 `ps` 输出,看**我们**那个 server 进程还在不在(纯函数,单测覆盖)。
///
/// 认得出来的只有两类行:
///   * 带我们的 jar 名 `scrcpy-server-pad.jar`(真机 `ps -A -o PID,NAME,ARGS`
///     会把 `sh -c CLASSPATH=...scrcpy-server-pad.jar app_process ...` 原样列出);
///   * 带我们的 `scid=xxxxxxxx`(哪怕换个 jar 名也是我们这条通道)。
///
/// 只见得到别的 scrcpy 实例(`com.genymobile.scrcpy.Server`,用户自己的投屏)
/// 时返回 `None` —— 那是**别人的**通道还在,不能拿来证明我们这条活着。
pub(crate) fn parse_ps_pad_server(text: &str, scid: u32) -> Option<bool> {
    const PAD_JAR: &str = "scrcpy-server-pad.jar";
    let scid_tok = format!("scid={scid:08x}");
    let mut rows = 0usize;
    let mut foreign_scrcpy = false;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        rows += 1;
        if line.contains(PAD_JAR) || line.contains(&scid_tok) {
            return Some(true);
        }
        if line.contains("scrcpy") {
            foreign_scrcpy = true;
        }
    }
    if rows == 0 {
        return None;
    }
    if foreign_scrcpy {
        // 有别的 scrcpy 实例但不含我们的标识:无法证明我们在,也无法证明我们不在
        return None;
    }
    Some(false)
}

// ===================== server 侧错误的可见化 =====================

/// 最近一条设备端 server 的错误行(界面显示用)。
/// 只在进程内保存最后一条:用户要的是"现在到底炸了什么",不是一份堆栈档案。
static SERVER_LAST_ERROR: Mutex<Option<(String, Instant)>> = Mutex::new(None);

/// 一行 server 的 stderr 算不算"错误"(纯函数,单测覆盖)。
///
/// 目的很具体:设备端 server 的**读线程死了**这类事故只会以
/// `Exception` / `Caused by` / Java 堆栈的形式出现在 stderr 上,而这些行必须
/// 以 ERROR 落盘(用户在 `error` 级别下也能看见),不能被级别过滤掉。
/// 宁可多报(把一句 `failed` 提成 ERROR 只是让它在日志里更显眼),不可漏报 ——
/// 漏掉的那一行往往正是唯一的现场。
pub(crate) fn server_stderr_is_error(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    [
        "exception",
        "error",
        "fatal",
        "panic",
        "caused by",
        "stack trace",
        "\tat ",
        "at com.genymobile",
        // 原生崩溃/资源失败那一路(不是 Java 异常,但同样致命)
        "sigsegv",
        "abort",
        "crash",
        "failed",
        "cannot",
    ]
    .iter()
    .any(|key| lower.contains(key))
}

/// 记下最近一条 server 侧错误(截断到 300 字符,避免整页堆栈占满界面)。
pub fn note_server_error(line: &str) {
    let clipped: String = line.trim().chars().take(300).collect();
    if let Ok(mut slot) = SERVER_LAST_ERROR.lock() {
        *slot = Some((clipped, Instant::now()));
    }
}

/// 最近一条 server 侧错误(文本, 发生时刻)。`None` = 本次运行还没见过。
pub fn last_server_error() -> Option<(String, Instant)> {
    SERVER_LAST_ERROR.lock().ok().and_then(|slot| slot.clone())
}

fn split_args(input: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for ch in input.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            } else {
                current.push(ch);
            }
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = Some(ch);
        } else if ch.is_whitespace() {
            if !current.is_empty() {
                result.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if escaped {
        current.push('\\');
    }
    if !current.is_empty() {
        result.push(current);
    }
    result
}

/// 从 scrcpy 的一行输出里解析 `--print-fps` 的帧率 —— 投屏流的**真实**帧率。
///
/// scrcpy 的 FPS 计数器每秒打印一行,格式(见 scrcpy 源码 `run_fps_counter`):
/// `%u fps` 或 `%u fps (+%u frames skipped)`;经 scrcpy 的日志器会带上
/// `INFO: ` 前缀,再经本程序日志通道转发时又会加 `scrcpy stdout: ` 前缀,
/// 所以两种前缀都剥掉。只认"数字 + fps(+ 可选括号说明)"的干净行,避免把
/// 别的含 " fps" 的字串(参数回显之类)误吃进来。
pub(crate) fn parse_scrcpy_fps(line: &str) -> Option<f32> {
    let t = line.trim();
    let t = t.strip_prefix("scrcpy stdout: ").unwrap_or(t).trim_start();
    let t = t
        .strip_prefix("INFO: ")
        .or_else(|| t.strip_prefix("DEBUG: "))
        .unwrap_or(t);
    let (num, rest) = t.split_once(" fps")?;
    let v: f32 = num.trim().parse().ok()?;
    let rest = rest.trim();
    if !(rest.is_empty() || rest.starts_with('(')) {
        return None;
    }
    // 0 是计数器停摆的占位;"每小时几十万帧"不可能是真实读数,当解析失败
    (v > 0.0 && v <= 1000.0).then_some(v)
}

/// 启动常规 scrcpy 窗口。子进程 stdout/stderr 会逐行写入 diagnostics.log，
/// 同时监控退出码，避免 GUI 只显示“已启动”而实际参数错误导致进程秒退。
///
/// 返回值里的 `u32` 是 scrcpy **子进程的 pid**(用户 2026-10-11 第二个问题 S①):
/// 「鼠标独占」的"仅投屏窗口在前台"档要靠它判定 —— 前台窗口的 pid 等于它,
/// 就说明用户此刻正在投屏窗口里操作(判据见 `capture::foreground_pid` 与
/// `keymap::mouse_shield_on`)。以前这里只返回 stdout 通道,pid 直接丢在
/// 内部那个等待线程里,界面无从比较。
pub fn launch_scrcpy(exe: &str, serial: &str, extra_args: &str) -> Result<(Receiver<String>, u32)> {
    let exe = if exe.trim().is_empty() { "scrcpy" } else { exe };
    let mut cmd = Command::new(exe);
    if !serial.is_empty() {
        cmd.args(["-s", serial]);
    }
    let args = split_args(extra_args);
    for a in &args {
        cmd.arg(a);
    }
    let command_line = format!(
        "{} -s {}{}",
        exe,
        serial,
        if args.is_empty() {
            String::new()
        } else {
            format!(" {}", args.join(" "))
        }
    );
    crate::diag_info!("scrcpy", "启动: {command_line}");
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("启动 scrcpy 失败: {exe}"))?;
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    if let Some(stdout) = child.stdout.take() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                // --print-fps 的行一秒一条:它只服务于界面的"投屏帧率"显示,
                // 记 info 级会把诊断日志刷爆 —— 降为 debug,需要时开级别看。
                if parse_scrcpy_fps(&line).is_some() {
                    crate::diag_debug!("scrcpy", "stdout: {line}");
                } else {
                    crate::diag_info!("scrcpy", "stdout: {line}");
                }
                let _ = tx.send(format!("scrcpy stdout: {line}"));
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                crate::diag_warn!("scrcpy", "stderr: {line}");
                let _ = tx.send(format!("scrcpy stderr: {line}"));
            }
        });
    }
    std::thread::spawn(move || {
        let message = match child.wait() {
            Ok(status) if status.success() => {
                crate::diag_info!("scrcpy", "进程 {pid} 正常退出: {status}");
                format!("scrcpy 已退出（正常，{status}）")
            }
            Ok(status) => {
                crate::diag_warn!("scrcpy", "进程 {pid} 异常退出: {status}");
                format!(
                    "scrcpy 异常退出（{status}）。请查看 diagnostics.log 中 scrcpy 的 stderr 输出"
                )
            }
            Err(error) => {
                crate::diag_warn!("scrcpy", "进程 {pid} 等待失败: {error}");
                format!("无法获取 scrcpy 退出状态: {error}")
            }
        };
        let _ = tx.send(message);
    });
    Ok((rx, pid))
}

/// 让设备重新走一遍 audio policy,把音频转发"叫醒"。
///
/// 现象:不少机器(小米/HyperOS 等)在 scrcpy 刚连上时,音频转发拿到的是静音,
/// 用户在手机上随便按一下音量键,声音立刻就有了 —— 因为音量变化会让系统重新
/// 配置音频输出,捕获流这才真正接通。这里用一次"加一档 + 减一档"把同样的动作
/// 自动做掉:净音量不变,但能触发同一条 audio policy 更新路径。
///
/// 用的是 `input keyevent`,和用户手动按键走的是同一条系统按键分发路径
/// (小米需要打开"USB 调试(安全设置)"才能注入,与触摸注入的权限要求一致)。
pub fn nudge_audio(serial: &str) -> Result<()> {
    // 24 = KEYCODE_VOLUME_UP,25 = KEYCODE_VOLUME_DOWN
    for code in ["24", "25"] {
        let out = adb_cmd(Some(serial))
            .args(["shell", "input", "keyevent", code])
            .output()
            .context("执行 adb shell input keyevent 失败")?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            bail!("音量键注入被拒绝: {}", err.trim());
        }
    }
    Ok(())
}

// ===================== 设备端输入取证探针(全程只读) =====================
//
// 为什么需要(2026-10-12 用户真机,第 6 条 bug「手碰一下手机屏 → 注入输入全失效,
// FPS 视角/开火/移动都不灵,按一下『唤醒视角』立刻夺回」):
// 卡死的那几秒我们**一直在发**合法坐标的 MOVE 与重叠换手 DOWN/UP,`out` 记录在
// 写线程里(命令真的进了 socket)、诊断日志零 WARN/ERROR,而设备端就是没反应;
// 只有把触点全部抬到 0(唤醒键)再重建手势才解得开。
// ⇒「设备端现在到底有几个触点、我们的注入被谁拦了」必须去**设备侧**取证。
//
// 这一节的查询**全部只读**:`dumpsys input` / `getevent`。绝不注入任何输入
// (`input tap` 那类东西会真的动玩家的游戏),也绝不 kill 用户进程 —— 取证的东西
// 一旦会改变现场,就不再是取证了。

/// 只读探针一次查询的等待上限:超时就 kill 子进程并报「没取到」。
/// GUI/自愈路径绝不能被一条卡住的 adb 查询挂死。
const READ_QUERY_TIMEOUT: Duration = Duration::from_secs(4);
/// 只读查询的输出上限:对端异常时也不让内存无界增长
const MAX_READ_BYTES: u64 = 8 * 1024 * 1024;
/// 收尾 kill 之后的等待上限(与 `ControlServer::drop` 同口径,绝不无条件 wait)
const CHILD_KILL_TIMEOUT: Duration = Duration::from_millis(800);

/// 设备端输入状态快照(一次 `dumpsys input` 的解析结果)
///
/// 每个「取不到」都用 `None` 表示:**未知 ≠ 否**。消费方必须把 `None` 当成
/// 「这一轮没有证据」而不是 0 —— 一次 adb 抖动被读成「账本 0 个触点」,
/// 足以骗出一次毫无必要的手势重建。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeviceInputState {
    /// `mInjectEventStatus`(MIUI 私有,AOSP 没有这个字段;读不到为 0)
    pub inject_status: i64,
    /// `mIsFilterInterceptMode`(MIUI 私有;缺字段/值不认识一律 false)
    pub filter_intercept: bool,
    /// `mSynergyMode`
    pub synergy: bool,
    /// `mOnewayMode`
    pub oneway: bool,
    /// `mPadMode`
    pub pad_mode: bool,
    /// InputDispatcher `TouchStates:` 行的**原文**(取证用,原样保留;找不到为空串)
    pub touch_line: String,
    /// 账本里当前按下的触点数;`None` = 没解析出来(不是 0)
    pub touch_pointers: Option<u32>,
    /// `Last Raw Touch: pointerCount=N`
    pub raw_pointers: Option<u32>,
    /// `Last Cooked Touch: pointerCount=N`
    pub cooked_pointers: Option<u32>,
}

/// 纯函数:解析 `dumpsys input` 全文(单测用 fixture,不碰设备)。
///
/// 容错口径:字段缺失 / 格式不认识 ⇒ 默认值或 `None`,**绝不 panic**。
/// `TouchStates` 各版 ROM 没有稳定打印契约(真机空闲时实测是
/// `TouchStates: <no displays touched>`,**有触点时的确切样式没见过**),
/// 所以只认明确认得出的形态,认不出就是 `None`。
pub fn parse_device_input_state(dump: &str) -> DeviceInputState {
    let (touch_line, touch_pointers) = parse_touch_states(dump);
    DeviceInputState {
        inject_status: number_after_key(dump, "mInjectEventStatus").unwrap_or(0),
        filter_intercept: bool_after_key(dump, "mIsFilterInterceptMode").unwrap_or(false),
        synergy: bool_after_key(dump, "mSynergyMode").unwrap_or(false),
        oneway: bool_after_key(dump, "mOnewayMode").unwrap_or(false),
        pad_mode: bool_after_key(dump, "mPadMode").unwrap_or(false),
        touch_line,
        touch_pointers,
        raw_pointers: parse_last_touch_pointers(dump, "Last Raw Touch:"),
        cooked_pointers: parse_last_touch_pointers(dump, "Last Cooked Touch:"),
    }
}

/// 跑一次 `dumpsys input`(只读,超时 [`READ_QUERY_TIMEOUT`]),
/// 命令失败 / 没拿到任何输出 ⇒ `None`。
pub fn device_input_state(serial: &str) -> Option<DeviceInputState> {
    let dump = adb_read_within(serial, &["shell", "dumpsys", "input"], READ_QUERY_TIMEOUT)?;
    if dump.trim().is_empty() {
        // 命令"成功"却一个字都没有:这不是一份设备端快照,宁可报"没取到"
        return None;
    }
    Some(parse_device_input_state(&dump))
}

/// 用 `getevent -pl` 找出触摸屏的 event 节点(只读)。
///
/// 判据:**该节点声明了 `ABS_MT_TRACKING_ID`**(没有它就没有"手指按下/抬起"这回事)。
/// 多个节点都满足时,名字像触摸屏的优先(touch / ts / focaltech / goodix / synaptics)。
/// 查询失败或没有这样的节点 ⇒ `None`。
pub fn touch_screen_path(serial: &str) -> Option<String> {
    let text = adb_read_within(serial, &["shell", "getevent", "-pl"], READ_QUERY_TIMEOUT)?;
    pick_touch_screen(&parse_getevent_devices(&text))
}

/// 「手摸/离开手机屏」的一次边沿(物理触摸,**不是**我们的注入)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhysicalTouch {
    /// 设备端触点从 0 变成 >0:有手指按下去了
    Down,
    /// 设备端触点从 >0 变成 0:手全抬了
    Up,
}

/// 只读监听「手摸手机屏」:起一个线程跑
/// `adb -s <serial> shell getevent -lt <触摸屏节点>`,把 0→>0 / >0→0 的边沿发给界面。
///
/// 调用方 drop 接收端 ⇒ 下一次 `send` 失败 ⇒ 线程收工并 kill 子进程。
/// **本函数不注入、不改变设备上任何东西。**
///
/// 已知边界(如实记录):std 的 `mpsc::Sender` 没有「接收端还在吗」这种查询接口,
/// 所以"接收端被 drop"只能在下一次要发边沿时才发现。若调用方 drop 接收端之后
/// 再没人碰屏幕,这个线程会一直停在读 `getevent` 上 —— 直到下一次触摸、adb 断开
/// 或进程退出。这是冻结签名带来的限制,不是漏写。
pub fn watch_physical_touch(serial: String) -> std::sync::mpsc::Receiver<PhysicalTouch> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        // 节点查找放在线程里:调用方拿到 Receiver 不该被一次 adb 查询卡住
        let Some(path) = touch_screen_path(&serial) else {
            return; // 没有触摸屏 / 查询失败:没人可听,安静收工
        };
        run_getevent_watch(&serial, &path, tx);
    });
    rx
}

/// 「手机被手触摸」监听子进程的 pid 表:退出前必须显式收掉。
///
/// 为什么需要:Windows **不会**在父进程退出时杀子进程 ⇒ 我们那条
/// `adb.exe … shell getevent -lt …` 会活下来,设备端的 `getevent` 也就一直挂在
/// 同一个触摸屏节点上(真机实测:同一节点叠了 3 个残留)。
static TOUCH_WATCH_PIDS: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// 进程退出前收掉全部触摸监听子进程(幂等;收不掉时静默,绝不阻塞退出)。
///
/// 子进程一死,adbd 那边的 `getevent` 也随连接断开而结束 —— 这是"零残留"的一环。
pub fn stop_physical_touch_watches() {
    let pids: Vec<u32> = match TOUCH_WATCH_PIDS.lock() {
        Ok(mut v) => v.drain(..).collect(),
        Err(_) => return,
    };
    for pid in pids {
        #[cfg(windows)]
        {
            let _ = Command::new("taskkill")
                .args(["/F", "/PID", &pid.to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        #[cfg(not(windows))]
        {
            let _ = Command::new("kill")
                .arg(pid.to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

/// getevent 读线程主体:读一行 → 算边沿 → 发一个,直到 EOF 或接收端消失。
fn run_getevent_watch(serial: &str, path: &str, tx: mpsc::Sender<PhysicalTouch>) {
    let Ok(mut child) = adb_cmd(Some(serial))
        .args(["shell", "getevent", "-lt", path])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    let pid = child.id();
    if let Ok(mut v) = TOUCH_WATCH_PIDS.lock() {
        v.push(pid);
    }
    let Some(stdout) = child.stdout.take() else {
        kill_child_bounded(&mut child, CHILD_KILL_TIMEOUT);
        if let Ok(mut v) = TOUCH_WATCH_PIDS.lock() {
            v.retain(|p| *p != pid);
        }
        return;
    };
    let mut tracker = TouchEdgeTracker::default();
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        let Some(edge) = tracker.feed_line(&line) else {
            continue;
        };
        if tx.send(edge).is_err() {
            break; // 接收端被 drop:立刻收工
        }
    }
    // 收尾必须有界(与 ControlServer::drop 同口径):kill + 轮询 try_wait,
    // 绝不无条件 wait —— 否则 adb 异常时线程会永远挂着。
    kill_child_bounded(&mut child, CHILD_KILL_TIMEOUT);
    if let Ok(mut v) = TOUCH_WATCH_PIDS.lock() {
        v.retain(|p| *p != pid);
    }
}

/// 有界地收掉一个子进程:kill 后只轮询 `try_wait`,到点就走。
fn kill_child_bounded(child: &mut Child, timeout: Duration) {
    let _ = child.kill();
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => return,
        }
    }
}

/// 有上限地跑一条**只读** adb 查询,返回 stdout;超时/失败/被杀 ⇒ `None`。
///
/// `Command::output()` 本身没有任何超时:adb server 一卡就把调用线程永久挂住。
/// 这条路径上跑的是巡检/取证,绝不能被一条查询卡死,所以:
/// spawn → 读线程抽干 stdout(防止管道写满互锁)→ 主线程按 deadline 轮询
/// `try_wait` → 超时 kill。进程退出与 stdout 读干**各自的等待都有上限**。
fn adb_read_within(serial: &str, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = adb_cmd(Some(serial))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.take(MAX_READ_BYTES).read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => break None,
        }
    };
    let status = status?;
    let buf = rx.recv_timeout(Duration::from_millis(500)).ok()?;
    if !status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&buf).to_string())
}

/// 从 `dumpsys input` 里取 InputDispatcher 的 `TouchStates:` 账本行:
/// 返回(该行原文, 账本触点数)。原文供日志/界面取证;计数认不出时是 `None`。
fn parse_touch_states(text: &str) -> (String, Option<u32>) {
    let mut raw = String::new();
    let mut pointers = None;
    for line in text.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("TouchStates:") else {
            continue;
        };
        raw = trimmed.to_string();
        let rest = rest.trim();
        if rest.contains("no displays touched") {
            // 实测真机空闲原文(小米 HyperOS):
            // `TouchStates: <no displays touched>` —— 账本里一个显示器都没被碰
            pointers = Some(0);
        } else if let Some(n) = count_pointers_in_states(rest) {
            pointers = Some(n);
        }
        // 认不出的形态:保留上一次的结果,绝不编一个 0 出来
    }
    (raw, pointers)
}

/// 从一段文本里数触点:认 `pointerCount=N` / `pointers=N` / `pointer=N`
/// (大小写不敏感,`=`/`:` 都行);多处出现则相加(多显示器分别打印时就是各显示器相加)。
/// 一个都认不出 ⇒ `None`(未知,绝不是 0)。
///
/// 这是**容错**口径:各 ROM 打印 `TouchStates` / `Last Raw Touch` 的样式并不统一,
/// 真机"有触点"时的确切格式我们没见过,所以只认这些明确形态。
fn count_pointers_in_states(text: &str) -> Option<u32> {
    let lower = text.to_ascii_lowercase();
    let mut total: u32 = 0;
    let mut found = false;
    for key in ["pointercount", "pointers", "pointer"] {
        let mut from = 0usize;
        while let Some(pos) = lower[from..].find(key) {
            let at = from + pos;
            let after = lower[at + key.len()..].trim_start_matches(['=', ':', ' ', '\t']);
            let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(n) = digits.parse::<u32>() {
                total = total.saturating_add(n);
                found = true;
            }
            from = at + key.len();
        }
    }
    found.then_some(total)
}

/// 取某一行里 `key` 后面的第一个整数(`mInjectEventStatus=1` → `1`)。
/// 没有这一行 / 取不出数字 ⇒ `None`(未知)。
fn number_after_key(text: &str, key: &str) -> Option<i64> {
    let line = text.lines().find(|l| l.contains(key))?;
    let rest = line.split_once(key)?.1;
    let start = rest.find(|c: char| c.is_ascii_digit() || c == '-')?;
    let tok: String = rest[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    tok.parse().ok()
}

/// 把 MIUI 的输入开关解析成 bool:`1`/`true`/`on` 为真,`0`/`false`/`off` 为假,
/// 字段缺失或值不认识 ⇒ `None`(由调用方按"未知"落地;`DeviceInputState` 里
/// 这几位只有 bool 一种表示,所以最终是 false —— 但**只有明确写成 1/true 才算真**)。
fn bool_after_key(text: &str, key: &str) -> Option<bool> {
    let line = text.lines().find(|l| l.contains(key))?;
    let rest = line.split_once(key)?.1.trim_start();
    let rest = rest.trim_start_matches(|c: char| c == '=' || c == ':' || c.is_whitespace());
    let tok: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    match tok.to_ascii_lowercase().as_str() {
        "1" | "true" | "on" | "yes" => Some(true),
        "0" | "false" | "off" | "no" => Some(false),
        _ => None,
    }
}

/// 取 `Last Raw Touch:` / `Last Cooked Touch:` 行里的 `pointerCount=N`。
/// 行不在或认不出 ⇒ `None`(未知 ≠ 0)。
fn parse_last_touch_pointers(text: &str, key: &str) -> Option<u32> {
    let line = text.lines().find(|l| l.contains(key))?;
    count_pointers_in_states(line.split_once(key)?.1)
}

/// 一行 `getevent -lt` 里我们关心的东西(其余一律忽略)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TouchEvent {
    /// `ABS_MT_SLOT`:协议 B 下"当前操作哪个槽"
    Slot(u32),
    /// `ABS_MT_TRACKING_ID`:`>= 0` = 这根手指按下(占用当前槽);`-1`(打印成
    /// `ffffffff`)= 这个槽抬起
    TrackingId(i64),
}

/// 从一行 `getevent -lt` 输出解析触摸事件(纯函数,单测覆盖)。
///
/// 真机行形如:
///
/// ```text
/// [   12345.678901] /dev/input/event5: EV_ABS       ABS_MT_SLOT          00000001
/// [   12345.678901] /dev/input/event5: EV_ABS       ABS_MT_TRACKING_ID   0000002a
/// [   12345.678901] /dev/input/event5: EV_ABS       ABS_MT_TRACKING_ID   ffffffff
/// ```
///
/// 只认 `ABS_MT_TRACKING_ID`(以及为了辨清"哪根手指抬起"而顺带记的
/// `ABS_MT_SLOT`),`ABS_MT_POSITION_*` 之类一律返回 `None`。
/// 老版本 getevent 不打标签时事件码是十六进制(`0039` = tracking id,
/// `002f` = slot,`0003` = EV_ABS),也认。
fn parse_getevent_touch_line(line: &str) -> Option<TouchEvent> {
    // 剥掉时间戳与设备路径:`... /dev/input/event5: EV_ABS ...`
    let rest = match line.find("/dev/input/") {
        Some(pos) => line[pos..].split_once(':')?.1,
        None => line,
    };
    // **必须再剥掉 `[  12345.678901]` 时间戳前缀**(2026-10-12 定位的根因)。
    //
    // getevent 有两种输出形态,取决于给它**几个**设备:
    // ① 不带设备参数(监听全部设备):`[ ts] /dev/input/event5: EV_ABS  ABS_MT_TRACKING_ID  2a`
    // ② **带一个设备参数**(本程序就是这么跑的):`[ ts] EV_ABS  ABS_MT_TRACKING_ID  2a`
    //    —— 单设备时 getevent **不打设备名**。
    //
    // 旧实现只认①:形态②下 `find("/dev/input/")` 落空,整行(以 `[` 开头)被当成事件体,
    // 于是第一个 token 是 `[`,**每一行都判为"不是我们要的事件"被丢掉** ⇒
    // "手离开手机"这条自愈触发在真机上**一条边沿都收不到**(子进程正常、节点正确、
    // 手指也确实在屏上,就是没有边沿)。单测 `parses_single_device_output_without_prefix`
    // 用真机原文钉住这一条。
    let rest = rest.trim_start();
    let rest = match rest.strip_prefix('[') {
        Some(after) => match after.split_once(']') {
            Some((_, tail)) => tail,
            None => rest,
        },
        None => rest,
    };
    let mut it = rest.split_whitespace();
    let ev_type = it.next()?;
    let code = it.next()?;
    let raw = it.next()?;
    if !(ev_type == "EV_ABS" || code_is_hex(ev_type, 0x03)) {
        return None;
    }
    let is_slot = code == "ABS_MT_SLOT" || code_is_hex(code, 0x2f);
    let is_tracking = code == "ABS_MT_TRACKING_ID" || code_is_hex(code, 0x39);
    if !is_slot && !is_tracking {
        return None;
    }
    let value = parse_getevent_value(raw)?;
    if is_slot {
        // 槽号只可能是小的非负数;畸形值(比如又一个 ffffffff)当解析失败
        u32::try_from(value).ok().map(TouchEvent::Slot)
    } else {
        Some(TouchEvent::TrackingId(value))
    }
}

/// 老 getevent 不打 `-l` 标签时事件码是十六进制(`0039`);带 `0x` 前缀也认。
fn code_is_hex(code: &str, want: u32) -> bool {
    let t = code.trim_end_matches(':');
    let t = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    u32::from_str_radix(t, 16)
        .map(|v| v == want)
        .unwrap_or(false)
}

/// getevent 打印的数值是**十六进制**(`0000002a`);`ffffffff` / 全 `f` 都表示 -1(抬指)。
fn parse_getevent_value(tok: &str) -> Option<i64> {
    let t = tok.trim();
    if let Some(neg) = t.strip_prefix('-') {
        return neg.parse::<i64>().ok().map(|v| -v);
    }
    let t = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    let v = u64::from_str_radix(t, 16).ok()?;
    if v == 0xffff_ffff || v == u64::MAX {
        Some(-1)
    } else {
        i64::try_from(v).ok()
    }
}

/// 把 getevent 行流折成「0→>0 / >0→0 边沿」的状态机(纯逻辑,单测覆盖)。
///
/// 为什么要记槽号:`ABS_MT_TRACKING_ID = ffffffff` 只说"**某个槽**抬起了",
/// 不说是哪一个。多指交错抬按时若把它当成"手全抬了",就会在还有手指按着屏时
/// 报 `Up` —— 而 `Up` 正是自愈的触发条件,误报会去动用户正在玩的手势。
/// 用 `ABS_MT_SLOT` 把 tracking id 映射到槽,只有**活跃集合真的空了**才报 `Up`。
///
/// 没有 `ABS_MT_SLOT` 的设备(协议 A)只能把每一次 `-1` 都归到默认槽 0,多指时
/// 第一根手指抬起就可能被报成 `Up`(早报)。带 `ABS_MT_TRACKING_ID` 的基本都是
/// 协议 B(有槽),这里不为此再加猜测逻辑;但这条歧义必须如实记下来。
#[derive(Debug, Default)]
struct TouchEdgeTracker {
    /// 当前槽(协议 B,默认槽 0)
    slot: u32,
    /// 槽号 -> 该槽当前的 tracking id(有值 = 这根手指还按着)
    active: std::collections::BTreeMap<u32, i64>,
    /// 上一次喂完之后的"有没有手指按着"
    was_down: bool,
}

impl TouchEdgeTracker {
    /// 喂一行 getevent 输出,返回这一行造成的边沿(没有边沿返回 `None`)
    fn feed_line(&mut self, line: &str) -> Option<PhysicalTouch> {
        match parse_getevent_touch_line(line)? {
            TouchEvent::Slot(s) => self.slot = s,
            TouchEvent::TrackingId(id) => {
                if id < 0 {
                    self.active.remove(&self.slot);
                } else {
                    self.active.insert(self.slot, id);
                }
                let down = !self.active.is_empty();
                if down != self.was_down {
                    self.was_down = down;
                    return Some(if down {
                        PhysicalTouch::Down
                    } else {
                        PhysicalTouch::Up
                    });
                }
            }
        }
        None
    }
}

/// `getevent -pl` 里识别出来的一个输入设备节点
#[derive(Debug, Clone, PartialEq, Eq)]
struct InputDeviceNode {
    path: String,
    name: String,
    /// 这个节点声明了 `ABS_MT_TRACKING_ID` 吗(触摸屏的硬判据)
    has_tracking_id: bool,
}

/// 解析 `getevent -pl`(纯函数,单测覆盖)。
///
/// 真机输出形如:
///
/// ```text
/// add device 2: /dev/input/event5
///   name: "focaltech_ts"
///   events:
///     ABS (0003): ABS_MT_SLOT            : value 0, min 0, max 9, fuzz 0, flat 0, resolution 0
///                 ABS_MT_TRACKING_ID     : value 0, min 0, max 65535, fuzz 0, flat 0, resolution 0
///     KEY (0001): KEY_POWER
///   input props:
///     INPUT_PROP_DIRECT
/// ```
///
/// 老版本不打标签时 `ABS_MT_TRACKING_ID` 打印成十六进制码 `0039`,而且**只在
/// ABS 段里才认它** —— `KEY (0001): 0039` 是 KEY_SPACE,不是触摸屏。
fn parse_getevent_devices(text: &str) -> Vec<InputDeviceNode> {
    let mut out: Vec<InputDeviceNode> = Vec::new();
    let mut in_abs = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(path) = device_path_in(trimmed) {
            out.push(InputDeviceNode {
                path,
                name: String::new(),
                has_tracking_id: false,
            });
            in_abs = false;
            continue;
        }
        let Some(cur) = out.last_mut() else {
            continue; // 第一个 `add device` 之前的东西不是任何节点的属性
        };
        if let Some(name) = field_value(trimmed, "name:") {
            cur.name = name;
        }
        if trimmed.contains("ABS (") {
            in_abs = true;
        } else if [
            "KEY (",
            "REL (",
            "SW (",
            "MSC (",
            "LED (",
            "FF (",
            "input props",
        ]
        .iter()
        .any(|k| trimmed.contains(k))
        {
            in_abs = false;
        }
        if in_abs && abs_section_has_tracking_id(trimmed) {
            cur.has_tracking_id = true;
        }
    }
    out
}

/// 这一行(已确认落在 ABS 段里)是不是在讲 `ABS_MT_TRACKING_ID`:
/// 打标签时是名字;不打标签时是十六进制码 `0039`。
fn abs_section_has_tracking_id(line: &str) -> bool {
    if line.contains("ABS_MT_TRACKING_ID") {
        return true;
    }
    let toks: Vec<&str> = line.split_whitespace().collect();
    let has_values = line.contains(": value");
    for (i, tok) in toks.iter().enumerate() {
        if !code_is_hex(tok, 0x39) {
            continue;
        }
        // 逐个列出的形态里,码后面紧跟 `:`;这时才敢把 0039 当事件码 ——
        // 否则 `min 0, max 39` 里的十进制 39 会被误认成触摸屏。
        // 紧凑形态(一串码、没有 value)没有这个歧义,直接认。
        let followed_by_colon =
            tok.ends_with(':') || toks.get(i + 1).is_some_and(|n| n.starts_with(':'));
        if followed_by_colon || !has_values {
            return true;
        }
    }
    false
}

/// 从 `add device 1: /dev/input/event5` 这类行里取设备节点路径(没有则 `None`)
fn device_path_in(line: &str) -> Option<String> {
    line.split_whitespace()
        .find(|t| t.starts_with("/dev/input/"))
        .map(|t| t.trim_end_matches(':').to_string())
}

/// 取 `name: "focaltech_ts"` 里的值(去掉引号与空白)
fn field_value(line: &str, key: &str) -> Option<String> {
    let rest = line.strip_prefix(key)?.trim();
    let v = rest.trim_matches('"').trim();
    (!v.is_empty()).then(|| v.to_string())
}

/// 从 `getevent -pl` 解析出的节点里挑触摸屏:先要有 `ABS_MT_TRACKING_ID`,
/// 名字像触摸屏的优先,否则取第一个带它的节点。挑不出 ⇒ `None`。
fn pick_touch_screen(devices: &[InputDeviceNode]) -> Option<String> {
    let with_tracking = || devices.iter().filter(|d| d.has_tracking_id);
    with_tracking()
        .find(|d| name_looks_like_touch(&d.name))
        .or_else(|| with_tracking().next())
        .map(|d| d.path.clone())
}

/// 节点名字像不像触摸屏(只在多个候选之间排序用,不用来判定触摸能力)
fn name_looks_like_touch(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["touch", "focaltech", "goodix", "synaptics", "ts"]
        .iter()
        .any(|k| n.contains(k))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 便携 scrcpy 目录的两种摆放都应能被发现:
    /// 1) 与程序目录同级;2) 位于程序目录内部的 scrcpy* 子目录(允许嵌套)
    #[test]
    fn find_scrcpy_portable_in_sibling_and_nested_dirs() {
        let root = std::env::temp_dir().join(format!("scrcpy-pad-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let prog = root.join("prog");
        let sibling = root.join("scrcpy-linux-x86_64-vX");
        std::fs::create_dir_all(&prog).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        let sibling_exe = sibling.join(scrcpy_exe_name());
        std::fs::write(&sibling_exe, b"x").unwrap();
        assert_eq!(find_scrcpy_portable_under(&prog), Some(sibling_exe));

        // 同级目录被移除后,应回退到程序目录内部的嵌套 scrcpy 目录
        std::fs::remove_dir_all(&sibling).unwrap();
        let nested = prog.join("scrcpy-win64").join("bin");
        std::fs::create_dir_all(&nested).unwrap();
        let nested_exe = nested.join(scrcpy_exe_name());
        std::fs::write(&nested_exe, b"x").unwrap();
        assert_eq!(find_scrcpy_portable_under(&prog), Some(nested_exe));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 非 scrcpy 命名的目录不应被误认
    #[test]
    fn ignore_dirs_not_starting_with_scrcpy() {
        let root = std::env::temp_dir().join(format!("scrcpy-pad-test-neg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let prog = root.join("prog");
        let other = root.join("some-tool");
        std::fs::create_dir_all(&prog).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join(scrcpy_exe_name()), b"x").unwrap();
        assert_eq!(find_scrcpy_portable_under(&prog), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 用户给出的位置可能是**目录**而不是 exe —— 必须能解析出目录里的可执行文件。
    ///
    /// 回归:用户原话"我设置好 scrcpy 目录后,再次启动,scrcpy 依旧没有被找到",
    /// 而旧逻辑只接受 `is_file()`,目录会被当作死路径直接丢掉。
    #[test]
    fn explicit_path_accepts_a_directory() {
        let root = std::env::temp_dir().join(format!("scrcpy-pad-test-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // 官方发行包布局:exe/server/adb 同目录
        let pkg = root.join("scrcpy-win64-v3.3.3");
        std::fs::create_dir_all(&pkg).unwrap();
        let exe = pkg.join(scrcpy_exe_name());
        std::fs::write(&exe, b"x").unwrap();
        std::fs::write(pkg.join("scrcpy-server"), b"x").unwrap();
        std::fs::write(pkg.join(adb_exe_name()), b"x").unwrap();

        // 1) 给目录 -> 解析出目录里的 exe
        assert_eq!(
            find_scrcpy_explicit(&pkg.display().to_string()),
            Some(exe.clone())
        );
        // 2) 给 exe 本身 -> 原样返回
        assert_eq!(
            find_scrcpy_explicit(&exe.display().to_string()),
            Some(exe.clone())
        );
        // 3) 目录里再多套一层也能找到(解压时多一层文件夹是常事)
        let outer = root.join("解压出来的文件夹");
        let nested = outer.join("再套一层").join("scrcpy-win64-v9.9");
        std::fs::create_dir_all(&nested).unwrap();
        let nested_exe = nested.join(scrcpy_exe_name());
        std::fs::write(&nested_exe, b"x").unwrap();
        assert_eq!(find_scrcpy_in_dir(&outer, 3), Some(nested_exe));
        // 空目录里不能凭空造一个出来
        let empty = root.join("空目录");
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(find_scrcpy_in_dir(&empty, 3), None);
        // 4) 空字符串 / 不存在的位置一律 None(交给自动寻找)
        assert_eq!(find_scrcpy_explicit("   "), None);
        assert_eq!(
            find_scrcpy_explicit(&root.join("nope").display().to_string()),
            None
        );

        // 5) 同目录推导:server 与 adb 都能从 scrcpy 的位置补齐
        assert_eq!(
            find_server(Some(&exe)).map(|p| p.file_name().unwrap().to_owned()),
            Some("scrcpy-server".into())
        );
        assert_eq!(
            find_adb(Some(&exe)).map(|p| p.file_name().unwrap().to_owned()),
            Some(adb_exe_name().into())
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// `find_scrcpy_under`:记住的目录可能是发行包本身,也可能是"解压到某个
    /// 文件夹"的那一层;它只往名字像 scrcpy 的子目录里下探,因此可以安全地
    /// 拿大目录(桌面/下载)当基准 —— 这是"scrcpy 换版本后仍能找回"的关键。
    #[test]
    fn find_scrcpy_under_handles_wrapped_and_versioned_dirs() {
        let root = std::env::temp_dir().join(format!("scrcpy-pad-under-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // 基准目录 -> 任意名字的文件夹 -> scrcpy 发行包
        let pkg = root.join("一个文件夹").join("scrcpy-win64-v3.4");
        std::fs::create_dir_all(&pkg).unwrap();
        let exe = pkg.join(scrcpy_exe_name());
        std::fs::write(&exe, b"x").unwrap();

        // 直接给发行包目录
        assert_eq!(find_scrcpy_under(&pkg, 3), Some(exe.clone()));
        // 给外面那层(名字完全不像 scrcpy)
        assert_eq!(
            find_scrcpy_under(&root.join("一个文件夹"), 3),
            Some(exe.clone())
        );
        // 给最外层大目录
        assert_eq!(find_scrcpy_under(&root, 3), Some(exe.clone()));
        // 非 scrcpy 命名的目录里即便有同名 exe 也不能被认成 scrcpy 包
        let other = root.join("别的东西");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join(scrcpy_exe_name()), b"x").unwrap();
        assert_eq!(
            find_scrcpy_under(&other, 3),
            Some(other.join(scrcpy_exe_name())),
            "基准目录本身就是包目录时应当认它(与 find_scrcpy_in_dir 一致)"
        );
        // 空目录 -> None
        let empty = root.join("空的");
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(find_scrcpy_under(&empty, 3), None);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 横屏设备上 wm size 是自然方向(1280x2772),真实坐标空间应从 cur= 取到 2772x1280
    #[test]
    fn scrcpy_argument_splitter_respects_quotes_and_spaces() {
        assert_eq!(
            split_args("--stay-awake --window-title \"Game Window\" --max-size=1920"),
            vec![
                "--stay-awake",
                "--window-title",
                "Game Window",
                "--max-size=1920"
            ]
        );
        assert_eq!(split_args("a\\ b \"c d\""), vec!["a b", "c d"]);
    }

    #[test]
    fn parses_refresh_rate_from_latency_and_display_text() {
        assert!((parse_refresh_rate_latency("16666666\n").unwrap() - 60.0).abs() < 0.01);
        assert!(
            (parse_refresh_rate_text("mDisplayInfo refreshRate=120.0 fps=120.0").unwrap() - 120.0)
                .abs()
                < 0.01
        );
    }

    #[test]
    fn parse_display_size_prefers_current_orientation() {
        let out = "  Display: mDisplayId=0\n\
             init=2772x1280 1dpi mMinSizeOfResizeableTaskDp=200 cur=2772x1280 app=2772x1280 rng=1280x1280-2772x2772\n\
             init=1280x2772 520dpi mMinSizeOfResizeableTaskDp=200 cur=2772x1280 app=2772x1280 rng=1280x1280-2772x2772\n";
        assert_eq!(parse_display_size(out), Some((2772, 1280)));
        // 解析不到时回退 None(由调用方退回 wm size)
        assert_eq!(parse_display_size("no size here"), None);
    }

    /// 遥测解析:焦点窗口/前台 Activity 取 `key=` 后面的整行值。
    /// `null` 是**真实取值**(当前没有焦点窗口),不能被当成"没查到" —— 它恰恰是
    /// "注入被系统丢掉"的一条线索。
    #[test]
    fn parses_focus_and_focused_app_from_window_dump() {
        let out = "Display: mDisplayId=0\n\
             mCurrentFocus=Window{8b0f3a1 u0 com.tencent.tmgp.dfm/com.epicgames.ue4.GameActivity}\n\
             mFocusedApp=ActivityRecord{1a2b3c u0 com.tencent.tmgp.dfm/.MainActivity t42}\n";
        assert_eq!(
            parse_window_field(out, "mCurrentFocus=").as_deref(),
            Some("Window{8b0f3a1 u0 com.tencent.tmgp.dfm/com.epicgames.ue4.GameActivity}")
        );
        assert_eq!(
            parse_window_field(out, "mFocusedApp=").as_deref(),
            Some("ActivityRecord{1a2b3c u0 com.tencent.tmgp.dfm/.MainActivity t42}")
        );
        // 真的没有焦点窗口:原样保留
        assert_eq!(
            parse_window_field("  mCurrentFocus=null\n", "mCurrentFocus=").as_deref(),
            Some("null")
        );
        // 字段不存在 / 值为空 -> None(不是空串)
        assert_eq!(parse_window_field("什么都没有", "mCurrentFocus="), None);
        assert_eq!(
            parse_window_field("  mCurrentFocus=   \n", "mCurrentFocus="),
            None
        );
    }

    /// 屏幕亮灭:只认 ON/OFF;DOZE/UNKNOWN 之类一律"未知"(不许把不确定说成灭屏)。
    #[test]
    fn parses_screen_power_state_only_when_unambiguous() {
        assert_eq!(
            parse_screen_on("  Display 0:\n    mScreenState=ON\n"),
            Some(true)
        );
        assert_eq!(
            parse_screen_on("  Display 0:\n    mScreenState=OFF\n"),
            Some(false)
        );
        // 新老版本关键字都认
        assert_eq!(parse_screen_on(" mGlobalDisplayState=ON\n"), Some(true));
        // 不确定的一律未知
        assert_eq!(parse_screen_on(" mScreenState=DOZE\n"), None);
        assert_eq!(parse_screen_on(" mScreenState=UNKNOWN\n"), None);
        assert_eq!(parse_screen_on(" mScreenState=ON_SUSPEND\n"), None);
        assert_eq!(parse_screen_on(" 什么都没有"), None);
    }

    /// `--print-fps` 的实况行解析(2026-10-06):只认 "N fps" /
    /// "N fps (+M frames skipped)" 两种形态,输入可能带着日志前缀;
    /// 非帧率行与畸形数字绝不能误判成帧率。
    #[test]
    fn parses_scrcpy_print_fps_lines() {
        assert_eq!(parse_scrcpy_fps("INFO: 60 fps"), Some(60.0));
        assert_eq!(
            parse_scrcpy_fps("INFO: 59 fps (+2 frames skipped)"),
            Some(59.0)
        );
        assert_eq!(parse_scrcpy_fps("scrcpy stdout: INFO: 61 fps"), Some(61.0));
        assert_eq!(parse_scrcpy_fps("29.5 fps"), Some(29.5));
        // 非帧率行与畸形数字
        assert_eq!(parse_scrcpy_fps("INFO: Texture: 2560x1184"), None);
        assert_eq!(parse_scrcpy_fps("INFO: Device: QOGQKBXOMFOVC6XS"), None);
        assert_eq!(parse_scrcpy_fps("INFO: abc fps"), None);
        assert_eq!(parse_scrcpy_fps("INFO: 60 fpsx"), None);
        assert_eq!(parse_scrcpy_fps("INFO: 0 fps"), None);
        assert_eq!(parse_scrcpy_fps("INFO: -1 fps"), None);
        assert_eq!(parse_scrcpy_fps("INFO: 5000 fps"), None);
    }

    /// fixture 直接照抄用户真机(小米 HyperOS / Android 15,2026-10-11 只读查询)
    /// 的 `/proc/net/unix`:abstract socket 的路径列带 `@` 前缀。
    /// 这里必须钉死两条判定:"查到没有" ⇒ 死;"这份输出不可信" ⇒ None(未知)。
    #[test]
    fn unix_socket_fixture_tells_dead_apart_from_unknown() {
        const HEADER: &str = "Num       RefCount Protocol Flags    Type St Inode Path\n";
        let with_us = format!(
            "{HEADER}\
             0000000000000000: 00000003 00000000 00000000 0005 03 417527 /dev/socket/lmkd\n\
             0000000000000000: 00000003 00000000 00000000 0001 03 276288363 @scrcpy_1a2b3c4d\n\
             0000000000000000: 00000003 00000000 00000000 0001 03 276294233 @scrcpy_57804caf\n"
        );
        assert_eq!(
            parse_unix_has_socket(&with_us, "scrcpy_1a2b3c4d"),
            Some(true)
        );

        // 我们的名字不在,但有别人的 scrcpy 通道:仍然是我们这条死了
        let foreign = format!(
            "{HEADER}\
             0000000000000000: 00000003 00000000 00000000 0001 03 276294233 @scrcpy_57804caf\n"
        );
        assert_eq!(
            parse_unix_has_socket(&foreign, "scrcpy_1a2b3c4d"),
            Some(false)
        );

        // 老内核不带 `@`:同样要认出来
        let no_at = format!(
            "{HEADER}0000000000000000: 00000003 00000000 00000000 0001 03 276288363 scrcpy_1a2b3c4d\n"
        );
        assert_eq!(parse_unix_has_socket(&no_at, "scrcpy_1a2b3c4d"), Some(true));

        // ---- 以下全是"这份输出不可信" ⇒ 未知,绝不当死 ----
        assert_eq!(parse_unix_has_socket("", "scrcpy_1a2b3c4d"), None);
        // 只有表头:输出不完整
        assert_eq!(parse_unix_has_socket(HEADER, "scrcpy_1a2b3c4d"), None);
        // 命令本身报错时可能出现在 stdout 上(不是一张 socket 表)
        assert_eq!(
            parse_unix_has_socket(
                "cat: /proc/net/unix: Permission denied\n",
                "scrcpy_1a2b3c4d"
            ),
            None
        );
    }

    /// 退路证据的 fixture 同样照抄真机 `ps -A -o PID,NAME,ARGS`:
    /// 我们这条通道(scrcpy-server-pad.jar + scid=1a2b3c4d)与用户自己的投屏
    /// 通道(scrcpy-server.jar + scid=57804caf)同时在跑。
    #[test]
    fn ps_fixture_only_counts_our_own_server() {
        const PAD_LINE: &str = "28039 sh                          sh -c CLASSPATH=/data/local/tmp/scrcpy-server-pad.jar app_process / com.genymobile.scrcpy.Server 3.3.3 scid=1a2b3c4d tunnel_forward=true video=false audio=false control=true\n\
                                28041 app_process                 app_process / com.genymobile.scrcpy.Server 3.3.3 scid=1a2b3c4d tunnel_forward=true video=false audio=false control=true\n";
        const MIRROR_LINE: &str = "28080 sh                          sh -c CLASSPATH=/data/local/tmp/scrcpy-server.jar app_process / com.genymobile.scrcpy.Server 3.3.3 scid=57804caf log_level=info max_size=2560 max_fps=144 stay_awake=true\n\
                                   28082 app_process                 app_process / com.genymobile.scrcpy.Server 3.3.3 scid=57804caf log_level=info max_size=2560 max_fps=144 stay_awake=true\n";

        let both = format!(
            "  PID NAME                        ARGS                       \n{PAD_LINE}{MIRROR_LINE}"
        );
        assert_eq!(parse_ps_pad_server(&both, 0x1a2b3c4d), Some(true));

        // 只有用户那条投屏通道:证明不了我们这条还活着 ⇒ 未知(不是"死",更不是"活")
        let mirror_only =
            format!("  PID NAME                        ARGS                       \n{MIRROR_LINE}");
        assert_eq!(parse_ps_pad_server(&mirror_only, 0x1a2b3c4d), None);

        // 表里没有任何 scrcpy:确认不在
        let none = "  PID NAME                        ARGS                       \n    1 init                        init second_stage\n 2310 com.miui.home               com.miui.home\n";
        assert_eq!(parse_ps_pad_server(none, 0x1a2b3c4d), Some(false));

        // 空输出/命令失败:未知
        assert_eq!(parse_ps_pad_server("", 0x1a2b3c4d), None);
    }

    /// server 侧异常行必须以 ERROR 级别落盘(用户把日志压到 `error` 也要看得见),
    /// 并记进"最近一条 server 侧错误"。"Unknown server option" 那种 WARN 不算错误。
    #[test]
    fn server_stderr_error_lines_are_recognized() {
        assert!(server_stderr_is_error(
            "[server] ERROR: Could not listen on socket"
        ));
        assert!(server_stderr_is_error(
            "java.io.IOException: Broken pipe\n\tat com.genymobile.scrcpy.Server.main(Server.java:200)"
        ));
        assert!(server_stderr_is_error(
            "Caused by: java.lang.NullPointerException: Attempt to invoke..."
        ));
        assert!(server_stderr_is_error("FATAL EXCEPTION: main"));
        assert!(server_stderr_is_error(
            "\tat com.genymobile.scrcpy.ControlMessageReader.run"
        ));
        // 原生崩溃那一路(不是 Java 异常,但同样让 server 停摆)
        assert!(server_stderr_is_error(
            "Fatal signal 11 (SIGSEGV), code 1, fault addr 0x0"
        ));
        assert!(server_stderr_is_error("libc: Fatal signal 6 (SIGABRT)"));
        assert!(server_stderr_is_error(
            "[server] ERROR: Failed to listen on socket"
        ));
        assert!(!server_stderr_is_error(
            "[server] WARN: Unknown server option: send_stream_meta"
        ));
        assert!(!server_stderr_is_error("INFO: Device: QOGQKBXOMFOVC6XS"));
    }

    // ---- 设备端输入取证探针(全部 fixture 解析,不需要真机) ----

    /// ① 真机(小米/Redmi MIUI)只读 dump 的形态:MIUI 私有输入开关 + 空闲账本。
    /// 空闲时 InputDispatcher 打印 `TouchStates: <no displays touched>`(实测原文)。
    #[test]
    fn parses_miui_input_dump_with_private_switches() {
        const DUMP: &str = "\
Input Manager State:
  mInjectEventStatus=1
  mIsFilterInterceptMode=1
  mSynergyMode=0
  mOnewayMode=1
  mPadMode=1
Input Dispatcher State:
  DispatchEnabled: 1
  TouchStates: <no displays touched>
  Last Raw Touch: pointerCount=0
  Last Cooked Touch: pointerCount=0
";
        let st = parse_device_input_state(DUMP);
        assert_eq!(st.inject_status, 1);
        assert!(st.filter_intercept);
        assert!(!st.synergy);
        assert!(st.oneway);
        assert!(st.pad_mode);
        assert_eq!(st.touch_line, "TouchStates: <no displays touched>");
        assert_eq!(st.touch_pointers, Some(0));
        assert_eq!(st.raw_pointers, Some(0));
        assert_eq!(st.cooked_pointers, Some(0));
    }

    /// ①b 真机(Redmi / Android 15 / MIUI)**实测原文**片段(2026-10-12,只读抓取)。
    ///
    /// 为什么必须留这一条:①里的写法是"等号 + 猜的缩进",而真机实测是**冒号**
    /// (`mInjectEventStatus: 0`),而且同一份 dump 里 `mIsFilterInterceptMode` /
    /// `mSynergyMode` 会各出现**两次** —— `InputCommonConfig` 里是 `1`/`0`,
    /// 后面 `Mi InputManagerService` 里是 `true`/`false`;解析必须取**第一处**,
    /// 且两种写法都认。真机这份还顺手钉住:`TouchStates` 行带缩进、会出现两次、
    /// `Last Raw/Cooked Touch` 是 `pointerCount=0`。
    #[test]
    fn parses_real_xiaomi_dump_verbatim() {
        const REAL: &str = "\
InputCommonConfig State
    mTouchWakeupMode: 0
    mSynergyMode: 0
    mInjectEventStatus: 0
    mRecordEventStatus: 0
    mPadMode: 0
    mOnewayMode: 1
    mCtsMode: 0
    mIsFilterInterceptMode: 1
      Last Raw Touch: pointerCount=0
      Last Cooked Touch: pointerCount=0
  TouchStates: <no displays touched>
  TouchStates: <no displays touched>
  mSynergyMode: false
  mIsFilterInterceptMode: true
";
        let st = parse_device_input_state(REAL);
        assert_eq!(st.inject_status, 0, "真机是 mInjectEventStatus: 0(冒号)");
        assert!(st.filter_intercept, "真机 mIsFilterInterceptMode: 1");
        assert!(!st.synergy, "真机 mSynergyMode: 0");
        assert!(st.oneway, "真机 mOnewayMode: 1");
        assert!(!st.pad_mode, "真机 mPadMode: 0");
        assert_eq!(st.touch_line, "TouchStates: <no displays touched>");
        assert_eq!(st.touch_pointers, Some(0));
        assert_eq!(st.raw_pointers, Some(0));
        assert_eq!(st.cooked_pointers, Some(0));
    }

    /// ② 没有 TouchStates、也没有 MIUI 字段(纯 AOSP)⇒ 全是默认值,
    /// 触点数一律 `None`(**未知 ≠ 0**);值认不出的开关不能算"开"。
    #[test]
    fn missing_input_dump_fields_are_unknown_not_zero() {
        const AOSP: &str = "\
Input Manager State:
  DispatchEnabled: 1
Input Dispatcher State:
  DispatchEnabled: 1
  FocusedWindow: Window{8b0f3a1 u0 com.tencent.tmgp.dfm/com.epicgames.ue4.GameActivity}
";
        let st = parse_device_input_state(AOSP);
        assert_eq!(st, DeviceInputState::default());
        assert_eq!(st.touch_pointers, None, "没有 TouchStates 行 ⇒ 未知,不是 0");
        assert_eq!(st.raw_pointers, None);
        assert_eq!(st.cooked_pointers, None);
        assert_eq!(st.touch_line, "");

        // 字段在、但值不认识:不能算"开",也不能 panic
        let weird = "mIsFilterInterceptMode=?\nmPadMode=maybe\nmInjectEventStatus=unknown\n";
        let st = parse_device_input_state(weird);
        assert!(!st.filter_intercept);
        assert!(!st.pad_mode);
        assert_eq!(st.inject_status, 0);
        // 空 dump 也不能 panic
        assert_eq!(parse_device_input_state(""), DeviceInputState::default());
    }

    /// ③ 账本"有触点"的形态:真机上没见过确切样式,这里按 AOSP/MIUI 常见写法造
    /// fixture,并把容错口径钉死 —— 认得出就给数字,**认不出必须是 `None` 而不是 0**。
    #[test]
    fn touch_states_with_pointers_stays_tolerant() {
        assert_eq!(
            parse_touch_states("  TouchStates: displayId=0 pointerCount=1\n"),
            (
                "TouchStates: displayId=0 pointerCount=1".to_string(),
                Some(1)
            )
        );
        // 多显示器分别打印 ⇒ 相加
        assert_eq!(
            parse_touch_states(
                "TouchStates: displayId=0 pointerCount=2, displayId=1 pointerCount=1\n"
            )
            .1,
            Some(3)
        );
        // 换几种写法也要认
        assert_eq!(
            parse_touch_states("TouchStates: displayId=0, pointers: 2\n").1,
            Some(2)
        );
        assert_eq!(parse_touch_states("TouchStates: pointer=0\n").1, Some(0));

        // 认不出的形态:未知(绝不能编成 0)
        assert_eq!(
            parse_touch_states("TouchStates: displayId=0 pointers=[unknown]\n").1,
            None
        );
        assert_eq!(parse_touch_states("TouchStates:\n").1, None);
        // 原文原样保留(取证用)
        assert_eq!(
            parse_touch_states("  TouchStates: displayId=0 pointers=[unknown]\n").0,
            "TouchStates: displayId=0 pointers=[unknown]"
        );
        // Last Raw/Cooked Touch 同样容错
        let st = parse_device_input_state(
            "TouchStates: displayId=0 pointerCount=2\nLast Raw Touch: pointerCount=2\nLast Cooked Touch: <none>\n",
        );
        assert_eq!(st.touch_pointers, Some(2));
        assert_eq!(st.raw_pointers, Some(2));
        assert_eq!(st.cooked_pointers, None, "认不出 ⇒ 未知,不是 0");
    }

    /// ④ `getevent -lt` 行解析 + tracking id 边沿(多指、交错抬按)。
    #[test]
    fn parses_getevent_tracking_id_lines_and_edges() {
        const SLOT0: &str =
            "[   12345.678901] /dev/input/event5: EV_ABS       ABS_MT_SLOT          00000000";
        const SLOT1: &str =
            "[   12345.678901] /dev/input/event5: EV_ABS       ABS_MT_SLOT          00000001";
        const ID_2A: &str =
            "[   12345.678901] /dev/input/event5: EV_ABS       ABS_MT_TRACKING_ID   0000002a";
        const ID_2B: &str =
            "[   12345.678901] /dev/input/event5: EV_ABS       ABS_MT_TRACKING_ID   0000002b";
        const ID_UP: &str =
            "[   12345.678901] /dev/input/event5: EV_ABS       ABS_MT_TRACKING_ID   ffffffff";
        const POS_X: &str =
            "[   12345.678901] /dev/input/event5: EV_ABS       ABS_MT_POSITION_X    00000abc";

        // 行解析:只认 tracking id(与槽号),位置事件不能来捣乱
        assert_eq!(
            parse_getevent_touch_line(ID_2A),
            Some(TouchEvent::TrackingId(0x2a))
        );
        assert_eq!(
            parse_getevent_touch_line(ID_UP),
            Some(TouchEvent::TrackingId(-1)),
            "ffffffff = 抬起"
        );
        assert_eq!(parse_getevent_touch_line(SLOT1), Some(TouchEvent::Slot(1)));
        assert_eq!(parse_getevent_touch_line(POS_X), None);
        assert_eq!(
            parse_getevent_touch_line(
                "[   12345.678901] /dev/input/event5: EV_SYN       SYN_REPORT           00000000"
            ),
            None
        );
        // 老版本不打标签:十六进制事件码也要认
        assert_eq!(
            parse_getevent_touch_line("[ 1.0] /dev/input/event5: EV_ABS 0039 0000002a"),
            Some(TouchEvent::TrackingId(0x2a))
        );
        assert_eq!(
            parse_getevent_touch_line("[ 1.0] /dev/input/event5: 0003 002f 00000001"),
            Some(TouchEvent::Slot(1))
        );
        // 畸形/半截行不能 panic
        assert_eq!(parse_getevent_touch_line(""), None);
        assert_eq!(parse_getevent_touch_line("EV_ABS"), None);
        assert_eq!(parse_getevent_touch_line("[ 1.0] /dev/input/event5:"), None);
        assert_eq!(
            parse_getevent_touch_line("[ 1.0] /dev/input/event5: EV_ABS ABS_MT_TRACKING_ID zzzz"),
            None
        );

        // 边沿:0 → >0 报 Down;多指交错抬按**不许**误报 Up;>0 → 0 才报 Up
        let mut t = TouchEdgeTracker::default();
        assert_eq!(t.feed_line(SLOT0), None);
        assert_eq!(t.feed_line(ID_2A), Some(PhysicalTouch::Down));
        assert_eq!(t.feed_line(POS_X), None, "位置事件不产生边沿");
        assert_eq!(t.feed_line(ID_2A), None, "同一根手指重复报:没有边沿");
        assert_eq!(t.feed_line(SLOT1), None);
        assert_eq!(
            t.feed_line(ID_2B),
            None,
            "第二根手指按下:仍然 >0,不重复报 Down"
        );
        // 槽 0 抬起(交错):还有一根按着 ⇒ 绝不能报"手全抬了"
        assert_eq!(t.feed_line(SLOT0), None);
        assert_eq!(t.feed_line(ID_UP), None, "只有一根手指抬起,手指还在屏上");
        // 槽 1 也抬起 ⇒ 活跃集合空 ⇒ Up(这才是"手离开手机")
        assert_eq!(t.feed_line(SLOT1), None);
        assert_eq!(t.feed_line(ID_UP), Some(PhysicalTouch::Up));
        // 已经空了再来抬:不重复报
        assert_eq!(t.feed_line(ID_UP), None);
        // 重新按下:再报 Down
        assert_eq!(t.feed_line(ID_2A), Some(PhysicalTouch::Down));
    }

    /// ④b **真机原文**:`getevent -lt <单个设备路径>` 的输出**没有设备名前缀**
    /// (2026-10-12 在 Redmi / Android 15 上抓的两条流同一次触摸对比)。
    ///
    /// 为什么必须留这条:本程序给 getevent 传了设备路径(`getevent -lt /dev/input/event5`),
    /// 而**单个**设备时它不打 `/dev/input/event5:` 这一截 —— 旧解析器找不到它就整行
    /// 当事件体,第一个 token 变成 `[`,于是**每一行都被丢掉** ⇒ 真机上"手离开手机"
    /// 这条自愈触发**一条边沿都收不到**(子进程活着、节点正确、手指确实在屏上)。
    #[test]
    fn parses_single_device_output_without_prefix() {
        // 真机原文(单设备形态,没有 `/dev/input/...:`)
        const TOUCH_DOWN: &str =
            "[ 1024366.965761] EV_KEY       BTN_TOUCH            DOWN                ";
        const ID: &str = "[ 1024366.965761] EV_ABS       ABS_MT_TRACKING_ID   000434d7            ";
        const ID_UP: &str =
            "[ 1024366.965761] EV_ABS       ABS_MT_TRACKING_ID   ffffffff            ";
        const TOUCH_UP: &str =
            "[ 1024366.965761] EV_KEY       BTN_TOUCH            UP                  ";
        assert_eq!(
            parse_getevent_touch_line(ID),
            Some(TouchEvent::TrackingId(0x434d7)),
            "单设备形态(无设备名前缀)必须能解析"
        );
        assert_eq!(
            parse_getevent_touch_line(ID_UP),
            Some(TouchEvent::TrackingId(-1))
        );
        assert_eq!(parse_getevent_touch_line(TOUCH_DOWN), None);
        assert_eq!(parse_getevent_touch_line(TOUCH_UP), None);

        // 端到端:一次完整的按下→抬起必须算出 Down 与 Up
        let mut t = TouchEdgeTracker::default();
        assert_eq!(
            t.feed_line(ID),
            Some(PhysicalTouch::Down),
            "单设备形态也必须认得出按下"
        );
        assert_eq!(
            t.feed_line(ID_UP),
            Some(PhysicalTouch::Up),
            "单设备形态也必须认得出抬起(手离开手机)"
        );

        // 带设备前缀的形态(监听全部设备时)不能被这次改动破坏
        assert_eq!(
            parse_getevent_touch_line(
                "[   12345.678901] /dev/input/event5: EV_ABS       ABS_MT_TRACKING_ID   0000002a"
            ),
            Some(TouchEvent::TrackingId(0x2a))
        );
        // 老 getevent(无 `-l` 标签、十六进制事件码):两种形态都要认
        assert_eq!(
            parse_getevent_touch_line("[ 1024366.965761] 0003 0039 000434d7"),
            Some(TouchEvent::TrackingId(0x434d7))
        );
        assert_eq!(
            parse_getevent_touch_line("[ 1024366.965761] /dev/input/event5: 0003 0039 000434d7"),
            Some(TouchEvent::TrackingId(0x434d7))
        );
    }

    /// ⑤ `getevent -pl` fixture:只有带 `ABS_MT_TRACKING_ID` 的节点才是触摸屏,
    /// 名字像触摸屏的优先;`KEY (0001): 0039`(KEY_SPACE)绝不能被当成触摸屏。
    #[test]
    fn finds_touch_screen_node_from_getevent_pl() {
        const PL: &str = r#"add device 1: /dev/input/event0
  name: "gpio-keys"
  events:
    KEY (0001): KEY_VOLUMEUP           KEY_VOLUMEDOWN
  input props:
    <none>
add device 2: /dev/input/event5
  name: "focaltech_ts"
  events:
    ABS (0003): ABS_MT_SLOT            : value 0, min 0, max 9, fuzz 0, flat 0, resolution 0
                ABS_MT_TOUCH_MAJOR     : value 0, min 0, max 255, fuzz 0, flat 0, resolution 0
                ABS_MT_POSITION_X      : value 0, min 0, max 4095, fuzz 0, flat 0, resolution 0
                ABS_MT_TRACKING_ID     : value 0, min 0, max 65535, fuzz 0, flat 0, resolution 0
    KEY (0001): KEY_POWER
  input props:
    INPUT_PROP_DIRECT
add device 3: /dev/input/event2
  name: "vendor_sensor"
  events:
    ABS (0003): ABS_MT_POSITION_X      : value 0, min 0, max 4095, fuzz 0, flat 0, resolution 0
    KEY (0001): 0039
  input props:
    <none>
"#;
        let devices = parse_getevent_devices(PL);
        assert_eq!(devices.len(), 3);
        assert_eq!(devices[0].name, "gpio-keys");
        assert!(!devices[0].has_tracking_id);
        assert_eq!(devices[1].name, "focaltech_ts");
        assert_eq!(devices[1].path, "/dev/input/event5");
        assert!(devices[1].has_tracking_id);
        assert_eq!(devices[2].name, "vendor_sensor");
        assert!(
            !devices[2].has_tracking_id,
            "只有 ABS_MT_POSITION_X 的节点不是触摸屏;KEY 段里的 0039 更不算"
        );
        assert_eq!(
            pick_touch_screen(&devices).as_deref(),
            Some("/dev/input/event5")
        );

        // 都不打标签(只有十六进制码)时靠 ABS 段里的 0039 认出来;
        // 多个候选时名字像触摸屏的优先。
        const PL_HEX: &str = r#"add device 1: /dev/input/event4
  name: "vendor_panel"
  events:
    ABS (0003): 0039  : value 0, min 0, max 65535, fuzz 0, flat 0, resolution 0
  input props:
    <none>
add device 2: /dev/input/event3
  name: "goodix_ts"
  events:
    ABS (0003): 002f  : value 0, min 0, max 9, fuzz 0, flat 0, resolution 0
                0039  : value 0, min 0, max 65535, fuzz 0, flat 0, resolution 0
  input props:
    INPUT_PROP_DIRECT
"#;
        let devices = parse_getevent_devices(PL_HEX);
        assert!(devices[0].has_tracking_id && devices[1].has_tracking_id);
        assert_eq!(
            pick_touch_screen(&devices).as_deref(),
            Some("/dev/input/event3"),
            "都带 tracking id 时,名字像触摸屏的优先"
        );

        // 紧凑形态(一串码、没有 `: value`)也认;min/max 里的十进制 39 不能误判
        let compact = "add device 1: /dev/input/event7\n  name: \"panel\"\n  events:\n    ABS (0003): 002f 0039\n";
        assert!(parse_getevent_devices(compact)[0].has_tracking_id);
        let decoy = "add device 1: /dev/input/event8\n  name: \"panel\"\n  events:\n    ABS (0003): 0030  : value 0, min 0, max 39, fuzz 0, flat 0, resolution 0\n";
        assert!(
            !parse_getevent_devices(decoy)[0].has_tracking_id,
            "`max 39` 是十进制取值,不是事件码 0039"
        );

        // 认不出任何节点 ⇒ None(交给调用方按"没有触摸屏"处理,不 panic)
        assert_eq!(parse_getevent_devices(""), Vec::new());
        assert_eq!(pick_touch_screen(&[]), None);
        assert_eq!(
            pick_touch_screen(&parse_getevent_devices("什么都没有\n")),
            None
        );
    }

    /// **接线层全链回归**(2026-10-12,协议 §6.3 必测缝③):
    /// 子进程 stdout 字节流 → 逐行读取 → 解析器 → 边沿事件,整条链必须通。
    ///
    /// 为什么必须有这一条:`parses_single_device_output_without_prefix` 只覆盖
    /// "一行文本 → 一个事件"这段**纯函数**;而真实故障恰恰发生在**缝**上 ——
    /// 生产给 getevent 传了单个设备路径(输出无设备名前缀),旧解析器在缝里把
    /// 每一行都丢掉,于是自愈触发链**整条死亡**却没有任何单测报警。
    ///
    /// 这里用**内存字节流**喂真机原文,走的是与生产完全相同的读取路径:
    /// `BufReader::lines()` → `TouchEdgeTracker::feed_line`。
    #[test]
    fn getevent_pipe_to_edge_is_wired_end_to_end() {
        use std::io::{BufRead, BufReader};

        // 真机原文(单设备形态:`getevent -lt /dev/input/event5`),含一次完整点按
        const STREAM: &str = "\
[ 1024366.965761] EV_KEY       BTN_TOUCH            DOWN
[ 1024366.965761] EV_ABS       ABS_MT_TRACKING_ID   000434d7
[ 1024366.965761] EV_ABS       ABS_MT_POSITION_X    0000d780
[ 1024366.965761] EV_ABS       ABS_MT_POSITION_Y    00021b22
[ 1024366.965761] EV_SYN       SYN_REPORT           00000000
[ 1024366.965761] EV_ABS       ABS_MT_POSITION_X    0000d781
[ 1024366.965761] EV_SYN       SYN_REPORT           00000000
[ 1024366.965761] EV_ABS       ABS_MT_TRACKING_ID   ffffffff
[ 1024366.965761] EV_KEY       BTN_TOUCH            UP
";
        let mut tracker = TouchEdgeTracker::default();
        let mut edges: Vec<PhysicalTouch> = Vec::new();
        for line in BufReader::new(STREAM.as_bytes())
            .lines()
            .map_while(Result::ok)
        {
            if let Some(edge) = tracker.feed_line(&line) {
                edges.push(edge);
            }
        }
        assert_eq!(
            edges,
            vec![PhysicalTouch::Down, PhysicalTouch::Up],
            "整条链必须产出『按下 → 抬起』两个边沿(这就是『手离开手机』的自愈触发源)"
        );
    }

    /// 停止监听必须**幂等**且不 panic(零残留纪律:`finish()` 会无条件调它)。
    ///
    /// 这里只验证"没有在途监听时调用是安全的";真正的 kill 语义需要真进程,
    /// 由真机只读验证覆盖(退出后 `ps -A | grep getevent` 为空)。
    #[test]
    fn stopping_touch_watches_is_idempotent() {
        stop_physical_touch_watches();
        stop_physical_touch_watches();
        assert!(
            TOUCH_WATCH_PIDS
                .lock()
                .map(|v| v.is_empty())
                .unwrap_or(true),
            "无在途监听时 pid 表必须为空"
        );
    }
}
