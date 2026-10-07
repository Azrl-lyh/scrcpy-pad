> **【已归档 · 2026-10-06】** 2026-09-27 的一次性交接文档（Windows 端验证/调试清单）。对应的验证步骤已并入现行交接流程。**现行入口是《全面审计与提升方案-2026-10-05.md》与《文件与执行逻辑.md》《功能组分说明.md》**；本文保留作历史记录。

# 交接文档：Windows 端验证与调试清单

> 给在 Windows 电脑上调试本程序的 AI。**先读这两份再动手**：
> - `文档/下一位AI-排查与修复清单.md` —— 四个问题的**成因分析**（含代码行号与依赖库源码证据）
> - `README.md` 的 **2026-09-27** 更新日志 —— 这一版**实际改了什么**
>
> 你手上这份代码的最大特点：**Windows 输入层刚被整体重写，但只在 Linux 上通过了交叉编译，没有任何人在真 Windows 上运行过。**
> 所以你的首要任务不是"加功能"，而是**验证它到底能不能用**，并把发现的问题修掉。

---

## 0.0 先确认你拿到的是哪一份代码

| 项目 | 值 |
|---|---|
| 分支 | `developing`（**不是** `main`；`main` 是只放二进制的发布分支） |
| 提交 | `0ceb488`，提交信息以 **"可靠性大改：底层诊断日志 + 外接键盘热插拔 + Windows 输入层重写"** 开头 |
| 该提交的父提交 | `3dda164`（v0.1.4 simple update） |

```bash
git log --oneline -2      # 应看到 0ceb488 与 3dda164
```

**若对不上，先别调试** —— 本文提到的这些东西都是该版本才有的，在老代码里找不到：

- `src/diag.rs`（新文件）与 `diagnostics.log`
- `src/capture.rs` 里的 `mod vktable` / `mod windows`（自建钩子，已无 rdev）
- 界面左栏的 **[诊断]** 面板
- `Cargo.toml` 里 Windows 依赖**只有 `rfd` 与 `windows-sys`**（没有 `rdev`）

> 顺便：如果你在代码里搜到 `rdev`，那说明是旧代码。

### 取代码的方式（会影响构建脚本能不能跑）

**优先 `git clone` / `git checkout`**，不要直接拷文件夹。原因：

- 仓库里 `.gitattributes` 定了 `*.bat text eol=crlf`，**checkout 出来的 `.bat` 是 CRLF**；
- 而 `release/build.bat` 在作者的工作目录里是 **LF 行尾**（它本身不影响 git，因为提交进去的 blob 会被规范化）；
- **如果你直接拷文件夹**，`release/build.bat` 会带着 LF 过去，`cmd.exe` 跑批处理时可能出问题（尤其是有 `goto`/`label` 的地方）。

万一你只能拷文件夹：先 `unix2dos release/build.bat` 把行尾转成 CRLF，或者干脆别用它，直接 `cargo build --release`。

---

## 0. 一句话背景：为什么 Windows 侧要被重写

旧实现用 `rdev` 0.5.3 的低级钩子。它的回调在**调用我们的代码之前**，会对每个 `KeyPress` 调一次 `Keyboard::get_name()`，而那里做了：

```
GetForegroundWindow → GetWindowThreadProcessId → AttachThreadInput(我们的线程, 前台线程, TRUE)
   → GetKeyboardState → AttachThreadInput(..., FALSE) → GetKeyboardLayout → ToUnicodeEx → 堆分配
```

`AttachThreadInput` 会**同步附着到另一个线程的输入队列**上，对方不泵消息就会一直阻塞。而 Windows 对低级钩子有硬性时限 `LowLevelHooksTimeout`（默认 **300ms**）：

- 超时 → 该事件被**静默丢弃**（不投给目标程序，也不给下一个钩子）
- 反复超时 → Windows **静默把整个钩子摘掉**，此后所有输入事件都收不到

而"切换程序焦点"正是 `GetForegroundWindow()` 变化的时刻，因此最容易触发 —— 这与"切窗口之后按键没反应、必须重新开一次映射"的现象精确对应。

**最讽刺的是：`event.name` 我们从未使用过。** 所以这次直接把 rdev 去掉，自己装钩子，回调里只做"读 vkCode → 查表 → 塞进无锁队列"三件事。

