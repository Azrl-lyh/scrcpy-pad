use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

fn adb_cmd(serial: Option<&str>) -> Command {
    let mut c = Command::new("adb");
    if let Some(s) = serial {
        if !s.is_empty() {
            c.args(["-s", s]);
        }
    }
    c
}

/// 列出已连接设备序列号
pub fn list_devices() -> Vec<String> {
    let out = Command::new("adb").arg("devices").output();
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

/// 自动寻找 scrcpy 可执行文件:PATH 优先,其次各平台常见安装位置
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

/// 各平台默认的 scrcpy-server 位置(静态回退,优先用 find_server 自动发现)
pub fn default_server_path() -> &'static str {
    if cfg!(windows) {
        "scrcpy-server"
    } else {
        "/usr/share/scrcpy/scrcpy-server"
    }
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
