# 交接文档：外接键盘 / FPS 瞄准 / 断触 / 诊断日志 —— 成因已查明，照此施工

> 写给下一位接手的 AI。本文只做**成因分析 + 施工方案**，不含已改好的代码。
> 分析基于 2026-09-27 对源码、依赖库源码（rdev 0.5.3 / evdev 0.13.2）、scrcpy 4.1 服务端 Java 源码，
> 以及本机内核日志与 sysfs 的实测取证。**每条结论都标了证据强度**，请按强度决定动手顺序。

---

## 0. 开工前必读

### 0.1 工作约定（用户明确要求，务必遵守）

1. **不删除 `release/` 下的旧版本产物**（0.1.2 / 0.1.3 / 0.1.4 一律保留），只新增。
2. **每次改完代码默认执行构建**，验证顺序固定：
   ```
   cargo test
   cargo check --all-targets          # 要求 0 warning
   cargo check --target x86_64-pc-windows-gnu
   ./release/build.sh all
   ```
3. **同一文件不可在一条消息里放多个 Edit**（并行写会互相覆盖丢编辑），必须逐条串行。
4. 持锁（`MutexGuard` 活着）期间不能调 `self.log()` / 任何 `&mut self` 方法 → 在锁作用域内算好 `String`，出作用域再 log。

### 0.2 ⚠️ 当前没有版本控制

`.git` 是一个文件，内容指向 `/home/azrl/文档/新建DSH工作区/scrcpy-pad/.git/worktrees/scrcpy-pad-developing`，
**该目录已不存在**（实测 `ls` 报"没有那个文件或目录"），所以 `git log` / `git diff` / `git checkout` 全部报
`致命错误：不是 Git 仓库：(null)`。

后果：**没有 diff 安全网，改坏了回不去。** 动手前请先：
- 要么 `git init` 重新建仓并整体提交一次（历史上做过一次，commit 45bf269，后来 `.git` 目录又丢了）；
- 要么手工复制一份备份目录。同级目录已有 `v0.1.4备份（json最终版）`、`scrcpy-pad-developing` 等手工备份可参考命名。

**这是第 0 优先级**，在做下面任何修改之前先解决。

### 0.3 源码地图

| 文件 | 行数 | 管什么 |
|---|---|---|
| `src/main.rs` | 178 | 入口、`--selftest`（12 项无头自检）、窗口创建。**没有 panic hook** |
| `src/capture.rs` | 587 | 全局输入捕获。Linux=evdev，Windows=rdev 0.5.3。**问题 3/4/6 的主战场** |
| `src/engine.rs` | 2196 | 映射引擎线程。触点/轮盘/瞄准/切换键。**问题 6 的主战场** |
| `src/control.rs` | 243 | scrcpy 4.x 控制协议客户端（写线程 + 无锁队列） |
| `src/adb.rs` | 732 | adb/scrcpy 定位、control-only server 启停、设备信息 |
| `src/keymap.rs` | 1391 | 配置数据模型（`ConfigFile`/`Profile`/`Aim`/`Wheel`）、坐标换算 `Mapper`、`YAML_HEADER` |
| `src/app.rs` | 5657 | egui 界面 + 配置读写 + 全部持久化路径 |
| `src/settings.rs` | 479 | `settings.json`（scrcpy 三件套路径/启动参数） |
| `src/theme.rs` | 702 | 外观、配色、弹窗透明度 |
| `src/help.rs` / `help.md` | 583 | 使用说明窗口 |

### 0.4 三条引擎铁律（改 engine.rs 前背下来）

1. **触点绝不能漏抬**：任何 DOWN 必须有配对的 UP，否则设备端触点槽位永久泄漏。
2. **归属切换立刻对账**：凡是让某个物理键"改换门庭"的操作（临时轮盘启用/停用、开关映射、配置改动、切换按键组合），之后必须按 `Held`（物理按键镜像）重新对账一遍。
3. **状态一律按"当前配置"重算**，不缓存旧索引。

硬上限：`DEVICE_MAX_POINTERS = 10`（= scrcpy 服务端 `PointersState.MAX_POINTERS`，已核对源码），
`MAX_CONCURRENT_KEYS = 8`（普通键位并发，留出余量给轮盘与瞄准）。

### 0.5 本机实测事实（Fedora / ThinkPad，取证命令见附录 A）

```
uid=1000(azrl) 组=1000(azrl),10(wheel),104(input)      ← input 组已生效，权限不是本机的问题

event3  | AT Translated Set 2 keyboard | rel=0          ← 内置 PS/2 键盘（i8042，永不掉线）
event5  | Synaptics TM3276-031        | rel=0          ← 触摸板：没有 REL 轴 → is_mouse() 判假
event14 | TPPS/2 IBM TrackPoint       | rel=3          ← rel=3 = REL_X|REL_Y → 算鼠标
event15 | SIGMACHIP Usb Mouse         | rel=3          ← 05:42:24 热插入，随后被拔掉，by-id 目录整个消失
```

内核日志里的关键片段（**问题 3 的直接证据**）：

```
22:29:31 usb 2-3: reset SuperSpeed USB device ...      ← 三个总线同时 reset = 从挂起(suspend)恢复
22:29:31 usb 1-1: new low-speed USB device number 12
22:29:31 usb 1-1: Product: USB Gaming Keyboard   (SEMICO, idVendor=1a2c idProduct=9b06)
22:29:31 input: SEMICO USB Gaming Keyboard            as .../1-1:1.0/...0005/input/input32
22:29:31 input: SEMICO USB Gaming Keyboard Consumer Control as .../1-1:1.1/...0006/input/input33
22:29:31 input: SEMICO USB Gaming Keyboard System Control   as .../1-1:1.1/...0006/input/input34
22:29:31 input: SEMICO USB Gaming Keyboard Keyboard         as .../1-1:1.1/...0006/input/input36
22:31:37 usb 1-1: USB disconnect, device number 12    ← 用户把键盘拔了
22:31:38 usb 1-1: new low-speed USB device number 14  ← 又插上（input38/39/40/42，节点号全变了）
```

两分钟内拔插一次外接键盘 = 典型的"它不工作，我拔了重插试试"。**而本程序只在启动时枚举一次设备，
拔插之后新节点永远不会被打开** —— 见 §2。

---

## 1. 结论速览

| # | 问题 | 已查明的根因 | 证据强度 | 建议顺序 |
|---|---|---|---|---|
| 3 | Linux 外接键盘无法操作 | ①设备只在启动时枚举一次，**无热插拔**；②设备失联后 `device_loop` 永久空转不重开；③`enumerate()` 静默跳过打不开的设备且**没有任何"打开了哪些设备"的日志** | ★★★ 代码事实 + 内核日志实证 | 第 2 位（先做 5） |
| 4a | FPS 在老 Fedora 上不可用 | ①触摸板 `rel=0` 不被认作鼠标（设计如此）；②`mouse_found` 是启动时快照，热插鼠标永远为 false；③YAML 迁移不做转换 → 旧 `profile.json` 里配好的 `aim` 全丢，新默认是 `enabled:false` + 锚点 (0,0) | ★★★ 代码事实 + sysfs 实测 | 第 3 位 |
| 4b | Windows 上 FPS 幅度极小、一卡一卡 | ①位移来自**光标绝对坐标差分**（受指针加速/边缘截断/事件合并影响）；②`skip_next` 与回中回声**竞态** → 吃掉真实位移并注入**反向**位移；③回中阈值 64px 太小；④`cursor_center()` 用 `SM_CXSCREEN` 只认主屏、不认 DPI 缩放；⑤Windows(屏幕像素) 与 Linux(设备计数) 单位不一致却共用同一个 sensitivity | ★★★ 已读 rdev 源码确认 | 第 4 位 |
| 6 | 断触（切焦点后失灵 / 快速按下短暂无响应） | ①`Held` **全文没有任何清空点**，丢一个 release 就少一次上升沿 + 留下 Hold 幻影触点；②通道未连接/断开时引擎直接 `continue`，**不 release_all** → `fingers.count` 虚高吃掉触点预算（F8 关一次即恢复，正好对上"必须重新映射"）；③设备端触点槽位只在收到 UP 时释放，漏一个 UP 就永久少一格；④**Windows 侧丢事件的源头**：rdev 在低级钩子里对每个 KeyPress 调 `get_name()` → `AttachThreadInput(前台窗口线程)` 同步阻塞 → 超过 `LowLevelHooksTimeout` → Windows 静默丢事件、反复超时还会静默摘钩子 | ★★★ 全部代码级确认 | 第 5 位 |
| 5 | 需要底层诊断日志 + 纠错体系 | 现状：只有 5 处 `eprintln!`，**无 panic hook**，`strip=true`，Windows GUI 版根本没有控制台 → 所有底层信息全部丢失 | ★★★ | **第 1 位** |

**施工顺序建议：0.2（恢复版本控制）→ 5（日志与自检）→ 3 → 6 → 4。**
理由：3 / 4 / 6 都是"偶发、跨设备、无法在开发机复现"的问题，没有 5 提供的现场证据，
任何修复都只是猜。5 做完之后，3 和 6 的验证成本会下降一个数量级。

---

## 2. 问题 3：Linux 下外接键盘无法操作

### 2.1 已证实的代码事实

**① 设备只在启动时枚举一次，没有任何热插拔机制**

`capture.rs:81` —— `platform_start()` 里 `for (path, device) in evdev::enumerate()`，
每个匹配的设备 `std::thread::spawn` 一个 `device_loop`（`capture.rs:103`），然后**这个函数就返回了**。

而 `evdev::enumerate()` 的实现（`evdev-0.13.2/src/raw_stream.rs:736`）就是一次 `read_dir("/dev/input")`：

```rust
pub fn enumerate() -> EnumerateDevices {
    EnumerateDevices { readdir: std::fs::read_dir("/dev/input").ok() }
}
```

全项目搜索无 `inotify`、无 `udev`、无定时重扫。**程序启动之后出现的任何输入设备都是隐形的。**