`Cargo.toml` 里 Windows 的依赖现在只有 `rfd` 与 `windows-sys`（features 见文件）。**如果你在老笔记/老对话里看到 rdev，那已经是历史。**

---

## 1. 环境准备（Windows）

| 项目 | 做法 |
|---|---|
| Rust | 装 `rustup-init.exe`；国内慢就先设 `RUSTUP_DIST_SERVER`（见 README 的"从源代码安装"） |
| scrcpy | 下载发行包，解压到与 `scrcpy-pad.exe` **同目录**（`scrcpy.exe`、`scrcpy-server`、`adb.exe` 三者在一起） |
| 手机 | 打开 USB 调试；小米额外打开"USB 调试（安全设置）"。插数据线 |
| 首次构建 | 会拉依赖并编译，耐心等 |

构建与体检：

```powershell
cargo test                          # 62 项；其中 13 项专测 Windows 键码表，在 Windows 上会全部跑到
cargo build --release               # 产出 target\release\scrcpy-pad.exe
# 或者直接双击 启动.bat
```

> **注意**：`cargo check --target x86_64-pc-windows-gnu` 是给 Linux 机器做交叉检查用的，你在 Windows 上**不需要**它（也不需要 mingw）。

**无界面自检**（不打开窗口，只验证权限/adb/scrcpy/控制通道，并会写一份诊断日志）：

```powershell
.\target\release\scrcpy-pad.exe --selftest
```

退出码 0 = 全过。它会在 `%APPDATA%\scrcpy-pad\config\diagnostics.log` 里留下环境快照。

---

## 2. 第一优先级：确认钩子**装上**了

这是最重要的一步。如果钩子装不上，**键盘会完全没反应**（而且旧版能用的功能也会一起失效），必须先排除它再谈别的。

1. 启动程序，左栏展开 **[诊断]**。
2. 打开日志：`%APPDATA%\scrcpy-pad\config\diagnostics.log`
   （或点 [诊断] 面板里的 **[打开日志目录]**，也可用 **[显示日志末尾]** 直接看）
3. **期望看到**（在启动快照之后）：

```
[INFO ][capture] 键盘低级钩子已安装
[INFO ][capture] 鼠标低级钩子已安装
```

4. 若看到 **`安装键盘钩子失败(GetLastError=N)`** 或鼠标钩子失败，**先把 N 记下来**，排查方向：

| 可能原因 | 现象/线索 |
|---|---|
| 安全软件拦截全局钩子 | 360、火绒、卡巴斯基等常见；临时关闭后重试 |
| 权限不足（UIPI） | 目标程序以管理员运行时，普通权限进程的钩子收不到它的事件；以管理员身份运行本程序再试 |
| "游戏模式"/加速器/覆盖层软件 | 它们自己也装钩子，可能冲突；退出它们再试 |
| 会话/桌面问题 | 在 RDP 会话里跑全局钩子行为怪异；用本地会话试 |

> 日志里那一行是 `GetLastError()` 的原始值。把它连同 Windows 版本一起记下来。

---

## 3. 逐项验证清单

按重要性排序。**每一项都请把"实际看到什么"记下来**，哪怕是"正常"。

### 3.1 键盘基础（先确认能收到键）

- [ ] 点某个键位行的"改键"进入捕获态，依次按 `A` `W` `S` `D` `U` `F8` → 都能捕获到
- [ ] **小键盘**：`Num0`–`Num9`、`+` `-` `*` `/` → **旧版完全绑不上（会落到 Unknown 被静默丢弃），这一版应该能**
- [ ] **多媒体键**：音量加/减/静音、播放/暂停 → 同上，应该能
- [ ] **鼠标侧键**（前进/后退）→ 应分别显示为 `BTN_SIDE` / `BTN_EXTRA`（旧版错位成 276/277，与 Linux 不一致）
- [ ] 左/右 `Shift`、`Ctrl`、`Alt` 能分别绑定（不是混成一个）
- [ ] `F8` 总开关：按一下开、再按一下关，日志里有对应记录
- [ ] 若某个键**怎么按都捕获不到**：日志里会有一行 `未映射的虚拟键码 N (0xNN)`（首次才打，不刷屏）。**把 N 报出来**，往 `src/capture.rs` 的 `VK_TABLE` 里补一行即可。

