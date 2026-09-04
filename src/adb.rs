use anyhow::{Context, Result, bail};
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

/// 解析本机 scrcpy 版本号 (如 "4.1")
pub fn scrcpy_version() -> Option<String> {
    let out = Command::new("scrcpy").arg("--version").output().ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    // 第一行: "scrcpy 4.1 <https://...>"
    stdout
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)
        .map(|s| s.to_string())
}

/// 各平台默认的 scrcpy-server 位置
pub fn default_server_path() -> &'static str {
    if cfg!(windows) {
        // Windows: scrcpy 官方发行包里 server 与 scrcpy.exe 同目录
        "scrcpy-server"
    } else {
        "/usr/share/scrcpy/scrcpy-server"
    }
}

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
pub fn launch_scrcpy(serial: &str, extra_args: &str) -> Result<Child> {
    let mut cmd = Command::new("scrcpy");
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