对上 §0.5 的内核日志：外接键盘在 22:29:31（从挂起恢复时）和 22:31:38（用户拔插后）两次枚举，
节点号从 input32/33/34/36 变成 input38/39/40/42。只要程序在这两个时刻之前就已启动，
它持有的仍是**旧节点号的 fd**，而旧节点已经不存在了。

**② 设备失联后线程永久空转，不退出也不重开**

`capture.rs:209-215`：

```rust
Err(e) if e.kind() == WouldBlock => sleep(2ms),   // 正常：没事件
Err(_) => sleep(50ms),                            // ← 设备被拔掉/USB 复位后走这里，永远出不来
```

设备被移除后 `fetch_events()` 返回 `ENODEV`，落到 `Err(_)` 分支 → 睡 50ms → 再试 → 再失败，
**20 Hz 无限空转**，既不上报、不退出、也不通知任何人。用户看到的就是"这个键盘彻底没反应了"，
而且**没有任何日志**。

这一条同时解释了"内置键盘好好的、外接键盘不行"：内置键盘走 i8042（`event3`），
节点号从开机到关机不变，永远不会 ENODEV；外接键盘走 USB，
**挂起/恢复、拔插、USB autosuspend 复位**都会换节点号。

**③ `enumerate()` 会静默跳过打不开的设备，且我们没有留下任何痕迹**

`evdev-0.13.2/src/lib.rs:475-477` 的文档原话：

> Will not bubble up any errors in opening devices or traversing the directory.
> Instead returns an empty iterator or **omits the devices that could not be opened**.

`capture.rs:107-129` 只在 `opened == 0`（**一个都没打开**）时才去探测 `/dev/input` 区分"没设备"和"没权限"，
然后 `bail!`。**部分设备打不开的情况完全静默** —— 比如内置键盘能开、某个 USB 节点因为
`udev` 规则/ACL 开不了，用户只会觉得"外接键盘坏了"。

而 `capture.rs:222-225` 的 `log_line()` 目前只被 `set_nonblocking` 失败这一处调用（`capture.rs:88`）。
**"我到底打开了哪几个设备、每个设备叫什么、判定成键盘还是鼠标、为什么跳过了某个设备"——一条都没有记录。**

### 2.2 次要嫌疑（需要日志才能确认，先别急着改）

**④ `is_keyboard()` 启发式可能漏判**（`capture.rs:135-144`）

判据是同一个 evdev 节点上同时具备 `KEY_A` + `KEY_Z` + `KEY_ENTER`。
外接键盘是复合设备（§0.5 里 SEMICO 一把键盘产生 **4 个 input 节点**），
标准键矩阵通常只落在其中一个节点上。若那把键盘的节点划分比较特殊（例如某些可编程键盘把
字母键放在一个只报 `KEY_*` 子集的节点上），就会被整体跳过。

**⑤ 同一把键盘的多个节点都判成键盘 → 同一次按键被上报两遍**

`capture.rs:97-101` 对每个匹配节点都开一个线程。若 interface 0 与 interface 1 的两个节点
都含 A/Z/Enter，一次物理按键会产生两条 `Button{pressed:true}`。
对 `Held` 是幂等的（`HashSet`），但**对 `Tap{duration:0}`（开关型）是致命的**：
两条上升沿之间夹着两条下降沿时，开关会翻两次 = 等于没按。
用户描述里的"有一个键出现短暂响应问题"（问题 6）也可能是这一条，而不是丢事件。
**必须靠日志区分**：记录每个事件的来源设备，就能一眼看出是不是重复上报。

**⑥ 既是键盘又是鼠标的设备（一体机/带触摸板的键盘/2.4G 套装接收器）拿错了 grab 标志**

`capture.rs:97-101`：`if keyboard { grab } else { mouse_grab }` —— 复合设备被判成键盘，
于是它的鼠标部分跟着**键盘** grab 标志走。后果：勾了"独占键盘"会连鼠标一起独占；
反过来 FPS 想要 `mouse_grab` 时它没被独占，系统光标照样能撞到屏幕边缘 → 位移丢失（见 §3.2）。

### 2.3 施工方案

**P0-1｜设备清单日志（依赖问题 5 的 diag 模块，先做这个）**

在 `platform_start()` 里，对 `enumerate()` 出来的**每一个**设备记一行：
路径、`device.name()`、`input_id()`（bus/vendor/product）、`is_keyboard` 判定结果、
`is_mouse` 判定结果、`set_nonblocking` 结果、最终"已打开/已跳过（原因）"。
另外单独扫一遍 `/dev/input/event*`，把"存在但打不开"的节点连同 `io::Error`（含 `raw_os_error()`）记下来。

> 这一条做完，问题 3 从"玄学"变成"看一眼日志就知道"。**这是整个 §2 里性价比最高的一步。**

**P0-2｜设备失联要能被发现**

`device_loop` 的 `Err(_)` 分支：区分 `ENODEV`/`EBADF`（设备真的没了）与其它错误。
设备没了就 **记日志 + 上报一个"设备丢失"事件 + 退出线程**，不要空转。
退出前如果处于 grab 状态，`ungrab()` 已经没意义（fd 已死），但要保证不再持有。

**P0-3｜热插拔重新枚举**

三选一，按代价从低到高：

1. **定时重扫（最省事，建议先做）**：起一个看门狗线程，每 2~3 秒 `read_dir("/dev/input")`，
   与"当前已打开的设备集合"（按路径 + `input_id` 去重）比对，发现新节点就走一遍
   `is_keyboard`/`is_mouse` + `set_nonblocking` + spawn。**注意去重要用设备身份而不是节点号**
   （节点号会变），并且新增设备时要更新 `mouse_found`。
2. **inotify 监听 `/dev/input`**：事件驱动、零轮询。Linux 专属，`inotify` crate 或直接 `libc`。
3. **`evdev::Device::into_event_stream()` + `futures` select**：evdev 0.13 自带异步流，
   但引入 async 运行时对一个同步架构来说代价偏大，不推荐。

推荐 **1**，因为：①不引新依赖；②2~3 秒的延迟对"插上键盘"这种操作完全够用；
③顺手就把 P0-2 的"设备丢失后重新找回"覆盖了。

**P0-4｜`mouse_found` 改成动态**

`capture.rs:105` 现在是启动时 `store(mice > 0)` 一次就再也不动。
重扫时一并更新。界面上「鼠标瞄准(FPS)」的"生效条件自检"第 2 项直接读它
（`app.rs:4461`），改完热插鼠标就不用重启程序了。

**P1-1｜复合设备的 grab 标志**

`is_keyboard && is_mouse` 的设备，grab 标志应当是"两个标志的或"，
或者干脆为它同时监听两个 AtomicBool（任一为真就 grab）。

**P1-2｜重复上报去重**

先靠 P0-1 的日志确认是否真的存在重复上报。确认后再决定：
按"同一物理设备（同一 `input_id` + 同一 USB 接口）只保留一个键盘节点"过滤，
或者在引擎侧对同一 code 的连续同向事件做幂等（`Held` 已经幂等，
真正要处理的是 `Tap{duration:0}` 这类边沿动作）。

**P1-3｜放宽 `is_keyboard`**

改成"含 `KEY_A` **或** 具备 ≥ 30 个 `KEY_*` 且含 `KEY_ENTER`"这类更宽松的组合，
并把判定依据写进日志（"因为它有 A/Z/Enter 所以判为键盘" / "它只有 12 个键所以跳过"）。
**改之前先有日志**，否则是在盲改。

### 2.4 验收

1. 启动程序 → 看日志里的设备清单，条数应与 `ls /dev/input/event*` 一致，且每条都有名字和判定结论。
2. 程序运行中插入外接键盘 → **不重启**，3 秒内日志出现"发现新设备 …，已打开"，按键立刻可用。
3. 程序运行中拔掉外接键盘 → 日志出现"设备 … 已移除"，且 CPU 占用不升高（证明没有 50ms 空转）。
4. 重新插回 → 再次自动打开。
5. `systemctl suspend` 唤醒后 → 所有设备自动恢复（这是 §0.5 内核日志里的真实场景）。
6. 故意 `sudo chmod 000 /dev/input/eventN` 一个节点 → 日志明确写出"打不开 + errno"，而不是静默。

---

## 3. 问题 4：FPS 鼠标瞄准

### 3.1 Linux（老 Fedora 不能用 / CachyOS 能用）

#### 已证实的事实

**① 触摸板根本不算鼠标，这是设计使然**

`capture.rs:148-155` 的 `is_mouse()` 要求设备同时具备 `REL_X` 和 `REL_Y`。
本机实测：Synaptics 触摸板 `rel=0`（现代触摸板走 ABS 多点触控协议，没有 REL 轴）→
**用触摸板移动，一个 `Motion` 事件都不会产生**，FPS 必然完全没反应。
TrackPoint `rel=3`、USB 鼠标 `rel=3` → 这两个可以用。

"另一台 CachyOS 能用"很可能只是因为那台机器接了真鼠标，或者用的是 TrackPoint/触控笔之类有 REL 轴的设备。
**先去问用户：不能用的那台上，他是用什么在动视角？** 如果是触摸板，那不是 bug，是缺功能。

**② `mouse_found` 是启动时的一次性快照**（`capture.rs:105`）

热插鼠标之后它永远是 `false`，界面自检第 2 项永远打 ✗。
与问题 3 是同一个根因（无热插拔），修法见 §2.3 的 P0-3 / P0-4。

**③ ⚠️ YAML 迁移把旧的 aim 配置全丢了**

`Aim::default()`（`keymap.rs:710-726`）是：

```
enabled: false, anchor_x: 0.0, anchor_y: 0.0, sensitivity: 2.0/2.0,
recenter: Idle, recenter_idle_ms: 120, recenter_threshold: 400,
hold_key: 0, capture_mouse: true
```