### 3.2 断触 —— 本次的重点回归项

这是用户最常抱怨的问题，请在 Windows 上专门复现一次：

- [ ] 开映射，按住 3 个"长按"型键位（不松手），然后**拔掉 USB 线**（或新开一个终端跑 `adb kill-server`）让控制通道断开

  **期望**：
  - 日志出现 `控制通道不可用: 已清空本地触点状态(重连后按当前按键状态重建)`
  - 日志出现 `控制通道断开时仍有 N 个触点未抬起`
  - 左栏「引擎: 触点 N/10」**归 0**（这一点是关键）

  **旧版本的表现（用来对照判断是否真的修好了）**：N 会**停住不动**，此后按任何键都没反应，必须按一次 F8 关一次映射才能恢复。如果你的机器上仍然这样，说明引擎里那段修复没生效，去看 `src/engine.rs` 主循环里 `conn_ok` / `connected_prev` / `rebuild_now` 那一段。

- [ ] 重新插上线、重新 [连接控制] → **不重启程序**，按几个键，应立刻可用（日志有 `控制通道已连接: 已按当前按键状态重建触点`）

### 3.3 焦点切换 / 快速连按（钩子超时的正面验证）

- [ ] 开映射，把 FPS 瞄准打开（`capture_mouse = true`）
- [ ] 切换到另一个窗口，然后**快速连按键盘**
- [ ] 日志里每 5 秒会有一条钩子回调耗时统计：

```
[DEBUG][capture] 钩子回调 12345 次,耗时 p50≈2.1µs p99≈8.4µs 最大 41.0µs(超时线 300ms)
```

  **判读**：
  - `p99` 是**微秒量级** = 健康，离 300ms 超时线极远
  - `p99` 到了**毫秒甚至几十毫秒** = 还有阻塞源，需要查（注意：这条统计只在 `p99 > 5ms` 时才升为 `WARN`，否则是 `DEBUG`，所以要先把日志级别调成 `debug` 才看得到）

- [ ] 想更狠地测：写一个**故意不泵消息**的小程序（例如 `Sleep(100000)` 不停的那种窗口，或者一个死循环里不处理消息的窗口），把前台切到它，再快速连按。旧版在这个场景下会大面积丢事件。

- [ ] 顺便验证退出：**关闭程序窗口**，应能正常退出、日志末尾出现 `===== 正常退出 =====`。
  ⚠️ **如果关不掉、进程卡住不退出，第一时间告诉我** —— 那是钩子线程的消息循环没被唤醒（`PostThreadMessageW` 那条路径），是这次改动里我最没把握的地方。

### 3.4 FPS 鼠标视角手感

- [ ] 单显示器：慢慢移动鼠标 → `[诊断]/[鼠标瞄准(FPS)]` 面板里"运行状态"的 `最近(dx,dy)` 应随之变化，且**慢移小、快甩大**（成比例）
- [ ] 快速甩动 10 次，手机侧视角转动幅度应大致一致，**不应有明显卡顿感**
- [ ] `最近(dx,dy)` **不应频繁出现负值**（除非你真的反向移动了）。若频繁出现负值 → 回声识别逻辑没生效，去看 `Motion::echo`
- [ ] **双显示器**：把光标移到副屏，再转视角 → 应正常，且光标**不会被拽回主屏**（旧版会）
- [ ] **系统缩放 125%/150%** → 同上
- [ ] 抓一次 Watch：`GetForegroundWindow()` 每次变化都会让程序重取一次显示器中心（日志里是 `DEBUG` 级的"前台窗口变化,重新取显示器中心"）

### 3.5 其它

