mod adb;
mod app;
mod capture;
mod control;
mod engine;
mod keymap;

fn main() -> eframe::Result<()> {
    if std::env::args().any(|a| a == "--selftest") {
        selftest();
        return Ok(());
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([900.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native(
        "scrcpy-pad 游戏控制台",
        options,
        Box::new(|cc| Ok(Box::new(app::PadApp::new(cc)))),
    )
}

/// 无界面自检:验证键盘捕获权限 / adb / 控制通道 / 协议注入(仅发无害 hover)
fn selftest() {
    let mut failed = false;
    let mut check = |name: &str, ok: bool, detail: &str| {
        println!("[{}] {name} {detail}", if ok { "PASS" } else { "FAIL" });
        if !ok {
            failed = true;
        }
    };

    // 1. 键盘捕获权限
    #[cfg(target_os = "linux")]
    {
        let evdev_ok = std::fs::read_dir("/dev/input")
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.file_name().to_string_lossy().starts_with("event"))
                    .any(|e| std::fs::File::open(e.path()).is_ok())
            })
            .unwrap_or(false);
        check(
            "evdev 读取权限",
            evdev_ok,
            if evdev_ok { "" } else { "→ sudo usermod -aG input $USER 后重新登录" },
        );
    }
    #[cfg(windows)]
    check("键盘捕获(rdev)", true, "(Windows 无需特殊权限)");

    // 2. adb 设备
    let devices = adb::list_devices();
    check("adb 设备在线", !devices.is_empty(), &format!("({} 台)", devices.len()));
    if devices.is_empty() {
        std::process::exit(1);
    }
    let serial = devices[0].clone();

    // 3. 分辨率
    let size = adb::screen_size(&serial);
    check("读取分辨率", size.is_ok(), &format!("{size:?}"));
    let Ok((w, h)) = size else { std::process::exit(1) };

    // 4. scrcpy-server 控制通道
    let version = adb::scrcpy_version().unwrap_or_else(|| "4.1".into());
    let server = adb::start_control_server(
        &serial,
        adb::default_server_path(),
        &version,
        0x1a2b3c4d,
        28383,
    );
    let Ok(server) = server else {
        check("启动 control server", false, &format!("{:?}", server.err()));
        std::process::exit(1);
    };
    check("启动 control server", true, "");

    let mut client = None;
    for _ in 0..20 {
        match control::ControlClient::connect(28383, w, h) {
            Ok(c) => {
                client = Some(c);
                break;
            }
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(150)),
        }
    }
    check("TCP 连接控制通道", client.is_some(), "");
    let Some(client) = client else { std::process::exit(1) };

    // 5. 协议注入(hover 移动,不触碰屏幕内容)
    let mouse = u64::MAX;
    client.send(control::ControlCmd::Touch { action: 7, pointer_id: mouse, x: 640, y: 1386 });
    client.send(control::ControlCmd::Touch { action: 7, pointer_id: mouse, x: 700, y: 1400 });
    std::thread::sleep(std::time::Duration::from_millis(500));
    check("协议注入(hover)", client.is_connected(), "");

    drop(client);
    drop(server);
    println!("{}", if failed { "存在失败项" } else { "全部通过" });
    std::process::exit(if failed { 1 } else { 0 });
}