而 `aim_active()`（`engine.rs:540-545`）要求
`enabled && anchor_set() && !released && (hold_key==0 || gate_down)`，
`anchor_set()`（`keymap.rs:730-732`）判据是 `|anchor_x| > 1e-6 || |anchor_y| > 1e-6`。

也就是说 **默认配置下 FPS 一定是关的**。上一轮改动按用户要求"不做旧版兼容/转换"，
`profile.json` 不再被读取，所以**用户原来在 profile.json 里配好的 aim（启用状态、锚点、灵敏度、门控键）全部丢失**，
而 `~/.config/scrcpy-pad/profile.json`（3042 字节，实测仍在）还原封不动躺在那里。

**先去确认用户是否重新配过 aim。** 如果没有，这一条就是"fps 功能无法使用"的全部答案，
不需要改任何代码 —— 但**必须把这件事明确告诉用户**，因为他不会想到是配置迁移导致的。
（可选的补救：写一个一次性的"从旧 profile.json 导入 aim 段"的按钮，不做全自动迁移，
这样既不违背用户"不必兼容"的要求，又能救回他的锚点。）

#### 排查决策树（界面已经自带，直接用）

左栏「鼠标瞄准(FPS)」面板（`app.rs:4432-4520`）已经有一个很好的自检区，**先让用户念出这几行**：

```
生效条件自检:
  ✓/✗ 已勾选 [启用鼠标瞄准]
  ✓/✗ 检测到鼠标设备
  ✓/✗ 已设置锚点
  ✓/✗ 映射已开启
  ✓/✗ 控制通道已连接
触摸坐标空间: WxH (横屏/竖屏)
运行状态: 位移N次 最近(dx,dy) 偏移(ox,oy) 触点按下/抬起 已注入M条
```

判读方式（**这三段是 FPS 问题的完整分诊法，务必写进问题 5 的纠错规则表**）：

| 现象 | 结论 | 往哪儿查 |
|---|---|---|
| `位移N次` 的 N **不增长** | 捕获层没有产生 `Motion` | `is_mouse` 判定 / 热插拔 / 触摸板无 REL 轴 / 设备线程已 ENODEV 空转 |
| N 增长，`已注入M条` 的 M **不增长** | 事件到了引擎但没注入 | `aim_active()` 四条件、`s.enabled`、`ctl.is_connected()`、触点池满（看左栏"引擎: 触点 x/10 · 被放弃 y 次"，`app.rs:2549-2552`） |
| N、M 都增长，手机没反应 | 传输或坐标问题 | 锚点是否在屏幕内（面板会红字提示）、坐标空间方向是否对、控制通道是否假活 |
| N 增长但 `最近(dx,dy)` 恒为极小值或**负值** | 位移来源有问题 | Windows 见 §3.2；Linux 检查是不是复合设备的 grab 标志错了（§2.2 ⑥） |

> 注意 `motions` 计数在 `engine.rs:1041` 是在 `if s.enabled` **之前**加的，
> 所以映射关着的时候它也会涨。用它判"捕获层通不通"是可靠的。

### 3.2 Windows（能动，但幅度极小、一卡一卡）

先说结论：**这不是灵敏度调小了，是位移事件在被系统性地吃掉、甚至反向注入。**

#### 根因 ①：位移来源是"光标绝对坐标差分"，不是原始输入

已读 `rdev-0.5.3/src/windows/common.rs:77-83` 确认：

```rust
Ok(WM_MOUSEMOVE) => {
    let (x, y) = get_point(lpdata);          // = MSLLHOOKSTRUCT.pt
    Some(EventType::MouseMove { x: x as f64, y: y as f64 })
}
```

`MSLLHOOKSTRUCT.pt` 是**已经过 Windows 指针加速（"提高指针精确度"）处理后的屏幕绝对坐标**。
用它做差分有三重损失：
- 非线性加速 → 同样的手速，慢移和快移得到的 dx 不成比例，灵敏度手感完全不对；
- 系统会**合并**（coalesce）鼠标移动消息 → 高频鼠标（1000Hz）的中间位置被丢弃；
- 光标撞到屏幕边缘后坐标不再变化 → 位移彻底丢失（这正是代码要用"回中"hack 的原因）。

#### 根因 ②：`skip_next` 与回中回声的竞态 → 吃掉真实位移 + 注入反向位移

`capture.rs:307-336` 的逻辑（简化）：

```
d = (x,y) - last            // 差分
last = (x,y)                // ← 先无条件更新 last
if skip_next { skip_next = false }        // 丢弃这一条
else if d != 0 { send(Motion{d}) }
if 抓取中 && |x-center| > 64 {
    SetCursorPos(center);  last = center;  skip_next = true
}
```

设计意图是：`SetCursorPos` 自己会触发一次 `WM_MOUSEMOVE`（"回声"），用 `skip_next` 把它丢掉。
**但回声到达的时机没有保证。** 物理鼠标移动快时，真实事件可能**抢在回声之前**到达：

| 步骤 | 事件 | `last` 变化 | 发出去的 Motion |
|---|---|---|---|
| A | 真实移动到 center+70 | center → center+70 | **+70** ✓，随后触发回中：`SetCursorPos(center)`、`last=center`、`skip_next=true` |
| B | **真实**移动到 center+30（抢在回声前） | center → center+30 | **被 `skip_next` 丢弃，+30 丢失** |
| C | 回声到达，位置 = center | center+30 → center | d = center − (center+30) = **−30 → 被当成真实位移发出去** |

净效果：本该 +100，实际只发出 +70 −30 = **+40**，而且中间夹了一次**反向**位移。
鼠标越快 / DPI 越高，回中越频繁，损失比例越大 ——
**这与用户描述的"移动范围极为微小、一卡一卡地停顿"完全吻合。**

还有一个独立的坏情况：**回声可能根本不来**（`SetCursorPos` 到当前位置、或系统合并掉了），
那 `skip_next` 会一直挂着，白白吃掉下一条**真实**位移。

#### 根因 ③：回中阈值 64px 太小

`capture.rs:352` `CURSOR_RECENTER_PX = 64.0`。在 1080p 上，1600 DPI 的鼠标
一次快速甩动单个事件就能走 50~100px，等于**几乎每个事件都触发一次回中**，
配合根因 ② 就是"几乎每个事件都被吃掉一次"。

#### 根因 ④：`cursor_center()` 只认主显示器、不认 DPI 缩放

`capture.rs:356-359`：

```rust
(GetSystemMetrics(SM_CXSCREEN) / 2, GetSystemMetrics(SM_CYSCREEN) / 2)
```

`SM_CXSCREEN/SM_CYSCREEN` 是**主显示器的分辨率**，而且返回的是**当前进程 DPI 感知级别下**的值。
后果：
- **多显示器**：光标在副屏上时，其坐标可能完全落在 `[0, SM_CXSCREEN]` 之外 →
  回中条件**每个事件都成立** → 每个事件都 `SetCursorPos` 到主屏中心 + `skip_next` 丢弃 →
  **位移几乎全丢**，同时光标被死死钉在主屏中央。这是"幅度极小 + 卡顿"的另一个充分解释。
- **系统缩放 125%/150%**（笔记本默认就是）：进程若非 Per-Monitor DPI Aware，
  拿到的是虚拟化后的值，与 `MSLLHOOKSTRUCT.pt` 的坐标系不一致 → 中心点算错 → 同上。

#### 根因 ⑤：两个平台的 `Motion` 单位不一致，却共用同一个 `sensitivity`

- Linux（`capture.rs:195-207`）：`dx/dy` 是**设备计数**（mouse counts，取决于 DPI，一次事件通常 1~20）。
- Windows（`capture.rs:307-321`）：`dx/dy` 是**屏幕像素**，且经过指针加速。

引擎里 `st.ox += dx * aim.sensitivity_x`（`engine.rs:638-644`）用的是同一个系数，默认 2.0。
同一份配置在两个平台上的手感必然差好几倍，而且 Windows 侧还非线性。
**这一条不是"卡顿"的原因，但是"跨平台配置不通用"的原因，必须一并解决。**

#### 施工方案（按推荐度排序）

**方案 A（推荐）：Windows 改用 Raw Input，彻底抛弃光标差分**

- `RegisterRawInputDevices` + `RIDEV_INPUTSINK`（后台也能收），窗口消息 `WM_INPUT` → `GetRawInputData`
  拿到 `RAWMOUSE.lLastX/lLastY`：**未经加速、未合并、与光标位置无关的原始计数**。
- 好处一次性解决根因 ①②③④⑤：不需要回中、不需要 `skip_next`、不受屏幕边缘/多屏/DPI 影响、
  单位与 Linux 的 evdev 计数一致（都是 device counts），`sensitivity` 从此跨平台通用。
- 需要新增 `windows-sys` feature：`Win32_UI_Input_KeyboardAndMouse`（`RegisterRawInputDevices`/`GetRawInputData` 在这里），
  现有 feature 只有 `Win32_Foundation` + `Win32_UI_WindowsAndMessaging`（见 `Cargo.toml`）。
- 隐藏/冻结光标仍然用现在的 `ShowCursor`，但**不再需要 `SetCursorPos` 回中**。
- Raw Input 需要一个窗口来接收 `WM_INPUT`。可以建一个 `HWND_MESSAGE` 消息窗口（不需要可见），
  或用 `RIDEV_INPUTSINK` + 一个隐藏顶层窗口。**注意 rdev 已经在自己的线程里跑 `GetMessageA` 消息循环**
  （`rdev-0.5.3/src/windows/listen.rs:53`），别和它抢同一个线程。

**方案 B（过渡，代价小）：先修竞态与阈值，把 Windows 拉回"能用"**

如果暂时不想动 Raw Input，至少要做：
1. **删掉 `skip_next`**，改成"识别回声"：`SetCursorPos` 之前记下 `center`，
   收到的下一条事件若**位置恰好等于 center**才判为回声并丢弃；否则按真实位移处理。
   （更稳的做法：`MSLLHOOKSTRUCT.flags & LLKHF_INJECTED` 判注入来源 —— 但 rdev 没暴露 flags，
   所以要么自己装钩子，要么用位置比对。）
