mod adb;
mod adbcmd;
mod app;
mod capture;
mod control;
mod diag;
mod engine;
mod filedialog;
mod help;
mod keyboard;
mod keymap;
mod priority;
mod settings;
mod theme;
mod timer;

fn main() -> eframe::Result<()> {
    // 定时器精度必须最先提升:否则引擎主循环的 recv_timeout 会被量化到 ~10ms 网格,
    // 计划动作(点按抬起/连发/宏步进)误差 ±6~10ms。见 timer.rs 的实测说明。
    timer::raise();
    // 诊断日志必须是**第二件**发生的事:紧接着 PadApp::new 就会开始
    // 枚举输入设备、读配置、连 adb —— 那些正是最需要留下现场的地方。
    diag::init();
    diag::install_panic_hook();

    if std::env::args().any(|a| a == "--selftest") {
        diag::snapshot_environment();
        let code = selftest();
        // 收尾必须放在这里:`selftest` 内部曾经直接 process::exit,
        // 那样会跳过 shutdown 的"正常退出"标记,于是下一次启动会误报
        // "上次运行是崩溃或被强杀"。
        timer::restore();
        diag::shutdown();
        std::process::exit(code);
    }

    diag::snapshot_environment();
    // ---- 界面风格(主题)切换的重启循环 ----
    // 风格改动牵动整体布局,无法热应用(见 theme::UiStyle)。用户在"外观"里换风格时,
    // 界面先关闭窗口,run_native 返回后由这里**立刻用新风格重新打开** ——
    // 表现出来就是"关一下 UI 再打开,确保完全切换完毕"。风格存放在 look.json,
    // 重启后 PadApp::new 自动读回,于是"上一次设的主题,下次启动还在"。
    let result = loop {
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
        let result = eframe::run_native(
            "scrcpy-pad 游戏控制台",
            options,
            Box::new(|cc| Ok(Box::new(app::PadApp::new(cc)))),
        );
        // 兜底恢复系统光标方案,避免异常退出路径把透明光标遗留给桌面。
        capture::set_cursor_visible_from_ui(true);
        if app::take_style_restart() {
            continue; // 风格切换:窗口已关,马上按新风格重开
        }
        break result;
    };
    // 兜底恢复系统光标；PadApp::on_exit/Drop 已经做过一次，这里防异常路径漏掉。
    capture::set_cursor_visible_from_ui(true);
    // 与开头的 raise() 配对(进程退出本身也会清掉,这里显式还原更干净)。
    timer::restore();
    // 正常退出也留一行标记:下次启动靠它判断"上次是不是崩溃/被强杀"
    diag::shutdown();
    match result {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            eprintln!("scrcpy-pad 退出错误: {error}");
            std::process::exit(1);
        }
    }
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
fn selftest() -> i32 {
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
            if evdev_ok {
                ""
            } else {
                "→ sudo usermod -aG input $USER 后重新登录"
            },
        );
    }
    #[cfg(windows)]
    check("键盘捕获(Windows 低级钩子)", true, "(Windows 无需特殊权限)");

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
            &format!(
                "{mice} 个{}",
                if mice == 0 {
                    " → FPS 瞄准不可用"
                } else {
                    ""
                }
            ),
        );
    }

    // 2. scrcpy 定位与版本
    let exe = adb::find_scrcpy();
    check("自动寻找 scrcpy", exe.is_some(), "");
    let Some(exe) = exe else {
        return 1;
    };
    let ver = adb::scrcpy_version_at(&exe.display().to_string());
    check("scrcpy 版本", ver.is_some(), &format!("{ver:?}"));
    let Some(version) = ver else {
        return 1;
    };

    let server_file = adb::find_server(Some(&exe));
    check("定位 scrcpy-server", server_file.is_some(), "");
    let Some(server_file) = server_file else {
        return 1;
    };
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
        return 1;
    }

    // 4. adb 设备
    let devices = adb::list_devices();
    check(
        "adb 设备在线",
        !devices.is_empty(),
        &format!("({} 台)", devices.len()),
    );
    if devices.is_empty() {
        return 1;
    }
    let serial = devices[0].clone();

    // 5. 分辨率
    let size = adb::screen_size(&serial);
    check("读取分辨率", size.is_ok(), &format!("{size:?}"));
    let Ok((w, h)) = size else {
        return 1;
    };

    // 6. scrcpy-server 控制通道
    let server = adb::start_control_server(&serial, &server_path, &version, 0x1a2b3c4d, 28383);
    let Ok(server) = server else {
        check("启动 control server", false, &format!("{:?}", server.err()));
        return 1;
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
    let Some(client) = client else {
        return 1;
    };

    // 7. 协议注入(hover 移动,不触碰屏幕内容)
    let mouse = u64::MAX;
    client.send(control::ControlCmd::Touch {
        action: 7,
        pointer_id: mouse,
        x: 640,
        y: 1386,
    });
    client.send(control::ControlCmd::Touch {
        action: 7,
        pointer_id: mouse,
        x: 700,
        y: 1400,
    });
    std::thread::sleep(std::time::Duration::from_millis(500));
    check("协议注入(hover)", client.is_connected(), "");

    drop(client);
    drop(server);
    println!(
        "{}",
        if failed {
            "存在失败项"
        } else {
            "全部通过"
        }
    );
    if failed { 1 } else { 0 }
}
