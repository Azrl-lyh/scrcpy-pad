use std::env;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use scrcpy_pad_hdc::backend::{HdcInputBackend, InputBackend};
use scrcpy_pad_hdc::hdc::Hdc;
use scrcpy_pad_hdc::keymap::Profile;
use scrcpy_pad_hdc::runtime::MappingRuntime;
use scrcpy_pad_hdc::service::MappingService;
use scrcpy_pad_hdc::{capture, input};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let mut args = env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "help".to_string());
    let hdc = Hdc::locate()?;

    match command.as_str() {
        "help" | "-h" | "--help" => {
            print_help();
            Ok(())
        }
        "devices" => {
            let targets = hdc.targets()?;
            if targets.is_empty() {
                println!("(no HDC targets)");
            } else {
                for serial in targets {
                    println!("{serial}");
                }
            }
            Ok(())
        }
        "doctor" => doctor(&hdc),
        "tap-fast" => {
            let serial = take_serial(&mut args)?;
            let x = parse_i32(args.next(), "x")?;
            let y = parse_i32(args.next(), "y")?;
            let backend = HdcInputBackend::new(hdc.clone(), serial);
            backend.tap(x, y, 40)?;
            println!("tap sent");
            Ok(())
        }
        "map-smoke" => {
            let serial = take_serial(&mut args)?;
            let backend = Arc::new(HdcInputBackend::new(hdc.clone(), serial));
            let mut runtime = MappingRuntime::new(backend, Profile::default(), (1084, 2412));
            runtime.set_enabled(true);
            runtime.handle_key_event(17, true, &[17]); // W -> default wheel up
            std::thread::sleep(std::time::Duration::from_millis(150));
            runtime.handle_key_event(17, false, &[]);
            println!("mapping runtime sent W down/up");
            Ok(())
        }
        "map-live" => {
            let serial = take_serial(&mut args)?;
            let seconds = args
                .next()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(8);
            let service = MappingService::start(serial, (1084, 2412), Profile::default())?;
            service.runtime().lock().unwrap().set_enabled(true);
            service.sync_state();
            println!("mapping live for {seconds}s; press mapped keys now");
            std::thread::sleep(std::time::Duration::from_secs(seconds));
            Ok(())
        }
        "tap" => {
            let serial = take_serial(&mut args)?;
            let x = parse_i32(args.next(), "x")?;
            let y = parse_i32(args.next(), "y")?;
            let output = input::tap(&hdc, &serial, x, y)?;
            print!("{output}");
            Ok(())
        }
        "swipe" => {
            let serial = take_serial(&mut args)?;
            let x1 = parse_i32(args.next(), "x1")?;
            let y1 = parse_i32(args.next(), "y1")?;
            let x2 = parse_i32(args.next(), "x2")?;
            let y2 = parse_i32(args.next(), "y2")?;
            let duration = parse_i32(args.next(), "duration_ms")?;
            let output = input::swipe(&hdc, &serial, x1, y1, x2, y2, duration)?;
            print!("{output}");
            Ok(())
        }
        "down" => {
            let serial = take_serial(&mut args)?;
            let x = parse_i32(args.next(), "x")?;
            let y = parse_i32(args.next(), "y")?;
            let output = input::touch_down(&hdc, &serial, x, y)?;
            print!("{output}");
            Ok(())
        }
        "move" => {
            let serial = take_serial(&mut args)?;
            let x1 = parse_i32(args.next(), "x1")?;
            let y1 = parse_i32(args.next(), "y1")?;
            let x2 = parse_i32(args.next(), "x2")?;
            let y2 = parse_i32(args.next(), "y2")?;
            let smooth = parse_i32(args.next(), "smooth_ms")?;
            let output = input::touch_move(&hdc, &serial, x1, y1, x2, y2, smooth)?;
            print!("{output}");
            Ok(())
        }
        "up" => {
            let serial = take_serial(&mut args)?;
            let x = parse_i32(args.next(), "x")?;
            let y = parse_i32(args.next(), "y")?;
            let output = input::touch_up(&hdc, &serial, x, y)?;
            print!("{output}");
            Ok(())
        }
        "key" => {
            let serial = take_serial(&mut args)?;
            let code = parse_i32(args.next(), "key_code")?;
            let output = input::key_tap(&hdc, &serial, code)?;
            print!("{output}");
            Ok(())
        }
        "hold" => {
            let serial = take_serial(&mut args)?;
            let x = parse_i32(args.next(), "x")?;
            let y = parse_i32(args.next(), "y")?;
            let duration = parse_i32(args.next(), "duration_ms")?;
            let output = input::hold(&hdc, &serial, x, y, duration)?;
            print!("{output}");
            Ok(())
        }
        "key-down" => {
            let serial = take_serial(&mut args)?;
            let code = parse_i32(args.next(), "key_code")?;
            let output = input::key_down(&hdc, &serial, code)?;
            print!("{output}");
            Ok(())
        }
        "key-up" => {
            let serial = take_serial(&mut args)?;
            let code = parse_i32(args.next(), "key_code")?;
            let output = input::key_up(&hdc, &serial, code)?;
            print!("{output}");
            Ok(())
        }
        "mouse-move" => {
            let serial = take_serial(&mut args)?;
            let dx = parse_i32(args.next(), "dx")?;
            let dy = parse_i32(args.next(), "dy")?;
            let output = input::mouse_move(&hdc, &serial, dx, dy)?;
            print!("{output}");
            Ok(())
        }
        "mouse-button" => {
            let serial = take_serial(&mut args)?;
            let button = parse_i32(args.next(), "button")?;
            let down = args.next().context("missing down/up")? == "down";
            let output = input::mouse_button(&hdc, &serial, button, down)?;
            print!("{output}");
            Ok(())
        }
        "mouse-scroll" => {
            let serial = take_serial(&mut args)?;
            let amount = parse_i32(args.next(), "scroll_amount")?;
            let output = input::mouse_scroll(&hdc, &serial, amount)?;
            print!("{output}");
            Ok(())
        }
        "shell" => {
            let serial = take_serial(&mut args)?;
            let command: Vec<String> = args.collect();
            if command.is_empty() {
                bail!("missing shell command");
            }
            let output = hdc.shell(&serial, command)?;
            print!("{output}");
            Ok(())
        }
        "capture" => {
            let serial = take_serial(&mut args)?;
            let output_path = args.next().context("missing local output.png")?;
            let path = capture::capture(&hdc, &serial, &output_path)?;
            println!("captured: {}", path.display());
            Ok(())
        }
        other => bail!("unknown command: {other}"),
    }
}

