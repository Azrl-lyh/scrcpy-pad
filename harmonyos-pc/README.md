# scrcpy-pad HarmonyOS HDC client

This directory is the PC-side HarmonyOS product line. It is independent from:

- the original Windows/Linux scrcpy-pad desktop app;
- `harmonyos/`, which is only an optional on-device ArkTS companion for Bluetooth keyboard mapping.

## Why this exists

HarmonyOS NEXT/HarmonyOS 6 does not run the Android `scrcpy-server.apk`. The replacement path is:

```text
scrcpy-pad PC client
        |
        | HDC: shell / fport / file
        v
HarmonyOS device
  - uinput: touch/key/mouse injection
  - uitest screenCap: screen snapshots
```

OpenHarmony's `uinput` tool supports independent touch events:

- `uinput -T -d x y`
- `uinput -T -m x1 y1 x2 y2 [keep_ms] [smooth_ms]`
- `uinput -T -u x y`

It also supports keyboard and mouse including buttons and scroll:

- `uinput -K ...`
- `uinput -M ...`

`uitest screenCap -p /data/local/tmp/file.png` captures the current screen.

## Current CLI probe

Build:

```powershell
cd D:\GitHub\scrcpy-pad-developing-yaml\harmonyos-pc
cargo build
```

Run:

```powershell
cargo run -- devices
cargo run -- doctor
cargo run -- tap <serial> 540 1200
cargo run -- swipe <serial> 900 1600 300 1600 300
cargo run -- capture <serial> screen.png
cargo run -- map-smoke <serial>
cargo run -- map-live <serial> 8
```

`doctor` checks whether the connected device exposes:

- `uinput`
- `uitest`
- `snapshot_display`
- HarmonyOS version and API level

The desktop GUI uses the same `Profile` model as the original scrcpy-pad and
writes an independent YAML configuration. Commands are sent through one-shot
`hdc shell` calls: the tested HDC 3.2 build reports `Not support stdio TTY
mode`, so redirecting an interactive shell's stdin cannot execute `uinput`
commands reliably. A future companion HAP/fport path can restore a true
low-latency stream.

## GUI Probe

Build and run:

```powershell
.\build.ps1
.\target\release\hdc-pad-gui.exe
```

The GUI currently provides:

- HDC device discovery
- device selection
- manual screenshot capture through `uitest screenCap`
- auto capture at roughly 2 FPS through the same PNG path
- tap, swipe and hold injection tests through `uinput`
- profile switching with independent YAML save/load
- key binding editor for tap, hold, swipe and system keys
- virtual wheel editor with classic and sensitive modes and temporary wheels
- FPS settings for its independent toggle, hold-to-suspend and pointer hiding
- pointer hiding follows the live FPS/suspend state; the Windows capture layer uses a global transparent system cursor so it also works when scrcpy or another window has focus
- FPS mouse motion is coalesced and injected as touch aiming, with hold-to-aim gating and recentering
- `启动并开启映射` turns the runtime on immediately; `F8` can then temporarily toggle it
- one-click physical key capture for bindings, wheels and FPS controls
- click-to-pick coordinates from the live screenshot for tap/hold points, swipe
  start/end/circle direction, wheel centers and the FPS anchor

## Key capture

Every physical-key field has a `捕获` button. The next keyboard key or mouse
button pressed in the GUI window is written into that field, and the UI also
shows the decoded key name. Key capture is disabled while the global mapping
service is running so a configuration keystroke cannot be injected into the
phone. Stop mapping before editing keys.

## Verification

```powershell
cargo test
cargo build --release --bins --offline
```

The current suite contains 37 tests, including HDC parsing, persistent input
transport models, YAML round-trips, sensitive-wheel behavior, FPS pointer
hiding and GUI key capture mapping.