2. 回中阈值从 64 提到 **屏幕短边的 1/4 左右**，并且用 `GetSystemMetrics(SM_CXVIRTUALSCREEN)/SM_CYVIRTUALSCREEN`
   或者干脆"光标进入任一显示器边缘 N px 内才回中"。
3. `cursor_center()` 改成 `MonitorFromPoint` + `GetMonitorInfo` 取**光标所在显示器**的中心；
   并在 app manifest 里声明 Per-Monitor V2 DPI Aware。
4. 建议同时把 `SetCursorPos` **移出钩子回调**（丢给另一个线程执行），
   理由见 §4.2 根因 ④ —— 在 LL hook 里做同步 Win32 调用是超时丢事件的元凶之一。

**方案 C（Linux 侧的补强）：让触摸板也能用于 FPS**

触摸板没有 REL 轴（§3.1 ①）。若要支持，需要读 `ABS_MT_POSITION_X/Y` 自己算差分，
并且要处理多点触控（哪一根手指算视角）。**这是新功能，不是修 bug，建议单独立项，
优先级低于 A/B。** 短期内先在界面上明确告知："触摸板不能用于 FPS 瞄准，请接鼠标"，
把 `mouse_found` 那一项的文案改成能说明原因的（例如"检测到鼠标设备（触摸板不支持，需接鼠标）"）。

#### 验收

1. Windows：`aim_live.last_dx` 的量级应与鼠标 DPI 匹配（1600 DPI 慢移约 5~20/事件），**不应出现负值**（除非真的反向移动）。
2. Windows：快速甩动 10 次，手机侧视角转动幅度应基本一致，无"卡顿感"。
3. Windows：双显示器、光标放副屏 → FPS 正常，且光标不会被拽回主屏。
4. Windows：系统缩放 150% → 同上。
5. 同一份配置（同 sensitivity）在 Linux 与 Windows 上转同样角度所需的手部位移应大致相同（方案 A 之后才能达成）。
6. 关掉"提高指针精确度"与打开它，手感**不应有差别**（方案 A 的判据）。

---

## 4. 问题 6：断触

用户描述的两个症状，**分别对应两条独立的机制，都要修**：

- 症状甲：「切换程序焦点后直接无法操作程序，必须重新映射」→ 机制 ② + 机制 ④
- 症状乙：「操作了一会，手部快速按下后，有一个键出现短暂响应问题」→ 机制 ① + 机制 ④

用户已用机械键盘复测，排除了硬件 ghosting（上一轮定性过的键盘矩阵歧义这次不成立）。

### 4.1 机制 ①：`Held`（物理按键镜像）**永远不被清空**

`Held` 定义在 `engine.rs:158-184`，全项目搜索 `held` 的结果里，
**唯一的写入点是 `engine.rs:1095`**：

```rust
let fresh_press = rising_edge(ev.pressed, held.has(ev.code));   // 1094
held.set(ev.code, ev.pressed);                                  // 1095  ← 唯一写入点
```

`release_all()`（`engine.rs:799-850`）清了 `fingers`、`wheels`、`active_android_keys`、`aim`，
**唯独没清 `held`**。映射开关、通道重连、配置改动、窗口焦点变化、设备重新枚举 —— 没有任何一处复位它。

**后果（精确对应用户的两个症状）：**

一旦某个键的 release 事件丢失（原因见机制 ④），`held` 里就永久留着这个 code：

1. `fresh_press = rising_edge(true, held.has(code)=true) = false`
   → 所有**只认上升沿**的动作全部失效：`Tap{duration:0}`（开关型点按）、`Tap{duration>0}`、`Swipe`、
   总开关键、切换按键组合的键、Ctrl+Alt 交还鼠标、`TempMode::Toggle` 型启用键。
2. 对 `Hold` / `AndroidKey` 型绑定，`reconcile_binds`（`engine.rs:224`）算出
   `want = held.has(bind.key) = true` → **凭空按下一个幻影触点**，占掉一个设备端槽位。
3. 用户再完整地按一次、松一次这个键：按下时 `held` 已有它（无变化），
   **松开时 `held.remove(code)` 生效 → 状态被治愈**。
   → 这正是"**短暂**响应问题"：坏一次，一个完整的按下-松开周期之后自动恢复。

### 4.2 机制 ②：通道不可用时引擎直接 `continue`，**不做任何收尾**

`engine.rs:1243-1253`：

```rust
if !enabled { continue; }
let g = shared.lock().unwrap();
let Some(ctl) = g.control.as_ref() else { continue; };   // ← 未连接：事件被吃掉
if !ctl.is_connected() { continue; }                     // ← 通道断了：事件被吃掉
```

关键在于：**这两条 `continue` 之前没有 `release_all`**。所以通道断开的那一刻，
`fingers.down[]` 里凡是 `true` 的槽位会**一直留着**，`fingers.count` 一直虚高。

而 `Fingers::try_down`（`engine.rs:123-138`）的两道闸门：

```rust
if self.is_down(idx) || self.count >= MAX_CONCURRENT_KEYS { return false; }   // 8
if self.count + others >= DEVICE_MAX_POINTERS { return false; }               // 10
```

→ `count` 虚高会**永久吃掉触点预算**，新的按下被 `refuse`（`engine.rs:1057`、`reconcile_binds` 内）。
用户看到的就是"按什么都没反应"。

**为什么"必须重新映射"能治好**：总开关键在 `engine.rs:1130` 处理，**位置在 1243 的 `continue` 之前**，
所以 F8 永远有效；关映射会走 `release_all` → `fingers.free_all()`（`engine.rs:844`）→ `count = 0` → 预算恢复。
**用户"必须重新映射"这个自救动作，反过来正好证明了这条机制。**

同时左栏「引擎: 触点 N/10 · 因触点池满被放弃 M 次」（`app.rs:2549-2552`）会**在用户没按任何键时显示 N > 0**。
这是本机制的**现场判据**，让用户复现时立刻截图/报数即可确认，不需要猜。

`app.rs:2275-2280` 检测到断开后把 `control` 置 `None` 并打一条"控制通道已断开"，
**但引擎侧的 `fingers`/`wheels`/`aim` 完全没有被告知**。

### 4.3 机制 ③：设备端触点槽位泄漏（不可逆，只能重连）

已核对 scrcpy 4.1 服务端源码：

`PointersState.java:12` `MAX_POINTERS = 10`；
`:97-104` `cleanUp()` **只移除 `isUp()` 的指针**；
`:49-68` `getPointerIndex()` 在 `pointers.size() >= MAX_POINTERS` 时返回 `-1`；
`Controller.java:523-527` 收到 `-1` 就打一句 `Ln.w("Too many pointers for touch event")` 然后 `return false`
—— **静默丢弃，客户端毫不知情**。

也就是说：**漏发一个 UP，设备端就永久少一格，直到 Controller 重建。**
而我们的代码里有三处会漏发 UP：

1. `aim_release_local()`（`engine.rs:561-566`）：只把 `aim.down = false`，**不发 `touch_up(AIM_PID)`**。
   它在 `engine.rs:992`（通道不在/未连接时）**每轮循环都会被调用**，也在 `release_all` 的 `None` 分支
   （`engine.rs:841`）里被调用。→ 瞄准触点 `AIM_PID = 3000` 一旦在通道抖动时落下，就永久泄漏一格。
2. `release_all(ctl = None)` 分支（`engine.rs:839-842`）：`active_android_keys.clear()` + `fingers.free_all()`
   全部只清本地，**一个 UP 都没发**。
3. **重连不补发**：`app.rs:2030-2031`
   ```rust
   self.server = Some(server);
   self.shared.lock().unwrap().control = Some(client);
   ```
   旧 `ControlClient` 被直接 drop，**没有先在旧通道上抬起所有触点**。
   旧 server 进程若还活着（`ControlServer::drop` 只 kill 主机侧 adb 子进程并 `forward --remove`，
   见 `adb.rs:545-553`，设备端进程未必立刻退出），它持有的触点就全泄漏了。

### 4.4 机制 ④：Windows 侧为什么会丢事件（症状的**触发源**）

机制 ①②③ 都是"丢了事件之后会怎样"，那**事件为什么会丢**？Linux 侧的答案是设备失联（§2.1 ②），
Windows 侧的答案在 rdev 0.5.3 里，已逐行读过：

`rdev-0.5.3/src/windows/listen.rs:20-42` 的钩子回调：

```rust
unsafe extern "system" fn raw_callback(code, param, lpdata) -> LRESULT {
    if code == HC_ACTION {
        let opt = convert(param, lpdata);
        if let Some(event_type) = opt {
            let name = match &event_type {
                EventType::KeyPress(_key) => match (*KEYBOARD).lock() {
                    Ok(mut keyboard) => keyboard.get_name(lpdata),   // ← 每个 KeyPress 都走这一趟
                    Err(_) => None,
                },
                _ => None,
            };
            ...
            if let Some(callback) = &mut GLOBAL_CALLBACK { callback(event); }
        }
    }
    CallNextHookEx(HOOK, code, param, lpdata)
}
```

`get_name()`（`rdev-0.5.3/src/windows/keyboard.rs:36-114`）在**低级键盘钩子的回调里**做了这些事：

```
GetKeyState(VK_SHIFT)
GetForegroundWindow()                       ← 取当前前台窗口
GetWindowThreadProcessId(...)               ← 取前台窗口的线程 id
AttachThreadInput(我们的线程, 前台线程, TRUE)   ← ★ 同步附着到另一个线程的输入队列
GetKeyboardState(state_ptr)
AttachThreadInput(..., FALSE)
GetForegroundWindow()  (第二次)
GetWindowThreadProcessId(...)  (第二次)
GetKeyboardLayout(前台线程)
ToUnicodeEx(...)
String::from_utf16(...)                     ← 堆分配
（若命中死键，再 ToUnicodeEx 一次；死键分支里还有一个 while len < 0 的循环）
```