fn doctor(hdc: &Hdc) -> Result<()> {
    let targets = hdc.targets()?;
    println!("HDC executable: {}", hdc.executable().display());
    println!(
        "Targets: {}",
        if targets.is_empty() {
            "none".to_string()
        } else {
            targets.join(", ")
        }
    );
    if targets.is_empty() {
        println!("Connect and authorize a HarmonyOS 6+ device, then rerun doctor.");
        return Ok(());
    }

    for serial in targets {
        println!("\n== {serial} ==");
        for (name, command) in [
            (
                "version",
                vec!["param", "get", "const.product.software.version"],
            ),
            ("api", vec!["param", "get", "const.ohos.apiversion"]),
            ("uinput", vec!["which", "uinput"]),
            ("uitest", vec!["which", "uitest"]),
            ("snapshot_display", vec!["which", "snapshot_display"]),
        ] {
            match hdc.shell(&serial, &command) {
                Ok(output) => {
                    let text = output.trim();
                    println!("{name}: {}", if text.is_empty() { "(empty)" } else { text });
                }
                Err(error) => println!("{name}: ERROR: {error:#}"),
            }
        }
        match hdc.shell(&serial, &["uinput", "--help"]) {
            Ok(output) => println!("uinput-help:\n{}", output.trim()),
            Err(error) => println!("uinput-help: ERROR: {error:#}"),
        }
    }
    Ok(())
}

fn take_serial(args: &mut impl Iterator<Item = String>) -> Result<String> {
    args.next()
        .context("missing device serial; run `scrcpy-pad-hdc devices` first")
}

fn parse_i32(value: Option<String>, name: &str) -> Result<i32> {
    value
        .with_context(|| format!("missing {name}"))?
        .parse::<i32>()
        .with_context(|| format!("invalid {name}"))
}

fn print_help() {
    println!(
        r#"scrcpy-pad-hdc - HarmonyOS HDC backend probe

USAGE:
  scrcpy-pad-hdc devices
  scrcpy-pad-hdc doctor
  scrcpy-pad-hdc tap <serial> <x> <y>
  scrcpy-pad-hdc tap-fast <serial> <x> <y>
  scrcpy-pad-hdc map-smoke <serial>
  scrcpy-pad-hdc map-live <serial> [seconds]
  scrcpy-pad-hdc swipe <serial> <x1> <y1> <x2> <y2> <duration_ms>
  scrcpy-pad-hdc down <serial> <x> <y>
  scrcpy-pad-hdc move <serial> <x1> <y1> <x2> <y2> <smooth_ms>
  scrcpy-pad-hdc up <serial> <x> <y>
  scrcpy-pad-hdc hold <serial> <x> <y> <duration_ms>
  scrcpy-pad-hdc key <serial> <key_code>
  scrcpy-pad-hdc key-down <serial> <key_code>
  scrcpy-pad-hdc key-up <serial> <key_code>
  scrcpy-pad-hdc mouse-move <serial> <dx> <dy>
  scrcpy-pad-hdc mouse-button <serial> <button> <down|up>
  scrcpy-pad-hdc mouse-scroll <serial> <amount>
  scrcpy-pad-hdc capture <serial> <local-output.png>
  scrcpy-pad-hdc shell <serial> <command...>

Set HDC=/path/to/hdc to override automatic HDC discovery."#
    );
}