- [ ] `--selftest` 全部 PASS（需要手机已连接）
- [ ] 配置文件位置正确：`%APPDATA%\scrcpy-pad\config\` 下有 `profile.yaml`、`look.json`、`settings.json`、`diagnostics.log`
- [ ] 左栏 [诊断] 面板的 ✓/✗ 自检全部为 ✓（映射关着时"映射已开启"一项自然是 ✗，属正常）

---

## 4. 我改过但**无法验证**的具体代码点（请优先 review 这几处）

这些是 Windows 专属代码，我在 Linux 上只能做到"编译通过"，逐条列出以便你带着假设去试：

| 位置 | 做了什么 | 风险 / 怎么验证 |
|---|---|---|
| `src/capture.rs` → `mod windows` → `install_and_pump` | 自己装两个钩子（键盘/鼠标），然后跑 `GetMessageW` 消息循环，退出时 `UnhookWindowsHookEx` | ⚠️ **最需要验证**：退出时用 `PostThreadMessageW(tid, WM_QUIT)` 唤醒阻塞的 `GetMessageW`。我们的线程没有窗口，靠 `SetWindowsHookExW` 强制创建了消息队列，理论上可行。**若关闭窗口后进程不退出，就是这里。** 备选方案：改用 `SetEvent` + `MsgWaitForMultipleObjects` 循环，或建一个隐藏窗口用 `PostMessageW` |
| 同上 → `keyboard_proc` / `mouse_proc` | `unsafe extern "system"` 回调，读完即 `CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)` 放行 | `CallNextHookEx` 的第一个参数官方说是"可忽略"，传 NULL 是常见写法。**若发现按键被吞掉、或系统级热键（Win 键、媒体键）失灵**，改成传入自己的钩子句柄再试 |
| 同上 → `mouse_proc` 的 `WM_XBUTTONDOWN` 分支 | `(mouseData >> 16) & 0xFFFF` 应为 1/2 → 映射到 `BTN_SIDE`(275) / `BTN_EXTRA`(276) | 若侧键绑出来还是错位，就打印一下原始 `mouseData` |
| 同上 → `Motion` 结构 + `handle_move` | 用 `echo: Option<(i32,i32)>` 记录"我们自己刚把光标挪到过哪里"，只丢弃位置**真的落在预期点**的那条事件 | 对应 §3.4 的负值检查。旧版是无条件丢下一条（`skip_next: bool`），会吃掉真实位移并把回声当反向位移发出去 |
| 同上 → `monitor_center` | `GetCursorPos` → `MonitorFromPoint(MONITOR_DEFAULTTONEAREST)` → `GetMonitorInfoW`，中心取光标所在显示器，阈值取短边 1/4 | `MONITORINFO.cbSize` 若写错，`GetMonitorInfoW` 会失败并静默回退成主屏中心 —— 那时双屏问题会复现 |
| 同上 → `Timing` 直方图 | 按 2 的幂分桶统计回调耗时 | `quantile_nanos` 是**分桶上界估计**，p50/p99 是量级不是精确值，别拿它当精确基准 |
| 同上 → `foreground_handle` | 用 `GetForegroundWindow()` 的句柄变化触发"重取显示器中心" | 它在**每轮 `recv_timeout` 超时后**（约 50ms 一次）被调用，不是每帧 |
| `src/capture.rs` → `mod vktable` | vk → evdev 的纯数据映射表（含小键盘/多媒体/浏览器键） | 已有 13 项单元测试在跑（Linux 上就能跑），所以**表本身可信**；问题只可能出在"某个键没被覆盖"，此时日志会报未映射的 vkCode |

---

## 5. Windows 侧**没有**做的两件事（别去找，它们不存在）

1. **没有输入设备枚举/热插拔重扫**。Windows 走的是钩子，装一次就监听全局输入，不存在 Linux 那套"设备节点失效"问题。所以 `文档/下一位AI-排查与修复清单.md` 里 §2、§3 关于 `evdev::enumerate()`、`/dev/input`、`EVIOCGKEY` 的修复**对 Windows 不适用**（那些是 Linux 专属实现，各自在 `mod linux` / `mod windows` 里）。
2. **grab（屏蔽原始按键）在 Windows 无法实现**。低级钩子只能观察，不能拦截 —— 这也是代码里 `let _ = grab;` 的原因。界面上开了"映射时屏蔽原键"在 Windows 上不会生效，这是系统限制。

顺带一提：`mouse_found` 在 Windows 侧被直接置为 `true`（钩子能收到鼠标事件就说明有鼠标），不像 Linux 要枚举 REL 轴。

---

## 6. 明确留给下一档的改进：Raw Input

**现状**：Windows 的位移来自 `MSLLHOOKSTRUCT.pt`（**已经过指针加速的屏幕绝对坐标**）做差分。因此仍有三重损失：

- 系统指针加速使"同样的手速、慢移与快移得到的 dx 不成比例"，灵敏度手感与 Linux 不一致；
- 系统会**合并**高频鼠标的移动消息，中间位置被丢弃；
- 我们与 Linux 的 `Motion.dx/dy` **单位不同**（Linux 是设备计数，Windows 是屏幕像素），却共用同一个 `sensitivity`。

**彻底解法：Raw Input。** 一次消掉上面全部三条 + 多屏/DPI/边界问题，而且单位与 Linux 一致（都是设备计数），配置从此跨平台通用。

做法要点（留给你，或留到下一档）：

1. 注册：`RegisterRawInputDevices` + `RAWINPUTDEVICE { usUsagePage: 0x01, usUsage: 0x02 /* Mouse */, dwFlags: RIDEV_INPUTSINK, hwndTarget: hwnd }`（`RIDEV_INPUTSINK` 让**非前台**时也能收到）
2. 需要一个窗口句柄来收 `WM_INPUT`。**建议在钩子线程之外另开一个专用线程**建隐藏消息窗口，别和钩子线程抢同一个消息循环（钩子线程那个循环现在只跑 `GetMessageW`，被别的东西干扰会很难查）。
3. `WM_INPUT` → `GetRawInputData` → `RAWMOUSE.lLastX / lLastY`（**原始计数，未经加速、未合并**）
4. 拿到原始计数后，**不再需要 `SetCursorPos` 回中**：光标就算飘到屏幕边缘也不影响位移。只需保留 `ShowCursor` 隐藏光标。
5. 需要新增 windows-sys feature：**`Win32_UI_Input`**
6. 注意 `RAWMOUSE.usFlags & MOUSE_MOVE_ABSOLUTE`：某些设备（部分触摸板、远程桌面、绘图板）报的是**绝对坐标**，此时 `lLastX/lLastY` 不是增量，要另做处理（最简单：这种设备直接忽略）
7. 建议保留现有光标差分路径作为**回退**（Raw Input 注册失败时用），避免一刀切换下去完全没位移

---

## 7. 反馈时请附上这些

程序内一键：左栏 **[诊断]** → **[导出诊断报告]**，会在日志同目录生成 `diagnostics-report-*.txt`（环境快照 + 配置摘要 + 日志全文）。**直接发这个文件。**

再补几条日志里没有的：

- Windows 版本（`winver`）、是否**以管理员身份**运行、装了什么安全软件
- 显示器数量 / 分辨率 / 系统缩放比例
- 鼠标 DPI 与回报率（1000Hz 的鼠标更容易触发事件合并）
- 现象 + 期望现象 + 复现步骤（越具体越好）

排查时建议先把日志级别调成 `debug`（[诊断] 面板的下拉框，会自动写进 `settings.json`；临时用也可以设环境变量）：

```powershell
$env:SCRCPY_PAD_LOG="debug"; .\target\release\scrcpy-pad.exe
```

---

## 附录：常用位置与命令

```powershell
# 诊断日志 / 配置文件
%APPDATA%\scrcpy-pad\config\diagnostics.log
%APPDATA%\scrcpy-pad\config\profile.yaml
%APPDATA%\scrcpy-pad\config\settings.json

# 低级钩子超时阈值（默认 300ms；被改小了会更容易丢事件）
reg query "HKCU\Control Panel\Desktop" /v LowLevelHooksTimeout

# 设备
adb devices

# 服务端触点池泄漏的证据（另一台终端跑着，看手机侧日志）
adb logcat | findstr /C:"Too many pointers" /C:"Ignore positional event"
```

**日志行的格式**：`[+相对启动毫秒][本地时间][级别][模块][线程] 消息`

- `[capture]` = 输入捕获层（Windows 侧就是你最该看的地方）
- `[control]` = scrcpy 控制协议通道
- `[engine]` = 映射引擎（触点池、对账、拒按）
- `[diag]` = 日志系统自身
- `[panic]` = 崩溃记录（含线程名、位置、调用栈）