**`AttachThreadInput` 是这里的核心问题。** 微软文档明确警告它开销大：
它会把两个线程的输入队列同步起来，**调用会阻塞到对方线程处理完输入队列为止**。
如果前台窗口的线程没有及时泵消息（scrcpy 正在解码渲染、游戏正在加载、
浏览器正在跑脚本、或者前台线程干脆卡住了），这个调用就会**在低级钩子里阻塞**。

而 Windows 对低级钩子有硬性时限 `LowLevelHooksTimeout`
（`HKCU\Control Panel\Desktop\LowLevelHooksTimeout`，默认 **300ms**）：
- 超时 → **该事件被静默丢弃**（不投递给目标线程，也不给下一个钩子）；
- 反复超时 → Windows **静默移除整个钩子**，此后所有输入事件都收不到。

**"切换程序焦点"正是 `GetForegroundWindow()` 返回值变化的时刻** ——
新的前台线程可能是 scrcpy 的视频窗口（正在高频渲染）或游戏本身，
`AttachThreadInput` 卡住的概率在这时最高。
这与用户"切换程序焦点后直接无法操作"的描述**精确对应**。

而"手部快速按下"对应的是：单位时间内 KeyPress 数量暴增 → 每个都要走一遍上面那串调用 →
累积延迟逼近 300ms → 开始丢事件，丢的往往就是某个 **KeyRelease** → 机制 ① 启动 → "一个键短暂失灵"。

**最荒谬的一点：`event.name` 我们从来没用过。**
`capture.rs:274-338` 只读 `event.event_type`。
也就是说，**我们为一个完全用不到的字段，付出了整个程序最昂贵的性能与正确性代价。**

补充两个 rdev 0.5.3 的结构性缺陷：
- `common.rs:22` `pub static mut HOOK: HHOOK` 是**单个**静态，
  `set_key_hook` 之后 `set_mouse_hook` 会把它覆盖掉 → **键盘钩子的句柄丢失，无法 `UnhookWindowsHookEx`**。
- `listen.rs:44-56` 的 `listen()` 最后 `GetMessageA(...)` 阻塞，**永不返回，也没有任何卸载入口**。
  → 钩子一旦被系统摘掉，**我们无法察觉、也无法重装**。这就是"必须重启程序"的另一半原因。

### 4.5 已排除的假设（别在这些方向上浪费时间）

**✗ "注入消息里的 `screen_w/h` 过期导致服务端丢弃事件"**

我一度认为这是主因，核对 scrcpy 源码后**排除**：

- `PositionMapper.java:34-47` 确实在 `!videoSize.equals(clientVideoSize)` 时 `return null`（整条丢弃）；
- **但** `Controller.java:492-507` 只在 `displayData != null` 时才走 `positionMapper.map()`，
  否则走 `else { // No display, use the raw coordinates }`；
- 我们是 **control-only 模式**（`video=false`，见 `adb.rs:487-537` 的 `start_control_server`），
  没有视频采集 → 不会触发 `onNewVirtualDisplay` → `displayData` 恒为 `null` → **原始坐标直接用，w/h 不参与判定**。

不过这里仍留着一个**陷阱**（见 §6.1），改协议相关代码时别踩。

**✗ 键盘硬件 ghosting/blocking**：上一轮已定性过一次，用户本次用机械键盘复测排除。

**✗ 触点池被正常操作占满**：`MAX_CONCURRENT_KEYS=8` + `DEVICE_MAX_POINTERS=10`，
正常玩法（几个键 + 1~2 个摇杆 + 1 个瞄准）远不到上限。
只有在机制 ②③ 造成**虚高/泄漏**之后才会撞上限。
所以看到"被放弃 N 次"不要直接去调大常量，**先查为什么 count 虚高**。

**✗ `refresh_display_space` / `space_recheck` 把坐标搞坏**：
`app.rs:4407-4421` 后台查 `adb::display_size`，失败回传 `(0,0)`；
`sync_display_space`（`app.rs:4127-4130`）对 `w==0 || h==0` 直接 return。良性。

### 4.6 施工方案

**P0-1｜给 `Held` 加"权威对账"能力（Linux 有现成的内核接口）**

`evdev 0.13.2` 提供 **`Device::get_key_state() -> io::Result<AttributeSet<KeyCode>>`**
（`sync_stream.rs:282`、`raw_stream.rs:459`，底层是 `EVIOCGKEY` ioctl）。
它直接返回**内核此刻记录的"哪些键正被按住"**，是绝对权威的地面真值。

用法：让 `capture` 层保留每个键盘设备的句柄（或提供一个 `resync_keys()` 回调），
在关键时刻查询所有键盘设备的当前按键集合，合并成一份"物理真值"推给引擎，
引擎用它**整体替换** `held`，然后立刻 `reconcile_binds` + `reconcile_wheels` 一次。

Windows 侧对应的真值是 `GetAsyncKeyState(vk) & 0x8000`，逐键扫描我们关心的 vk 集合即可
（`GetAsyncKeyState` 很便宜，可以每 100~200ms 扫一次，**但绝不能放进 LL 钩子回调里**）。

**P0-2｜在四个时机强制全量对账（这是修断触的核心）**

| 时机 | 现在做了什么 | 应该做什么 |
|---|---|---|
| 映射 关→开 | `aim.released = false`（`engine.rs:1165`） | 清空 `held` 并按物理真值重建 → `release_all` → `reconcile_*` |
| `control` 从 `None`→`Some`（重连成功） | 只 `sync_display_space` | 先假定设备端**干净**：`fingers.free_all()` + `wheels`/`aim`/`active_android_keys` 全清本地状态，再按 `held` 真值重建 |
| 通道断开（`is_connected()` 变 false） | **什么都不做**，直接 `continue` | 立刻 `release_all(ctl=None)` 清本地，并记一条"欠设备端 N 个 UP"的诊断日志 |
| 窗口重获焦点 | 无 | 触发一次 `resync_keys()` + 对账 |

**P0-3｜幻影键看门狗（兜底，跨平台都要有）**

给 `Held` 里每个 code 记一个"最后一次收到该键事件的时刻"。
Linux 上按住不放会有 `value == 2` 的自动重复（`capture.rs:186-189` 现在直接 `continue` 丢掉了 —— 
**改成用它来喂狗**，不要发给引擎但更新时戳），间隔约 30~50ms。
Windows 上 rdev 也会重复上报 `KeyPress`。
所以：**某个键在 `held` 里超过 ~500ms 没有任何后续事件 → 判定 release 丢失 → 主动移除并按物理真值对账**，
同时记一条 WARN 日志（这条日志本身就是"我们在丢事件"的铁证，可以直接拿来验证机制 ④）。

**P0-4｜Windows：把 rdev 换成自己装的钩子（或至少让回调变轻）**

目标：**钩子回调里只做"塞进无锁队列"这一件事**，然后立刻 `CallNextHookEx` 返回。

- 不要调 `get_name()` / `AttachThreadInput` / `ToUnicodeEx` / `SetCursorPos` / 任何加锁操作。
  我们自己的回调（`capture.rs:251-339`）现在里面有 `tx.lock()`（`capture.rs:269`）和
  `SetCursorPos`（`capture.rs:332`），**这两个也必须移出去**。
- 用 `crossbeam`/`ringbuf` 之类的无锁队列，或者 `std::sync::mpsc::Sender`（`Sender` 本身不需要额外 Mutex；
  现在套一层 `Mutex<Sender>` 只是为了绕 `FnMut` 的捕获限制，换成 `move` 进闭包或用 `Sender::clone` 就不需要了）。
- 自己 `SetWindowsHookExA(WH_KEYBOARD_LL/WH_MOUSE_LL, ...)`，**分别保存两个 HHOOK**，
  提供 `unhook()`；并起一个健康检查（例如定期比较"上次收到事件的时间"，
  长时间无任何事件且用户明显在操作 → 怀疑钩子被摘 → 重装并记日志）。
- 顺便解决 §3.2 的方案 A（Raw Input 走同一套窗口消息循环）。
- **过渡措施**（如果一时不想弃用 rdev）：`rdev::listen` 的回调里，把整个事件**原样丢进队列就返回**，
  所有处理（包括 `SetCursorPos` 回中）搬到消费线程去做。这样至少把 `SetCursorPos` 移出了钩子。
  但 `get_name()` 是 rdev 在调我们的回调**之前**做的（`listen.rs:24-30`），**绕不过去** ——
  所以这条路只能缓解，不能根治。根治必须自建钩子。

**P1-1｜重连前先在旧通道上收尾**

`app.rs:2030-2031` 之前，若旧 `control` 还在且 `is_connected()`，先走一遍 `release_all(Some(old_ctl), ...)`。
并且**先 drop 旧 `ControlServer`（kill 旧 adb）再启动新的**，避免 §6.2 的端口/socket 竞态。

**P1-2｜互斥锁中毒**

全项目 30+ 处 `shared.lock().unwrap()`（`engine.rs` 里就有 9 处：907/929/964/1039/1081/1131/1218/1247/1562）。
**任何一个线程在持锁时 panic，Mutex 就中毒，之后每一处 `.unwrap()` 都会 panic** → 引擎线程死亡 →
`rx` 被 drop → 捕获层 `tx.send()` 全部静默失败 → **整个程序彻底失去输入，只能重启**。
建议统一改成 `lock().unwrap_or_else(|e| e.into_inner())`（中毒后仍取出数据继续跑），
并在恢复时记一条 ERROR 日志。这是"必须重启程序"这类最坏情况的最后一道保险。

**P1-3｜安装 panic hook**（与问题 5 合并做，见 §5.3）

`main.rs` 现在**没有** `std::panic::set_hook`。引擎线程 panic 只会往 stderr 打一行，
而 Windows GUI 版根本没有 stderr。

### 4.7 验收

**可复现的构造实验（不需要等偶发）：**

