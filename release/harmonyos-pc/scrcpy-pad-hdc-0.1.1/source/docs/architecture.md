# HarmonyOS PC-side architecture

## Product goal

Keep scrcpy-pad as a Windows/Linux desktop product. Connect to a HarmonyOS 6+ phone through HDC and provide screen mirroring, keyboard mapping, virtual wheels and FPS controls without relying on the Android `scrcpy-server.apk`.

## Confirmed device capabilities

OpenHarmony source confirms these device tools:

### Touch

`uinput -T` supports independent touch actions:

```text
uinput -T -d x y
uinput -T -m x1 y1 x2 y2 [keep_ms] [smooth_ms]
uinput -T -u x y
uinput -T -c x y [click_interval]
uinput -T -g x1 y1 x2 y2 [press_ms] [total_ms]
```

The move command supports up to three simultaneous finger traces.

### Keyboard

`uinput -K` supports:

```text
uinput -K -d <key>
uinput -K -u <key>
uinput -K -l <key> [long_press_ms]
uinput -K -r <key> [repeat_ms]
uinput -K -t <text>
```

### Mouse

`uinput -M` supports move, down, up, click, double-click, drag and scroll. Buttons include left, right, middle, side, forward and back.

### Screen

`uitest screenCap -p <path>` captures the current screen. HDC then moves the PNG to the PC with:

```text
hdc file recv <remote> <local>
```

`snapshot_display` is another device tool worth probing. Its exact output behavior must be checked on the target device.

### Transport

HDC provides:

- `hdc shell` for commands
- `hdc file send/recv` for files
- `hdc fport/rport` for TCP and local socket forwarding
- `hdc install` for optional companion HAP deployment

## Backend plan

### Backend A: Shell + uinput (first implementation)

```text
PC -> hdc shell -> uinput -T/-K/-M -> HarmonyOS input service
PC -> hdc shell -> uitest screenCap -> file recv -> PC
```

Advantages:

- No special phone app required.
- Uses shell-accessible system tools.
- Supports independent touch down/move/up.
- Supports mouse buttons and scroll.

Limitations:

- One HDC process per command unless a persistent shell is built.
- Screen capture through PNG is not a real-time video stream.
- Huawei retail firmware may remove or restrict some tools. The `doctor` command must be run on the real device.

### Backend B: Companion HAP streaming bridge

This is the path to scrcpy-like performance and full functionality.

```text
Windows/Linux scrcpy-pad
        |
        | hdc fport tcp:27183 tcp:<phone-port>
        v
HarmonyOS companion HAP
  - AVScreenCapture / AVScreenCaptureRecorder
  - H.264 or frame stream over local TCP
  - optional input authorization and injection
```

The companion HAP is optional. It is not the main product.

#### Screen capture

`AVScreenCaptureRecorder` is a public HarmonyOS API and uses a user-consent picker. It can produce encoded video suitable for streaming.

#### Input injection

OpenHarmony API 20 exposes native input injection APIs:

- `OH_Input_RequestInjection`
- `OH_Input_QueryAuthorizedStatus`
- `OH_Input_InjectTouchEvent`
- `OH_Input_InjectKeyEvent`
- `OH_Input_InjectMouseEvent`

These support explicit finger IDs and therefore potentially true multi-touch. Whether Huawei HarmonyOS 6 grants injection authorization to a normal signed HAP must be tested on the target phone. If it does, this is the highest-quality companion backend.

### Backend C: UiTest fallback

`uitest uiInput` supports:

- click
- double click
- long click
- swipe
- drag
- fling
- key event
- text input
- mouse wheel and multi-pointer injection

It is useful as a fallback when `uinput` is restricted, but it is less direct for continuous game controls.

## PC client layers

```text
hdc discovery
  -> device selection
  -> backend capability probe
  -> input transport
  -> capture transport
  -> mapping runtime
  -> egui desktop UI
```

The existing scrcpy-pad mapping model has already been reused in `harmonyos-pc/src/keymap.rs`.
The GUI editor can capture physical keyboard and mouse buttons directly and
displays the decoded key name next to each numeric code.

Implemented so far:

1. `InputBackend` is backed by direct `hdc shell` + `uinput`; the tested HDC
   build does not support interactive stdin, so per-command invocation is the
   reliable path.
2. The desktop `Held`/`FingerPool` reconciliation engine runs in `MappingRuntime`.
3. Key, wheel and FPS editors share the desktop `Profile` model.
4. Physical key capture maps GUI keyboard/mouse input back to evdev codes.
5. FPS motion is coalesced at 8 ms and injected as a dedicated touch-aim contact.
6. Screenshot coordinates can be picked directly for bindings, swipes, wheels and the FPS anchor.

Next implementation steps:

1. Stress-test simultaneous touch pointers on real Huawei firmware.
2. Replace PNG polling with the companion HAP bridge when available.
3. Add a production package script for the latest GUI binaries and sources.
