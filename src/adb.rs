use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

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
    let exe_name = scrcpy_exe_name();
    // 候选扫描根:程序目录本身;以及程序目录的父目录(即"同级"位置)
    let mut roots = vec![base.to_path_buf()];
    if let Some(parent) = base.parent() {
        roots.push(parent.to_path_buf());
    }
    for root in roots {
        let Ok(rd) = std::fs::read_dir(&root) else {
            continue;
        };
        let mut dirs: Vec<PathBuf> = rd
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| e.path())
            .filter(|p| is_scrcpy_dir_name(p))
            .collect();
        dirs.sort();
        for dir in dirs {
            if let Some(exe) = find_file_recursive(&dir, exe_name, 3) {
                return Some(exe);
            }
        }
    }
    None
}

/// 目录名是否以 "scrcpy" 开头(不区分大小写)
fn is_scrcpy_dir_name(p: &Path) -> bool {
    p.file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase().starts_with("scrcpy"))
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
    if cfg!(windows) { "scrcpy.exe" } else { "scrcpy" }
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
            v.push(
                PathBuf::from(&home).join(r"AppData\Local\Android\Sdk\platform-tools\adb.exe"),
            );
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
        for s in ["/usr/share/scrcpy/scrcpy-server", "/usr/local/share/scrcpy/scrcpy-server"] {
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
        .args(["forward", &format!("tcp:{port}"), &format!("localabstract:{socket_name}")])
        .output()
        .context("adb forward 失败")?;
    if !st.status.success() {
        bail!(
            "adb forward 失败: {}",
            String::from_utf8_lossy(&st.stderr)
        );
    }

    let args = format!(
        "CLASSPATH={DEVICE_JAR} app_process / com.genymobile.scrcpy.Server {version} \
         scid={scid:08x} tunnel_forward=true video=false audio=false control=true \
         send_device_meta=false send_frame_meta=false send_stream_meta=false \
         send_dummy_byte=true cleanup=false power_on=false log_level=warn"
    );
    let child = adb_cmd(Some(serial))
        .args(["shell", &args])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("启动 scrcpy-server 失败")?;

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
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = adb_cmd(Some(&self.serial))
            .args(["forward", "--remove", &format!("tcp:{}", self.port)])
            .output();
    }
}

/// 启动常规 scrcpy 窗口(独立子进程,关闭本程序时不强杀,由用户自行关闭窗口)
pub fn launch_scrcpy(exe: &str, serial: &str, extra_args: &str) -> Result<Child> {
    let exe = if exe.trim().is_empty() { "scrcpy" } else { exe };
    let mut cmd = Command::new(exe);
    if !serial.is_empty() {
        cmd.args(["-s", serial]);
    }
    for a in extra_args.split_whitespace() {
        cmd.arg(a);
    }
    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("启动 scrcpy 失败")
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

    /// 横屏设备上 wm size 是自然方向(1280x2772),真实坐标空间应从 cur= 取到 2772x1280
    #[test]
    fn parse_display_size_prefers_current_orientation() {
        let out = "  Display: mDisplayId=0\n\
             init=2772x1280 1dpi mMinSizeOfResizeableTaskDp=200 cur=2772x1280 app=2772x1280 rng=1280x1280-2772x2772\n\
             init=1280x2772 520dpi mMinSizeOfResizeableTaskDp=200 cur=2772x1280 app=2772x1280 rng=1280x1280-2772x2772\n";
        assert_eq!(parse_display_size(out), Some((2772, 1280)));
        // 解析不到时回退 None(由调用方退回 wm size)
        assert_eq!(parse_display_size("no size here"), None);
    }
}