1. **验证机制 ①**：在 `device_loop` 里临时加一个"丢弃所有 release 事件"的调试开关
   （或用 §5 的日志级别控制），按一个绑了 `Tap{duration:0}` 的键 → 应复现"按第二次没反应"；
   修好后（看门狗介入）应在 ~500ms 内自愈并留下 WARN 日志。
2. **验证机制 ②**：开映射、按住 3 个 Hold 键，然后拔掉手机 USB / `adb kill-server` 让通道断开。
   看左栏「引擎: 触点 N/10」——**修复前 N 会停在 3 不动**；重连后按任何键都无反应；
   按 F8 关一次即恢复。修复后：断开瞬间 N 归 0，重连后立刻可用。
3. **验证机制 ③**：反复执行实验 2 十次，然后正常操作。修复前设备端槽位会逐步泄漏到 10，
   日志（服务端 `adb logcat`）出现 `Too many pointers for touch event`；修复后不会。
4. **验证机制 ④**：Windows 上开启 FPS 瞄准（`capture_mouse=true`），
   把前台窗口切到一个**故意不泵消息**的程序（例如一个 `Sleep(100000)` 的窗口），
   然后快速连按键盘。修复前会大面积丢事件；修复后钩子回调耗时应稳定在微秒级
   （在回调里打点计时，写进 §5 的日志，取 P99）。
5. **现场判据（给用户的）**：复现"无法操作"时，**先别按 F8**，
   立刻看左栏「引擎: 触点 N/10 · 因触点池满被放弃 M 次」。
   N > 0 而手没按键 → 机制 ②/③ 确认；N = 0 且 M 不涨 → 是事件没进来，走机制 ①/④ 的排查。

---

## 5. 问题 5：底层诊断日志 + 纠错体系

### 5.1 现状盘点（**几乎等于没有**）

全项目 `eprintln!` 只有 5 处：

| 位置 | 内容 |
|---|---|
| `capture.rs:224` | `[capture] 设置非阻塞失败 {path}: {e}` |
| `capture.rs:341` | `[capture] rdev 监听失败: {e:?}` |
| `settings.rs:319` | `[settings] {path} 解析失败({e})，改用默认设置` |
| `app.rs:579` | `[font] 未找到中文字体…` |
| `app.rs:5024` | `[profile] {path} 解析失败({e})，改用默认配置` |

以及 `main.rs:63` 的 `--selftest` 用 `println!` 打 12 项结果（只在手动跑 selftest 时可见）。

**三个致命缺口：**

1. **没有 panic hook**（全项目搜 `set_hook` / `panic::` 无结果）。线程 panic 只往 stderr 打。
2. **Windows GUI 版没有控制台** → 上面所有 `eprintln!` 全部进黑洞。
   而问题 3/4/6 里最需要现场证据的恰恰是 Windows 侧。
3. `[profile.release] strip = true`（`Cargo.toml`）→ 即使拿到 backtrace 也没有符号，只有地址。

界面内日志（`self.log`）是 UI-only 的，**不落盘**，除非用户手动点「保存日志」。
而且它只记"业务层结论"，不记任何底层异常。

### 5.2 落盘位置

用户要求"在和默认键位同级的地方"，即 `app::config_dir()`（`app.rs:4976-4986`）：

```
Linux   : ~/.config/scrcpy-pad/
Windows : %APPDATA%\scrcpy-pad\config\
macOS   : ~/Library/Application Support/dev.scrcpy-pad/
（取不到时退化为程序所在目录 = 便携模式）
```

同目录已有：`profile.yaml`（键位）、`look.json`（外观缓存）、`settings.json`（scrcpy 路径）。

**建议文件名：`diagnostics.log`**，语义是"本次运行的完整底层记录"。
- **每次启动 truncate 重写**（用户明确要求"每次运行后完全更新"）→ 用 `File::create()` 而不是 append。
- 若想保留上一次的现场，可在启动时把旧文件改名为 `diagnostics.prev.log`（只留一份），
  **但这与用户"完全更新"的要求有冲突，先按 truncate 做，是否保留上一份问过用户再定。**
- 注意 `.gitignore` 里有 `*.log`，所以万一落到仓库目录也不会被提交。
- 目录不可写时（便携模式 + 只读介质）必须优雅降级为"只写 stderr"，**绝不能因为写日志失败而 panic**。

### 5.3 建议新建 `src/diag.rs`

**API 形态（示意，不是实现）**：一个全局 `OnceLock<Mutex<BufWriter<File>>>` + 宏 `diag!(LEVEL, tag, fmt..)`，
外加 `diag_install_panic_hook()`、`diag_snapshot_env()`、`diag_flush()`。

**必备要素：**

1. **每行格式**：`[相对时间 ms][绝对时间][级别][模块 tag][线程名] 消息`。
   相对时间对分析"卡顿/超时/竞态"至关重要（例如量 LL 钩子回调的耗时分布）。
2. **级别**：`ERROR / WARN / INFO / DEBUG / TRACE`，通过环境变量（如 `SCRCPY_PAD_LOG=trace`）
   或 `settings.json` 里的字段控制。**默认 INFO**：既能拿到关键异常，又不会因为 TRACE 把磁盘写满。
3. **panic hook**：`std::panic::set_hook` → 写 ERROR + `Backtrace::force_capture()` + 线程名 + 当前配置摘要，
   然后 `flush`。**并且要保证 hook 自身不会 panic**（内部全部 `let _ =`）。
   同时建议：release 构建单独提供一个不 `strip` 的变体（或在 `build.sh` 里额外产出
   `scrcpy-pad-debug`），否则 backtrace 只有地址没符号，等于白记。
4. **`BufWriter` + 显式 flush 策略**：普通行缓冲写入，**ERROR/WARN 与 panic 立即 flush**。
   否则程序崩溃时最关键的几行还在缓冲区里没落盘 —— 那就完全失去意义了。
5. **高频事件绝不逐条写盘**。鼠标 `Motion` 在 1000Hz 鼠标上每秒上千条。
   做法：引擎/捕获层维护**计数器与最近值**（现成的 `AimLive`/`EngineLive` 就是这个思路），
   每 500ms~1s 打一条聚合快照；只有在"状态跃迁"（首次落下瞄准触点、首次被 refuse、
   通道断开、设备丢失）时才逐条记。
6. **不要引入 `log`/`tracing` 之外的重依赖**。项目现在依赖很干净，
   建议直接用 `log` + 一个手写的 `Write` 实现，或者干脆全手写（不到 200 行），避免 async 化。

### 5.4 必须记录的内容清单

**A. 启动快照（每次运行开头打一整块，这是跨设备排错的关键）**

- 程序版本（`Cargo.toml` 的 0.1.4）、构建目标（`cfg!(target_os)` / `arch`）、编译时间
- OS 发行版与内核版本（Linux 读 `/etc/os-release` + `uname`；Windows 读 `RtlGetVersion`）
- 会话类型（`XDG_SESSION_TYPE` = wayland/x11 —— 影响 grab 行为）
- 显示器数量/分辨率/缩放（Windows 侧还要记 DPI awareness 上下文，见 §3.2 根因 ④）
- **用户与组**（`id` 等价信息；Linux 上特别记"是否在 input 组"）
- `config_dir()` 实际路径 + 三个配置文件的存在性/大小/解析结果
- **evdev 设备全清单**（见 §2.3 P0-1）：路径、名字、`input_id`(bus/vendor/product)、
  `supported_keys` 数量、REL 轴集合、ABS 轴集合、判定结论、打开成功/失败(含 errno)
- scrcpy / scrcpy-server / adb 的路径与版本（`adb::find_*` / `scrcpy_version_at` 已有现成函数）
- 当前生效的按键组合名、`binds`/`wheels` 数量、`aim` 全字段、`switch_keys` 全表

**B. 运行期事件（状态跃迁才记）**

- 捕获层：设备打开/移除/重开、`grab`/`ungrab` 成功失败、`fetch_events` 的非 WouldBlock 错误（含 errno）
- Windows 钩子：安装成功/失败(`GetLastError`)、**回调耗时 P50/P99/最大值**、
  怀疑被系统摘除的时刻、重装动作
- 控制通道：连接（含读到的 dummy 字节）、`is_connected` 由真变假、`write_all` 失败(含 errno)、
  读线程收到 EOF、重连次数
- 引擎：每次 `release_all`（**并记录是谁触发的**：总开关键/顶栏按钮/通道断开/组合切换）、
  每次 `refuse`（哪个键、当时 pointers/count 各是多少）、
  每次 `sync_structures` 判定为 restructured、
  `held` 对账的**前后差异**（"移除了 3 个幻影键: KEY_W KEY_S BTN_RIGHT" ← 这一行就是断触的直接证据）、
  看门狗主动清除幻影键（WARN）
- 瞄准：`aim.down` 跃迁、`aim_recenter` 触发原因（阈值/边界/静止）、锚点越界钳制
- 配置：读/写/另存/备份损坏文件（`backup_broken_config`）的完整路径与结果

**C. 关闭**

- 退出原因（正常/panic）、退出时是否还有未抬起的触点（**若有，记 ERROR：这就是下次启动时手机上卡键的原因**）

### 5.5 纠错体系（用户要求的"依据此报错输出建立一套纠错体系"）

分两层做，**不要只做第二层**：

**第一层：日志 → 规则表（离线，给 AI/开发者看）**

把"症状 → 日志特征 → 结论 → 处置"整理成一张表，放进本文档同目录，随代码维护。
先把已经查明的这些填进去：

