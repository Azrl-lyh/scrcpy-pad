mod adb;
mod adbcmd;
mod app;
mod capture;
mod control;
mod diag;
mod engine;
mod filedialog;
mod fonts;
mod help;
mod instance;
mod keyboard;
mod keymap;
mod mask;
mod priority;
mod settings;
mod sim;
mod theme;
mod timer;
mod trace;

fn main() -> eframe::Result<()> {
    let argv: Vec<String> = std::env::args().collect();
    // ---- 命令行开关(纯解析,不碰文件也不读环境变量)----
    // 必须排在最前面:级别是**第一行日志之前**就要定下来的东西(diag::init 会整体
    // 重写诊断文件并写头部,头部那句"级别 …(来源)"就是给人排查用的)。非法值也不能
    // 变成"半个程序启动再崩":打印用法 + 明确退出码,连日志文件都不去动。
    let cli = match parse_cli(&argv) {
        Ok(cli) => cli,
        Err(e) => {
            eprintln!("scrcpy-pad: {e}");
            eprint!("{}", usage());
            std::process::exit(EXIT_USAGE);
        }
    };
    // "命令行 > 环境变量 > settings.json"的优先级由 diag::pick_level 实现,这里只把
    // "命令行说过话"这一事实先钉进去,好让 init 写进头部的那句来源是准的。
    if let Some(level) = cli.log_level {
        diag::set_cli_level(level);
    }
    // 镜像开关可以提前置位:真正的落点要等 trace 打开,在那之前
    // diag_mirror_enabled() 恒为 false,不会白写。
    trace::set_diag_mirror(cli.trace_diag);

    // 定时器精度必须最先提升:否则引擎主循环的 recv_timeout 会被量化到 ~10ms 网格,
    // 计划动作(点按抬起/连发/宏步进)误差 ±6~10ms。见 timer.rs 的实测说明。
    timer::raise();
    // 诊断日志必须是**第二件**发生的事:紧接着 PadApp::new 就会开始
    // 枚举输入设备、读配置、连 adb —— 那些正是最需要留下现场的地方。
    diag::init();
    diag::install_panic_hook();

    // ---- 单实例守卫(必须早于任何捕获层初始化、也早于窗口创建)----
    // 僵尸实例的病根是"没有窗口却仍有进程":它继续攥着全局键盘/鼠标钩子与 FPS
    // 隔膜开关,再双击一次就是两个实例互抢输入(用户 2026-10-11:"怎么都不行")。
    // 所以这里**明确拒绝**第二个实例:打印 + 记日志 + 明确退出码,绝不静默再起一个。
    // 守卫的位置也有讲究:`trace` 之前 —— 被拒绝的这次启动不该再留下一份轨迹文件。
    // `--selftest` / `--mask-selftest` 同样经过守卫:实例在跑时这两个自检会被拒
    // (退出码 2)—— 它们要 bind 28383 控制端口、还会碰钩子,与在跑实例必然互抢。
    // 凭据由内核持有(Windows 命名互斥体 / POSIX 文件锁),见 src/instance.rs。
    let _instance_guard = match instance::acquire() {
        Ok(guard) => guard,
        Err(already) => {
            eprintln!("scrcpy-pad: {already}");
            crate::diag_error!("instance", "拒绝启动:{already}");
            finish(EXIT_ALREADY_RUNNING);
        }
    };

    // 注入轨迹:命令行 `--trace [路径]` 优先,其次环境变量 `SCRCPY_PAD_TRACE`,
    // **两者都没有时也落盘到配置目录**(双击启动不会继承环境变量,用户 2026-10-11
    // 因此整局没留下现场;总量限制见 trace::prune_old_traces)。
    // 放在 diag 之后、任何输入设备枚举之前 —— 第一条按键事件也要能被记下。
    trace::init_from_args(&argv);

    // `--trace-diag` 的语义是**自动开启轨迹**(而不是"轨迹没开就忽略")。
    // 理由:这个参数的全部用途就是把诊断行并进轨迹;轨迹没开时静默忽略 = 参数
    // 无声失效,用户会以为拿到了合并现场。轨迹本来就默认开启,所以"自动开启"不
    // 引入任何新的默认行为 —— 它只在 open 因权限/只读失败时补一次尝试。
    // 已知取舍:diag::init 写的那几行头部进不了轨迹(单实例守卫必须早于 trace,
    // 见上面的注释;头部又是 diag::init 写的),顺序上无法两全,这里如实留着。
    if cli.trace_diag {
        trace::ensure_enabled();
    }

    if argv.iter().any(|a| a == "--selftest") {
        diag::snapshot_environment();
        let code = selftest();
        finish(code);
    }

    // 蒙版窗口自检(批次 0):透明 / 置顶 / 绘制 / 点击归属 / 不抢焦点 / 无残窗。
    // 与 `--selftest` 分开:那个要连真机,这个纯本机窗口层,几秒钟就能跑完。
    if argv.iter().any(|a| a == "--mask-selftest") {
        diag::snapshot_environment();
        let code = mask::selftest();
        finish(code);
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
    // 关掉 GUI 窗口**必然**结束进程 —— 这不是想当然,是按 eframe 0.36 的实际路径核过的:
    // 主视口(ROOT)收到 CloseRequested 后,只要应用没有回 `ViewportCommand::CancelClose`,
    // eframe 就把 close 置真 -> `EventResult::CloseRequested` -> `save_and_destroy()`
    // (顺手调用 PadApp::on_exit 的幂等清理)-> `run_native` 返回,循环随即 break 到这里。
    // 本工程既不发 CancelClose,也没有"隐藏主窗但不退出"的路径:调试悬浮窗与截图小窗
    // 都是 `show_viewport_immediate` 的子视口,它们的 X 只关自己(app.rs 里各有处理)。
    // 唯一让循环继续的是 `take_style_restart()` 的**主动重启** —— 那是同一次进程内
    // 关窗重开,守卫与捕获层本就该继续持有,不是"关了还赖着"。
    capture::set_cursor_visible_from_ui(true);
    match result {
        Ok(()) => finish(0),
        Err(error) => {
            eprintln!("scrcpy-pad 退出错误: {error}");
            finish(1);
        }
    }
}

/// 已有实例在跑时的退出码(与"自检失败 = 1"分开:脚本/启动器能分辨
/// "起不来"和"跑失败"是两回事)。
const EXIT_ALREADY_RUNNING: i32 = 2;

/// 命令行用法给错时的退出码(sysexits.h 的 EX_USAGE)。
///
/// 与"起不来 = 2""跑失败 = 1"都分开:启动脚本能一眼分辨"我参数写错了"
/// 和"程序自己出了问题",不用去翻 stderr 文本。
const EXIT_USAGE: i32 = 64;

/// 命令行用法串。级别写错时**必须**能看到正确写法 —— 否则用户只能靠猜,
/// 而这条链路上本来就有一个环境变量和一份配置文件,已经够绕了。
fn usage() -> String {
    let mut s = String::new();
    s.push_str("用法:scrcpy-pad [选项]\n\n选项:\n");
    s.push_str("  --log-level <级别>   日志级别:error/warn/info/debug/trace(默认 info)\n");
    s.push_str(&format!(
        "                       优先级:命令行 > 环境变量 {} > settings.json\n",
        diag::ENV_LEVEL
    ));
    s.push_str("  --trace-diag         把诊断日志每行同时追加进轨迹 jsonl(kind=\"diag\");\n");
    s.push_str("                       轨迹未开启时自动开启\n");
    s.push_str("  --trace [路径]       注入轨迹的落盘位置(默认写配置目录,保留最近 3 份)\n");
    s.push_str("  --selftest           自检(需要真机)\n");
    s.push_str("  --mask-selftest      蒙版窗口自检(纯本机)\n");
    s
}

/// 命令行解析结果(只收本程序自己消费的开关,其余参数原样留给下游)。
#[derive(Debug, Default, PartialEq, Eq)]
struct Cli {
    /// `--log-level` 指定的级别;None = 命令行没说话(交给环境变量/配置)。
    log_level: Option<diag::Level>,
    /// `--trace-diag`:诊断日志每行同时追加进轨迹。
    trace_diag: bool,
}

/// 解析命令行。**纯函数**(不读环境变量、不碰文件),所以能直接单测;
/// 也因此 main 必须**先**调它,再让任何东西去碰配置目录。
///
/// 不认识的参数一律忽略:本程序允许把陌生开关透传给下游(例如 `--trace` 的路径),
/// 解析器不该替它们做主 —— 但它自己那两个参数的**取值**是严格的。
fn parse_cli(args: &[String]) -> Result<Cli, String> {
    let mut cli = Cli::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--log-level" => {
                let Some(v) = args.get(i + 1) else {
                    return Err("--log-level 后面缺少级别".to_string());
                };
                if v.starts_with("--") {
                    return Err(format!("--log-level 后面缺少级别(读到的是开关 {v})"));
                }
                let Some(level) = diag::Level::parse(v) else {
                    return Err(format!(
                        "无法识别的日志级别 {v:?}(可用 error / warn / info / debug / trace)"
                    ));
                };
                // 重复给以**最后一个**为准:命令行习惯如此,也让"先给默认值再覆盖"
                // 这种包装脚本写法能用。
                cli.log_level = Some(level);
                i += 2;
            }
            "--trace-diag" => {
                cli.trace_diag = true;
                i += 1;
            }
            _ => i += 1,
        }
    }
    Ok(cli)
}

