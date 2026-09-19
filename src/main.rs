mod adb;
mod app;
mod capture;
mod control;
mod engine;
mod filedialog;
mod keymap;
mod settings;
mod theme;

fn main() -> eframe::Result<()> {
    if std::env::args().any(|a| a == "--selftest") {
        selftest();
        return Ok(());
    }

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1180.0, 760.0])
        .with_min_inner_size([900.0, 600.0]);
    if let Some(icon) = app_icon() {
        viewport = viewport.with_icon(std::sync::Arc::new(icon));
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "scrcpy-pad 游戏控制台",
        options,
        Box::new(|cc| Ok(Box::new(app::PadApp::new(cc)))),
    )
}

/// 程序图标:编译期直接嵌入二进制,运行时不依赖任何外部图片文件
const APP_ICON_PNG: &[u8] = include_bytes!("../icons/scrcpy-pad.png");

/// 解码内嵌的程序图标;解码失败时返回 None,由系统使用默认图标
fn app_icon() -> Option<egui::IconData> {
    let img = image::load_from_memory(APP_ICON_PNG).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    // 过大时缩到 256 以内,避免个别平台不接受超大图标
    let rgba = if w > 256 || h > 256 {
        let img2 = image::DynamicImage::ImageRgba8(rgba);
        img2.resize(256, 256, image::imageops::FilterType::Triangle)
            .to_rgba8()
    } else {
        rgba
    };
    let (w, h) = rgba.dimensions();
    Some(egui::IconData {
        rgba: rgba.into_raw(),
        width: w,
        height: h,
    })
}

/// 无界面自检:验证键盘捕获权限 / adb / scrcpy 定位 / 控制通道 / 协议注入(仅发无害 hover)
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

    // 1b. 鼠标设备(FPS 瞄准依赖相对位移,须能被读到)
    #[cfg(target_os = "linux")]
    {
        let mice = evdev::enumerate()
            .filter(|(_, d)| {
                d.supported_relative_axes()
                    .map(|a| {
                        a.contains(evdev::RelativeAxisCode::REL_X)
                            && a.contains(evdev::RelativeAxisCode::REL_Y)
                    })
                    .unwrap_or(false)
            })
            .count();
        check(
            "鼠标设备(REL_X/REL_Y)",
            mice > 0,
            &format!("{mice} 个{}", if mice == 0 { " → FPS 瞄准不可用" } else { "" }),
        );
    }

    // 2. scrcpy 定位与版本
    let exe = adb::find_scrcpy();
    check("自动寻找 scrcpy", exe.is_some(), "");
    let Some(exe) = exe else { std::process::exit(1) };
    let ver = adb::scrcpy_version_at(&exe.display().to_string());
    check("scrcpy 版本", ver.is_some(), &format!("{ver:?}"));
    let Some(version) = ver else { std::process::exit(1) };

    let server_file = adb::find_server(Some(&exe));
    check("定位 scrcpy-server", server_file.is_some(), "");
    let Some(server_file) = server_file else { std::process::exit(1) };
    let server_path = server_file.display().to_string();

    // 3. adb 定位(优先 scrcpy 同目录,对应 Windows 发行包同目录 adb.exe 场景)
    let adb_path = adb::find_adb(Some(&exe));
    adb::set_adb_bin(adb_path.as_deref());
    let adb_detail = adb_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(仅 PATH)".to_string());
    check("定位 adb", adb_path.is_some(), &adb_detail);
    if adb_path.is_none() {
        std::process::exit(1);
    }

    // 4. adb 设备
    let devices = adb::list_devices();
    check("adb 设备在线", !devices.is_empty(), &format!("({} 台)", devices.len()));
    if devices.is_empty() {
        std::process::exit(1);
    }
    let serial = devices[0].clone();

    // 5. 分辨率
    let size = adb::screen_size(&serial);
    check("读取分辨率", size.is_ok(), &format!("{size:?}"));
    let Ok((w, h)) = size else { std::process::exit(1) };

    // 6. scrcpy-server 控制通道
    let server = adb::start_control_server(&serial, &server_path, &version, 0x1a2b3c4d, 28383);
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

    // 7. 协议注入(hover 移动,不触碰屏幕内容)
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