| 日志特征 | 结论 | 处置 |
|---|---|---|
| 启动快照里 evdev 设备清单**少于** `/dev/input/event*` 的个数 | 有设备打不开 | 看对应 errno；权限 → `usermod -aG input` 后重新登录 |
| 运行期出现"设备 … 已移除"且之后没有"重新打开" | 热插拔未恢复 | §2.3 P0-2/P0-3 |
| `held 对账` 行反复出现"移除幻影键" | 正在丢 release 事件 | Windows → §4.6 P0-4；Linux → 查设备是否失联 |
| Windows 钩子回调 P99 > 50ms | 逼近 `LowLevelHooksTimeout`，随时会丢事件 | §4.6 P0-4（回调里绝不能有 `AttachThreadInput`/`SetCursorPos`） |
| `refuse` 增长且用户没按那么多键 | `fingers.count` 虚高 | 机制 ②/③，§4.6 P0-2/P1-1 |
| 服务端 logcat 出现 `Too many pointers for touch event` | 设备端槽位泄漏 | 机制 ③，§4.6 P0-2 |
| `AimLive.motions` 不增长 | 捕获层没产生 Motion | §3.1 决策树第一行 |
| `motions` 增长但 `sent` 不增长 | `aim_active()` 不成立或触点池满 | §3.1 决策树第二行 |
| `last_dx` 频繁为负且用户没有反向移动 | 回中回声被当成真实位移 | §3.2 根因 ② |

**第二层：程序内自检（在线，给用户看）**

界面已经有一个很好的样板：「鼠标瞄准(FPS)」的**生效条件自检**（`app.rs:4461-4520`）——
逐项打 ✓/✗，最后给一句"→ 现在没反应，因为: XXX"。**把这个模式推广出去**：

- 左栏加一个「诊断」折叠面板，把同样的"条件 → ✓/✗ → 一句话结论"做到：
  键盘捕获、鼠标捕获、输入权限、控制通道、触点预算、配置合法性、scrcpy 三件套。
- 每条 ✗ 都配一个**可点的按钮**直达修复动作（"打开配置目录"、"重新扫描输入设备"、
  "重新连接控制"、"复制诊断日志路径"）。
- 一个「导出诊断包」按钮：把 `diagnostics.log` + 启动快照 + 当前配置（**脱敏：去掉绝对路径里的用户名**）
  打成一个 zip，用户直接发给作者。这是跨设备问题唯一现实的收集方式。
- 启动时若检测到**上一次运行是异常退出**（panic hook 留下的标记文件 / 退出时未抬起触点的 ERROR），
  主动在日志区提示，并建议一键对账。

---

## 6. 顺带发现的其它缺陷（不在用户清单里，但就在同一片代码，改的时候一并处理）

### 6.1 `ControlClient.screen_w/h` 是可变的，但**实际序列化用的是连接时固化的旧值**

`control.rs:61-71` 的写线程：

```rust
std::thread::spawn(move || {
    let w = screen_w as u16;      // ← 连接时的值，move 进线程，之后永不改变
    let h = screen_h as u16;
    while let Ok(cmd) = rx.recv() {
        let buf = serialize(cmd, w, h);
```

而 `app.rs:4135-4140`（`sync_display_space`）会去改 `c.screen_w = w; c.screen_h = h;`。
引擎读的也是 `ctl.screen_w/h`（`engine.rs:1256`、`engine.rs:989`）。

**于是：引擎按新尺寸算坐标，消息里却声明旧尺寸。**
当前**无害**，因为 control-only 模式下服务端走 `Controller.java:503-507` 的 raw 坐标分支，
不校验尺寸（§4.5 已核实）。**但只要将来有人打开视频、或换成 `--new-display`，
`PositionMapper.map()` 就会因尺寸不符把每一条触摸事件全部丢弃**，而且现象是"完全没反应、无任何报错"。

处置：把 `w/h` 改成写线程每轮从 `Arc<AtomicU32>` 读，或者干脆让 `ControlClient` 的这两个字段变成只读、
改尺寸必须重建客户端。**至少要在这个字段上写一条警告注释**，说明它与实际序列化值不同步。

### 6.2 重连存在端口/socket 竞态

`adb.rs:487-537` 的 `start_control_server` 用**固定** `scid = 0x1a2b3c4d` 与**固定** `port = 28383`，
socket 名恒为 `scrcpy_1a2b3c4d`。`app.rs:2030-2031` 先 `self.server = Some(new)`（此时旧 `ControlServer`
被 drop → `adb.rs:545-553` kill 旧 adb 子进程 + `forward --remove`），**再**装新 client。

顺序问题：新 server 可能因为旧的设备端 server 还占着 `scrcpy_1a2b3c4d` 而 bind 失败；
而旧 forward 被移除的时机与新 client 连接的时机也在打架。
建议：**先 drop 旧 server（并等设备端进程确实退出）→ 再启新 server → 再连**。
`scid` 也建议每次随机（scrcpy 官方就是随机的），避免复用带来的残留。

### 6.3 Windows 鼠标侧键映射差一

`capture.rs:385`：`B::Unknown(n) => 275 + n`。
rdev 对 `WM_XBUTTONDOWN` 取 `HIWORD(mouseData)`（`common.rs:69-76`），XBUTTON1 = 1、XBUTTON2 = 2
→ 得到 276、277。而 Linux evdev 是 `BTN_SIDE = 275`、`BTN_EXTRA = 276`。
**同一颗物理侧键在两个平台上键码不同 → 配置不通用**（在 Windows 上绑好的侧键拿到 Linux 上会错位一个）。
处置：改成 `274 + n`。

### 6.4 Windows 上小键盘与多媒体键**完全无法绑定**

`rdev-0.5.3/src/rdev.rs` 的 `Key` 枚举**没有任何 keypad 变体**（实测搜索 `KeyPad` 无结果），
小键盘数字/运算符、多媒体键、`VK_OEM_*` 等一律落到 `Key::Unknown(u32)`；
而 `capture.rs:481` 是 `_ => return None` → **事件被静默丢弃**。

后果：Windows 用户按小键盘或侧键去"改键"，界面永远捕获不到，看起来像程序坏了。
处置：`map_win_key` 里为 `K::Unknown(vk)` 增加一条 **vkCode → evdev 码**的兜底映射
（至少覆盖 VK_NUMPAD0-9 = 0x60-0x69、VK_ADD/SUBTRACT/MULTIPLY/DIVIDE/DECIMAL、
VK_MEDIA_*、VK_BROWSER_*），映射不到的**记一条 DEBUG 日志并带上 vkCode**，
这样用户报"某个键绑不上"时我们能立刻知道是哪个 vk。

### 6.5 Linux 侧 `value == 2`（自动重复）被直接丢弃

`capture.rs:186-189`。这在正确性上没问题（引擎用 `fresh_press` 收口），
但 §4.6 P0-3 的幻影键看门狗**正好需要这个信号来喂狗**。
处置：不要发给引擎，但要更新"该键最后一次有事件的时刻"。

### 6.6 rdev 无法卸载钩子

`rdev-0.5.3/src/windows/common.rs:22` 单个 `static mut HOOK`，
`set_mouse_hook` 覆盖了 `set_key_hook` 的句柄 → 键盘钩子再也无法 `UnhookWindowsHookEx`。
`listen.rs:53` 的 `GetMessageA` 永不返回。**只要还在用 rdev，就没有"干净退出"和"重装钩子"的能力。**
这是 §4.6 P0-4 必须自建钩子的另一个理由。

---

## 7. 建议的动手顺序

> 每一步都要跑完 §0.1 的四条验证命令，并且**不删 release 旧产物**。

| 步 | 做什么 | 涉及文件 | 验收 |
|---|---|---|---|
| 0 | 恢复版本控制（`git init` 或手工备份） | — | `git log` 有内容 |
| 1 | 新建 `diag` 模块：文件日志 + 级别 + panic hook + 启动快照 | 新 `src/diag.rs`、`src/main.rs`、`Cargo.toml` | 启动后 `~/.config/scrcpy-pad/diagnostics.log` 存在且含完整环境快照；`kill -SEGV` 能留下 backtrace |
| 2 | evdev 设备全清单日志（含打不开的设备与 errno） | `src/capture.rs:81-132` | 日志里的设备条数 = `ls /dev/input/event*` 的条数 |
| 3 | 设备失联可发现 + 定时重扫（热插拔）+ `mouse_found` 动态化 | `src/capture.rs:105/209-215` | §2.4 的 6 条验收全过 |
| 4 | `Held` 权威对账（`get_key_state` / `GetAsyncKeyState`）+ 四个对账时机 + 幻影键看门狗 | `src/capture.rs`、`src/engine.rs:1094-1095/1243-1253/799-850` | §4.7 的实验 1、2 |
| 5 | 通道断开也走 `release_all`；重连前先在旧通道收尾；锁中毒容错 | `src/engine.rs:1243-1253`、`src/app.rs:2030-2031` | §4.7 的实验 2、3 |
| 6 | Windows：自建 LL 钩子（回调只入队）+ Raw Input 取原始位移 | `src/capture.rs:229-375`、`Cargo.toml` | §3.2 验收 1~6、§4.7 实验 4 |
| 7 | 界面「诊断」面板 + 「导出诊断包」+ 纠错规则表落地 | `src/app.rs`（参考 `app.rs:4461-4520` 的自检样板）、`src/help.md` | 用户能自己看懂 ✗ 在哪、并一键导出 |
| 8 | 顺手修 §6.1~§6.6 | 各处 | 各自验收 |
| 9 | 更新 `README.md` 与 `src/help.md`，加更新日志 | — | — |

**注意**：第 6 步（Windows 自建钩子 + Raw Input）改动最大、风险最高，
**务必放在 1~5 之后**，因为那时已经有日志能量化"钩子回调耗时"和"丢事件次数"，
改前改后可以直接对比，否则无法证明修好了。

---

## 附录 A：本次排查用到的取证命令（可直接复制）