/// **所有**退出路径的唯一收尾:刷轨迹 -> 还原定时器精度 -> 写"正常退出"标记 -> 退出。
///
/// 为什么必须只有这一处:收尾漏掉任何一句都有具体代价 —— 轨迹不刷盘(用户刚复现完
/// 就要拿这个文件)、定时器不还原(把整机计时精度一直抬高)、标记不写(下次启动
/// 误报"上次是崩溃或被强杀")。以前三个提前退出分支各抄一份,抄漏一句没人发现
/// (`selftest` 内部就曾直接 `process::exit`,跳过标记);现在 `--selftest` /
/// `--mask-selftest` / 单实例守卫 / 正常关闭 / 退出错误全走这里,新增退出路径
/// 也只能走这里。
fn finish(code: i32) -> ! {
    // 先收掉"手机被手触摸"监听子进程:Windows 不会随父进程退出杀子进程,
    // 不收就是设备上一个永远挂着的 `getevent`(用户明确要求零残留)。
    adb::stop_physical_touch_watches();
    trace::flush();
    timer::restore();
    diag::shutdown();
    std::process::exit(code)
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
    //
    // 2026-10-22(平台不对等修复):这一项以前只有 Linux 分支,Windows 上直接跳过。
    // 但 Windows 恰恰是"Raw Input 可能降级到光标差"的那一侧 —— 用户在这里看不到
    // 任何一条关于鼠标的检查结论,就只能去游戏里试。这里补上系统级判据:
    // `SM_MOUSEPRESENT` 是"系统认为有没有鼠标设备",0 表示没有可用鼠标
    // (无鼠标的平板/远程会话),此时 FPS 瞄准与准星映射都无输入可用。
    // 注意它**不**验证 Raw Input 是否注册成功 —— 那要等捕获层起来才知道,
    // 界面的[诊断]页有专门一行(raw_probe),这里不越权冒充。
    #[cfg(windows)]
    {
        let present = unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                windows_sys::Win32::UI::WindowsAndMessaging::SM_MOUSEPRESENT,
            )
        } != 0;
        check(
            "鼠标设备(系统报告存在)",
            present,
            if present {
                "(Raw Input 是否接管见界面[诊断]页)"
            } else {
                "→ 系统未报告鼠标设备,FPS 瞄准/准星映射不可用"
            },
        );
    }

    // 1c. 鼠标相对位移能否真的读到(Windows:原始输入到底接管了没有)
    //
    // 2026-10-22(平台不对等修复):上面那条只回答"系统认为有没有鼠标",而 FPS
    // 瞄准要的是**相对位移**读不读得到。Windows 上这条通道是 Raw Input,它可能在
    // 建接收窗口/注册那一步就失败,也可能注册成功却一条鼠标事件都收不到(被别的
    // 程序占着、或对面是数位板/远程桌面这类只有绝对坐标的设备),那时捕获层会按
    // 700ms 判据退回"光标差"路径 —— 位移还能用,但不再按设备计数上报,手感与
    // 设备原生计数不同。这些都不体现在系统报告里,只能起一次捕获层实测。
    //
    // 所以这里**真的**起一次捕获层(与生产同一条路),等 Raw Input 注册落定后读
    // 只读探针 raw_probe();若停在"已注册、还没收到事件",就自己注入一次
    // **净零**相对位移去取证(先 +8 再 -8,光标回原位,不留可见副作用)。
    // 判据只用 state(与界面[诊断]页同一口径;count == 0 时 age 是硬编码 0,
    // 不能拿来判"多久没事件")。
    #[cfg(windows)]
    {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        // 注入一次相对位移。返回 false = SendInput 自己就被系统挡了(会话被锁、
        // 桌面不允许注入),那时"收不到事件"不能怪 Raw Input。
        let nudge = |dx: i32, dy: i32| -> bool {
            use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
                INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_MOVE, MOUSEINPUT, SendInput,
            };
            let ins = [INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx,
                        dy,
                        mouseData: 0,
                        dwFlags: MOUSEEVENTF_MOVE,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            }];
            unsafe {
                SendInput(
                    ins.len() as u32,
                    ins.as_ptr(),
                    std::mem::size_of::<INPUT>() as i32,
                ) == ins.len() as u32
            }
        };

        let (tx, rx) = mpsc::channel();
        let started = capture::Capture::start(tx);
        // 接收端要活到检查结束(与生产同形:引擎侧也是长期持有 rx)。这里**不起**
        // 收件线程 —— 捕获层退出时若有某条线程没交出 tx,`join` 会把自检挂死;
        // 而 `send` 的返回值本来就是被忽略的,活着即可。
        let _keep_rx = rx;

        let (ok, detail) = match started {
            Err(error) => (false, format!("→ 捕获层没起来({error}),FPS 瞄准不可用")),
            Ok(cap) => {
                // Raw Input 的注册发生在 start() 返回**之后**(它只等钩子安装
                // 结果),所以先给它一点时间,别把"还没注册完"读成"注册失败"。
                let wait_until = |limit: Duration| -> (u8, u64, u64) {
                    let deadline = Instant::now() + limit;
                    let mut probe = capture::raw_probe();
                    while probe.0 != 2 && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(50));
                        probe = capture::raw_probe();
                    }
                    probe
                };
                let mut probe = wait_until(Duration::from_millis(1500));
                // 1 = 已注册、等首个事件:注册成功但还没有"相对位移真的读到了"
                // 的证据,注入一次净零位移来取证。
                let mut injected = None;
                if probe.0 == 1 {
                    // 两次注入都要发:第一条成功、第二条失败时若用 `&&` 短路,
                    // 光标就被留在偏移 8px 的地方 —— 自检不该有可见副作用。
                    let out = nudge(8, 0);
                    let back = nudge(-8, 0);
                    injected = Some(out && back);
                    probe = wait_until(Duration::from_millis(600));
                }
                let (state, count, _age) = probe;
                let verdict = match state {
                    2 => (true, format!("(已接管,收到 {count} 条原始输入事件)")),
                    1 => (
                        false,
                        if injected == Some(false) {
                            "→ 原始输入已注册但收不到鼠标事件,且本次注入被系统拒绝(锁屏/受限桌面?);FPS 瞄准不可用,请在正常桌面下重试".to_string()
                        } else {
                            "→ 原始输入已注册但一条鼠标事件都没收到:此刻位移两条路都不发(要等 700ms 兜底判据才退回光标差),FPS 瞄准会不动 —— 关掉可能占用/过滤原始输入的程序(游戏叠加层、宏/输入软件、部分安全软件)后重试".to_string()
                        },
                    ),
                    3 => (
                        false,
                        "→ 只收到绝对坐标(数位板/远程桌面/部分触控板),位移已退回光标差路径;FPS 瞄准仍可用,但灵敏度受系统指针速度与加速影响".to_string(),
                    ),
                    _ => (
                        false,
                        format!(
                            "→ 原始输入未注册(建接收窗口或注册失败,state={state});位移只能走光标差路径,FPS 瞄准灵敏度受系统指针速度与加速影响"
                        ),
                    ),
                };
                drop(cap);
                verdict
            }
        };
        check("鼠标相对位移(原始输入)", ok, &detail);
    }
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

#[cfg(test)]
mod tests {
    use super::{Cli, parse_cli, usage};
    use crate::diag::Level;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// 守护 parse_cli 里 `--log-level` 的**取值分支**(程序每次启动都走这段)。
    /// 变异验证用:把 `i += 2` 改成 `i += 1`,"trace" 会被当成陌生参数再解一遍,
    /// 下面的断言立刻红;或把 `Some(level)` 改成 `None`,首个断言红。
    #[test]
    fn log_level_is_parsed_from_the_command_line() {
        assert_eq!(parse_cli(&argv(&["x.exe"])), Ok(Cli::default()));
        let cli = parse_cli(&argv(&["x.exe", "--log-level", "trace"])).expect("应当解析成功");
        assert_eq!(cli.log_level, Some(Level::Trace));
        // 与 SCRCPY_PAD_LOG 同一套写法(大小写不敏感 + 常见别名都容错)
        assert_eq!(
            parse_cli(&argv(&["x.exe", "--log-level", "DEBUG"]))
                .unwrap()
                .log_level,
            Some(Level::Debug),
            "大小写必须容错(环境变量那边就是这样)"
        );
        assert_eq!(
            parse_cli(&argv(&["x.exe", "--log-level", "warning"]))
                .unwrap()
                .log_level,
            Some(Level::Warn),
            "别名要和 SCRCPY_PAD_LOG 一致,否则同一个词在两处行为不同"
        );
        // 重复给以最后一个为准
        assert_eq!(
            parse_cli(&argv(&[
                "x.exe",
                "--log-level",
                "info",
                "--log-level",
                "error"
            ]))
            .unwrap()
            .log_level,
            Some(Level::Error)
        );
    }