```bash
# 1) 输入设备总览 + 权限 + 当前用户所属组
for d in /sys/class/input/event*; do
  n=$(basename $d)
  printf '%s | %s | rel=%s | key=%s\n' "$n" "$(cat $d/device/name)" \
    "$(cat $d/device/capabilities/rel)" "$(cat $d/device/capabilities/key)"
done
ls -l /dev/input/ ; id

#   判读：rel=3 表示同时有 REL_X(bit0) 与 REL_Y(bit1) → is_mouse() 为真
#        rel=0 的触摸板不会产生任何 Motion 事件

# 2) 外接设备的插拔历史（问题 3 的关键证据来源）
journalctl -k --no-pager | grep -iE "input:|usb .*Keyboard|hid|USB disconnect|reset .* USB device" | tail -40

#   判读：同一端口短时间内 disconnect + new device = 用户拔插过
#        多条 "reset ... USB device" 同时出现 = 从挂起(suspend)恢复

# 3) 指定时间窗的完整内核日志
journalctl -k --no-pager --since "2026-09-26 22:28:00" --until "2026-09-26 22:33:00"

# 4) 服务端触点泄漏的证据（问题 6 机制 ③）
adb logcat | grep -iE "Too many pointers|Ignore positional event"

# 5) 全链路自检（12 项）
sg input -c './target/release/scrcpy-pad --selftest'
#   注：若会话已有 input 组则不需要 sg input -c 前缀，直接跑即可

# 6) Windows 侧：低级钩子超时阈值（问题 6 机制 ④）
reg query "HKCU\Control Panel\Desktop" /v LowLevelHooksTimeout
#   默认 300(ms)。若被改小，丢事件会更频繁
```

## 附录 B：关键代码位置索引

```
capture.rs   81        evdev::enumerate() 一次性枚举（问题 3 根因①）
capture.rs   87-90     set_nonblocking 失败 → log_line + continue
capture.rs   97-101    grab 标志选择（复合设备拿错标志，§2.2 ⑥）
capture.rs   103       每设备一个线程
capture.rs   105       mouse_found = 启动时快照（§3.1 ②）
capture.rs   107-129   opened==0 时的权限探测与 bail
capture.rs   135-144   is_keyboard: KEY_A && KEY_Z && KEY_ENTER
capture.rs   148-155   is_mouse: REL_X && REL_Y
capture.rs   186-189   Linux value==2 自动重复被丢弃（§6.5）
capture.rs   195-207   REL_X/REL_Y 累积成 Motion
capture.rs   209-215   WouldBlock→2ms；其它错误→50ms 空转不退出（问题 3 根因②）
capture.rs   229-345   Windows platform_start
capture.rs   251-266   抓取状态切换（ShowCursor/回中/skip_next 初始化）
capture.rs   268-272   send 闭包（内含 Mutex::lock，需移出钩子）
capture.rs   307-336   MouseMove 处理（差分 + skip_next + 回中，§3.2 根因②）
capture.rs   352       CURSOR_RECENTER_PX = 64.0（§3.2 根因③）
capture.rs   356-359   cursor_center 用 SM_CXSCREEN/CYSCREEN（§3.2 根因④）
capture.rs   362-367   move_cursor = SetCursorPos（在钩子回调里，§4.4）
capture.rs   379-387   map_win_button（侧键差一，§6.3）
capture.rs   391-483   map_win_key（_ => None，小键盘/多媒体键全丢，§6.4）

engine.rs    40        MAX_CONCURRENT_KEYS = 8
engine.rs    46        DEVICE_MAX_POINTERS = 10（已核对服务端 PointersState.MAX_POINTERS）
engine.rs    49        AIM_PID = 3000
engine.rs    123-138   Fingers::try_down（两道闸门）
engine.rs    150-155   Fingers::free_all
engine.rs    158-184   Held（物理按键镜像）
engine.rs    210-260   reconcile_binds（224: want = held.has(key) && !key_owned_by_wheel）
engine.rs    540-545   aim_active 四条件
engine.rs    561-566   aim_release_local（不发 UP → 泄漏，§4.3 ①）
engine.rs    616-666   aim_on_motion（638-644 灵敏度累加）
engine.rs    799-850   release_all（839-842 None 分支不发 UP；844 free_all；**不清 held**）
engine.rs    977-986   mouse_grab.store 的六个条件
engine.rs    1036-1070 Motion 分支（1041 motions++ 在 if enabled 之前）
engine.rs    1094-1095 fresh_press + held.set（**held 唯一写入点**）
engine.rs    1119-1127 空闲≥3s → space_recheck（已确认良性）
engine.rs    1130-1210 总开关键（在 1243 的 continue 之前，所以 F8 永远有效）
engine.rs    1243-1253 !enabled / control None / !is_connected → continue（**不 release_all**，§4.2）
engine.rs    1263-1299 sync_structures + reconcile

control.rs   61-71     写线程；62-63 w/h 在连接时固化（§6.1）
control.rs   66-69     write_all 失败 → connected=false → break
control.rs   78-89     读线程；82-84 EOF/错误 → connected=false
control.rs   104-106   send 忽略错误
control.rs   143-175   serialize（159-160 写入 w/h 大端）

app.rs       2019-2038 connect_rx 回收；2030-2031 换 server/client（不补发 UP，§4.3 ③）
app.rs       2216-2236 notices + space_recheck
app.rs       2238-2256 grab_flag 同步；2253-2255 由关到开 → refresh_display_space
app.rs       2275-2280 通道断开检测 → control = None（未通知引擎）
app.rs       2528-2557 左栏「引擎: 触点 N/10 · 被放弃 M 次」（问题 6 的现场判据）
app.rs       4127-4141 sync_display_space（4135-4140 改 c.screen_w/h，见 §6.1）
app.rs       4405-4421 refresh_display_space（后台 adb::display_size）
app.rs       4432-4520 ui_aim_body 生效条件自检 + 运行状态（§3.1 决策树的数据来源）
app.rs       4976-4986 config_dir()（诊断日志应落在这里）
app.rs       4988-4990 profile_path() = config_dir()/profile.yaml
adb.rs       487-537   start_control_server（494 固定 socket 名；scid/port 固定，§6.2）
adb.rs       545-553   ControlServer::drop → child.kill() + forward --remove
keymap.rs    690-708   Aim 结构
keymap.rs    710-726   Aim::default（enabled:false, anchor 0,0）
keymap.rs    730-732   Aim::anchor_set
keymap.rs    735-760   Profile::default
keymap.rs    797-826   ConfigFile / Default
main.rs      12-16     --selftest 分支（**无 panic hook**）
main.rs      60-178    selftest 12 项
```

**依赖库源码（已核对，路径为本机 cargo registry）**

```
~/.cargo/registry/src/index.crates.io-*/evdev-0.13.2/src/lib.rs:475-481
    enumerate() 文档：静默跳过打不开的设备
~/.cargo/registry/src/index.crates.io-*/evdev-0.13.2/src/raw_stream.rs:736-739
    enumerate() = 一次 read_dir("/dev/input")
~/.cargo/registry/src/index.crates.io-*/evdev-0.13.2/src/raw_stream.rs:459 / sync_stream.rs:282
    Device::get_key_state() → EVIOCGKEY，**Held 权威对账的现成原语**
~/.cargo/registry/src/index.crates.io-*/rdev-0.5.3/src/windows/listen.rs:20-42
    raw_callback：每个 KeyPress 调 get_name()；41 CallNextHookEx
~/.cargo/registry/src/index.crates.io-*/rdev-0.5.3/src/windows/listen.rs:44-56
    listen()：53 GetMessageA 永不返回，无卸载入口
~/.cargo/registry/src/index.crates.io-*/rdev-0.5.3/src/windows/common.rs:22
    单个 static mut HOOK（被鼠标钩子覆盖）
~/.cargo/registry/src/index.crates.io-*/rdev-0.5.3/src/windows/common.rs:77-83
    WM_MOUSEMOVE → MSLLHOOKSTRUCT.pt（绝对屏幕坐标，已含指针加速）
~/.cargo/registry/src/index.crates.io-*/rdev-0.5.3/src/windows/keyboard.rs:36-114
    get_name → set_global_state(46-71，54/59 AttachThreadInput) + get_code_name(73-114，80/98 ToUnicodeEx)
~/.cargo/registry/src/index.crates.io-*/rdev-0.5.3/src/rdev.rs:104-211
    Key 枚举无 keypad 变体；211 Unknown(u32)

/home/azrl/文档/scrcpy/server/src/main/java/com/genymobile/scrcpy/control/PositionMapper.java:34-47
    map()：clientVideoSize != videoSize → return null（整条丢弃）
/home/azrl/文档/scrcpy/server/src/main/java/com/genymobile/scrcpy/control/Controller.java:482-510
    getEventPointAndDisplayId：503-507 else 分支 "No display, use the raw coordinates"
    ← control-only 模式下不校验尺寸，这是 §4.5 排除该假设的依据
/home/azrl/文档/scrcpy/server/src/main/java/com/genymobile/scrcpy/control/Controller.java:523-527
    getPointerIndex == -1 → Ln.w("Too many pointers for touch event") → return false（静默丢弃）
/home/azrl/文档/scrcpy/server/src/main/java/com/genymobile/scrcpy/control/PointersState.java:12
    MAX_POINTERS = 10
/home/azrl/文档/scrcpy/server/src/main/java/com/genymobile/scrcpy/control/PointersState.java:97-104
    cleanUp()：只移除 isUp() 的指针 → 漏 UP 即永久泄漏
```

## 附录 C：给下一位 AI 的三句话

1. **先做日志（§5），再修 bug。** 问题 3/4/6 全是偶发 + 跨设备的，
   现在整个程序在 Windows 上连一行错误都留不下来，任何修复都无法验证。
2. **不要相信"调大常量"能修断触。** `MAX_CONCURRENT_KEYS` / `DEVICE_MAX_POINTERS` 是对的，
   撞上限是因为状态虚高（机制 ②）或槽位泄漏（机制 ③），要先查为什么虚高。
3. **界面里已有的两个自检区是宝藏**：
   「鼠标瞄准(FPS)」的生效条件自检 + 运行状态（`app.rs:4432-4520`）、
   左栏的「引擎: 触点 N/10 · 被放弃 M 次」（`app.rs:2528-2557`）。
   让用户在复现时念出这两处，多数问题当场就能定性；
   新的「诊断」面板应该沿用同一套"✓/✗ + 一句人话结论 + 一键修复"的表达方式。