    /// 守护"非法值 / 缺值 ⇒ Err":main 据此打印用法并非零退出(EXIT_USAGE),
    /// 而不是带着一个"什么都没配"的状态照常启动。
    #[test]
    fn bad_or_missing_log_level_is_rejected() {
        assert!(parse_cli(&argv(&["x.exe", "--log-level", "loud"])).is_err());
        assert!(parse_cli(&argv(&["x.exe", "--log-level", ""])).is_err());
        assert!(
            parse_cli(&argv(&["x.exe", "--log-level"])).is_err(),
            "结尾缺值必须报错,不能静默当没写"
        );
        assert!(
            parse_cli(&argv(&["x.exe", "--log-level", "--trace-diag"])).is_err(),
            "下一个是开关而不是级别时必须报错,不能把开关当级别吞掉"
        );
    }

    /// 守护 `--trace-diag` 的开/关二态,以及"陌生参数原样放行"
    /// (尤其 `--trace <路径>`:路径可能长得像任何东西,解析器不许误伤它)。
    #[test]
    fn trace_diag_flag_and_foreign_arguments_are_handled() {
        assert!(!parse_cli(&argv(&["x.exe"])).unwrap().trace_diag);
        assert!(
            parse_cli(&argv(&["x.exe", "--trace-diag"]))
                .unwrap()
                .trace_diag
        );
        let cli = parse_cli(&argv(&[
            "x.exe",
            "--trace",
            "C:\\临时 目录\\轨迹 1.jsonl",
            "--trace-diag",
            "--selftest",
        ]))
        .expect("陌生参数必须原样忽略");
        assert!(cli.trace_diag, "--trace-diag 夹在陌生参数中间也要认出来");
        assert_eq!(cli.log_level, None, "没有 --log-level 时不许凭空给一个级别");
    }

    /// 守护用法串:两个新参数必须**出现在用法里**,否则用户只能靠猜。
    /// (命令行 > 环境变量 > 配置 的优先级本身由
    /// diag::tests::level_priority_is_cli_then_env_then_settings 守护)
    #[test]
    fn usage_lists_both_new_switches() {
        let u = usage();
        assert!(u.contains("--log-level"), "用法串缺 --log-level:{u}");
        assert!(u.contains("--trace-diag"), "用法串缺 --trace-diag:{u}");
        assert!(
            u.contains(crate::diag::ENV_LEVEL),
            "用法串要写清环境变量名字,不然优先级那句没意义"
        );
    }
}
