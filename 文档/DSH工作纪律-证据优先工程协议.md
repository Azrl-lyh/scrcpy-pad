# DSH 工作纪律：证据优先的工程协议

> **本文件是常驻纪律，不是建议。** 每次会话开始、每次 debug 连续两轮无果、每次交付前，重读本文件并按它执行。
> 文件中每条纪律后面都标注了它来自哪个真实失败 —— 这些都是本项目用十几轮真机对话换来的，不是纸上谈兵。
>
> 来源：对 session-7ab7f8ee（2026-10-10 ~ 10-12，scrcpy-pad v0.2.0 开发会话）的完整复盘。

---

## 0. 核心公理（最高优先级，不可违反）

1. **门禁绿 ≠ 问题解决。** `cargo fmt --check` / `cargo test` / `clippy_diff` 是代理指标，与"用户的症状消失了"只有弱相关。它们全绿而 bug 依旧存在，在本项目发生过不止一次。
2. **没有独立证据的"修复了"是谎言，即使是真诚的谎言。** 声明修复时，必须同时给出：验证命令/观察点 + 实际输出。
3. **用户不是测试 Oracle。** 用户每测一轮 = build 55 秒 + 打一局游戏 + 反馈 + 信任消耗。能本地验的，绝不让用户验。
4. **猜一个根因 5 分钟，证伪一个根因一整轮真机测试** —— 这个成本差是一切连环错误的根源。本协议的全部设计就是把这两者的成本拉平。
5. **对用户可见的每一条承诺都是工程债务**，必须登记、必须销账。承诺了却没实现，比不承诺更糟。

---

## 1. 失败模式识别清单（见到即警觉）

| # | 反模式 | 本项目的真实案例 | 你该问自己 |
|---|---|---|---|
| F1 | 承诺-实现脱节 | 承诺"诊断日志里会留下设备端证词"，但 `PadApp::log()` 只把消息推进内存环形缓冲、根本不写文件，自愈层取证输出为 0 行 | "这条承诺的证据会在哪里出现？我去看了吗？" |
| F2 | 验证形态与生产不一致 | 用不带路径的 `getevent -lt` 验证（输出有设备名前缀），而生产代码用带路径的（无前缀），解析器把每行都丢了，自愈触发链整条死亡；同类：测试 fixture 写 `mInjectEventStatus=1`（等号），真机是冒号 | "我的验证命令和生产代码逐字一致吗？" |
| F3 | 假设未证先建机制 | `TouchStates` 不统计注入触点（瞄准指按着时账本仍显示"无触点"）——实测后才知道，第二条自愈触发天生是死的 | "这个外部行为我实测过吗？还是我以为？" |
| F4 | 假设驱动擦屁股 | 第 6 条 bug：归中抖动、Raw stall、换手重 arm、坐标空间过期、通道死亡……每个假设都先改码再请用户真机验，被逐一证伪，耗掉十几~二十轮 | "我是在找证据，还是在找许可？" |
| F5 | 测试覆盖表达式，不覆盖接线 | 332 个单测全是判定表达式/纯函数；log→文件、引擎循环→热键动作、子进程 spawn→解析→边沿这些缝全裸奔 | "这个改动跨了几个模块？缝上测试了吗？" |
| F6 | 用户当测试 Oracle | GUI 永远无法自验，每轮交付只能说"没起界面点过"；手感类声明无法自证 | "这个声明是 A 级还是 B 级？我标了吗？" |

---

## 2. 证据先行调试协议（debug 时强制执行）

**Step 0 — 症状签名。** 写下可观测的症状签名（哪几行日志、哪个 trace 字段、什么条件下出现）。**没有签名的 bug 不许动手修。**

**Step 1 — 假设必须带"一证一假"。** 每条假设在提出时必须同时写出：
- 若假设对，我应该观察到：___（具体到日志行 / trace 记录 / 探针输出）
- 若假设错，我应该观察到：___

写不出这两句的，不是假设，是猜测——**不许基于它改任何代码**。

**Step 2 — 假设登记册。** 会话内维护一张表（格式见 §4），每条假设带状态：未测 / 已测-成立 / 已测-证伪。已证伪的假设永远移出"待议"，并在压缩摘要里携带，**禁止再次提议**。

**Step 3 — 两轮规则。** 同一个症状连续两轮修复无效 → **停止一切代码改动，转去建仪表**（trace、探针、日志落盘、回放）。本项目实证：仪表就位后 2 轮定死根因；仪表缺失时 10+ 轮都在盲猜。**可观测性债是所有其他错误的放大器。**

**Step 4 — 一次实验区分所有存活假设。** 请用户测试前，把当前所有存活假设列出来，设计**一次运行/一份日志输出**就能区分它们（本项目最后的"四行验收清单"是正确范式）。禁止一次只验一个假设地消耗用户。

**Step 5 — 根因陈述必须含证据链。** "根因是 X"必须附带：谁在什么时刻产生了什么记录、排除了什么、为什么排除。**不允许只有结论没有链。**

---

## 3. 承诺登记册（交付前销账）

每次交付前，把本会话对用户做过的所有用户可见承诺列成表，逐条销账。

规则：
- **未销账的承诺，交付物中必须显式列出"未兑现及原因"**，禁止假装不存在。
- 承诺涉及"某条日志会出现"的，**必须自己先制造一次该日志并确认它落到了用户能看到的介质上**（F1 的教训：接口落盘 ≠ 界面缓冲落盘）。

---

## 4. 假设登记册（外部行为假设治理）

凡涉及外部系统行为（MIUI/Android、`dumpsys` 语义、`getevent` 输出格式、adb 管道缓冲、scrcpy 行为、游戏引擎）的机制，机制所依赖的每条假设必须登记。

规则：
- **未测假设不许承重**：不允许在未实测的外部行为假设上建机制。先做只读探测实验（`adb shell` 只读命令），拿到结论再写代码。
- **探测实验本身也必须用与生产代码逐字相同的命令形态**（F2 的教训：验证命令和生产命令差一个参数，验证就失效）。
- 假设被证伪后，**依赖它的机制必须重新评估，禁止"将就着用"**。

---

## 5. 声明分级与交付模板

每条完成性声明必须分级：

- **A 级（agent 可验）**：编译、单测、日志、trace、本地回放、静态检查。A 级声明**必须附命令与输出**。
- **B 级（仅用户可验）**：手感、GUI 交互、真机游戏内表现。B 级声明**必须单独列出，禁止混入"已完成"叙事**。

### 交付模板（每次交付必须按此结构）

```markdown
## 交付
- 文件：<路径> — md5 <hash>（大小）
- 改动清单：每个文件改了什么、为什么

## A 级验证（我自己跑的）
| 检查 | 命令 | 实际输出 |
|---|---|---|
| 例 | cargo test --bin scrcpy-pad | 332 passed / 0 failed |

## B 级未验证（需要你测的）
- 逐条列出，附"一次运行就能判定"的验收观察点：
  例：跑一轮，日志里必须依次出现 X → Y → Z，缺哪行断点就在它前一环

## 已知问题（诚实列出，含复现条件）
```

---

## 6. 验证工程纪律

1. **真机原文 golden test**：一旦从真机抓到某种输出的原文（`getevent` 的两种形态、`dumpsys` 的冒号/等号写法、MIUI 变体），**立即固化为一条永久单测**，解析正确不再靠猜。
2. **生产形态逐字一致**：任何人工验证，命令、参数、**重定向方式**必须与生产代码逐字相同（本项目曾因"我重定向到文件、代码用管道"白查一轮）。
3. **接线层测试（seam test）**：单测必须覆盖模块之间的缝，不只是判定表达式。本项目的必测缝：
   - 界面日志 → 诊断文件（落盘，不是内存缓冲）
   - 引擎 `run` 循环 → 热键动作（接线，不只是判定函数）
   - 子进程 spawn → 管道读取 → 解析器 → 边沿事件（全链）
4. **预验代理（红队）**：重要交付前，派一个独立 agent 只做一件事：**用生产形态重跑验证、专找"承诺-证据"缺口、专挑新机制的未测假设**。它的产出不是代码，是"这条声明不成立"的列表。

---

## 7. 本项目特有红线与已知坑（永久有效）

- **日志双汇流**（F1 修复后的不变量）：`crate::diag::log()` 落诊断文件；`PadApp::log()` 曾只进内存环形缓冲（已于 2026-10-12 修复为双写）。**取证链路（自愈、账本、探针、重连）的日志必须落盘**，新增日志前先确认走的是哪条汇流。
- **`getevent` 双形态**（F2 修复后的不变量）：
  - `getevent -lt`（不带路径）→ 输出**有** `/dev/input/eventN:` 前缀
  - `getevent -lt <单设备>` → 输出**无**前缀
  - 解析器必须**先剥前缀（若有）、再剥时间戳**，两种形态都要有真机原文回归测试。
- **设备账本不可用**：本机 `TouchStates` 不统计注入触点，"账本对账"触发链**不可用，仅作取证展示，禁止作为主触发机制重建**。
- **第 6 条已证伪假设（禁止再次提议为根因）**：归中抖动、Raw Input stall、换手/重 arm、坐标空间过期、控制通道死亡。当前结论：手在屏上期间注入流不被设备/游戏受理，本地记账与设备现实错位；**唯一验证有效的恢复是"零触点后重建手势"**。
- **已知待修缺陷**：
  - **F5** 键盘钩子探针误判（同一秒鼠标钩子回调 300 次却判"钩子被摘除"，永久停用自愈 + 常驻吓人告警，稳定复现）
  - **F6** scrcpy 启动参数重复（`--max-size`、`--max-fps` 各出现两次）

### 用户长期规则（必须始终遵守）

- 可能影响已有体验的操作，**动手前必须问**。
- **不要残留进程/子进程**（Windows 不会随父进程杀子进程，`finish()` 里显式收）。
- **不代替用户执行 PowerShell。**
- 用户可见 UI 文本**不出现第三方程序名**（代码注释可以）。
- exe 交付**必须附验证过的 md5**。
- **声称修复前必须有效果证据**；用户发现"声称修复但实际没变"是最高级别信任事故。

---

## 8. 会话压缩时必须携带的三张清单

上下文压缩（compaction）的摘要中，以下三张表**必须完整转移，缺一张视为压缩失败**：

1. **未兑现承诺清单**（承诺登记册中未销账项）
2. **已证伪假设清单**（含证伪证据，防止反复提议同一根因）
3. **未测假设清单**（机制正承重、但外部行为还没实测的假设）

### 当前状态（截至 2026-10-12 13:1x）

**④ 必测缝（§6.3）实测状态 —— 本次自审发现的不合规项**

| 缝 | 状态 | 证据 |
|---|---|---|
| ③ 子进程 spawn → 管道 → 解析 → 边沿 | ✅ 已有 | `adb::tests::parses_single_device_output_without_prefix`（真机原文 golden test） |
| ① 界面日志 → 诊断文件（落盘） | ❌ **无测试** | `grep "crate::diag::log" src/app.rs` 只有实现（app.rs:2709），无任何断言它真的落盘 |
| ② 引擎 `run` 循环 → 热键动作（接线） | ⚠️ **只有判定函数级测试** | `wake_key_action` / `self_heal_lift` 有单测，但"`run()` 里那行接线"无测试 |
| — `adb::stop_physical_touch_watches`（本轮新增） | ❌ **无测试** | `grep` 无任何 test/assert 引用它 |

> 这三项是**已知不合规**，不是"已完成"。补测试前，任何"接线已验证"的说法都不成立。

**① 未兑现承诺清单**

| ID | 承诺内容 | 证据位置 | 状态 |
|---|---|---|---|
| P1 | "锁死时诊断日志会留下设备端证词" | `diagnostics.log` 含"设备端输入账本"行 | **已销账**（F1 修复后真机日志已含该行） |
| P2 | 自愈能自动解决"手碰手机后锁死" | 日志 `卡死自愈#N` + 轨迹孤立 UP | **已销账**（真机 3 次，用户确认"确实有效果"） |
| P3 | 两个自救热键（唤醒视角 / 强制重连） | 用户真机实测 | **已销账**（用户："两个按键都是好用的"） |
| P4 | 零残留 | 退出后设备端 `ps -A \| grep getevent` 为空 | **已销账** |
| P5 | `AGENTS.md` 指向的 `文档/环境开发注意事项.md` | 该文件**不存在** | **已销账**（改为把环境事实直接写在 AGENTS.md 内，并加了 §0 硬性开场检查 C1–C8） |
| P6 | 已证伪假设不再提议 | —— | **已销账**（清单固化在 §7 与本表②） |
| P7 | §6.3 三条必测缝要有接线层测试 | `cargo test --bin scrcpy-pad` 输出 | **已销账**（缝①`interface_log_reaches_the_diagnostics_file`、缝③`getevent_pipe_to_edge_is_wired_end_to_end`、`stopping_touch_watches_is_idempotent`；缝②见下"仍未销账"） |
| P8 | F5 键盘钩子探针误判要修 | 日志不再出现"探针无回音" | **已修 + 有回归测试**（`probe_miss_is_counted_not_timestamped`）；真机效果 → B 级 |
| P9 | F6 启动参数重复要修 | 启动日志 `scrcpy 启动参数:` 无重复键 | **已修 + 有回归测试**（`arg_upsert_replaces_in_place_instead_of_duplicating`、`arg_dedupe_keeps_the_last_value_like_the_command_line_does`）；真机效果 → B 级 |
| P10 | 宏录制开始/终止热键（含独立启用、单键、不写入宏） | 界面两行 + 行为 | **已实现 + 有验收测试**（`macro_record_hotkeys_follow_the_spec`、`macro_record_hotkey_events_never_enter_the_macro`）；GUI 与真机 → B 级 |

**仍未销账（如实列出）**

| ID | 承诺/缺口 | 原因 |
|---|---|---|
| P11 | 缝②「引擎 `run` 循环 → 热键动作」**接线层测试** | 仍只有判定函数级测试（`wake_key_action`/`self_heal_lift`/`macro_hotkey_action`）；`run()` 里那行接线无测试。**未兑现**，不是"已完成" |
| P12 | F5/F6/宏热键的**真机效果** | 属 B 级（需用户实测）；本地只能证明逻辑与回归，不能证明"症状消失" |
| P13 | 宏热键的 **GUI 走查** | 未起界面点过；只做到编译通过 + 与既有 `key_button`/`checkbox` 同构 |
| P14 | 宏「取消录制」热键（全局公共、丢弃不保存） | **已实现 + 有验收测试**（`macro_cancel_hotkey_discards_without_saving`）；GUI/真机 → B 级 |
| P15 | 右上角红色哑光救援按钮（优先级仲裁、常亮不闪） | **已实现 + 有仲裁测试**（`rescue_button_priority_is_highest_wins_and_fifo_on_ties`）；GUI 外观 → B 级 |
| P16 | F6 去重要**写回** `settings.json` | **已修**（`prepare_scrcpy_args` 去重后调 `remember_now()`） |
| P17 | 鼠标接管可选:外观页「遮板 / 隔膜」二选 + 遮板完整接线 + 热切换 | **已实现 + 有仲裁测试**（`theme::tests::mouse_capture_modes_are_mutually_exclusive`）；GUI 观感 → B 级 |
| P18 | 遮板定位投屏窗口 | **已实现 + 实测**（生产同形只读探测:可见窗口定位到 `(300,200) 640x480` 逐位一致） |
| P19 | 遮板图形**位置准确**（用户反馈"圆的位置确实歪了"） | **已修**：改为**等比 letterbox 居中**适配（原先是非等比拉伸）；有单测 `mask_shape_scaling_is_uniform_and_centered` |
| P20 | 遮板**跟随**投屏窗口移动/最小化（用户反馈"盖在屏幕表面不走了"） | **已修**：重定位节流 500ms→**100ms**，且不再只在"还没起窗"时定位；最小化/关闭 ⇒ 收窗退回隔膜 |
| P21 | 遮板**显示项目**下拉（用户第 2 条：与截图取点项目相同） | **已实现**：复用同一个 `OverlayFilter`；存 settings.json 位标志；有单测 `mask_filter_bits_round_trip_and_match_overlay_default` |
| P22 | **架构错误**：`mouse_capture` 曾存在 `Look`（每方案一份 + 被 look.json 覆盖）⇒ 实测"选了遮板却跑成隔膜" | **已修**：移到 `Settings`（程序级）。证据：`look.json` 是 `Mask` 而 `profile.yaml` 生效方案是 `Shield` |
| P23 | 遮板日志级别（用户 `log_level=warn` 时看不到任何遮板记录） | **已修**：起窗/切回/贴不上全部改用 `diag_warn!`，`warn` 级也可见 |
| P24 | 遮板按**手机长宽比**算画面区域，三种情况（全屏 / 最大化 / 任意缩放）都要对 | **已改**：改用 `client_area_of_pid`（客户区，不含标题栏）+ 按 `dw:dh` 等比居中；有单测 `mask_viewport_follows_phone_aspect_in_all_three_cases` |
| P25 | `--fullscreen` 要在参数表里 | **本来就有**（`-f` / `--fullscreen`，参数表第 570 行）；新增断言锁住它不会被误删 |
| P26 | 参数表按**中文拼音**排序 | **已实现**：`cmp_pinyin` 用 `CompareStringEx`+`zh-CN`（零维护，已只读探测验证是真拼音序）；有单测 `arg_entries_sort_by_pinyin` |
| P27 | 参数**搜索** | **已实现**：四路匹配（中文名/参数名/说明/示例）；有单测 `arg_search_matches_name_flag_desc_and_usage` |
| P28 | 遮板**图形缩到窗口外 / 偏左上角**（用户第四轮反馈） | **已修**：根因=GDI `Ellipse` 用**窗口客户区坐标**，而旧代码传的是**屏幕坐标**（多加了 `client_x/client_y`）。改法：**蒙版窗口直接等于画面矩形** ⇒ 只剩一次纯等比缩放；有单测 `mask_viewport_rect_uses_phone_aspect_and_centers` |
| P29 | 遮板**摇杆比按键偏得多** | **已修**（随 P28）：摇杆半径是按键的 4.4 倍（真机数据 0.0553 vs 0.0127），同样的坐标系偏差在大圆上视觉更明显 |
| P30 | 坐标空间未知时不许贴遮板 | **已修**：`space=(0,0)` 时返回 `None`（旧代码 `.max(1)` 会拿假的 1×1 空间算出满屏矩形）；单测当场抓出这个 |
| **P11** | 缝②「引擎 `run` 循环 → 热键动作」接线层测试（**协议里唯一长期未兑现项**） | **已销账**（2026-10-13）：抽出 `reconnect_hotkey_hit`（5 条件纯函数）与 `handle_gui_button_event`（界面层热键唯一接线点）；测试 `reconnect_hotkey_wiring_requires_all_five_conditions` + `gui_hotkey_consumes_the_event_so_it_never_enters_the_macro`；**并用变异测试证明有效**（放宽 capturing 判断 ⇒ 测试立刻变红） |
| P31 | V2-2 Tap 连点 | **已实现 + 引擎级实测**：`down 7 up 7`（700ms/100ms）；关闭时 `down 1 up 1`（默认不变） |
| P32 | V2-2 滑动轨迹录制 | **已实现 + 引擎级实测**：折返轨迹 `down 1 up 1 move 3`，设备端 x 同时到过左右两半 |
| P33 | 测试 flaky（`interface_log_reaches_the_diagnostics_file` 单跑绿/全量红） | **已修**：加 `diag::DIAG_TEST_LOCK` 串行锁；连跑 3 次全绿 |
| P34 | V2-3 轨迹录制重做（UI / 操作逻辑 / 蒙版 / 截图风格四件套） | **已实现**，见下方「V2-3 交付登记」。三条根因②③（**录制点从未被渲染**、**遮板不画轨迹**）都有变异验证 |

### V2-3 交付登记：轨迹录制重做（2026-10-13）

**用户原话**：「连点功能不错，但是轨迹录制功能极为不明显。给你权限，你把轨迹录制的UI及操作逻辑、蒙版显示风格、截图上显示的风格全部重做优化一下。」
**本轮追加**：「重做轨迹录制，按照最高标准和最佳表现的UI。」

#### 三条根因（改前逐条定位，全部有代码位置）

| # | 根因 | 位置（改前） | 表现 |
|---|---|---|---|
| ① | `recorded_points` **从未被渲染** —— `Action::Swipe` 一律走 `draw_swipe_track`（只画起点→终点推导出的条形/方形/圆形几何），录制点从没进过绘制 | `app.rs` 浮层 `Action::Swipe` 分支 | 录了等于没录，截图上看不出任何区别 ← **头号原因** |
| ② | 录制中**无任何实时反馈** —— 只有一行文字"● 录制中… 已记 N 点" | `app.rs` 原 L7293–7335 | 边划边看不见自己划了什么 |
| ③ | 遮板**完全没有轨迹通道** —— `mask::Shape` 只有圆，`paint()` 只有 `Ellipse` | `mask.rs` / `mask/win.rs::paint` | 开遮板时轨迹彻底不可见 |
| ④ | 录制入口只在「新增草稿」面板里，`swipe_controls()`（已有键位/组合键共用）里**没有录制 UI** | `app.rs::swipe_controls` | 已有滑动想重录只能删了重建 |
| ⑤ | 录制**没有取消路径**（"有始必有终"违反）：开始后只能划完再点清除 | 原 L7297–7320 | 划歪了无法丢弃重来 |

#### 改动清单

| 文件 | 改动 |
|---|---|
| `src/keymap.rs` | 新增 `polyline_len_px`（折线像素长）、`track_arrow_marks`（沿**弧长等距**取方向标，含"全部重合/点数不足"退化保护）、`decimate`（保首末抽稀到 ≤256）、`push_track_sample`（采样抽稀 + 上限，**纯函数**）、常量 `TRACK_MAX_POINTS=2000` / `TRACK_MIN_STEP_PX=4` |
| `src/mask.rs` | 新增 `ShapeKind::Swipe`、`Polyline{points,arrows,label}`、`Frame.tracks`（与 `shapes` 并列，圆走 `Ellipse`／折线走 `Polyline`，不混一个 `Vec`） |
| `src/mask/win.rs` | `paint()` 新增轨迹分支：一笔 `Polyline` + 起点空心圈 + 终点实心点 + 方向箭头；`ShapeKind::Swipe` 配色亮青 `rgb(120,230,255)`；新增 `draw_gdi_arrow`；**自检新增两条像素级断言**（折线中段 + 折返段） |
| `src/app.rs` | ① 新增 `TrackTarget{Draft,Bind,Combo}` 与 `TrackRec`**会话缓冲**：录制期点记在会话里，结束时才**一次性提交**（`begin/finish/cancel_track_recording`）；② 新增 `track_rec_card`（三种状态：未录/录制中/**已有轨迹**），四个入口共用；③ 新增 `clear_track`；④ 新增 `draw_recorded_track`（折线 + 方向标 + 起/终点 + 「N点·Xpx」统计 + 录制中 REC 呼吸角标）；⑤ 浮层 6 处 `Action::Swipe` 全部改为**录制轨迹优先**；⑥ `fill_mask_shapes` 把录制轨迹推上遮板；⑦ Esc 取消、换目标自动收尾、`cancel_draft`/`commit_draft` 收口 |
| `src/help.md` | 「录制轨迹」章节重写：录制中可见/录完可见/随时可改/时间轴/上限/遮板 |

#### A 级验证（可复现的命令与输出）

```
$ cargo fmt --check                      → 退出 0（无输出）
$ cargo test --bin scrcpy-pad --locked    → test result: ok. 363 passed; 0 failed
$ python ci/clippy_diff.py                → 当前告警对: 36   基线对: 36   与基线零差异
$ ./scrcpy-pad.exe --mask-selftest        → 14 项全 PASS，含：
    [PASS] GDI 绘制(轨迹折线画上了) 折线中段像素=Some(16770680) 期望=0x00FFE678
    [PASS] GDI 绘制(折返段也画上了) 折返段像素=Some(16770680) 期望=0x00FFE678
    蒙版自检:全部通过
```

新增 5 条单测（358→363）：`polyline_len_sums_segments`、`track_arrow_marks_are_evenly_spaced_with_segment_directions`、`decimate_keeps_ends_and_bounds_the_count`、`track_sampling_dedupes_and_stops_at_the_cap`、**`recorded_pixels_survive_rel_yaml_and_replay`（接缝测试）**。

**接缝测试覆盖的链**：浮层像素 → `rel_x/rel_y` 相对坐标 → YAML 往返 → `Mapper::point` 引擎回放像素，逐点必须回到原轨迹；并断言换分辨率（1280×2772 → 720×1560）后相对位置不变。这条链是录制的**全部价值所在**（任何一环口径不一致都会"录了但走偏"，而三道门全绿照样漏）。

#### 变异验证（证明测试与自检真的有效，不是"绿了就算"）

| 变异 | 结果 |
|---|---|
| `Swipe::point_path` 改为恒返回 `[start,end]`（模拟"录了但没生效"） | `recorded_pixels_survive_rel_yaml_and_replay` + `swipe_track_timeline_follows_distance` **立刻变红**（2 failed） |
| `Polyline(hdc, pts, 2)` 只画第一段（模拟"轨迹画半截"） | 自检 `折返段也画上了` **立刻 FAIL**（折线中段仍 PASS，说明两条断言各管一段、互不冗余） |

#### 交付物

| 文件 | 大小 | md5 |
|---|---|---|
| `release/windows-x86_64/scrcpy-pad.exe` | 11,697,152 | `12835cd65a93e80555af3422cd811586` |
| `release/scrcpy-pad-0.2.0-windows-x86_64.zip` | 4,954,380 | `2ba9ad00751f5879fd81131de2e3cf65` |

启动烟测：7s 存活无崩溃、退出无残留进程；遮板自检 14/14 PASS。

#### B 级（未验证，需用户真机确认）

1. **浮层观感**：轨迹折线粗细/方向箭头大小/起终点标记/统计标签在实际分辨率截图上的可读性——只做了几何与像素级验证，**好不好看必须人眼定**。
2. **录制手感**：`hover_pos` 采样（不要求按住鼠标）是否符合你的划轨迹习惯；4px 抽稀是否够细。
3. **遮板叠加**：轨迹与"响应范围圈"同屏时是否互相干扰（已把几何轨迹排除在遮板外，只画录制轨迹）。
4. **Esc 取消**：录制中按 Esc 丢弃整份会话——确认这个键位不与你其它操作冲突。

#### 已知问题（如实登记）

1. 录制中**改不了目标**：一旦开始录制，这条会话的目标就固定了；换目标（点别的键的[录制]）会把旧会话按取消处理并写日志。这是有意的（同时只允许一个会话），但用户若想"连续给三条键录轨迹"需要录一条、结束一条。
2. `finish_track_recording` 提交到 `Bind`/`Combo` 时按**当前 `Mapper`** 换算相对坐标：若录制期间改过坐标空间（重新截图导致 `screen` 变化），换算基准取的是**提交那一刻**的空间，而浮层显示用的是**录制那一刻**的——理论上两者不同会有偏差。实际场景（录一条几秒内完成）碰不到，未做防护。
3. 遮板上轨迹的标签画在**起点上方**（`y-26`）：起点贴近窗口顶边时标签会被裁掉。浮层用的是 egui（不裁），两边观感在此边界情形不一致。

### V2-4 交付登记：录制入口补全 + 触发配置（2026-10-14）

**用户原话**：「其实，你的手势功能的入口和UI，不明显。你现在告诉我怎么去用这个功能？」
→（我给出使用说明后）用户贴两张截图指出：**「仍然没有录制，或者说，是部分有了，你看左侧，在那里加上。现在只有默认有。」**
→ 随后给出本轮需求：快捷键启用 / 启用录制勾选 / 起始按键（默认空缺）/ 终止按键（默认抬起，可改键），并要求先答四个问题再动手。

#### 本轮修掉的真实缺陷（用户截图直接指出的）

| # | 缺陷 | 根因 | 性质 |
|---|---|---|---|
| ① | **可视化风格的滑动草稿里没有录制按钮** | 上一轮我把 `track_rec_card` 只接到默认风格的新增节（`ui_binds_new`），**漏了可视化风格的 `ui_vk_draft`** | 用户截图实测：左侧"仍然没有录制" |
| ② | **我留下的守护测试是假的** | `every_swipe_edit_entry_renders_the_track_recording_card` 用 `"\n    }\n"` 找函数边界，而 `cargo fmt` 会把这个转义串重排成**字面换行的多行字符串** ⇒ 模式永远匹配不上 ⇒ 函数体截到**文件末尾** ⇒ 任意一处有调用就让全部入口判 PASS | **会漏报**：实测删掉可视化风格的卡片调用后它照样 PASS。比没有这个测试更糟 |

#### 本轮新增功能（用户 2026-10-14 需求）

| 需求 | 实现 |
|---|---|
| 快捷键启用录制 | `track_rec_key`（单键，可空缺）+ `track_rec_enabled`（总开关）；**同一个键按两下切换**（按一下开始、再按一下结束）—— 用户拍板 |
| 勾选「启用录制」 | 总开关：未勾选时**快捷键与鼠标键都不触发**，只有卡片按钮能用（避免误按突然开始录制） |
| 起始/终止按键栏目 | **触发方式**三选一：`快捷键` / `鼠标左键` / `鼠标右键`。用户明确"选择后是**代替**快捷键的操作"，故三者**互斥**（不是叠加） |
| 起始按键默认空缺 | `track_rec_key` 缺省 0；空缺时该方式不触发，界面给出橙色提示（"建议填一个键，如 F9"） |
| 终止 = 抬起 | 鼠标键方式：**按下开始画、抬起停止画**（按下/抬起天然成对 ⇒ 一次按住 = 一次录制，可连续多段，且不会留下"只按不抬"的孤立触点） |
| 不穿透，仅限录像期间 | `track_boundary_swallow_bit(recording, trigger)`：**只在录制中**把边界键的吞键位置上；录制一结束下一帧自动恢复穿透（无需显式清理） |

**「有始必有终」在本轮的新增路径**：切触发方式时若正有会话 ⇒ 先 `cancel_track_recording()`；快捷键/鼠标键开始录制时若没有"正在编辑的滑动" ⇒ **记一条明确提示**，不静默开一个悬空会话。

#### A 级验证（可复现的命令与输出）

```
$ cargo fmt --check                      → 退出 0
$ cargo test --bin scrcpy-pad --locked    → test result: ok. 368 passed; 0 failed   (358 → 368)
$ python ci/clippy_diff.py                → 当前告警对: 36   基线对: 36   与基线零差异
$ ./scrcpy-pad.exe --mask-selftest        → 14 项全 PASS，蒙版自检:全部通过
$ 启动烟测                                 → 8s 存活未崩溃、退出无残留进程
```

新增 6 条单测：`track_rec_hotkey_requires_all_five_conditions`、`track_rec_trigger_maps_to_the_right_mouse_button`、`boundary_key_is_swallowed_only_while_recording_with_a_mouse_trigger`、`track_recording_trigger_roundtrips_and_legacy_json_stays_inert`、`every_swipe_edit_entry_renders_the_track_recording_card`（由假修真）。

#### 变异验证（每条都实测"变异 ⇒ 立刻变红"）

| 变异 | 结果 |
|---|---|
| 删掉可视化风格（`ui_vk_draft`）的 `track_rec_card` 调用 | 守护测试**变红**并精确报出"新增草稿(可视化风格)…没有调用" ← 修复前的版本在这里 PASS |
| `track_rec_hotkey_hit` 去掉"总开关已勾选"条件 | `track_rec_hotkey_requires_all_five_conditions` 变红（报"① 总开关未勾选 ⇒ 不触发"） |
| `track_rec_hotkey_hit` 去掉"触发方式必须是快捷键"条件 | 同上变红（报"② 触发方式是鼠标键 ⇒ 快捷键完全不走"） |
| `track_boundary_swallow_bit` 去掉"仅在录制中"判断 | `boundary_key_is_swallowed_only_while_recording_with_a_mouse_trigger` 变红（报"没在录制时(MouseLeft)不许吞任何鼠标键"，left=1024 right=0） |

#### 交付物

| 文件 | 大小 | md5 |
|---|---|---|
| `release/windows-x86_64/scrcpy-pad.exe` | 11,714,560 | `7a0ab974220a1fe87a04d63c7c10d9c2` |
| `release/scrcpy-pad-0.2.0-windows-x86_64.zip` | 4,961,026 | `7039f06994987f1b55cceab4bf533420` |

#### B 级（未验证，需用户真机确认）

1. **入口是否终于"明显"了**：录制卡片现在四种入口都有（默认/可视化风格的新增草稿、已有键位、已有组合键），且配置区附在卡片下方。**是否够显眼必须你看实际界面定**。
2. **快捷键选哪个键**：默认空缺（按你的口径），建议 `F9`；空缺时该方式不触发。
3. **鼠标键手感**：按下开始/抬起结束是否符合你的预期；录制期间左键不穿透是否影响你其它操作。
4. **触发方式互斥**是否就是你要的（选了鼠标键后快捷键完全不走）。

#### 已知问题（如实登记）

1. **快捷键的"录给谁"是推断的**：快捷键不像按钮那样自带上下文。当前优先级 = ① 可视化风格里选中的那条滑动键位 → ② 正在新建的滑动草稿 → ③ 都没有则记提示、不开始。若你在默认风格下用快捷键、且没有正在编辑的草稿，快捷键会提示"没有正在编辑的滑动"（这是刻意的，避免静默开一个悬空会话）。
2. **配置区在同一帧只画一次**（`track_cfg_drawn_pass` 用帧号去重）：同一屏有多张卡片时，配置区只出现在**先渲染的那张**下方，不会跟着每张卡片重复出现——这是为避免控件 id 冲突与视觉噪音。
3. **鼠标右键方式与 FPS 的"按住才瞄准"可能撞键**：若你把 `aim.hold_key` 也绑成右键，录制期间右键会被录制优先级先吃掉（`capture` 的吞键在引擎读取之前）。实际场景（录制时不瞄）碰不到，但两者同时用会不好用。
   - **已失效（2026-10-14 撤回）**：鼠标键控制录制整条路已被用户要求删除，本条随之不存在。

### V2-5 撤回登记：删除鼠标控制录制（2026-10-14）

**用户原话**：「把鼠标控制绘制录像的功能去掉吧，你干不明白。」

#### 删掉的东西（用户明确点名的范围）

| 删除项 | 位置 |
|---|---|
| `TrackRecTrigger` 枚举（快捷键 / 鼠标左键 / 鼠标右键） | `keymap.rs` 整块删除 |
| `Settings.track_rec_trigger` 字段 | `settings.rs` |
| `PadApp.track_rec_trigger` 字段 + 读/写接线 | `app.rs` |
| `track_boundary_swallow_bit()`（录制期间吞边界键） | `app.rs` 整函数删除 |
| `gui_rx` 里"鼠标按下开始 / 抬起停止"那段边界处理 | `app.rs` |
| 界面上"触发方式"下拉框 | `app.rs::track_rec_config_ui` |
| 两条针对鼠标键的测试（`track_rec_trigger_maps_to_the_right_mouse_button`、`boundary_key_is_swallowed_only_while_recording_with_a_mouse_trigger`） | `app.rs` |

#### 保留的东西

- **`☑ 启用录制`** 总开关（未勾选时快捷键不触发，只有卡片按钮能用）
- **快捷键**（同一个键按两下切换；可空缺，空缺时给出橙色提示）
- **卡片按钮**（[● 录制轨迹] / [■ 结束并保存] / [取消] / [重录] / [清除轨迹]）
- **`Esc` 取消录制**
- 轨迹的浮层绘制、遮板绘制、四入口卡片（这些是上一轮的成果，未受影响）

录制入口从"三个"收敛为**两个**：快捷键 + 卡片按钮。

#### A 级验证

```
$ cargo fmt --check                      → 退出 0
$ cargo test --bin scrcpy-pad --locked    → test result: ok. 366 passed; 0 failed   (368 → 366，移除 2 条鼠标键测试)
$ python ci/clippy_diff.py                → 当前告警对: 36   基线对: 36   与基线零差异
$ ./scrcpy-pad.exe --mask-selftest        → 14 项全 PASS，蒙版自检:全部通过
$ 启动烟测                                 → 8s 存活、退出无残留进程
$ grep -c TrackRecTrigger src/app.rs      → 0（引用全部清除，无残留死代码）
```

#### **旧配置兼容性实测**（本次删除最可能出的真实故障，已用用户真机文件验证）

删除 `track_rec_trigger` 字段后，用户**已落盘的 `settings.json` 里仍残留该字段**（实测 `%APPDATA%\scrcpy-pad\config\settings.json` 含 `"track_rec_trigger": "hotkey"`）。风险是 serde 因未知字段报错 ⇒ 整份设置回退默认 ⇒ 用户所有设置丢失。**实测结论：不会**。

| 验证项 | 结果 |
|---|---|
| 用**用户真实配置文件**（含 `track_rec_trigger`）启动 | 无解析错误、无 `改用默认设置` 日志、**无 `.broken` 备份文件** |
| 你原有的两项设置是否保住 | `track_rec_enabled: true` 与 `track_rec_key: 44` **原样保留** |
| 是否被程序多余重写 | `saved_at` 未变（`1791977555`）⇒ 未触发无谓写盘 |
| 单测兜底 | `track_recording_trigger_roundtrips_and_legacy_json_stays_inert` 覆盖"残留旧字段必须被静默忽略" |

#### 交付物

| 文件 | 大小 | md5 |
|---|---|---|
| `release/windows-x86_64/scrcpy-pad.exe` | 11,707,904 | `559750c194f82e68ca3314aeb9c0be3d` |
| `release/scrcpy-pad-0.2.0-windows-x86_64.zip` | 4,958,271 | `2784397a2f5cd2848605326743e29947` |

#### B 级（需用户真机确认）

1. **"触发方式"下拉框是否确实从界面消失了** —— 现在配置区只剩 `☑ 启用录制` + `快捷键` 两样。
2. **快捷键是否可用**：你的配置里已存有 `track_rec_key: 44`（键码 44 = `Z`）。若不想用这个键，点卡片上的键位按钮重绑或点取消清空。
3. 你原来用鼠标键录制时的手感已不存在 —— 现在只能用快捷键或卡片按钮开始录制。

#### 已知问题

1. **快捷键的"录给谁"是推断的**（沿用上一轮，未变）：优先级 = ① 可视化风格里选中的滑动键位 → ② 正在新建的滑动草稿 → ③ 都没有则记提示、不开始。
2. **你的 `settings.json` 里会继续留着 `track_rec_trigger` 这一行** —— serde 不写回未声明的字段，但也不主动清理它。它现在是**无害的残留**（实测不影响解析）；若你想让文件干净，可以手动删掉那一行，或者由我加一段"清理已废弃字段"的逻辑（需要你确认，因为那属于"替你改配置"）。

### B1 登记：Raw 四态机回归锁 + 变异验证（2026-10-15，W1/B1 = R1 验证并入 + R2/R3/R4）

**性质**：纯测试/缝批次（执行计划:75）。不重建发布物 ⇒ C9 锚点仍为 V2-5（exe `559750c1…` / zip `2784397a…`）；linux 包过期问题按用户 2026-10-15 令**本轮忽略、不登记**。

#### 改动清单（仅 src/capture.rs，5440→5666 行）

| 项 | 位置 | 内容 |
|---|---|---|
| R2 转移表补全 | 测试 `raw_state_machine_transition_table_is_total` :5067 | 补 5 格缺失转移（UNREG+Relative/Empty、REL+Relative/Empty、ABS+Absolute 自循环） |
| R3 降级集成测试 | `absolute_only_device_keeps_hook_motion_and_deltas_reach_coalescer` :5244 | 绝对设备⇒ABS_ONLY⇒钩子照发；mouse_proc 双坐标→`RawEvent::Move`→`handle_move` 差值进 coalescer；含 `raw_probe()` 通道在位断言（R1 通过条件） |
| R4 watchdog 缝 | `watchdog_fallback_step` :4873 + 生产调用点 :4318 | 单步判定提取为可注入 now_ms/告警闭包缝；生产行为逐字不变（告警时刻改单次采样，语义不变已注释） |
| R4 恢复缝 | `note_raw_recovery` :2967（签名加 `&AtomicBool` 旗标）+ 调用点 :3106 | "恢复只记一条"可内存 spy 驱动；生产传原静态旗标，行为不变 |
| R4 缝测试×2 | :5412 / :5458 | 699/700ms 单步三拍；恢复计数 0→1→1 且非兜底态不记 |

#### 变异验证（§3.1，10/10 红且精确，全部还原复绿）

| # | 变异点 | 红测试（panicked at） |
|---|---|---|
| R2① | Relative 接管分支→Drop | 表测试 :5050 + 既有 :5005 |
| R2② | Absolute 降级 Fallback→Drop | 表测试 :5066 + 既有 :5010 |
| R2③ | MixedAbsolute→Emit | 既有 :5019 |
| R2④ | Relative 分支加 ABS_ONLY 特判 | 既有 :5005 + 表测试 :5050 |
| R2⑤ | Empty 保持→UNREG | 表测试 :5062 + 既有 :5024 |
| R2⑥ | hook_forwards_motion 恒 true | `shield_never_stops_motion_capture` :5096 |
| R3 | ABS_ONLY 让位钩子 | 新测试 :5264 + `shield…` :5094 |
| R4① | 700ms 门 `>=`→`>` | 既有 :5397 + 新 :5435 |
| R4② | 删 `drop_ms != 0` 门 | 既有 :5392 |
| R4③ | 删恢复 helper 内 log() | 新 :5465 |

#### A 级验证（Lead 冻结树亲跑）

```
$ cargo fmt --check                       → 退出 0
$ cargo test --bin scrcpy-pad --locked    → test result: ok. 370 passed; 0 failed   (366→367→368→370，账本：R2 +1 / R3 +1 / R4 +2)
$ python ci/clippy_diff.py                → 当前告警对: 36  基线对: 36  与基线零差异
$ 用户 diagnostics.log md5 跑前/跑后      → 41724A0DFA9107F6926F2EC9D3098A69 两次一致（§3.5 未污染）
$ 残留扫描 capture.rs（MUTATION/dbg!/TODO）→ 0
```

#### B 级（未验证，需独立观察）

1. 真实绝对坐标设备 / 远程桌面输入形态下四态机行为——R3 只锁**模拟路径**（直调 mouse_proc/handle_move），真实 `WM_INPUT` 接收证明属 B4 专会。
2. watchdog 缝提取后消费线程真实 700ms 退回的手感/日志观感未变——缝为纯逻辑提取，但真机日志形态需下次真机会话顺带确认。

#### 已知问题

1. **并发写事故（本批次过程事故，已处置）**：R2 前台执行器死亡时遗留两处未还原变异（Relative 接管分支、MixedAbsolute 分支），Lead 冻结树审计以 2 条精确红定位并还原；此后改用"pristine bak + 单点替换 + touch mtime"协议，R3/R4 零残留（md5 级还原证明）。
2. R1 符号表曾有两个执行器版本行号不一致（并发写窗口产物）；登记以 Lead 收尾实测（5666 行态）为准，见执行计划附录 C1。

### B2 登记：Raw 状态行 + 诊断面板日志可见（2026-10-15，W1/B2 = R11,R15,T1,T6）

**性质**：用户可见成果前置批次（执行计划:76）。纯代码/测试，不重建发布物 ⇒ C9 锚点仍为 V2-5；linux 包问题按用户 2026-10-15 令忽略。

#### 改动清单（src/app.rs + src/diag.rs 两个文件）

| 块 | 位置 | 内容 |
|---|---|---|
| R11 | `raw_status_line` app.rs:13825 + `ui_engine_status` 接入 :13233–13254 | 平台中立纯函数四态文案；引擎状态区在 hb_flag 之后插"原始输入"行（1 琥珀 / 2 正常 / 3 琥珀 / 0 不显示）；同一帧只探一次 `raw_probe()` |
| T1 | `raw_probe_alert` app.rs:13884 + 红字行 :13245–13254 | 判据 **`state==3 && count==0`**（"注册完成却零事件"）；**不用 age** —— `count==0` 时 `raw_probe` 把 age 硬编码为 0（capture.rs:2926–2930），用它会把"从未来过"误读成"刚来过" |
| R15 | `RECENT` 环形缓冲 diag.rs:125（容量 64）+ `push_recent` :372 + `recent_lines` :384 + 推入点 :363 + `ui_diagnostics` 渲染块 app.rs:5652–5666 | **补上缺失的桥**：此前 `ui_diagnostics` 不渲染任何日志行，capture 线程的 diag_info!/diag_warn! 不进 UI。现于 `write_raw` 落盘成功后推入环形缓冲，面板显示最近 8 行 |
| T6 | `raw_diag_row` app.rs:13859 + 诊断面板行 :5641–5650 | 四态×count 文案（state 3 分 count 0/非0）；"队列深度行模式"= 守护既有"控制队列 深度"行（**RawEvent 是无界 std mpsc，无 len()，不存在可读 Raw 深度**） |

#### 变异验证（§3.1，6/6 红且精确，全部还原）

| 块 | 变异点 | 红测试（panicked at） |
|---|---|---|
| R11① | `raw_probe_alert` 的 `count == 0` → `count > 0` | `raw_probe_alert_fires_only_for_registered_but_silent` app.rs:20937 |
| R11② | `raw_status_line` state 2/3 文案映射互换 | `raw_status_line_covers_all_four_states` app.rs:20908 |
| R11③ | 删 `ui_engine_status` 内 `raw_status_line(` 调用 | `raw_status_line_is_wired_into_engine_status` app.rs:20992 |
| R15/T6① | `raw_diag_row` state 3 两分支文案互换 | `raw_diag_row_covers_all_states_and_counts` app.rs:21070 |
| R15/T6② | 删 `ui_diagnostics` 内 `raw_diag_row(` 调用（**编译通过**） | `raw_diag_row_is_wired_into_diagnostics` app.rs:21128 |
| R15/T6③ | 删 `write_raw` 内 `push_recent(line)` | `recent_log_lines_are_capped` diag.rs:1140（并连带 `raw_channel_logs_reach_the_diagnostics_file` diag.rs:1086） |

> 变异②证明守护能抓到"三道门完全无感"的漏接线；③证明容量测试真的钉在推入点上（走 `write_raw` 而非 `push_recent`）。
> 另记一条**假变异识别**：R11 执行者最初用"交换 match arm 顺序"作变异 ②——Rust match 按模式取值、与 arm 顺序无关，那是语义空操作（全绿）。已识别并改为"文案映射互换"重做才正确变红；不把空操作当验证。

#### A 级验证（Lead 冻结树亲跑）

```
$ cargo fmt --check                       → 退出 0
$ cargo test --bin scrcpy-pad --locked    → test result: ok. 377 passed; 0 failed   (370→373→377，账本：R11+T1 +3 / R15+T6 +4)
$ python ci/clippy_diff.py                → 当前告警对: 36  基线对: 36  与基线零差异
$ 用户 diagnostics.log md5 B2 前/后        → 46AA0E65D1DBDF1D14FF92D44E3E5EC8 两次一致、mtime 仍 03:11:48（§3.5 未污染）
$ 新测试隔离连跑（§3.9）                   → 5 条新测试各自单跑绿；diag:: 组连跑 2 次 8 passed 稳定，无 flaky
$ 改动面 mtime 扫描                        → 仅 src/app.rs 与 src/diag.rs（两个文件，符合批次文件面）
```

#### B 级（未验证，需真机/人眼）

1. **warn 级启动烟测看行**（计划:122 的验收语）：需以 `SCRCPY_PAD_LOG=warn`（或 settings.json `log_level:"warn"`）启动，打开诊断面板确认：①Raw 三日志原文作为可见行出现；②"最近日志（含 Raw 通道）"块显示最近 8 行；③面板内容与 `diagnostics.log` 一致。**执行者未运行 GUI，不得视为已验证。**
2. **引擎状态行观感**：琥珀/正常/红三色实际显示、左栏排版与换行、hover 提示观感。一次运行即可判定的观察点：Windows 启动后左栏中段出现"原始输入待定/已启用/降级"一行；只报绝对坐标的指针设备（触摸板/数位板）应额外出现红字行；非 Windows 上这一整段不出现任何行。
3. **error 级别下面板"最近日志"只剩 error 行**：设计取舍（面板行 = 真正落盘行，随级别变化），是否造成"面板空了"的困惑属观感项。

#### 已知问题

1. `raw_status_line(state, count)` 目前忽略 `count`（`let _ = count;`）——四态文案只由 state 决定，零事件的红字由 `raw_probe_alert` 叠加。参数保留是为与 `raw_diag_row` 同形；若下轮要按 count 分叉可直接用上，不构成缺陷。
2. `RECENT` 为进程级全局缓冲（64 行）：锁序固定 `Diag → RECENT`、中毒静默降级、且**仅在 `writeln!` 成功后推入**，故界面缓冲不可能反过来影响日志落盘（取证生命线不受损）。
3. 面板"最近日志"块与既有 `ui_log_list`（self.logs，200 条）是**两条不同数据源**：前者=真正落盘的诊断行（含 capture 线程），后者=UI 自身记录。本轮未合并，是否合并属 UI 收敛（W7/U 线）。

### B3 登记：bench 高频位移注入器（2026-10-15，W2/B3 = R5）

**性质**：bench 基建（工作区外 `D:\GitHub\scrcpy-pad-bench`，**非 git 仓库**）。主仓库零改动 ⇒ 主仓三道门仍为 B2 的 377；C9 锚点不变（V2-5）。

#### 改动清单（bench 侧 3 个文件）

| 文件 | 内容 |
|---|---|
| `tools/injector/src/main.rs` | 356→893 行。新增 `--mode key\|motion`（默认 key，key 路径逐字不变）；Win32 手写 `MouseInput`(32B)/`Point`/`INPUT_MOUSE`/`MOUSEEVENTF_MOVE(_NOCOALESCE)`/`GetCursorPos`/`SetCursorPos`/`GetSystemMetrics`；`inject_motion()`；纯函数 `interval_ns_from_hz`/`plan_motion`/`motion_axis_ok`/`motion_delta_ok`/`rate_from_count`；严格解析 `*_strict`（非法 ⇒ exit 2，不静默）；`MotionCfg` + `run_motion`（阳性对照/测量段/光标复原）；motion 参数 `--rate-hz/--dx/--dy/--duration-ms/--alternate/--recenter-ms/--motion-csv/--no-coalesce/--selfcheck-only`；11 条单测 |
| `bin/injector.exe` | 重建并拷入（既有约定）。sha256 `03f890d2…9757`(215040) → **`31e8d084…ff81`**(229376) |
| `results/00-基建/hashes.txt` | 更新 injector.exe 一行；其余 9 条复算 10/10 OK |

> 落点裁决：**扩展 injector 而非新建 crate** —— R5 预算 `[改≤6]` 且指令明写"复用 bench 基建"；bench 无 workspace，新建 crate 要复制 ~150 行 Win32/QPC/CSV 代码并多一份构建/哈希登记。

#### 变异验证（§3.1，4 条红，全部还原）

| # | 变异点 | 红测试 |
|---|---|---|
| ① | `interval_ns_from_hz` 量纲错（`/1_000_000`） | `interval_ns_from_hz_three_rates` main.rs:811 |
| ①b | 同上改 `*rate_hz` | 同上 |
| ② | `plan_motion` 翻转条件 `==1`→`==0` | `plan_motion_alternate_*` :826/:834/:850 |
| ③ | `motion_axis_ok` 上界 `*4.0`→`*0.4` | `motion_delta_ok_accepts_one_to_one` :856 |

> **任务书自身一处口径被纠正（执行者提出，Lead 认可）**：任务书建议的变异"`interval_ns_from_hz` 改成 `1_000_000_000/rate_hz`"**其实就是正确公式**，拿它当变异不会红；改用两个真正的量纲错替代。
> **方法论事故**：`Copy-Item` 还原保留旧 mtime ⇒ cargo 跳过重编、复用变异版二进制，产生"已还原仍红"的**假红**。已定位（touch mtime 强制重编）；与 B1 同一坑、第二次踩到 ⇒ 建议写入环境坑点清单（§9）。

#### A 级验证（Lead 亲跑）

```
bench: cargo test            → test result: ok. 11 passed; 0 failed
bench: cargo build --release → Finished（exit 0）
bench: 阳性对照 --selfcheck-only → 自检通过 (光标 x 1351→1471)、exit 0、光标复原
bench: 负对照 --dx 3000 --count 5（强制边缘钳位）→ 聚合断言失败、exit 1、光标复原
bench: 参数校验 --rate-hz 9000/500/abc、--mode foo → 全部 exit 2
bench: key 路径回归 --vk 0x76 --count 3 → CSV 表头 idx,vk,event,planned_ns,actual_ns 未变、exit 0
主仓: fmt 0 / 377 passed 0 failed / clippy 36=36（零差异，主仓本批次未改动）
§3.5: 用户 diagnostics.log md5 46AA0E65… 未变
```

#### ⚠️ 关键负面发现：8000Hz 计划速率无法达成（Lead 独立复现）

Lead 亲跑速率扫描（各档 `--count 1000 --alternate 100`）：

| 计划 | 1000 | 2000 | 4000 | 8000 |
|---|---|---|---|---|
| 实测 | 998 | 1078 | 1075 | 1063 |

- **饱和在 ~1.06–1.14kHz，与计划值基本无关**；突发长度（100/300/1000/3000）、CSV 写/不写、`--no-coalesce` 都不改变该上限。
- 事件间隔中位数 ≈ 900–960µs，**最小值 250µs**（= 裸 `SendInput` 单次成本）⇒ 瓶颈是**系统输入队列背压**，不是本工具的节拍循环（计划时刻逐事件精确：8000Hz 的 planned delta 唯一值 125000ns）。
- 旁证：**未经改动的键盘路径**同样撞顶（`--interval-ms 0.1` 实测 ~730Hz，中位间隔 1364µs）⇒ 属平台既有上限，非 R5 引入。

**对后续 block 的影响（必须据此调整，不得沿用原假设）**：
1. **R9（8000Hz 合并率断言）**：原指令假设能真跑 8kHz。实际只能到 ~1.1kHz ⇒ R9 需改写为"**在 ~1kHz 上限下测合并率**"，或改用不经 SendInput 队列的手段（如直接喂 trace/回放）。**R9 原验收语在改写前不成立。**
2. **R10（端到端 p99 vs 注入速率 1000→8000）**：同样受此限；"速率"轴实际只有 1000–1100 可用区间。
3. **R6/R7/R8**（指针加速/DPI/1000Hz 甩动矩阵）：**1000Hz 档可达成**，不受影响；R8 的"1000Hz 甩动"仍成立。
4. R5 自身验收语"速率可调、计数可断言"**成立**（1000–8000 全档可配置、计划速率精确、计数可断言），但**"可调"≠"投递速率达标"**，该区分已写入工具输出与登记。

#### B 级（未验证）

1. **R6–R10 真实矩阵**未跑（属 B4 专会，需用户+设备在场）。
2. `run_scenario.ps1:216/:261` 的 events 门禁会把**纯位移场景**判 `INJECTION-DELIVERY-FAILED` —— 本轮按令未改脚本；R6–R10 场景需保留 ≥1 次键盘按下（建议 F7=0x76）或届时改脚本。
3. 多显示器/非主屏中心（`--recenter-ms` 用主屏中心）；系统鼠标加速设为 0 时判据带会显著收窄——均未实测。
4. 对称 `--alternate` 使 Σ=0 ⇒ 比例判据退化为"净漂移 ≤ slack"；有效 ratio 需 `--alternate 0` 跑（补充跑得 1.140/1.650）。

#### 已知问题

1. **`SendInput` 相对位移 → 生产 Raw Input(WM_INPUT) 的投递关系是未测外部假设**（外部证据表明 SendInput 位移不保证以 WM_INPUT 派发）⇒ 不得据此推断生产链路行为，需 B4 实测。
2. `--rate-hz > 1100` 时"计划速率"与"投递速率"背离，工具已把两者都打印，避免误读。

### B3r 登记：缓存索引 U1+U2（2026-10-15，W2/B3r）

**性质**：纯只读（`[读≤6 改0 验0]`），**零代码改动** ⇒ 主仓三道门仍 377；产出落执行计划附录 C2/C3。

| 块 | 产出 | 落点 |
|---|---|---|
| U1 | **规范条款索引 136 条**：TH-01..43（theme.rs 视觉规范）/ HP-01..67（help.md 行为承诺）/ UX-01..18（录入取消语义）/ CN-01..08（已知冲突） | 附录 C3 |
| U2 | **`ui_*` 53 函数清单 + 入口面矩阵 + 10 条缺口/存疑** | 附录 C2 |

#### Lead 独立复核（§8.1，全部命中）

| U2 论断 | Lead 实测 | 判定 |
|---|---|---|
| `track_rec_card` 调用点仅 4 处 | app.rs:7361/7655/11004/11260 + 定义 :18772 | ✅ |
| `track_rec_config_ui` 唯一调用在卡片内 | 定义 :18706、调用 :18877 | ✅ |
| 宏虚拟弹窗区间无录制卡片 | :9272–10025 区间 `track_rec_card` 命中 **0** | ✅ |
| 守护测试 entries 硬编码 4 条 | :20882–20887 与 4 调用点一一对应 | ✅ |
| 遮板无草稿/实时轨迹 | `fill_mask_shapes` :13583 区间 `self.draft`=**0**、`track_rec`=**0** | ✅ |
| `ShapeKind::Swipe` 生产不构造 | 定义 mask.rs:103；**app.rs 命中 0**；生产走 `frame.tracks.push(Polyline)` :13688/:13718 | ✅ |

#### 关键发现（U8+ 的直接输入）

1. **C2-C#1：浮层（扩展宏虚拟键位弹窗）里的滑动没有录制卡片** —— 与 V2-4 **同型遗漏**，只是发生在浮层；虚拟层能定义滑动却录不了轨迹且无提示。
2. **C2-C#2：'启用录制+快捷键'只在有滑动卡片渲染的那一帧存在** ⇒ 界面上没有滑动在显示时，用户**找不到**这两个程序级设置（2026-10-14 主推的快捷键录制整体不可达）。
3. **C2-C#4：遮板上没有草稿/实时录制轨迹** ⇒ 与 AGENTS §0.8"用户截图=地面真相"口径不一致。
4. **C2-C#5：遮板 `ShapeKind::Swipe` 是死变体** ⇒ 自检的"折线画上了"断言走的不是生产写法（生产用 `Polyline`）——**自检绿 ≠ 生产对**（§0.7 同类风险）。
5. **C2-C#6：入口守护测试的清单硬编码 4 条** ⇒ 新增第 5 个入口时测试**不会红**；它守护的是"这 4 个没被删"，不是"所有入口都接了"。
6. **CN-06：三处"只有完成、没有取消"**（改范围/滑动曲线/宏录制）与 UX-01/UX-16 成对性要求相冲。
7. **CN-08：UX 方案内 app.rs 行号已漂移**（文档 :2910/:2929/:2435 → 实际 :4574/:4593/:4098）⇒ 引用一律以当前文件为准。

#### 未验证（B 级 / 存疑，本轮只读未复现）

1. **C2-C#9 存疑**：宏弹窗画布把实时录制会话画到虚拟层（`virtual_layer=true` 时用虚拟 profile 作 src 却用实时 `track_rec` 按下标匹配）⇒ 下标未必对齐，**需运行时确认**。
2. **C2-C#10 存疑**：同帧双画布（宏弹窗 :9579 与小窗 :17830）可能重复 `push_track_point` ⇒ **需运行时确认**。
3. U1 的 CN 条目行号：审计文档 O-11 给的 app.rs 行号已失效，CN-02 改引本次实测行号；theme.rs 测试段行号仅作"有守护测试"佐证。

#### 已知问题

1. 缓存索引是**快照**：行号随后续 block 改动漂移，U8+ 引用时应以符号名重定位（与 R1 表同口径）。
2. U2 的 10 条缺口中 #7/#8 属"有意设计/能力不对称"，非缺陷，已在 C2 表内区分标注。

### B4-fix 登记：宏坐标越界 + 宏 lane 复用（合批纯测试，2026-10-15）

**性质**：真机轨迹分析（B3 后）暴露两条，用户 2026-10-15 裁定"影响没测过 → 不修复、生成测试"。**零生产行为改动**，只加特征测试。不重建发布物 ⇒ C9 锚点不变。

#### 两条问题与裁定

| # | 真机证据 | 取证结论 | 裁定 |
|---|---|---|---|
| ① 宏触点坐标越界 | trace-1792054736 出现 4 条 `(1031,1888)`/`(1016,2471)`，屏幕仅 2772×1280（rel y=1.475/1.931）；干净样本 14 例同点双指 | profile.yaml **无任何存储越界坐标**（全 0..1、无像素>50）⇒ 越界是**运行时产生、一次性、非系统性**（51168 触点中 4 条=0.008%）；来源静态未定位 | **不 clamp**（怕掩盖潜在坐标空间 bug）；生成特征测试锁定"换算/规整环节都不钳位"的现状，将来要 clamp 须让测试故意变红强制决策 |
| ② 宏 lane 复用余薄 | 真机实测 9.645s 内按 8 次（=lane 数），最坏同 lane 余量仅 703ms | `MACRO_EXEC_LANES=8` + `exec%8` 是**刻意设计**（engine.rs:6618–6623，0.1.5 撞车教训；现有测试 :13615 已锁取模） | **不改分配逻辑**；补测试把"第 8 次撞第 0 次 lane"的边界显式钉死 |

#### 新增测试（3 条，全在 mod tests 区，生产代码零编辑）

| 测试 | 位置 | 锁定 |
|---|---|---|
| `mapper_point_passes_through_out_of_range_relative` | keymap.rs:4544 | `Mapper::axis` 对越界相对坐标原样换算不钳位（复现真机 0.372,1.475→1031,1888；2.0→5544；负值→-1386；legacy 像素直通） |
| `normalize_clamps_wheel_geometry_but_leaves_macro_coordinates_untouched` | keymap.rs:4580 | `normalize()` 只钳轮盘几何，**完全不遍历宏坐标**（Click 2.0/1.475 分毫不动） |
| `macro_lane_reuse_collides_every_eight_executions` | engine.rs:13647 | 硬断言 `LANES==8`；第 8/16 次执行撞第 0 次 lane；前 8 条基址两两不同、间隔恰 STRIDE |

#### 变异验证（§3.1，3/3 红且精确，全部还原复绿）

| 条 | 变异点 | 红测试（panicked at） |
|---|---|---|
| T1 | `Mapper::axis` 加 `v.clamp(0,1)` | `mapper_point_passes_through…` keymap.rs:4548（left (1031,1280) vs right (1031,1888)） |
| T2 | `normalize()` 内给宏 Click 加 clamp | `normalize_clamps_wheel_geometry…` keymap.rs:4634 |
| T3 | `MACRO_EXEC_LANES` 8→9 | `macro_lane_reuse_collides…` engine.rs:13649 |

#### A 级验证（Lead 冻结树亲跑）

```
$ cargo fmt --check                       → 退出 0
$ cargo test --bin scrcpy-pad --locked    → ok. 381 passed; 0 failed  (378→381，+3 全为新增测试，无减少)
$ python ci/clippy_diff.py                → 36 = 36 与基线零差异
$ 生产符号零改动审计                        → Mapper::axis 无 clamp / normalize wheels 后直进 combos / LANES=8 / 三测试在位（Lead 逐一读源码确认）
$ 用户 diagnostics.log md5 跑前/跑后        → 3786284E4854985E4BB0F1BF633317E9 两次一致（§3.5 未污染）
$ 改动面                                    → 仅 src/keymap.rs(+122) src/engine.rs(+48)，全落 mod tests，生产逻辑一行未动
```

#### B 级（未验证）

1. 越界触点的**运行时来源未定位**（本次按裁定不追查）；"设备端如何丢弃/表现如何"未测。
2. 同点双指（14–16 例）本批**未处理**，另出方案（见 B4-fix-3）。

#### 已知问题

1. 这三条是**特征测试**（锁定现状），非"修复验证"。若将来决定钳位宏坐标或改 lane 数，T1/T2/T3 会红——那是**预期的强制重新决策**，不许改断言凑绿（§0.6）。
2. 越界来源追查需运行时守卫日志（在 `Mapper::point` 或宏展开处对 rel 坐标 ∉[0,1] 记一条 diag_warn），属**生产改动**，本批按裁定未做；若下轮要查，另开 block。

#### 结案补充（同日，真机轨迹复核 + #3 子智能体方案）

1. **#1 越界来源已破案（升级为"已测-成立"）**：4 条 `(1031,1888)`/`(1016,2471)` = **转屏瞬间坐标空间翻转**（横 2772×1280 ↔ 竖 1280×2772）时 key22/key36 的**正常落点**被投到当时真实空间。算术逐位命中：key22 存储相对坐标 `(0.8051948, 0.68125)` × 竖屏 (1280,2772) = (1031,1888)；key36 `(0.79401153, 0.89140624)` = (1016,2471)。相对坐标始终 ∈[0,1]，**非坏配置、非非法值** ⇒ **反证"不 clamp"裁定正确**（钳 [0,1] 空操作，钳像素会破坏转屏期合法触点）。上面"已知问题 2"的守卫日志需求降级为可选。
2. **#3 同点双指已出方案**：[宏触点同点治理方案-2026-10-15.md](宏触点同点治理方案-2026-10-15.md)。要点：根因=宏录制步骤回放到"该键自己的绑定落点"+手按同键⇒必然同点（instructions 为空却有 30 例，Lead 已复核）；推荐 **D 科学选点判据 + C 编辑期拦截为主、B 运行时兜底**，**否决 A 复用 pid**（与 engine.rs:79-86/6618-6623 撞车结论冲突）；阈值 24/56px 有实测双峰支撑（0px 与 ≥210px 之间全空，251 对共按）；Lead 决策：硬冲突=警告不禁存、坐标空间探针列为实现前置项。本轮**不改代码**。
3. 复核口径修正：子智能体报"aim+aim 同点 842 例"系口径差异，Lead 像素精确相等口径实测 **0 例**；方案的"排除瞄准"白名单仍保留（防口径变化误伤）。

### B4 会前登记：bench 专会基线与裁决口径（2026-10-15 深夜）

**性质**：**会前登记，非交付登记**。B4 = 纯实验/取材批（执行计划:79），不改生产逻辑 ⇒ 本轮主仓库**零代码改动**（仪表脚本落在工作区外 `D:\GitHub\scrcpy-pad-bench\results\12-motion-injection\`，非 git 仓库）。

#### 会前三道门（Lead 在冻结树上亲跑，输出完整读过，§3.4）

```
$ cargo fmt --check                       → 退出码 0，无输出
$ cargo test --bin scrcpy-pad --locked    → test result: ok. 381 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
$ python ci/clippy_diff.py                → 当前告警对: 36 基线对: 36 … 与基线零差异
```

381 与 B4-fix 登记 :648 一致 ⇒ **本轮增减 = 0**（符合 §3.10 测试数账本）。

#### C9 对账：发现不一致，已判明（不阻塞开工）

协议 V2-5 交付登记表（:394）记 `release/windows-x86_64/scrcpy-pad.exe` = 11,707,904 B / `559750c194f82e68ca3314aeb9c0be3d`；实盘为 11,712,512 B / `6c6a806aa670987c3e8e3bbe68d5793f`（2026-10-15 17:08 部署的 B3 aim 节流版，当时未登记）。另有 `scrcpy-pad - 副本.exe` = 11,707,904 B / `559750c1…`，是用户手工复制的 V2-5 老包。

**判定：发布目录比登记新**；B4-fix 只加测试、生产逻辑一行未动（:652），故 `6c6a806a…` 与当前树生产逻辑等价。zip/tar.gz 未重建，仍停 V2-5（linux 包过期按用户 2026-10-15 令忽略、不登记）。

**会前重建（用户 2026-10-16 选定"先重建再用"）**：`cargo build --release --locked` → `Finished release profile [optimized] target(s) in 2m 46s`，退出码 0；产物 `target/release/scrcpy-pad.exe` = 11,712,512 B / md5 `305e3a278a55650e640cc9837d454069`，已拷贝部署为 `release/windows-x86_64/scrcpy-pad.exe`（同大小同 md5，mtime 02:09:13）。部署前 `tasklist` 无 scrcpy 进程 ⇒ 无文件锁冲突。**专会一律用 `305e3a27…` 这一份**（这是当前树的权威锚点，取代上面 `6c6a806a…` 的推断等价说法）。`scrcpy-pad - 副本.exe` 保持 `559750c1…`（V2-5 老包，未动）。zip 包内 exe 仍是 11,707,904 B（V2-5）⇒ **已知过期，本批未重建**（B4 不产交付物，按 §6.3 显式声明）。

#### 会前取证锚点（§3.5 未污染自查）

**两处 02:3x 修正（后续会话必读，否则会误报事故）**：

> ① **权威 exe 锚点更新**：用户选"先重建再用"，Lead 的重建链 = `apply 变异 → build(legacy) → restore → 冻结树上复跑三门 → 再 build(生产)`。最终生产锚点 = `release/windows-x86_64/scrcpy-pad.exe` md5 **`897e0c397148bf2a5307be3389ab9a31`**（11,712,512 B，02:31:47 部署自 `target/release/`，同 md5）。上文 :687/:689 的 `6c6a806a…` 与后来短暂登记的 `305e3a27…` 均为历史锚点（cargo 不保证逐次同哈希，差异无源码含义）。**R6 对照二进制** = `D:\GitHub\scrcpy-pad-bench\bin\scrcpy-pad-legacy-r6.exe` md5 `0d3bf137af02d82c175cf9bed0715e4f`（11,713,536 B，与生产只差 `SCRCPY_PAD_NO_RAW` env 门控一处）。变异已当场还原：`src/capture.rs` md5 回到 pristine `c4337f7d6ddaf6aee990f6b868de983f`，`.bak-r6` 已删，还原后亲跑三门**全绿**（fmt 退出 0 / `381 passed; 0 failed` / clippy `36=36` 零差异），`git status --short` = 30 项（与 B4-fix 后一致，无残留）。
>
> ② **`diagnostics.log` 会被启动销毁**：`src/diag.rs:180` `File::create(&path)` + `:236` 自述「文件每次启动整体重写」⇒ 专会第一次启动程序就会覆盖会前这份 75,758 B 日志。§3.5 的"跑后 md5 比对"在 B4 **不能**用"与 `3786284e…` 相同"作判据（必然不同，属预期）。⇒ Lead 已在任何启动发生前把原始文件冻结备份：`D:\GitHub\scrcpy-pad-bench\results\12-motion-injection\preserved-traces\diagnostics.pre-B4.log`，md5 `3786284e4854985e4bb0f1bf633317e9`、75,758 B（与下一条登记的数值逐字一致，备份已校验）。**下一会话的 §3.5 自查口径 = 与这份备份比，不是与配置目录里那份比。**

用户 `diagnostics.log` md5 `3786284e4854985e4bb0f1bf633317e9`、75,758 B、mtime 2026-10-15 17:58:11，与 B4-fix 登记 :651 一致。

config 目录现存三份 trace：

| 文件 | 大小 | md5 |
|---|---|---|
| `trace-1792054172.jsonl` | 10,404,954 B | `59eee7d57d3249623d244c4fcae207a7` |
| `trace-1792054727.jsonl` | 8,053 B | `915b82e6ac2f1d780e7ef345a11df5ab` |
| `trace-1792054736.jsonl` | 78,928,397 B | `9762bcc7edc06ac505f0491be0e0b1a0` |

**风险**：`MAX_TRACE_FILES=3` 会在程序每次启动剪掉最旧一份 ⇒ **专会前不得再启程序**，除非先把这三份复制走。

#### 仪表口径（会前新发现，必须记录，否则 R9 会被误读为"数据缺失"）

`src/capture.rs:4436-4457` —— 5 秒汇总行里的 `" · 位移合并 {N}(条)→{M}(批)"` 与钩子耗时文案，在 `p99 <= HOOK_LAG_WARN_NANOS`（5ms，capture.rs:2568）时走 `diag_debug!`，仅超标时走 `diag_warn!`。用户 `settings.json` 当前 `log_level:"warn"` ⇒ **正常态下这行不落盘**。

**处置（零代码）**：专会一律以环境变量 `SCRCPY_PAD_LOG=debug` 启动（`diag.rs:82 ENV_LEVEL`；`settings.rs:55`/`help.md:189` 明写环境变量优先）。

**备份口径**：trace 的 `st.wmin`（合并前 WM_INPUT 累计，capture.rs:3071 / trace.rs:312-320）对 `in/motion` 行数（**合并后**，engine.rs:4343、capture.rs:543-552）之比，不受级别影响。

#### B4 承重前提与一证一假（§1 C7）

假设 **P** = `SendInput` 注入的相对位移会以 `WM_INPUT` 派发到生产 Raw 通道（协议 :573 已列为未测外部假设，此前**未进 ③ 表**，本轮补登记，见下 U6）。

| 分支 | 若对观察到 | 若错观察到 |
|---|---|---|
| P | `st.wmin` 随注入上升、`raw` 由 1→2、trace 出现 `in/motion`、`位移合并 N→M` 的 N>0 | 注入器 stdout 报"自检通过/光标Δ已动"（`main.rs:398-427` 只验系统光标），但 `wmin` 不涨、`raw` 停在 1、无 `in/motion` |

30 秒单档注入即可判定；P 若证伪，R6/R7/R8/R9/R10 全部改走"直接喂 trace/回放"手段，**不得再用 bench 注入**。

#### Lead 裁决口径（B4 会前定死，执行者不得自行改判据）

1. **R9**（计划 :116）采**双口径**：主 = debug 5s 行的 N→M；备 = Δwmin / 批数。通过条件由原 `M ≤ ~N/8`（8000Hz 前提已作废）**改为** `M ≤ N` 且位移守恒偏差 ≤ 5%；N:M 实测比**只登记为数据**，不设压缩比门限（1kHz 投递上限下该比值天然趋近 1，门它没有意义）。
2. **R10**（计划 :117）通过条件原写"增幅 < R5 登记阈值"，经全仓穷举**该阈值不存在**（协议 B3 登记 507-574 无 p99 数值；bench 侧唯一阈值是位移带 `[0.5,4.0]×|Σ|±4px` 与 `MOTION_SLACK_PX=4`，基建说明 §3.3 是锁/钩子耗时阈值）。本轮由 Lead 定为：**三档（1000/1050/1100Hz）p99 均 < 5ms 且相对 1000Hz 档增幅 < 20%**；5ms 取自生产既有告警线 `HOOK_LAG_WARN_NANOS`（capture.rs:2568），非自创新数。**该裁决需在交付登记里标注为"阈值来源=Lead 裁决"**。
3. **R8**（计划 :115）"钩子 300ms 超时日志=0"里的 300ms 是 Windows 系统钩子的外部事实，生产代码里没有该常量（只有 `RAW_STALL_TIMEOUT_MS=500` :4884、`RAW_PENDING_TIMEOUT_MS=700` :2853）。采**三条日志计数 = 0** 的可判定口径：`原始输入静默 …ms`（capture.rs:4337）、`原始输入注册后持续丢弃位移 …ms`（:4321）、`退回光标差路径`；配 `st.streak` 峰值与 `t` 空洞检查。
4. **T3**（计划 :131）"B4 真机 Raw 原文"计划未定义所指（全仓唯一出处就是那一行；§3.8 举的三类原文全是 adb 设备侧文本）。Lead 裁决：B4 只取三件 —— ① `st` 行序列原文（真机 Raw 通道行为证据）② `diagnostics.log` 里 Raw 三行原文（仓库已有同型先例 `diag.rs:1027`）③ **R6 的「Raw 关」对照格由临时变异产出的 legacy 二进制提供**（裁决补记，02:2x）：事实依据 = `raw_takes_motion(PENDING|RELATIVE)=true` ⇒ `hook_forwards_motion=false`（capture.rs:2905-2907 / 2947-2948），即 Raw 通道活着时光标差老路径**根本不发位移**，不建对照就没有 R6 的第二列；bench 里的 `baseline-013`/`baseline-014-0928` 是 0.1.x 老版（无 W1-3 合并器），用它们对照会把版本差异混进路径差异。做法 = 在同一棵树上单点插入 `SCRCPY_PAD_NO_RAW=1` env 门控（未设 ⇒ 逐字等价原文），构出 legacy exe 供专会选用，**跑完当场还原并复跑门禁**；施具与还原器 `D:\GitHub\scrcpy-pad-bench\results\12-motion-injection\r6_mutate.py`（拒绝叠层变异、锚点须恰好命中 1 次、还原后强制 touch mtime ⇒ §9 假红坑）。这是**临时变异，不构成交付**；若日后要长期保留该开关，另开 block。**第④件（逐条 `WM_INPUT` 的 `usFlags/lLastX/lLastY/usButtonFlags` 原文）目前生产零落盘**（全仓只有 `capture.rs:4301` 一处 trace 写点，字段集不含这些；`handle_raw_input` 的入参是 `HRAWINPUT` 句柄，仓库里没有"从固化数据重建 RAWINPUT 喂进去"的路径，可离线喂的最外层只有那三元组 + `usButtonFlags`，见 `raw_kind` :2876 / `raw_admit` :2890 / `note_raw_buttons` :3015），落它属**生产改动** ⇒ 另开 block，不进 B4。
5. **T5**（计划 :133）"逐点断言"的可行边界已核实：`trace::read` 丢弃 `st`/`aim` 行（trace.rs:459-521），`sim::replay` 只喂 `in/btn`+`in/motion`、忽略全部 `out`（sim.rs:623-629），且 `feed_motion` 直入引擎 `rx`、**绕过合并器与整个 Windows 捕获层**（sim.rs:446-454）⇒ 回放能验"引擎→设备"的守恒/泄漏/预算（`path_stats`/`assert_no_leak`/`assert_pointer_budget`/`assert_no_dropped_touch`/`assert_clean`，sim.rs:533/578/586/594/608），**不能**用来测 1ms 合并率。B4 取材段要求：时长 ≥180s、含 btn/motion/out 三类、屏幕 2772×1280、active 方案 = 「🔥正常」，回放入仓方式落 B6（计划 :82），仓库无 `tests/` 目录与 fixture 机制、且 §3.5 禁碰 `config_dir()` ⇒ 具体载体 B6 再定。
6. **滚轮取材**：R13（计划 :120）要"B4 滚轮数据"，而生产 Raw **不取滚轮**（`MixedAbsolute` 分支 capture.rs:3119-3141、`usButtonFlags` 位图 :3093）。裁决：专会加一段 60s 滚轮操作，只采 trace + debug 日志供 R13 **只读评估**，不改代码、不做去重。
7. **bench 侧门禁冲突**：`run_scenario.ps1:216/:261` 会把纯位移场景判 `INJECTION-DELIVERY-FAILED`（协议 :567）。裁决：**不改该脚本**（不属本轮授权），B4 用独立 runner + 手工对齐 stdout，避开该门禁。
8. **R6/R7 需改用户机器**（系统指针加速档位、显示缩放 100%/150%），AGENTS.md §4 :80 要求"写用户机器的探测需用户当轮同意" ⇒ **用户 2026-10-16 当轮已同意并裁定"本轮做，我自己改并复原"** ⇒ R6/R7 已排进跑批序列（README §3 ⑤⑥：R6 四格 = {Raw 开/关}×{加速 开/关}，R7 两格 = 100%/150%）。全部 Windows 设置改动与复原由用户手动执行，Lead 与执行者**不代改任何系统设置**。R6 的「Raw 关」列由 legacy 对照二进制提供（见 T3 裁决 ③）。
9. **三项用户裁决（02:0x 询问卡）**：① 专会 exe = 重建后那份（用户选"先重建再用"，非直接复用 6c6a806a）；② 取证粒度 = `SCRCPY_PAD_LOG=debug` + `--trace` 显式落 bench 目录（不改 settings.json）；③ 历史取证 = 三份 trace **全部**先复制到 `preserved-traces/` 再允许启动。⇒ 上面第 8 条与 :687-689 的"专会一律用 6c6a806a"按本条 ① 作废，权威锚点见「会前取证锚点」修正 ①。

#### 未验证（B 级）

1. 专会**全部数据尚未采集**。
2. 仪表脚本有效性待合成样本阳性/阴性对照（§3.3）跑过才算证过。

#### 已知问题

1. **测量工具本身是干扰源**——历史事故：读 30MB 活动 trace 造成被测程序 902ms 引擎停顿 + 826ms 捕获停顿 ⇒ 监视器**只准追加读**、大文件先拷贝再分析。
2. trace 的 `t` 是毫秒整数，逐事件 sub-ms 间隔读不出来，只有顺序（R10 的 p99 必须来自 debug 行，不能来自 trace `t`）。
3. `--recenter-ms` 用主屏中心（main.rs:143-149），多显示器/非主屏未实测（协议 :568）。
4. 用户当前跑的是手工副本（V2-5），新构建未经真机验证。

---

### W3/B5 + B5r 交付登记（2026-10-16 19:xx）

**批次**：B5（T2、T4、T7）+ B5r（U5、U6；U3/U4 推迟）。**Lead 亲跑门（改码前 → 改码后，同一冻结树）**：`cargo fmt --check` 退出 0 → 退出 0；`cargo test --bin scrcpy-pad --locked` **381 passed 0 failed 0 ignored → 386 passed 0 failed 0 ignored**（+5，全为新增；无删除；§3.10 账本）；`python ci/clippy_diff.py` **36:36 → 36:36 零差异**。

#### 改动清单（A 级）

| 文件 | 行数 / md5 | 改动 | 为什么 |
|---|---|---|---|
| `src/sim.rs` | 1916 行 / `4ca4091d50baec0287b66c7fd7d6e78e`（原 1273 行） | +643 行：T2 探针（`CrossVerdict` :681、`CrossReport` :709、`CrossLedger` :770、`ledger_from_trace` :807）；T4 助手（`touch_seq` :1479、`edges_of` :1492）；5 条测试（:1508、:1586、:1649、:1710、:1786） | T2 交叉对账（仅诊断）；T4 headless 端到端三场景；T7 降级演练 |
| 生产代码 | `src/engine.rs` `423f53480d4f98f4c69117a3667881be`、`src/keymap.rs` `4c35a8550f45e0ee06d873d9b0b1361d`、`src/capture.rs` `c4337f7d6ddaf6aee990f6b868de983f` | **零交付改动**（仅变异验证期间临时单点改，已还原并 touch mtime；md5 与变异前逐位一致） | §3.1 变异必须破坏被守护行为 |

**T2「不承重」的独立证明**：`grep -rn "ledger_from_trace\|CrossLedger" src/ --include=*.rs` 除 `src/sim.rs` 外**零命中** ⇒ 未接任何生产调用点；探针为纯数据结构 + 纯方法 + 只读解析（不写文件、不碰全局），源码注释内引用了协议 §7 账本红线。

#### 变异验证（§3.1；5/5 变红且报精确位置，均已还原）

| 测试 | 变异点 | 变红位置 | 还原证据 |
|---|---|---|---|
| T2 | `ledger_from_trace` 内 `note_raw_tick(state, wmin)` → `(state, 0)` | `src/sim.rs:1526` left 0 / right 4800 | `4ca4091d…` 复原一致 |
| T4-① 甩枪 | `engine.rs` `AIM_MAX_STEP_PX 32.0 → 0.0` | `src/sim.rs:1624` left 0 / right 1 | `423f5348…` 一致 |
| T4-② 点按 | `keymap.rs` `Mapper::axis` 非 legacy 分支 ×0.5 | `src/sim.rs:1695` left (160,1040) / right (320,2079) | `4c35a855…` 一致 |
| T4-③ 抬起 | `engine.rs:6326` Hold 释放分支加 `&& false` | `src/sim.rs:579`「T4 抬起(对照):设备端仍按着 [1000](卡键)」 | `423f5348…` 一致 |
| T7 | `engine.rs` `note_wheel` 的 `fresh_press` 恒 `false` | `src/sim.rs:1872`「滚轮齿的边沿不许丢(pid=1001…)」 left (0,0) / right (1,1) | `423f5348…` 一致 |

阳性/阴性对照（§3.3）：T2 探针四极（raw 在供 / raw 静默 / 绝对设备 / 空轨迹）；T4-① 未进 FPS ⇒ 零注入；T4-② 只喂计数不按键 ⇒ 零注入；T4-③ 只按不抬 ⇒ 仍按着；T7 降级 vs 正常两分支。**测试名**：`sim::tests::t2_cross_ledger_reconciles_replayed_traces_with_both_polarities` / `t4_flick_raw_count_stream_lands_as_one_monotone_touch_path` / `t4_tap_edge_is_exact_while_a_raw_count_stream_flows` / `t4_release_edge_ends_the_touch_path_exactly` / `t7_raw_unavailable_drill_keeps_key_and_wheel_edges`。

**T2 日志样例（测试实跑原文，两极性）**：`原始输入对账: raw=2 wmin=4800 心跳=3 位移=2条 净=(25.0,-5.0) 路径=26.2px → 原始输入在供位移(钩子光标差已让位)`；阴性 `…raw=0 wmin=0… → 有位移却一条 WM_INPUT 都没有:位移走钩子回退路径`。

#### §3.5 取证（测试绝不写用户配置目录）

`%APPDATA%\scrcpy-pad\config\diagnostics.log`：跑测试前 / 后 `md5 = 49a586307ceda631f1d3205812ef7eda`、`size = 46,717 B`、`mtime = 1792150231` **三项全等**（Lead 亲测）。

#### B5r（U5、U6）产出摘要

- **U5 定位表**（4 项均给到 `文件:符号:行号`）：每帧 clone（`app.rs` `ui_look:14005` / `ui_combos:11184` / `ui_aim_body:15173` / `ui_action_editor:10254` / `fill_mask_shapes:13595-13597`）；锁跨帧（`draw_overlay:11788→12461` 跨度 ~676 行、`ui_engine_status:13168→13346` ~179 行；守卫存活至函数末且无 `drop(g)`，对照 `request_style_restart:12822` 有 `drop(g)`）；遮板节流（`sync_mouse_capture:13497-13500`，`RELOCATE_MS=100`，字段 `mask_rect_at:2049`）；`fill_mask_shapes` 每帧推 Polyline（`:13583`，推入 `:13688/:13718`，`decimate(&pts,256)`）。
- **U6 候选表**：草稿双入口（`ui_binds_new:7570` vs `ui_vk_draft:10917`，同构双实现 ⇒ 合并 = U9）；日志三形态（`ui_log_list:13347` / `ui_log_card:13359` / `ui_log_window:13377` ⇒ 抽壳 = U14）；helper 双弹窗（`ui_args_helper:12464` / `ui_adb_helper:16836` ⇒ U15）；半径多处（7 处口径，含列表行 :7410/:7434、草稿 :7617、摇杆卡 :11610、宏虚拟轮盘 :9896、截图拖动 :17678-17696 无钳制、Ctrl+± :6144/:6155 无钳制、`ui_action_editor:10114/:10141` ⇒ 收敛 = U8，先统一范围口径）；宏虚拟窗族（:9272–10024）vs `ui_action_editor:10025`（主编辑器 `ui_bind_row:7276` / `ui_binds_new:7570` / `ui_vk_draft:10917` / `ui_combos:11173` 各自内联）⇒ 全仓三套动作编辑逻辑。
- **U5 的「耗时」列 4 项全部为「静态估计，未测」** ⇒ 见下方未取得项 1。

#### 未取得 / 推迟（不许当成已完成）

1. **U5 耗时未测量**（4 项）——**已于 2026-10-17 补采**：能测的 8 项已实测（见「W3/B6 + B7 交付登记 · B5r 补采」表），锁跨帧 / 每帧频率 / `EnumWindows` 命中段仍为「静态估计，未测」。
2. **U3（布局密度）、U4（按钮一致性）未做**——**已于 2026-10-17 补采**：U4 出 13 条差异/缺口表，U3 出静态密度指标 8 条（**视觉判定仍缺用户截图**）。
3. **T7 的「提示」只断言到探针层**：UI 三态文案（`raw_status_line` / `raw_probe_alert`）属 B2 已变异验证项，本批未重复、未起 GUI。
4. **T7 的「注册失败」是约定前提**（Harness 绕过捕获层），非真机实测。真机观察点 = 进 FPS 后 `st` 行 `raw=0` 且 `wmin` 不涨、同时位移仍在流。

#### 已知问题

1. `src/sim.rs` **未被 `cfg(test)` 门控**（`src/main.rs:16` 是裸 `mod sim;`）⇒ 本轮新增的 T2 探针会进 release 二进制（死代码）。属**既有设计**（Harness/FakeDevice 早就在里面），本轮未改；要收另开 block。
2. T4-① 为确定性把推程压在回转带内并关掉静止归中（换手/归中另有测试）⇒ **不代表产品默认参数**。
3. 队友自报一处**错报已由 Lead 纠正**：B5 执行者称"配置目录只剩 2 份 trace、78MB 那份已不在"——实测三份都在（10,404,954 / 8,053 / 78,928,397 B），且 `preserved-traces/` 有全量备份；`look.json`/`settings.json`/`diagnostics.log` 的 mtime=19:08 由 19:08:31 的 legacy 启动解释（§8.1：队友数字一律不信）。
4. **B4 会后门口径变更**：B5 落地后 passed 数 381 → 386 ⇒ B4 会后门必须按 **386** 读，差值全部来自 B5 新增测试（B4 本身未改任何生产代码，只动了 bench 侧脚本与块目录）。
5. **B4 剩余未采集**：④d 重跑（前置未满足：指针速度 `MouseSensitivity=20`≠正中间 10；程序停在"就绪"、设备 0 台、映射总开关未开 ⇒ 按左Ctrl 不生效，`aim` 行=0）、⑥ `wheel-05`、⑦ `realplay-06`。
6. **R6 矩阵缺一个未记录变量：显示缩放**。R7 两格显式记录（r7-a=100% / r7-b=150%），但 R6 四格（r6-a/b/c/d）的缩放**从未记录**，且从 `blocks/*/diagnostics.log` 切片里复原不出（切片不含启动快照；`前台窗口变化,重新取显示器中心 (1280,800)` 给的是**物理**屏中心，与 `GetSystemMetrics` 的逻辑桌面不等价，不能当缩放探针——r7-a 的 run-notes 已就同一字段记过口径修正）。⇒ ④d 若在 150%（当前值）跑，须在 run-notes 写清实际档位，并在 B4 登记里把"R6 四格缩放未记录"列为矩阵缺口（不许默认为一致）。

---

### B4/④d 重跑登记（2026-10-16 20:3x）—— R6 四格由此闭合

**块**：`blocks/r6-d-rawoff-accoff`（旧格保留为 `…-superseded-1810`，三条作废理由写在它的 `run-notes.txt` 里）。
**条件（Lead 逐项复核，全部写进 run-notes）**：加速=关（`MouseSpeed=0 / Threshold1=0 / Threshold2=0`）；指针速度=正中间（`MouseSensitivity=10` ⇒ 增益≈1.0）；显示缩放=100%（新进程启动快照：主屏 2560×1600）；瞄准态=开（trace `aim en=1 / mode=1 / anchor=1`，756 行心跳）；exe = legacy `0d3bf137af02d82c175cf9bed0715e4f`。
**旧格缺的那一半双证已补齐**：`baseline.txt` 记 `target_image=scrcpy-pad-legacy-r6.exe`、`md5_source=/d/GitHub/scrcpy-pad-bench/bin/scrcpy-pad-legacy-r6.exe`、`diag_NO_RAW行数=1`（旧格是 `未取到 / 生产包哈希 897e0c39… / 0`）。三个根因与修法（`tasklist /v` 被 MSYS 路径转换吃掉参数、md5 固定取候选列表第一个、`tail -c 200000` 窗口小于当时日志）见 `run_b4.sh` 内注释与本登记前文。
**结果**：`Δwmin=0`、`raw=0(未注册)`、`in/motion=30001`、**`aim=339` / `out/touch=44542`**（旧格 `aim=0 / out=0`）⇒ 与 ④c（`aim=300 / out=53251`）可比；p99=16.4 µs，WARN 0 行；注入器 rc=1 属 `--alternate Σ=0` 的已知假阳性。

**R6 验收判据（计划 :113「旧路径差异显著、Raw 一致」）—— 满足**（口径：trace 切片里 `in/motion` 的 `|dx|` 统计；只读静态切片）：

| 格 | 路径 | 加速 | px/事件 | p50 | p90 | p99 | max | ≥400px 条数 |
|---|---|---|---|---|---|---|---|---|
| r6-a | Raw 开 | 开 | 7.999 | 8 | 9 | 12 | 40 | 0 |
| r6-b | Raw 开 | 关 | 8.001 | 8 | 9 | 12 | 40 | 0 |
| r6-c | Raw 关(legacy) | 开 | 139.165 | 2 | 591 | 811 | 852 | 7169 |
| r6-d(新) | Raw 关(legacy) | 关 | 14.005 | 1 | 1 | 417 | 485 | 939 |

- **Raw 路径两档逐位一致**（7.999 vs 8.001 ≈ OS 8:1 聚合给出的 8 px/事件）⇒ 指针加速对 Raw 通道**无影响**。
- **旧路径差异显著**（139.2 vs 14.0，≈10×）⇒ R6 判据达成。
- **大 dx 判别（④c `run-notes` 里预设的实验）**：预设"若为参考点型则**仍出现**、斜率**降到 ~1.0/事件**" ⇒ **两条都成立**：加速关下异常仍在（939 条 ≈419 px/条，占该格总 `|dx|` 的 ~94%），而基座斜率确为 **p50 = 1.0 px/事件**（增益 1.0 的直接证据）。
- **机制仍未定性（不许当结论用）**：同一"加速关"下，aim 关的旧格只有 30 条大 dx（但该格条件不可信）；要判"aim 态是否必要条件"，需另开 block 做"aim 关 + 加速关"的同条件复跑。**未定性前不改任何生产代码。**

---

### B4 交付登记：bench 专会（2026-10-15 22:5x → 2026-10-16 21:0x）

**性质**：真机取材专会（关键路径上唯一的硬外部依赖），覆盖 R6–R10 + T3/T5 取材。**门**：会前 381 passed（B4-fix 后基线）→ 会后 **386 passed / 0 failed / 0 ignored**（+5 **全部来自 W3/B5**；**B4 本身未改任何生产代码**）；`cargo fmt --check` 退出 0；`python ci/clippy_diff.py` 36:36 零差异。两轮门都由 Lead 在冻结树上亲跑。

#### 采集清单（`blocks/`，12 块）

| 块 | 段 | 性质 | 关键结果 |
|---|---|---|---|
| `p-judge-01-01` | ① P 判定 | 注入 30s | P **成立**（Δwmin 3848 / raw 2 / 位移守恒 99.65%） |
| `r8-main-02` | ② R8 | 注入 30s | P 成立；R8 双零 |
| `coalesce-03-03` | ③ R9+R10 | 注入 90s（1000/1050/1100 各 30s） | P 成立；主/备合并口径一致 |
| `r6-a-rawon-accon` / `r6-b-rawon-accoff` | ④a/④b | 注入 30s ×2 | P 成立（Raw 开） |
| `r6-c-rawoff-accon` | ④c | legacy 注入 30s | P 证伪（预期）；aim=300 / out=53251 |
| `r6-d-rawoff-accoff` | ④d **重跑** | legacy 注入 30s | P 证伪（预期）；aim=339 / out=44542；**双证齐**（见上节） |
| `r6-d-rawoff-accoff-superseded-1810` | ④d 旧格 | 作废但保留 | 自带三条作废理由（其 `run-notes.txt`） |
| `r7-a-scale100` / `r7-b-scale150` | ⑤ R7 | 注入 30s ×2 | P 成立（两格 Δwmin 均 3840） |
| `wheel-05-05` | ⑥ 滚轮取材 | 用户操作 63.5s | 滚轮事件 798+758（code 277/278）、in/motion 316、out 2060 |
| `realplay-06-nofps` | ⑦b 真实操作取材（**T5 采用**） | 用户操作 243.7s（`--duration-ms 240000 --gap-before 2`） | **三类齐全**：in/motion 4257 / btn 2604 / out 2429（非 FPS 态） |
| `realplay-06-fps` | ⑦a 真实操作（FPS 态，对照） | 用户操作 183.5s | btn 1789 / out 1880 / aim 1802，**in/motion = 0**（见已知问题 1） |

#### 判据逐条

- **R6**「旧路径差异显著、Raw 一致」→ **达成**。Raw 开两档 px/事件 7.999 vs 8.001（≈OS 8:1 聚合）；legacy 两档 139.165 vs 14.005（≈10×）。详表见上节。
- **R7**「Raw 路径两 DPI 一致」→ **达成**。r7-a(100%) px/事件 8.002 / r7-b(150%) 7.989；分布 p50/p90/p99/max **逐档全同**（8/9/10/40 px）；Σ|dx| 29,854 vs 29,856（差 0.007%）；批数 3731 vs 3737（0.16%，OS 聚合的边界效应，故**不宣称"逐位一致"**）；两格 Δwmin 均 3840、raw=2、WARN 0。
  ⚠ 口径修正（沿用 r7-a `run-notes`）：`AppliedDPI` **不能**当缩放探针（r7-a 时该字段仍是 144 而实际 100%）；判缩放用 DPI-未感知进程的 `GetSystemMetrics`（2560×1600 ⇒ 1.0）。
- **R8**「双零」→ **达成**。7 个注入段全部：静默拍 0、退光标差 0、降级族 0；`st.streak` 峰 0；t 空洞 0（`r6-a` 有 1 处 t 非单调，与历史干净样本 `trace-…172` 第 58581 行同型，已登记良性）；WARN 行 0。
- **R9**（会前已改写口径：在 ~1kHz 上限测合并率）→ 主口径（5s 汇总行 `位移合并 N→M`）与备份口径（Δwmin / in-motion 批）逐格一致（1.0000–1.0589，差值 ≤0.003）；**OS 侧聚合比 = Δwmin / 注入条数 ≈ 0.117–0.129（≈8:1）** ⇒ 应用侧几乎不合并，该指标按会前改写口径解释为 **OS 聚合比**。
- **R10**（会前已改写口径：只测 1000–1100Hz）→ 1000Hz p99 中位 16.4 / 峰 65.5 µs；1050Hz 32.8 / 32.8；1100Hz 32.8 / 65.5；**WARN 行 0**；全库 max 峰 276.7 µs（`r6-c`，legacy 以 1000 条/s 灌控制队列）。**无随速率上升的趋势**。
- **T3 取材**（三件）→ 齐：① 各块 `trace-slice.jsonl` 里的 `st` 行序列原文；② 诊断切片里的 Raw 三行原文；③ legacy 对照二进制（`0d3bf137…`）的**双证**（`target_image` + `md5_source` + `diag_NO_RAW行数`）。
- **T5 取材** → **达标**（经询问卡选项 A 补采 ⑦b）：`realplay-06-nofps` = **243.7 s**（≥180 s）、**三类齐全**（in/motion 4257 / btn 2604 / out 2429）、屏幕 2772×1280、活动方案 `active: 0` =「🔥正常」。FPS 态的 ⑦a 保留为**对照**（`in/motion=0`）。

#### A 级验证表（Lead 亲跑，冻结树）

| 命令 | 实际结果 |
|---|---|
| `cargo fmt --check` | 退出 0（无输出） |
| `cargo test --bin scrcpy-pad --locked` | `ok. 386 passed; 0 failed; 0 ignored` |
| `python ci/clippy_diff.py` | `当前告警对: 36 基线对: 36 … 与基线零差异` |
| `python b4_analyze.py --dir . --out .` | exit 0，**13 块**（含 ⑦a/⑦b 两格与作废格），重出 `analysis.md` / `verdicts.csv`；两个用户操作段被正确标为「**不适用**」（非"证伪"） |

#### 仪表修复（本轮 3 处同类坑，均带阳性/阴性对照）

1. `tasklist /v` 在 git-bash 下被 MSYS 路径转换吃掉参数 ⇒ `target_image` 恒"未取到"（R6 双证缺一半的根因之一）。改 `MSYS_NO_PATHCONV=1 tasklist /fo csv /nh` + awk。
2. md5 固定取候选列表**第一个**（=生产包）⇒ legacy 格把生产包哈希记成本格 exe。改**按运行镜像名选文件**，并把 `md5_source` 落进 `baseline.txt`。
3. `tail -c 200000` 找 NO_RAW 行 ⇒ 日志长过 200KB 后**假阴性**（旧 ④d 格与 ⑥ 格各中一次）。改**全文扫描**（诊断日志每次启动整份重写 ⇒ 不会捡到上一进程的行）。

#### B 级未验证（需真机/用户）

1. 真机手感与观感（R6/R7 的"感觉"层面）。
2. **T5 回放可行性**：`in/motion=0` 的 ⑦ 段能否支撑 `sim::replay` 的逐点断言（B6 待定）。
3. R13（滚轮换源）与 R14（16ms 低频回中成本）属 B7 只读评估，本轮未做。

#### 已知问题（含复现条件）

1. **FPS/瞄准态下位移不进 trace 的 `in/motion`**：⑦ 段钩子回调在跑（每 5 s 3–111 次）却**一条「位移合并」都没有**（⑥ 段有 `140(条)→140(批)`）；两个独立仪表一致（trace 切片 + `monitor.csv` 的 `Σmotion_new=0`）⇒ 位移走 aim 路径、不走通用位移路径。**未定性前不改任何生产代码**。复现条件：进 FPS 模式后移动鼠标 ≥30 s，看诊断里是否只有「钩子回调」而没有「位移合并」。对照：19:08 那个 aim **关**的进程里 `in/motion=1788`。
2. **FPS 态取材不齐的处置已定**：⑦a（FPS 态）三类缺 motion ⇒ 已由 ⑦b（非 FPS 态、243.7 s、三类齐）承担 T5 取材；**B6 用 ⑦b**，⑦a 仅作对照。两格条件差异（FPS 态与否）与目录改名经过都写进了各自的 `run-notes.txt`。
3. **R6 四格的显示缩放未记录**（R7 两格有）⇒ 矩阵缺口，**不许默认四格一致**。
4. 注入器 `rc=1` 是 `--alternate Σ=0` 的**已知仪表假阳性**（`tools/injector/src/main.rs:552` 在 Σ=0 时把容差压成 [4,4] px）；冻结二进制未改（改会毁会话内可比性）。
5. 作废格 `r6-d-rawoff-accoff-superseded-1810` **保留在 `blocks/` 内**（自带作废理由），分析器把它单列一行、不混进结论。
6. ⑥ 格 `baseline.txt` 的 `diag_NO_RAW行数=0` 是修法 3 之前的**假阴性**，已在 `run-notes.txt` 写明独立证据（同日志全文 grep = 1）。

#### 交付物与锚点

| 产物 | md5 / 大小 |
|---|---|
| `release/windows-x86_64/scrcpy-pad.exe` | `897e0c397148bf2a5307be3389ab9a31` / 11,712,512 B（02:31 构建；**本轮未重建**） |
| `bin/scrcpy-pad-legacy-r6.exe`（对照） | `0d3bf137af02d82c175cf9bed0715e4f` / 11,713,536 B |
| `bin/injector.exe` | sha256 `31e8d084…ff81` / 229,376 B（未变） |
| `b4_analyze.py` | `cee9afdea9c16e46d9c74df6ff81f216` / 67,837 B |
| `run_b4.sh` | 本轮改 3 处（exe 身份 2 处 + NO_RAW 全文扫描 1 处） |

- **C9 锚点更新**：release 侧当前 exe 锚点登记为 `897e0c397148bf2a5307be3389ab9a31`（B3 节流版），旧锚点 `559750c1…`（V2-5）转为历史值。**linux 包仍显式标"过期，停留于 V2-3"**（本轮未动，未重建）。
- **承诺销账**：本轮无新增对用户的承诺；B4 的取材承诺见上表，其中 **T5 三类未达标已如实列出**，不作为"已完成"。

---

### W3/B6 + B7 交付登记（2026-10-17 14:xx）

**批次**：B6（T3、T5，动测试代码）+ B7（T8、R13、R14，只读评估）。**Lead 亲跑门（改码前 → 改码后）**：`cargo fmt --check` 0 → 0；`cargo test --bin scrcpy-pad --locked` **386 → 389 passed / 0 failed / 0 ignored**（+3 全新增、无删除）；`python ci/clippy_diff.py` **36:36 → 36:36 零差异**；另跑 `cargo check --bin scrcpy-pad` 无警告（证明测试缝不进生产构建）。

#### 改动清单（A 级）

| 文件 | md5 / 行数 | 改动 | 为什么 |
|---|---|---|---|
| `src/sim.rs` | `c1877143e504a699847d461d7f5390b3` / 2254 行（原 1916） | +3 测试（T3 `:2015`、T5 输入侧 `:2117`、T5 逐点 `:2208`）、+助手（`:1962-2013`）、+`#[cfg(test)] pub mod fed_probe`（`:629`）与两处 `#[cfg(test)]` push（`:438`、`:453`） | T3 golden / T5 回放逐点断言 |
| `src/sim_fixtures/t3_raw_state_lines.jsonl` | `b38b0e0794b3d0b1254f2d15239a7cbb` / 11,109 B | 新增 162 行 | T3 的真机 Raw 原文金样本 |
| `src/sim_fixtures/t5_realplay06_nofps_sample.jsonl` | `941fff129033de3a92d723090acd5c4b` / 18,177 B | 新增 281 行 | T5 的回放用例样本 |
| 生产代码 | engine `423f5348…` / keymap `4c35a855…` / capture `c4337f7d…` | **零改动**（md5 逐位一致） | — |

**fixture 出处（Lead 独立验证 = 精确子集）**：`t3_…` **162/162** 行原文命中 `blocks/r6-a-rawon-accon/trace-slice.jsonl`（t 684,879→717,409 = 整格窗口；`raw` 恒 2、150 个不同 `wmin`）；`t5_…` **281/281** 行原文命中 `blocks/realplay-06-nofps/trace-slice.jsonl`（t 3,311,810→3,317,634 = 5.8 s；`in/motion` 99 + `in/btn` 37 + `out/touch` 60 + `aim` 57 + `st` 28）。**这两个 fixture 是上一轮被网络错误（502/DNS）打断的子智能体留下的落盘**，Lead 审计（逐行溯源 + 源码零引用）后才复用，**没有在其未知状态上叠加改动**。

#### 变异验证（§3.1；3/3 变红且报精确位置，均已还原）

| 测试 | 变异点 | 变红位置 | 还原 |
|---|---|---|---|
| T3 | `ledger_from_trace` 的 `note_raw_tick(state,wmin)` → `(state,0)` | `sim.rs:2069` left 0 / right 26750 | md5 复原 + touch mtime + 复绿 |
| T5 输入 | `replay` 的 `feed_motion(*dx,*dy)` → `(*dx,0.0)` | `:2187` left `dy:0.0` / right `dy:-1.0` | 同上 |
| T5 逐点 | `replay` 丢弃全部 btn 抬起 | `:2239` left 7 / right 44 | 同上 |

阳性/阴性（§3.3）：T3 把全部 `wmin=26750` 改小 1 ⇒ 探针报 26749；T5 把轮盘模式改回 Classic ⇒ 折叠 46 条 ≠ 录制 44 条（证明比较非恒真）。**测试名**：`sim::tests::t3_real_raw_state_lines_are_frozen_and_parse_bit_exactly` / `t5_replay_feeds_every_recorded_input_verbatim` / `t5_replayed_touch_points_match_the_recording_after_keepalive_collapse`。

**T5「逐点一致」的实际严格度（如实登记，不许改口径）**：录制 60 条触点 = **44 个状态迁移 + 16 条 100 ms 保活重发**；`sim::replay` 不重放真实时间间隔 ⇒ 折叠相邻同点后 **44/44 逐点一致（含 9 DOWN / 9 UP）**，**原始 60 条并不逐条一致**。

#### §3.5 取证

`diagnostics.log` 跑测试前/后：md5 `f222087c3f39382812dbbe00c0cd6f58`、873,722 B、mtime `1792158298` **三项全等**（Lead 亲测）。

#### B7（只读评估）结论

- **T8 → 不接 CI**。① 输入在 runner 上不可复现（bench 在仓外、真机、用户手改系统设置）；② 分析器按用户令"不设任何阈值判定"，CI 断言须先改裁决口径（属口径变更，不是格式问题）；③ 先做零授权固化（冻结 CSV 列序/metric 枚举、md 去生成时刻与绝对路径、缺件补 `block-integrity` 行），待用户放行 W2-5 后再接最小闭环（离线重算 + 缺件/合成自证断言，exit 4 已具备）。
- **R13 → 不换滚轮源**。收益 ≈0（钩子已按 `WHEEL_DELTA` 累加、多齿上限 10，Raw 的 `usButtonData` 同为 120 倍数语义）；复杂度高（吞滚动只能在 LL 钩子里 `return 1`，钩子仍须处理每个事件 ⇒ CPU 零节省，且**没有**位移那样的 `raw_fallback_due` 自愈判据）；**去重不可行**（⑥ 实测同向齿最小间隔 6 ms、p50 16 ms、238/398 个间隔 ≤16 ms，而同齿 down→up 在 trace 里 0 ms ⇒ 时间窗须 <6 ms 才不误合并，而双路投递差本身就可能 ≥ 数 ms）。另：⑥ 跑在 legacy（`st` 全 `raw=0`）⇒ **零 Raw 滚轮样本**，"Raw 更准"本轮无一手证据。
- **R14 → 静态结论 + 实验设计，未实跑**。可推出：`GetCursorPos` ≤62.5 次/s（`capture.rs:4279-4283` 闸门 + `RECV_IDLE=50ms`）；回中阈值 `max(短边/4,96)`=400 px ⇒ 1000Hz×1px 时 `SetCursorPos` ≈2.5 次/s；**回中不产 WM_INPUT**（r6-a Δwmin=3840 vs r6-b=3841，同为 30000 条注入）；单次 `GetCursorPos` 耗时仓内**零测量点**，不编数字。实验设计 = 3 臂（A 捕获开/漂移 <400px 不触发；B 捕获开/每 400px 拉回；C 捕获关对照）+ 4 条判据（B−A 的钩子回调 ≈12.5 判"SetCursorPos 进 LL 钩子"；B 与 C 的 Δwmin 必须同为 3840；进程 CPU 时间差/墙钟；B 的 p99 不越 5 ms），**需用户在场且当轮同意**（注入器会动真实光标）。

#### 未取得 / 推迟

1. **R14 实跑**：需用户在场（程序未运行 + 注入器动真实光标）。
2. **R13 的双路对照**（Raw 开 + 滚轮同段采样）：需启动程序；本轮零 Raw 滚轮样本。
3. **T8 任何落地**：明令只评估；接入需用户放行 W2-5。
4. **T3 的第②③件**（诊断里 Raw 三行原文、legacy 双证）无本仓可解析载体，仍是 B4 登记里的证据。

#### 已知问题

1. **`sim::replay` 结构上不能验 1 ms 合并率**（`trace::read` 丢 `st`/`aim`；`feed_motion` 绕捕获层）⇒ 该指标只能靠真机 debug 5 s 行的 `位移合并 N→M`（B4 已采）。
2. **⑥ 的齿数对不上**：用户自述约 210 齿 vs trace 记 778 齿（双向 3.63/3.79 均匀，3.7×）。**无机器齿数记录 ⇒ 不能判**是自由滚轮多事件还是路径多计；定性需一次"单齿 = 几事件"的判别实验（另开 block）。
3. 测试缝 `fed_probe` 是 `#[cfg(test)]`（生产构建不存在，`cargo check` 无警告），理由写在源码注释里 —— 直接比对 `replay` 的返回值是**假守护**（V2-4 形状：删掉 `feed_*` 那行照样绿）。
4. `src/sim.rs` 仍未 `cfg(test)` 门控（既有设计，见 B5 登记）。

#### B5r 补采：U3 / U4 / U5（2026-10-17 14:4x）

**边界（为什么不能跑 GUI）**：本机**没有配置目录重定向 env**（只有 `SCRCPY_PAD_LOG/TRACE/UNHOOK_AFTER_MS/INSTANCE_*`）⇒ 起 GUI 会改写用户 `profile.yaml`/`settings.json`/`diagnostics.log`；且 U3 的"用户截图"主证据**仓内不存在**（只有 harmonyos 的图）。故三块按"静态指标 + 能测的真测"口径做，缺视觉证据处逐项标明。

**U3 布局密度（静态指标；视觉判定缺证据，不许当"已测观感"）**：以附录 C2 的 ui_* 面统计（控件/分组/常量命中），8 条候选（每条带 `文件:符号:行号`）：
- 过平铺：`ui_aim_body:14956`（86 控件 / 22 按钮 / **5 分组** / 1284 行）、`ui_macro_instruction:10295`（52 / 11 / **0 分组**）、`ui_wheels:11343`（36 / 11 / 0）、`ui_manual_adb:16585`（26 / 13 / 3）
- 过疏：`ui_action_editor:10025`（27 / 1 / 0）、`ui_bind_row:7276`（22 / 5 / 0）
- 过繁：`ui_aim_body` 同页 4 套层级手段（heading+small+strong+RichText）；`track_rec_card:18772` 3 处硬编码 `from_rgb` 与同页其它卡片取色路径不同
- **常量面**：全部面板页 `size::` 命中 **0**、`add_space` **0**（只有浮层 `draw_overlay:11778` 28 处、`draw_recorded_track:19301` 用常量）
- **缺的证据**：真实观感 / 首屏可见项 / 滚动长度 ⇒ **需用户截图**（哪一页、哪一段）。

**U3 视觉判定（2026-10-17 14:4x，用户提供 5 张截图，已入仓 `文档/截图-2026-10-17-UI走查/`）**：窗口宽度覆盖 **1705 / 1255 / 433** 三档，所以顺带验了响应式。

| 页 | 窗口 | 观感判定 | 具体证据 |
|---|---|---|---|
| 键位映射（`01`） | 1705×1015 | **过平铺 + 过繁** | 左栏单列堆 7 块（使用说明约 10 行长文 → 保存/另存/重载 → 选用配置 → tabs → 组合列表 → 新建/快速切换/总开关/按后延迟 → scrcpy 管理 → 日志），**无分组标题、无分隔线**；顶部"使用说明"占左栏首屏约 1/4 且是纯文本长句 ⇒ 视觉噪音高；右栏同屏 5 个功能区，表头"修改响应范围""按后延迟ms"折行 |
| 组合键（`02`） | 1255×597 | **过疏** | 整页只有一张卡，卡片下方约 45% 垂直空间空着；卡内"取点/落点 x/y/时长/范围"四个短字段横向铺满整行，字段间空隙远大于字段本身 |
| 窄窗（`03`） | **433×1018** | **横向溢出（真缺陷，唯一影响使用的项）** | `scrcpy 目录` 路径被截成 `D:\Users\Desktop\一个文件夹\`，**"浏览"按钮被切在右边缘**；顶栏"映射"被切 ⇒ 路径行没有换行/省略/最小宽度策略，窄窗下**控件不可达** |
| FPS 功能（`04`） | 1705×1018 | 分组有、但**行式浪费 + 状态混入设置区** | 3 个粗体分组（视角输入/瞄准锚点、后坐力补偿、指针隐藏）比 `01` 好；但"归中：静止归中 静止 80ms"、"按住才瞄准 未绑定"、"FPS 模式开关 KEY_F9 取消设置" 是**一行一个字段**、右侧大片留白；"指针隐藏(仅FPS模式) **当前:鼠标未被捕获**"把**运行时状态**塞进设置区，信息层级混乱 |
| 宏（`05`） | 1705×1012 | **过平铺 + 常驻面板挤压** | "设置宏"一行塞 **8 个**可点元素（按键/组合键/轮盘/FPS/点击/滑动/间隔和按紧/扩展宏…）；底部截图取点面板（游戏画面 34% + 浮层）占约 40% 垂直空间，且**在每个 tab 都常驻**（`01/04/05` 均有） |

**跨页一致性（与 U4 静态表互证）**：① 按钮语义 5 种观感混用 —— `保存配置/另存为/重新加载`（文本）、`+ 新增组合键`（带图标）、`新增按键映射`（实心）、`启用/取消设置`（链接式）、`删除`（行末）；② 单位格式不齐 —— `200 px` / `20.0 %` / `80ms` / `1.00x` / `0`；③ 分组标题三套层级 —— 粗体（`04`）/ `ui.heading`（`02`）/ 只有 `ui.small`（`01`），与静态表"`ui_aim_body` 同页 4 套层级手段"一致。

**结论**：U3 的三类问题（过平铺 / 过疏 / 过繁）各自都有截图证据，并额外发现**一条静态表看不出来的真缺陷**：433 px 窄窗下 `scrcpy` 路径行横向溢出、控件不可达。

**U4 一致性差异表（对照 `theme.rs:size:51-65`）**：13 条，关键 5 类：
- **同角色两值**：`draw_overlay:12027` 描边 `KEY_STROKE+2.5` vs `:12064` `+2.0`；`draw_recorded_track:19350` 起点圈描边 3.5 vs `draw_swipe_track:19489` 3.0
- **第三套值**：`draw_overlay:11831-11832` 绝对 3.0 / 1.5
- **违反 TH-01（硬编码颜色）**：`track_rec_card:18790/18801/18828`、`ui_look:14072`、`track_rec_config_ui:18734`
- **规范缺口（无常量可依）**：轮盘外圈 5.0/6.0、knob 1.0、终点圈 13.0/8.0、卡片圆角 4.0/描边 1.5、面板字号走 egui 默认、按钮尺寸无档位常量
- **合规**：全部 UI 字号硬编码 **0** 处（`FontId::*` 字面量 0、`.size(` 字面量 0）
- 只标差异与缺口，**不发明新规范**（取哪个值是设计决策）。

**U5 耗时（实测 / 静态分开）**：`src/keymap.rs` 的 `mod tests` 新增 2 条测量测试（`:4679`、`:4770`，后者 `#[cfg(windows)]`），32 次预热 + n=1000~2000，**debug 构建**，本机 AMD Ryzen 9 9850HX / Win11：

| 项 | p50 | p95 | max |
|---|---|---|---|
| 地板（空闭包） | 100 ns | 100 | 100 |
| `Profile::clone` 10 bind | 1.3 µs | 2.8 | 19.4 |
| `Profile::clone` 42 bind | 2.4 µs | 2.5 | 13.4 |
| `Look::clone` | <0.1 µs | 0.1 | 0.3 |
| `decimate(2000→256)` | 4.2 µs | 4.3 | 29.5 |
| `polyline_len_px(2000)` | 20.2 µs | 26.3 | 99.8 |
| `track_arrow_marks(256,3)` | 3.7 µs | 3.8 | 20.7 |
| `fill_mask_shapes` 三份 clone | 1.4 µs | 1.5 | 2.8 |
| `combos.clone`(20 组) | 1.7 µs | 3.8 | 22.2 |
| `client_area_of_pid`（EnumWindows 扫描，bogus pid） | 52.2 µs | 67.9 | 659.9 |

- 与 bench `02-microbench`（192 ns/10bind、533 ns/42bind）**不可直接比**（debug 测试构建 + Profile 内容不同），方向一致（42>10）⇒ **不宣称复现**。
- **静态估计，未测**：① 锁跨帧（`draw_overlay:11788→12461` 673 行无 `drop(g)`、`ui_engine_status:13168→13344` 176 行；`engine.rs` 45 处 `lock_shared` 同一把锁）② 每帧频率（`fill_mask_shapes` 每帧 1 次；60 fps 下 5.6 µs ≈ 帧预算 0.03%；浮层 3 条轨迹 ≈84 µs ≈ 0.5%，"3 条"是假设）③ `EnumWindows` **命中后**的 `GetClientRect`+`ClientToScreen`（需真实投屏窗 ⇒ 上表只是"枚举下界实测"）。

**门与取证**：`cargo fmt --check` 0；`cargo test --bin scrcpy-pad --locked` **389 → 391 passed / 0 failed / 0 ignored**（+2 全为新增测量测试）；`clippy` 36:36 零差异；§3.5 `diagnostics.log` 三项全等（`f222087c…` / 873,722 B / mtime 1792158298）；连跑两次全绿（2.20 s / 2.21 s，非 flaky）。`src/keymap.rs` md5 `d38778d83c4302466a58d019a7dd05ea`，新增项**全在 `mod tests` 内**（起点 :2941），断言刻意很松：`p50*1000 < 500ms`（对实测 2.4 µs 有 200× 余量）、`dec.len()==256`（确定性）、`client_area_of_pid(bogus).is_none()`（确定性）。

**未取得**：U3 的视觉判定（需用户截图）；锁持有时长 / 真实帧率（需 GUI 或生产计时缝，均未授权）。

---

### W5 交付登记：A1–A6 + U7 审计（2026-10-17 14:5x）

**性质**：全只读审计 + 一份文档落盘，**0 门禁**。**落盘物**：`文档/审计与提升方案-2026-10-17.md`（35,345 B / 260 行 / md5 `43ee59ae774afbdaca92d51089825f34`；Lead 复核：10 节齐、A6 七列表头逐字为「问题 \| 证据 \| 影响 \| 方案 \| 优先级 \| 成本 \| 风险」、29 行无空列）。
**代码零改动（Lead 亲验）**：`src/app.rs` mtime 仍是 **2026-10-15 05:34**（本次未动）；`keymap d38778d8…` / `sim c1877143…` / `engine 423f5348…` / `capture c4337f7d…` 与 W3 登记逐位一致。

**表规模**：A1 Raw 链路 **7 行**；A2 覆盖 **20 行**（判定：**判据不成立** —— 14 有变异 / 5 只有单测 / 1 无）；A3 性能 **4 行**；A4 划分 **5 行**；A5 汇总 **28 项**；A6 七列 **29 行**；U7 排序 **5 条**。
**A5 排序公式（可复算）**：`S = 2P − C − R`，P∈5..1、C∈1..5（≤0.2 人日 … >0.5）、R∈1..3（低/中/高）；排序键 `S↓ → P↓ → C↑ → R↑ → 编号`。前 3：`U7-1`（433 px 窄窗溢出，S=7）、`A1-2`（S=6）、`A1-1`（S=5）。

**Lead 抽查与纠正（§8.1：队友结论不直接采信）**
- ❌ **A2-3 原判错误，已纠正**：原文写"遮板自检走 `ShapeKind::Swipe` 死变体 ⇒ 自检绿 ≠ 生产对"。Lead 逐行读 `src/mask/win.rs:800-840`：自检构造的是 `Shape::new(ShapeKind::Bind, …)` **加上** `frame.tracks.push(Polyline::new(…))`，**正是生产写法**。正确结论 = 自检**覆盖**生产路径 ✓，缺的只是**变异验证**；`ShapeKind::Swipe` 全仓无构造点（只在 `color_for` 匹配臂与一处比较出现）⇒ 它是**死变体清理候选**，不是自检缺陷。已在交付文档 §2 #20、§2 判定、§7 A2-3 三处改写并标注"Lead 纠正"。
- ✅ **A2-1 站得住**：协议 `:169` 的 P7 确实把缝①/缝③/`stop_watches` 三条标"已销账"，而全文无这三条的变异记录（grep 仅命中 P7 与 P33）⇒ 按现行 §3.1 属"自称守护未变异"。**注**：P7 登记于 2026-10-13，早于 §3.1（V2-4 事故后立），属**历史遗留**，不算新违规，但必须补变异。

**未测项 12 条**（每条都写了"一次什么运行/测量能判定"）：锁持有时长、16 ms 回中（R14 三臂，需用户在场）、单次 `GetCursorPos`、绝对设备降级、`PENDING→fallback`、队列 256 深度、aim 态合并率、遮板命中后 `GetClientRect`、帧率/帧时间、C2-C#9/#10、8000 Hz 合并率（**结构性不可达**，非一次运行可判）、真实观感（B 级）。

---

### R12 交付登记：D4 盲测协议（2026-10-17 14:5x）

**落盘**：`文档/D4盲测协议-2026-10-17.md`（Lead 亲写；**0 门禁、不改代码**）。
**要定什么**：`st.ox += dx * sensitivity_x * move_speed`（`engine.rs:2815/2821`）里 `dx` 的语义从**光标像素**变成**原始计数**（Raw 上线）⇒ 定换算系数 **k**，使「新语义 × k」的观感 ≈「旧语义」。
**设计**：两臂 —— **A** = legacy `SCRCPY_PAD_NO_RAW=1`（旧语义，3 轮 → `R_A`）；**B** = prod × k 阶梯（≤6 轮，k 从 **1.00** 起、步长 **×1.25**；同向连续两次未穿过 `R_A` 则反向并减半 ×1.12；穿过或走满 6 轮即停）+ **二次回归**（`x = ln k`，最小二乘 `y = a x² + b x + c`，解 `= R_A` 取**已测区间内**的根 `k_cross`）。
**"一次运行可定"判据（三条全满足）**：`R² ≥ 0.5`、`k_cross ∈ [min k_i, max k_i]`（不许外推）、`|评分(k*) − R_A| ≤ 0.5`。不满足 ⇒ 判 **"未定"**，只给临时值 + 下次搜索区间。
**前置条件**：手机同向 2772×1280 / **`MouseSensitivity=10` + `MouseSpeed=0`（关加速，测完还原）** / 缩放 100%（用 `GetSystemMetrics` 判，不用 `AppliedDPI`）/ 方案与锚点固定 / 同一靶场同 FOV / 关录制与截图悬浮窗 / 每轮前手离鼠标 2 秒。
**已知偏差（必须随结果一起登记）**：① **弱盲**（界面仍显示灵敏度数值与"视角移速 1.00x"，只能做到"随机序 + 不标注 A/B"）② 两臂二进制不同（差异 = Raw 通道 + 该单点变异；已核 legacy 的 `st.raw`/`wmin` 恒 0）③ **两臂必须同一次运行内完成**，否则整轮作废（记忆漂移）④ 主观 1–5 量表，每 4 轮休息 ⑤ **k 只在"关加速"下成立**，且绑定 `move_speed=0.67`、`sensitivity=2.0` ⑥ 轮次上限 12。
**未做**：盲测本身（需用户在场 = 计划 B8）。**B9/R-coef 只认本登记里的 k**，不许把临时值当结论。

#### ⚠️ A 臂作废（2026-10-17 17:1x，用户实测反馈）—— 参照选错，D4 前提被推翻

- **用户现象（原话摘要）**："推几下鼠标就变成垂直向上/垂直向下的锁定旋转，只有小心翼翼上下推才能扶正；压枪很好，但转身几乎无法做到，转到一半就视角上天或下地。"
- **根因判定**：**A 臂的参照选错了**。legacy 二进制是 bench 对照（`SCRCPY_PAD_NO_RAW=1`，日志自述"实验态"），**不是"以前的本程序"**；它的 aim 位移走光标差路径，而该路径有**未定性的大 dx 跳变**（B4 实测：`r6-c` 7169/30001 条为 400–852 px、`r6-d` 939/30001 条为 400–485 px）⇒ 与"转到一半就上天/下地"的形态一致。
- **推翻 D4 前提的关键证据**：用户 **V2-5 时代**的真机 trace 里 `st.raw` **全部 = 2**（`trace-1792054172` 2620/2620、`trace-1792054736` 17099/17099）⇒ **"以前的手感"本身就在 Raw 语义下**。所以"语义切换需要换算系数"对用户不成立，`k` 按构造应 ≈ **1.0**。
- **处置**：A 臂作废、**不采任何分数**（3 轮未开始）；实验改为 **A′ = V2-5**（`release/windows-x86_64/scrcpy-pad - 副本.exe`，md5 `559750c194f82e68ca3314aeb9c0be3d`）**vs B′ = 当前 prod**（`897e0c397148bf2a5307be3389ab9a31`），同灵敏度、同条件 ⇒ **若有差异，那是 V2-5 之后的回归，不是语义切换**。
- **待判**：当前 **prod 是否也出现"垂直锁定"**（用户当时在 A 臂 legacy 上测）。若 prod 也复现 ⇒ 优先按"回归"立案排查，D4 的系数问题降级；若只有 legacy 复现 ⇒ 记为"bench 对照路径的已知缺陷（大 dx 未定性）"，不影响日常构建。
- **判定结果（2026-10-17 17:1x）**：**prod 正常** ⇒ 不是回归；症状锁在 bench 对照（legacy）路径。

#### D4 结案分析（代码级，2026-10-17 17:2x）

**一句话**：D4 的债务不是"系数没调好"，而是**两条位移路径的 `dx` 单位不一致**，且**常数系数在原理上无法还原旧手感**（OS 增益是鼠标速度的函数）；对本用户 **k ≡ 1.0**。

**机制（代码证据）**

| 环节 | 事实 | 位置 |
|---|---|---|
| raw 路径的 `dx` | `dx: mouse.lLastX, dy: mouse.lLastY` —— **原始计数，未经 OS 缩放** | `capture.rs:3113-3114` |
| legacy 路径的 `dx` | 由**光标位置差**得出（`GetCursorPos` 参与）—— **逻辑像素，已被 OS 指针速度 × 加速曲线缩放** | `capture.rs:4021 / 4286 / 4356`；`raw_takes_motion :2905`、`hook_forwards_motion :2947` |
| 汇合点 | 两条路写进**同一个** `CaptureEvent::Motion { dx, dy }`，引擎不知道来源 | — |
| 灵敏度施加点 | 只有两处，且**两条路共用**：`st.ox += dx * sensitivity_x * alpha * move_speed`（触摸）/ `add_x = dx * sensitivity_x * move_speed * GAMEPAD_AXIS_MOUSE_SCALE`（手柄） | `engine.rs:2815` / `:1804` |
| 现有系数 | **不存在**：`coef/gain/SENS_SCALE/MOUSE_SCALE` 在 capture/engine/keymap 里只命中 `GAMEPAD_AXIS_MOUSE_SCALE`（手柄轴，与鼠标路径无关） | grep 实测 |

**量化（B4/R6 的 p50 数据；同一「1 计数/事件」的注入流）**

| 路径 | 条件 | p50 \|dx\| / 事件 | 换算成「每鼠标计数」 |
|---|---|---|---|
| raw | 加速开 | 8.0 | **1.0**（OS 把 ~8 计数聚合成 1 条 WM_INPUT：3733 批 / 30000 计数） |
| raw | 加速关 | 8.0 | **1.0**（与加速无关，R6 已证） |
| legacy | 加速关 + 速度 10 | 1.0 | **1.0** |
| legacy | 加速开 | 2.0 | **2.0**（同一注入流下 OS 增益 ≈2.0） |

⇒ **"语义切换"的实质 = 把 OS 指针速度 × 加速曲线这条增益链整个去掉了**。加速开时该速度下增益 ≈2.0；速度最右可达 3.5。

**为什么"乘一个常数系数"注定只能对一个工作点**：OS 增益 = 指针速度档 × 加速曲线(鼠标速度)，是**速度的函数**；新路径把这条曲线整个消掉。任何单一 `k` 只能匹配一个速度点 + 一个指针档位。⇒ D4 选项 B（"乘换算系数尽量保持旧手感"）只能做到**近似**，必须写清"在哪个工作点成立"。

**对本用户的结论**：V2-5 时代真机 trace 的 `st.raw` **全 = 2**（2620/2620、17099/17099）⇒ 参照本身已在 Raw 语义下 ⇒ **k ≡ 1.0，活跃债务为零**；D4 只剩"给迁移用户写清语义"的文档义务。

**收尾三选项（待用户拍板，不许自行实施）**
1. **仅文档化（D4-A）**：写清 `sensitivity` 新语义 =「设备像素 / 原始计数」，并给迁移换算表（加速关 ×1.0、加速开 ×1.5~2.0、速度最右 ×3.0~3.5），**不自动改任何人的手感**。
2. **自动补偿（D4-B 的真实现）**：启动时读 `HKCU\Control Panel\Mouse` 的 `MouseSensitivity`/`MouseSpeed`，把 OS 增益折进 aim 灵敏度。**风险：会改变现有用户手感（含本用户 2~3.5×）⇒ 必须 opt-in、默认关**。
3. **不补偿，改显示口径**：UI 把「灵敏度 X」标注成「X 设备px / 每计数」，让用户按新语义自己调。

**附（另一个问题，别混）**：今天撞到的 legacy **大 dx 跳变**（`r6-c` 7169/30001 条 400–852 px、`r6-d` 939/30001 条 400–485 px）是**位移路径的跳变**，不是单位问题；prod 不复现（用户实测）。它让 bench 对照路径不可用于手感比较，应**另立 block**（定性 + 判别实验），不进 D4。

#### ①+③ 落地（2026-10-17 19:4x，用户拍板"1+3"）

| 项 | 做了什么 | 证据（Lead 亲验） |
|---|---|---|
| **① 文档化** | `src/help.md:129` 灵敏度条目改为「单位 = **设备像素 / 每 1 个鼠标计数**；实际增益 = 灵敏度 × 视角移速」，并加迁移折算（加速关 ≈×1.0 / 加速开 ≈×1.5–2.0 / 指针速度最右 ≈×3.0–3.5）+「本版本增益与系统鼠标设置无关」 | md5 `889c3d792ae372e79ee4b48339fcb819` / 45,363 B |
| **③ 显示口径** | `src/app.rs:15358` 标签改「灵敏度 X（设备px/计数）:」+ 悬浮四行说明（单位 / 实际增益 / 本版本与系统设置无关 / 迁移折算） | md5 `7aa4508df307a0abac7e696f9738d8dc` / 1,011,687 B；`grep -rn "灵敏度 X" src/` = **1 处** ⇒ 入口面矩阵无复制点 |
| **门** | `cargo fmt --check` 0；`cargo test --bin scrcpy-pad --locked` **391 passed / 0 failed**（纯文案改动，测试数不变）；`clippy` 36:36 零差异；`cargo check` 无警告 | Lead 亲跑 |
| **k** | **k = 1.0 结案**（本用户参照已在 Raw 语义下，活跃债务为 0）；② 自动补偿**未做**（需 opt-in，未获授权） | V2-5 时代 trace `st.raw` 全 = 2 |

**B 级（未在 GUI 上看过）**：标签变长后的排版与 tooltip 渲染未做 GUI 走查（起 GUI 会改写用户配置，本次未起）。**观察点**：下次启动 → FPS 功能页 → 灵敏度行应显示「灵敏度 X（设备px/计数）」，悬停出四行说明。
**已知副作用**：标签变长使该行更宽 ⇒ 对 U3 记过的「433 px 窄窗横向溢出」是**加重项**，修 U7-1 时一并处理。

---

### W7/B16 + W6 交付登记（2026-10-17 19:5x）

**B16（U8 + U9，动 `src/app.rs` 一个文件）**
- 门（Lead 亲跑）：`fmt` 0；`cargo test --bin scrcpy-pad --locked` **391 → 394 passed / 0 failed / 0 ignored**（+3，0 删除）；`clippy` 36:36；`cargo check` 无警告。
- `src/app.rs` md5 `6c01d07f47686b184bc7b527bc0ebddd`；`keymap d38778d8…` / `sim c1877143…` / `engine 423f5348…` / `capture c4337f7d…` **逐位未变**。
- **U8**：新增共用组件 `radius_input`(:18918) + `radius_clamp`(:18935) + 三条范围常量(:53-55)，**9 处**半径 `DragValue` 全改走组件，**范围/步长逐处不变**；`undo_needed` 触发点原样。守护测试 `every_radius_edit_site_uses_the_shared_input_with_its_own_range`(:20980)。
- **U9**：`ui_draft_card`(:7615) 成唯一编辑卡，`ui_binds_new`(:7596)/`ui_vk_draft`(:11042) 只剩外壳调用；守护测试 `both_draft_entries_share_the_same_edit_card`(:21072) + **扫描器自带阳性对照** `source_scanner_rejects_a_missing_function`(:21061，`should_panic`)。
- **变异验证 6/6 变红且报位置**（删钳制 `:21034`、改范围常量 `:20982`、删 `ui_vk_draft` 的共用卡调用 `:21090`、删 `track_rec_card` `:21112`（**V2-4 事故本体**）、扫描器静默退化 `:21059` 等），均已还原（md5 + touch）。
- **如实登记的未做项**：① **范围未统一**（`0.01..=100000` / `10..=1000` / `0.001..=0.5` 三套并存）——属计划 **Q6 待拍板**，本轮只做结构收敛；② **两处"无钳制"未动**（Ctrl+± `:6144/:6155`、截图拖动改半径 `:17683`）——加钳制属语义变更；③ 可视化风格在 `swipe_controls` 内改取点不置 `draft_active`（与默认风格不一致）——既有行为，按"不改可见行为"保留。

**W6（J1–J8，只读判定）**
- 落盘 `文档/三天判定报告-2026-10-17.md`（23,084 B / 219 行 / md5 `c1033c49d4dad55b40d8008b64d37a9f`；Lead 复核：J1–J8 **8 节齐**、三清单共 **23 条 = 升级 6 / 维持 9 / 废弃 8**）。
- **J3** 中风险 7 项：落地 2 / 证伪 1 / 部分覆盖 2 / 升级 1 / 无变化 1；**J5** HDC 维持（坐标空间 +1 人日、存活探针 +1 人日）；**J6** 8 条款：部分落地 2 / 未落地 5 / 规范落地执行未落地 1；**J7** 3 条 B 级走查仍有效、无静默废弃。
- **J1/J2/J4 待拍板**（Q3/Q4/Q2），卡片已**逐字复用**计划 §10（脚本校验 EXACT），并给推荐默认值：**Q2 不立项（仅改文案 0.1 人日）/ Q3 不清理（实测无害）/ Q4 剪贴板+`TouchStates` 废弃、无线+本地录制维持**。
- **证据不足 9 条**已逐条写明缺什么（真机轮次、`adb pair` 实测、录制落点实测、触边停转复现、GUI 观察、grab 收益计数、harmonyos-pc 实跑等）。
- **Lead 抽查**：`settings.rs:499` 的 `track_rec_trigger` 只是**旧配置解析的测试夹具**（不是清理逻辑）✓；全仓 `取消修改` = **0** ✓；`src/**` md5 与 B16 后一致（W6 未碰代码）✓。

---

### W7/B17 交付登记 + Q2/Q3/Q4 拍板结果（2026-10-17 20:3x）

**B17（U10 + U11 + 窄窗溢出 + help 文案）**
- 门（Lead 亲跑）：`fmt` 0；**394 → 397 passed / 0 failed / 0 ignored**（+3，0 删除）；`clippy` 36:36；`cargo check` 无警告。
- 文件：`src/app.rs` `7927dd106561a5e5d364613f206e0379`（20:34）；`src/help.md` `9a66c264e38abb77f113519ae4a377f5`（20:30）；**`src/theme.rs` 未改**（mtime 仍 2026-10-13 03:55，Lead 亲验）；keymap/engine/capture md5 未变。
- **窄窗溢出（U7-1）**：新增 `PATH_EDIT_MIN_WIDTH=56` + `fit_edit_width`/`row_edit_width`(:12986/:12992)，`ui_scrcpy_manage` 四行路径改 `desired_width(w)`(:13023/13108/13117/13126)，`当前 adb`/`server 版本` 两行 `.truncate()`(:13156/:13160)；守护测试 `narrow_window_path_rows_keep_their_buttons_reachable`（同时锁"宽窗 220/280 不变"与"433 px 下 `avail 330 − reserved 110` 仍放得下按钮"）。
- **U10 布局重分组（保守版）**：`ui_aim_body` **只加** 5 个分组标题 + 2 条分隔线，**字段零删/零改/零重排**；守护测试 `aim_page_keeps_its_field_order_and_group_titles`（谁挪了某行位置立刻红并报第几项）。
- **U11 颜色收敛**：`Color32::from_rgb(` 在 `src/app.rs` 由 **6 → 0**（`ui_look`→`th.warn`、`track_rec_config_ui`→`th.warn`、`track_rec_card` 三处→`th.danger`/`th.accent` 推导）；**`theme.rs` 未改**（6 处全部命中既有语义色，未自创新色）；守护测试 `ui_surface_has_no_hardcoded_rgb_literals`。
- **help.md 文案（Q2 的"先修正"）**：:130「隐藏并锁定光标」→「隐藏光标（**当前不锁定光标**）」；`grep -rn ClipCursor src/` = **0** 复核一致。
- **变异 4/4 变红且报位置**（去 `- reserved` ⇒ `:21254`「窄窗下输入框必须收缩」；删 `.truncate()` ⇒ 1/2；改「归中:」标签 ⇒ `:21335` 报第 8 项；恢复硬编码色 ⇒ `:21352` 列出命中行），均已还原。
- **如实登记的未做**：① 433 px **顶栏**仍在横向 `ScrollArea` 内（"映射"需横向滚动才可见；换行属布局决策）；② U10 的"必要两列排布"按保守口径跳过（无 GUI 走查条件）；③ 6 条 Q6 规范缺口（输入框最小宽度无常量、描边"同角色两值"、深红底无专属语义色、`app.rs:8043` 的 `rgba(255,255,255,190)` 字面描边等）。
- **B 级未验（一次启动 + 窗口缩到 433 px 即可判定）**：四行路径框变窄但按钮全在右边缘内且可点；宽窗 1705 下逐像素不变；FPS 页出现 6 个分组标题且字段顺序不变；录制卡片呼吸点色相由「红→橙」变为「暗红→亮红」。

**Q2 / Q3 / Q4 拍板结果（用户 2026-10-17 20:2x）**

| 卡 | 用户裁决 | 落地 |
|---|---|---|
| **Q3** settings 残留 `track_rec_trigger` | **不清理** | J1 判定转正：不动用户配置（实测无害）、不新增清理逻辑 |
| **Q2** Windows 准 grab | **立项** + **文案先修正** | 文案已在 B17 完成（help.md:130）；立项按报告 §J4 拆块表拆 4 块 —— G-a 文案三档 0.5 / G-b 吞键可配 1.5 / G-c `ClipCursor` 1.0 / G-d `RIDEV_NOLEGACY` 1.5 = **4.5 人日**，登记为 **B21（待排期）** |
| **Q4** 四项推后批次 | **按推荐** | J2 判定转正：**剪贴板 + `TouchStates` 废弃**；**无线 + 本地录制维持**（继续推后，不立项） |

> `文档/三天判定报告-2026-10-17.md` 的 §J1/§J2/§J4「待拍板」**以本节为准**；§J8 的「预演清单」按本节转为正式结论。

---

### W7/B18 交付登记（2026-10-17 20:5x）—— 先测后改：两个 block 都判「不改」

**门（Lead 亲跑）**：`fmt` 0；**397 → 398 passed / 0 failed / 0 ignored**（+1 测量测试，0 删除）；`clippy` 36:36；`cargo check` 无警告。
**文件**：只改 `src/keymap.rs` 的 `mod tests`（md5 `2c4da717dc9083726d82caaa81a0c47a`）；`src/app.rs` md5 与 B17 一致（`7927dd10…`）⇒ **生产代码 0 改动**。

**U12 · `ui_combos` 每帧 clone → 不改**

| 项（每帧 1 次） | p50 | p95 | max |
|---|---|---|---|
| `Profile::clone` 42 bind | 3.1 µs | 3.4 | 24.1 |
| `combos.clone` 20 组 | 1.6 | 1.7 | 44.8 |
| `fill_mask_shapes` 三份 | 1.6 | 1.7 | 21.7 |
| `Look::clone` | <0.1 | 0.1 | 0.2 |
| **合计** | **6.3 µs（0.038%）** | **6.9 µs（0.041%）** | 90.8（0.54%） |

阈值 = 1% 帧预算（167 µs @60 fps）⇒ 实测 p95 只有阈值的 **1/24**；且这是 debug 测试构建（release 只会更快）；`ui_combos` 只在配置窗口的「键位映射」页与新增组合键流程绘制（`app.rs:7615`/`:8612`），**游戏内浮层与遮板不走这条路径**。⇒ **不改**。复测条件：release + GUI 实测 p95 帧时间，或组合键 > 200 组。

**U13 · 撤销栈 ×50 → 不改**

| 档 | 单份 B | ×50 | 算式 |
|---|---|---|---|
| empty（阴性对照） | 568 | 28,400（0.027 MB） | `size_of::<Profile>()` |
| exact 100 Tap（阳性对照） | 8,568 | 428,400（0.409 MB） | 568 + 100×80 |
| **user 真实最大方案** | **2,574** | **128,700（0.123 MB）** | 568 + 12×80 + 4 宏步 + 1 组合(104+4) + 4×144 + 8×32 + name 11 |
| stress42 | 6,920 | 346,000（0.330 MB） | 568 + 42×80 + 20×(104+4) + 4×144 + 8×32 |
| worst 42×2000 点轨迹 | 675,928 | 33,796,400（32.2 MB） | 568 + 42×(80 + 2000×8) |

用户档来自**只读**解析 `profile.yaml`（32,467 B / 5 套；最大一套 5,086 B 序列化；12 binds / 1 combo / 4 wheels / 8 directions / 4 宏步 / `recorded_points` **0 处**）⇒ **0.123 MB ≪ 2 MB 阈值（1/16）** ⇒ 不改。`worst` 档超阈值属**结构上界**（要求 42 条各带 2000 点录制轨迹），触发复测条件 = 某方案录制轨迹累计 > 50 条，或 `UNDO_DEPTH` 上调。
**Lead 独立复核**：亲跑 `cargo test --bin scrcpy-pad u13_undo_snapshot_memory -- --nocapture`，五档数字与上表**逐字一致** ✓（568 / 8,568 / 2,574 / 6,920 / 675,928 B）。

**变异 2/2 变红且报位置**（`keymap.rs:4829` 的 `× size_of::<KeyBind>()` 改 0 ⇒ `:5007` left 568 / right 8568；`size_of::<Profile>()` 改 +1 ⇒ `:5001` left 569 / right 568），已还原 + touch mtime。
**B 级未验证**：release/真机 p95 帧时间与 RSS 峰值（需 GUI）。观察点：打一局后把撤销栈拉满 50 步，看任务管理器内存增量（预期 < 1 MB）与 CPU 是否随「键位映射」页开关跳变。
**已知问题**：① "每帧 1 次"是上界假设，未在 GUI 计实际调用次数；② `deep` 按容量算、未含分配器元数据（RSS 会略高）；③ 测量测试把 `UNDO_DEPTH=50` 写死（`app.rs` 常量私有），改常量需同步；④ `worst` 档 32 MB 未降档，属病态配置边界。

### W7/B19 + B20 交付登记（2026-10-17 23:0x–23:5x）—— W7 收尾

⚠️ **接手状态（先读）**：本批次开工时树上已有**未登记的半截落盘**——U14 渲染核已落地（`log_card_title`/`log_popup_button`/`log_row_text`/`ui_log_list`/`ui_log_card`/`ui_log_window`）但未过 fmt；U15 只落了 `enum HelperSource`（`helper_window` 不存在）。后果：`cargo fmt --check` **红**、`ci/clippy_diff.py` **38:36**。同一时刻还存在**第三方写者**：`release/windows-x86_64/scrcpy-pad.exe` 在 **22:51:22** 由 `897e0c39…` 变为 `68709a1d…`（11,715,072 B），`release/*.zip` 同刻重建，运行实例 PID 由 21688 变为 43912。这两件事都**不是本批次造成的**，但按 §8.3 冻结审计后登记在案；本批次就地收编半截落盘。

**B19（U14 日志三形态渲染核 + U15 helper 骨架复用）** —— 派 `b19-exec`，写范围仅 `src/app.rs`。
- 文件：`src/app.rs` md5 `5bc208df7162e6b721603792fdfc800f` / 22,222 行；**其余 18 个 `src/*.rs` md5 与开工前快照逐位一致**（Lead 亲验）。
- U14：三形态（日志卡/浮窗/列表核）文案一律取自 `log_card_title` / `log_popup_button` / `log_row_text` 三个纯函数；`ui_log_list`(:13468) 成为唯一渲染核，`id_salt` 由调用方按形态区分（两处 `ScrollArea` 不互顶）。
- U15：新增自由函数 `helper_window(app, ctx, source, open, body)`（`:2081`）。**为什么不是 `&mut self` 方法**：调用方要在骨架执行期间捕获自己的局部状态（取走的 `ArgHelp`/`AdbHelperState`），方法版闭包里再借 `self` 即二次可变借用（借用检查拒绝）。`ui_args_helper`(:12535) / `ui_adb_helper`(:17008) 改走骨架，「take ⇒ 渲染 ⇒ 仍开则放回」语义逐字保留。
- 新增守护测试：`log_forms_share_one_text_source`(:22107)、`helper_windows_use_distinct_ids`(:22177)。前者两层判据（纯函数**逐位**比对 + **源码逐行扫描**三函数体禁中文字面量，§3.2 口径、无转义串模式）+ 阳性/阴性对照 + 「三纯函数真被调用」接线断言；后者含「骨架真用 `source.title()/default_width()/window_id()`」与「两个 helper 未绕过骨架自建 `egui::Window`」断言。

**B20（U16 浮层可读性四层收敛为单一入口 + 预设档）** —— 派 `b20-exec`，写范围仅 `src/app.rs`（B19 完成后串行，共享任务板 `blocked_by` 硬依赖）。
- 新增 `enum ReadabilityPreset{Auto,ForceDark,ForceLight,Custom}` + `preset_of(auto,manual)`(:2085) + `apply_preset(p,manual_now)`(:2104) + `PadApp::ui_readability_controls(ui,&mut Look)`(:14247)，调用点 `:14500` 替换原两个并列控件（自动对比 checkbox + 亮度滑块）。
- **配置 schema 零变更**：`Look::overlay_auto` / `overlay_tone` 一字未删未改名；档位只是这两个字段上的**视图**。旧配置 `(auto=true, manual=m)` 读成 `Auto` 且 **m 原样保留**（Auto 档进入时不写回 `overlay_tone`），切「自定义」可接着调。
- **端点判据（Lead 裁定，避免测试与实现打架）**：`auto ⇒ Auto`；`!auto && manual<=TONE_MIN ⇒ ForceDark`；`!auto && manual>=TONE_MAX ⇒ ForceLight`；其余 `⇒ Custom`。理由 = **往返一致性优先**（`preset_of(apply_preset(p))==p` 对三档必须成立）；代价是「手改 profile.yaml 写到端点」会显示成强制档——端点值本就是那两档的值，绘制结果逐位相同。

**A 级验证（Lead 在冻结树上亲跑，输出完整读过，§3.4）**

| 门 | 结果 |
|---|---|
| `cargo fmt --check` | **FMT_EXIT=0**（无输出） |
| `cargo test --bin scrcpy-pad --locked` | **402 passed; 0 failed; 0 ignored**（开工 398 → B19 400 → B20 402） |
| `python ci/clippy_diff.py` | **36:36 与基线零差异**（开工 38:36，B19 归零） |
| `cargo check --locked` | 0 warning |
| `src/app.rs` | md5 `6ad53a71e9521f3c74a28cffb9896909` / 22,507 行 / `grep -c CR` = **0** |
| `%APPDATA%\scrcpy-pad\config\diagnostics.log` | md5 `fe685d934d1dd50c1c3716fb2060b19a`（1669 B，mtime 23:00:23）**改前=改后**；§3.5 守护 `tests_never_touch_the_production_log_path` 常绿 |
| 越界审计 | 18 个 `src/*.rs` md5 与开工快照**逐位一致**（含红线文件 `theme.rs`、`keymap.rs`）；未新增临时物 |

**变异验证（§3.1，含 Lead 亲验）**
- 执行者 3/3 变红（B19：`log_card_title` 改串 ⇒ 红于 `:22110` 打印实际值；`ui_log_window` 塞中文字面量 ⇒ 红于 `:22136` 报出函数体内行号；`window_id` 两分支合并 ⇒ 红于 `:22184` 打印 left/right）。
- **Lead 亲验 3 条**（§8.11「不许把 §3.1 整体外包」）：① 在 `ui_log_window` 内联中文字面量 ⇒ 扫描测试红于 `:22135` 并报行号；② `apply_preset(ForceDark)` 的 manual 由 `TONE_MIN` 改 `0.0` ⇒ `readability_presets_map_uniquely` 红于 `:22389`（`left:(false,0.0) right:(false,-1.0)`）；③ `apply_preset(Auto)` 由 `(true,0.0)` 改 `(false,0.0)` ⇒ 两条测试同红于 `:22384`/`:22479`。字节级还原后 md5 精确回到 `6ad53a71…`、CR=0、复绿。

**⚠️ 事故登记：Lead 自己的变异脚本把 `src/app.rs` 写成 CRLF（2026-10-17 23:3x）**
- **现象**：Lead 的 Python 变异脚本用**文本模式** `open(P, "w")` 写回 ⇒ Windows 上 `\n` 被翻译成 `\r\n`，**全文件 22,222 行换行被改写**：md5 由 `5bc208df…` 变 `712efce3…`（1,066,799 B），而 `cargo test` **仍然全绿**（Rust 不在乎 CRLF）⇒ **门禁绿掩盖了「磁盘上的文件已不是交付物」**。
- **检出**：还原后按纪律复算 `md5sum` 并与自己记录的交付 md5 对账，立刻发现不符。⇒ **新增红线：任何脚本改写仓库源文件，还原后必须复算 md5 并与交付值逐位比对，不能只看测试颜色。** Python 一律二进制模式或 `newline=""`。
- **修复**：字节级 `\r\n` → `\n` 写回 ⇒ md5 精确回到 `5bc208df7162e6b721603792fdfc800f` / 22,222 行。B20 变异改用 `rb`/`wb` 字节读写，**未再发生**。

**已知问题 / 未取得（如实列出）**
1. **B 级未验（需一次 GUI 启动）**：预设档下拉观感、`Custom` 才展开滑块是否合直觉；两个 helper 弹窗**同时打开**时位置/大小/滚动是否互不干涉。观察点：启动 →「外观」页选四档看浮层颜色跟随；「参数助手」与「adb 助手」同时打开并拖动其一。
2. **`--mask-selftest` 未跑**：用户实例很可能在跑，单实例守卫（`main.rs:35-65`）会拒；本轮改动是 egui 控件层、不碰 GDI 遮板绘制，无像素断言义务。B4 交付时的「14 项全 PASS」**本轮未复跑**。
3. **「默认档与改前逐像素一致」目前只是纯函数级断言**（`Look::default=(true,0.0)`、`auto_tone(0.85,0)=+` / `(0.15,0)=−` / `(0.5,0)=0`），**未做真机截图对比**；唯一支撑是绘制路径 `tone_at`(:11859-11863) 一行未改。
4. **上一轮半截落盘的来源未查明**：`HelperSource` / U14 渲染核是谁在 23:0x 前写进去、为何未登记——本批次就地收编，不追究；但同一写者若仍在活动，其后续改动仍须按 §8.3 冻结审计。
5. **`ci/clippy_diff.py` 口径更正**：其 docstring 自述比对的是「(文件, 消息文本) 去重集合」，**不是**「诊断 id 计数」。B20 执行者报告里「脚本只比 id 计数、对文案改动是盲的」是**误判**，Lead 已按源码更正。

**交付物与锚点（C9，2026-10-17 23:5x 实测）**

| 产物 | md5 / 大小 |
|---|---|
| `release/windows-x86_64/scrcpy-pad.exe` | `68709a1d1797bf3ae02246bd573c28de` / 11,715,072 B（22:51:22；**本轮未重建**，非本批次产物） |
| `release/scrcpy-pad-0.2.0-windows-x86_64.zip` | `225e3b591ab2ad15c0241d21ddc08aba` / 4,957,505 B（22:51:24；zip 内 `windows-x86_64/scrcpy-pad.exe` 与上一行**逐位相同**，解包核验） |
| `release/scrcpy-pad-0.2.0-linux-x86_64.tar.gz` | `00812f9ce7a2c3a5c8f63fc29038e5eb` / 6,796,261 B（10-13 19:55）⇒ **显式声明：已知过期**；登记表此前记 `2ba9ad00…`(V2-3) / `7039f069…`(V2-4) / `2784397a…`(V2-5) ⇒ **该包在 10-13 被重建但未登记**，本批次显式记账，**不重建** |
| `release/windows-x86_64/scrcpy-pad - 副本.exe` | `559750c194f82e68ca3314aeb9c0be3d`（V2-5 用户手抄包，未变） |

**承诺销账**：本轮无新增对用户的承诺；上表「已知问题 1–5」即未销账项，逐条给了观察点或成因。

**B 收尾（零残留，§6.6）**：已删 `_dsh_tmp_b4_logtail.txt`（4,224 B，10-16 13:49，B4 遗留）、根目录 1 个 0 字节乱码名文件、`probe_pinyin.pdb` / `probe_rect.pdb`（1.28 / 1.37 MB，10-13 只读探针残留，全仓无引用）。**保留未动**：`dsh对话记录.zip`（用户自己的对话记录归档）、`作废AGENTS.md`、`.workbuddy-ai/`（平台数据目录）。

### B21 方案登记：Windows「准 grab」（2026-10-17，规划件，零代码）

| 项 | 内容 |
|---|---|
| 产物 | `文档/B21-Windows准grab实施方案-2026-10-17.md`（md5 `0ba4fcf2fa231fb1edf73df3f0532c63` / 14,397 B / 149 行） |
| 定位 | 用户 2026-10-17 拍板 Q2「立项」后的**实施方案**（旧文 `Windows-grab可行性-2026-10-10.md` 是可行性分析）。**本轮零代码、零构建、零配置变更。** |
| 拆块 | G-a 文案三档 0.5 / G-b 吞键可配 1.5 / G-c `ClipCursor` 1.0 / G-d `RIDEV_NOLEGACY` 1.5 / **G-0 仪表 0.5（本文新增，旧文档漏项）** ⇒ 合计 **5.0 人日**；分 B21-1 ~ B21-4 四批 |
| 对旧文档的三处更正 | ① **G-c 不依赖 G-b**（旧文写「G-1 之后」）——一鼠一键，无技术依赖，仅因同文件须串行；② **G-d 兜底风险重述**：真实风险不是「强杀后永久失灵」（Windows 在窗口销毁时自动移除注册），而是**重注册时序空窗**；③ **补 G-0**：无仪表则 G-b/G-d 只能算 B 级，不构成验收 |
| 前置硬条件 | §4 的 **P1/P3 只读探测**（`RIDEV_NOLEGACY` 的失焦语义 / 钩子吞键是否链式吞掉其它进程）**未出结论前不许动 G-c/G-d 代码**（协议 §4「未测假设不许承重」）；P2 与全部 B 级验收**需用户在场** |
| 现值勘验（Lead 实测） | `ClipCursor` 全仓 **0 命中**；`RIDEV_NOLEGACY` **1 命中且仅注释**（`capture.rs:209`）；吞键表仍是 **10 键 `u16` 掩码**（`capture.rs:95`）；键盘侧**故意不吞字母数字**（`capture.rs:196-200`）；`app.rs:13373` 仍写「Windows 无需该项」⇒ G-a 必须改 |
| 待拍板（3 点） | ① 是否接受「G-a + G-0 先行」；② G-d 即使做完是否仍默认关并标「实验性」；③ G-c 是否允许同批动「回中 + 回声识别」（本文建议**先并存、后简化**） |
### B21 探测登记：P1 / P3 / P1b（2026-10-18 00:4x–01:2x，工作区外探针，主仓零改动）

产物：`D:/GitHub/scrcpy-pad-bench/tools/vprobe/`（main.rs 827 行 + driver.py + driver_p1b.py + `out/*.csv` 16 份 + 两份报告）。两批执行者：`g0-probe`（任务 task-4；其第一次追加尝试失败，第二次 `p1b-run` 接棒完成 task-8）。**主仓 src/ 零改动**（Lead 亲验 18 个 `src/*.rs` md5）。

| 探测 | 结论 | 证据（Lead 亲验 CSV 行数） |
|---|---|---|
| **P3 链式吞** | **成立** | 自包含对照：吞 0x7C ⇒ 第三进程 0 条；未吞 0x5A ⇒ 3 按 3 抬。对**真实运行的 scrcpy-pad.exe** 实测：0xFF+magic 注入 0/6 到达，去掉 magic 6/6 到达（`p3_real_program.csv` 13 行） |
| **P1 失焦语义** | **对（失焦即失效）**，并**证伪**「系统级独占」 | `NOLEGACY|INPUTSINK` 注册 ret=1、注册方自收 WM_INPUT×6，但**另一进程前台窗仍收 6/6 legacy**；自行 `RIDEV_REMOVE` 后恢复 6/6，`taskkill /F` 强杀后也恢复 6/6 |
| **P1b 前台变体（G-d 存亡）** | **零收益 ⇒ 砍掉 G-d** | 前台注册成功（`GetForegroundWindow()==自己` 前置断言 SUCCESS）后：注册窗**自己** legacy 6→0 且 WM_INPUT 0→6（重路由，机制为真）；但**另一进程仍 6/6**，与「不注册」对照逐条一致（`p1b_cross_obs_REG.csv` 6 事件 = `..._CTRL.csv` 6 事件）；`p1b_recovery.csv` 6 事件 |

**为什么 G-d 被砍（Lead 裁定，2026-10-18 01:2x）**：`RIDEV_NOLEGACY` 的生效域 = 「注册窗**仍为前台**期间」，且只对注册窗自己生效。于是出现悖论——**「别人能收到」时它什么都不做（用户切走后它不拦），「它生效」时别的程序本来也收不到**（legacy 本就投给前台窗）。⇒ 对 G-d 唯一有用武之地的场景零收益。**省 1.5 人日。** 其任务(task-7)已从任务板删除，证据留在此节以防未来会话重做。

**这些结论直接改写的能力口径（已落进 G-a 文案，Lead 审过措辞）**：
- 能**无条件**链式吞、让别的程序收不到的，只有带系统默认行为的那一档（Esc / F11 / 鼠标左右中侧键 / 滚轮）。
- **G-b 落地后**，「已绑定键」（含 W/A/S/D）也进入吞食域；但**拦不住通过 Raw Input 读输入的程序**（大量 PC 游戏与专业输入软件）——这是原理性天花板，界面文案不许承诺「完全独占/系统级独占」。
- 崩溃风险不成立：进程死亡时系统自动撤销 RawInput 注册（强杀后 legacy 6/6 自动恢复）。

**副产物（对仪表有普适价值）**：本机 legacy 键盘消息的 `WM_KEYDOWN` **一律携带 `VK_PROCESSKEY(0xE5)`**，只有 `WM_KEYUP` 带真实 vk（MessageLoop/TranslateMessage 的 IMM32 替换）⇒ 任何「按 VK 统计按键」的观察者必须按**事件数**判定，否则会误判成「没收到」。

**未做（如实登记，均不影响上述结论）**：`nolegacy`-only(hwndTarget=NULL) 变体；鼠标 usage 0x02 的 NOLEGACY 语义；销毁/最小化/`RIDEV_REMOVE` 后 100ms 内的竞态窗；独占全屏 D3D 程序；真实物理按键（全程 SendInput，已用 `GetAsyncKeyState` 阳性对照证明进了系统输入流）。
### B21-1a + B21-2 交付登记：G-a 文案两档 + G-b 吞键可配（2026-10-18 01:3x–05:1x）

**B21-1a（G-a 文案两档）**——执行者 `g-a-copy`。
- `src/app.rs:185` 新增常量 `WIN_GRAB_MODES_NOTE`（**唯一来源**），`:13425` 唯一调用点替换旧「仅 Linux 可用…Windows 无需该项」；`:13377` 函数注释同步；`src/help.md:131` + `:263/:264` 两条 FAQ 同步。
- 文案按 P1/P3 实测口径写：① 默认档按键照常传给其它程序（映射开着也能打字）；② 独占模式**只在 legacy 消息层扣键**，实测能无条件扣下的只有 Esc/F11/鼠标左右中侧键/滚轮，W/A/S/D 这类普通键仍被别的程序收到；边界两条（拦不住 Raw Input 程序 / F8 一键关掉）。**零「完全独占」承诺**。
- 守护测试 `win_grab_copy_states_two_tiers_without_overpromising`（:22561，两档齐/无第三档/Raw Input 边界在/入口唯一/旧措辞归零 + 阳阴对照）。变异 4/4 红且报位置。
- md5：`app.rs 129bec8525fbf497940496253c4d4753`（22650 行）/ `help.md 1f77b098103955a65b2b250318035924`（270 行）。门：402 → **403 passed**。
- **Lead 顺手修正的越界项**：`README.md:358` 「Windows 走全局钩子、无需独占设备」同源旧表述（`g-a-copy` 如实上报为超范围）已由 Lead 改为两档口径（README md5 `e0a085245067250543c75a960ab518db`）。

**B21-2（G-b 吞键可配）**——执行者：subagent（前两任 teammate 均因模型额度耗尽中途死亡且**零落盘**，Lead 审计确认树为干净基线后才重派）。
- `src/capture.rs`：新增 `BoundKeys`（`[AtomicU64;4]` 256 位 vk 位图，全 Relaxed）+ 纯函数 `keyboard_swallows(enabled,_pressed,vk,&bound)`（**按下/抬起同判**，`_pressed` 保留为显式契约）+ `bound_key_words(keys, fps_active)`（fps_only 过滤在纯函数内、NumLock 双 vk 经 `vks_for_evdev` 全置位、非 windows/test 给同形空实现）。`keyboard_proc` 在既有 `swallow_bit` 分支旁新增独占分支（**vk 0xFF 心跳探针分支未动**）。`Capture`/`WinShared`/`install_shared`/`windows::start` 全链穿线。
- `src/settings.rs`：`swallow_bound_keys: bool` `#[serde(default)]` **默认 false**（老 JSON 兼容）。
- `src/app.rs`：每帧**同一把 `lock_shared`** 内收集 `(b.key, b.fps_only)` → 锁外 `bound_key_words` 全量重算 → `store_words` **逐 word 换入、不 clear**（避免回调恰好读到空窗）；`swallow_bound_keys_flag.store(enabled && 设置)`。UI checkbox 在 `ui_toggle_key_row` Windows 分支、`WIN_GRAB_MODES_NOTE` 上方。`WIN_GRAB_MODES_NOTE` 删「尚未实现」并同步改守护测试。
- 新增测试 6 条（403 → **409 passed**）：`capture::bound_keys_tests` ×4（256 vk 双态表驱动、KP5 双 vk、fps_only 常驻断言、跨 64 位边界）+ settings round-trip/缺省/旧 JSON + **windows 缝测试直调 `keyboard_proc`**（吞按下+抬起、`raw_tx` 照收、开关关放行、未绑定键放行）。
- md5：`capture.rs 0e336d950de192b93af06db46c6336dd`（6015 行）/ `app.rs a492aef967b82a42bdc6c69cb67ac48c`（22732 行）/ `settings.rs a67189011b519cf50acea36990082153`（727 行）/ `help.md 9495f1430a8ce13fea4480cda5b76176`。

**A 级验证（Lead 在冻结树上亲跑，输出完整读过）**：`cargo fmt --check` = 0；`cargo test --bin scrcpy-pad --locked` = **409 passed; 0 failed**；`python ci/clippy_diff.py` = **36:36 零差异**；`cargo check --locked` = 0 warning；`grep -c CR` 四文件全 0；`%APPDATA%\scrcpy-pad\config\diagnostics.log` md5 `c4d1f4652f20a7dd79cff6526afa3124`（mtime 00:02:47）**未被测试触碰**。
**越界审计**：与 B21-1a 基线逐文件比对，仅 `app.rs` / `capture.rs` / `settings.rs` / `help.md` 变动；红线文件 `theme.rs`、`keymap.rs`、`engine.rs` **逐位未变**。

**变异验证（§3.1，执行者 3/3 + Lead 亲验 1 条）**：执行者 M1 删 `keyboard_swallows` 的 enabled 早退 ⇒ 红于 `capture.rs:441`；M2 删 `bound_key_words` 的 fps_only continue ⇒ 红于 `capture.rs:481`；M3 文案塞回「尚未实现」⇒ 红于 `app.rs:22658`。**Lead 亲验**（§8.11）：删 enabled 早退 ⇒ 红于 `capture.rs:440` 并打印 `开关关闭时 vk 0x0c(pressed=true)不许被吞`；字节级还原后 md5 精确回到 `0e336d95…`、CRLF=0、复绿 5 passed。

**已知问题 / 未取得**
1. **组合键（combos）里的键本期不进位图**——组合键"生效中"语义复杂（部分按下算不算），代码注释、UI hover、help.md 三处均已如实写明。
2. **B 级未验（一次运行可判定）**：左栏总开关键下方应出现新 checkbox「已绑定键不再传给其它程序(独占模式)」+ 两档说明；勾上后按功能键，记事本/其它程序应**收不到**已绑定键（关闭时行为与本版逐字节一致）。
3. 任务勘验里「KEY_KP5=evdev 82」是 Lead 笔误，实际 `VK_TABLE` 中 KP5=76（0x0C/0x65 都映射到 76）；测试按 76 写并钉住。
4. 真机吞键的最终手感仍属 B 级；链式吞机制已由 B21-1b 实测证实（含真实运行程序）。
### B21-3 交付登记：G-c `ClipCursor` 钉住光标（2026-10-18 05:1x–05:3x）

执行者：subagent `cf8eca35`。写范围：`capture.rs` / `settings.rs` / `app.rs` / `help.md`。

**设计（决策与副作用分离）**
- 三个**纯函数**（两平台都编译、可单测）：`clip_rect_for(pt)`（1×1 矩形，**故意不做边界钳制**——钳到 `(0,0)` 会产生零宽矩形被系统拒掉，钳负坐标会把光标拽到别的显示器）、`should_clip(grabbing, cursor_hide, setting)`（三输入真值表）、`clip_step(applied, want)`（状态机：施加与释放共用同一决策）。
- **Win32 调用只在消费线程**：`windows::clip_cursor_to` / `clip_cursor_release` / `ClipGuard{active}`（`Drop` 里**无条件**释放，幂等）。测试只覆盖**决策**，不覆盖副作用——Win32 是进程级全局状态，在测试进程里真调会把跑测试这台机器的光标钉住，**如实登记不假装有覆盖**。
- 施加时机 = 既有 `mouse_grab`（引擎写），**不另造判据**；`cursor_hide`（全局消隐）**单独为真时不钉**——用户可能只想让光标消失，并非在瞄准。
- **回中逻辑一行未删**（`RAW_RECENTER_INTERVAL`），只加注释说明并存、作为"钉子被目标程序覆盖/忽略"时的兜底。
- `Settings::clip_cursor_in_aim` **默认 true**（`#[serde(default = "default_clip_cursor")]`，老配置升级也默认享受）；FPS 页「鼠标独占与自救」分组一个勾选框，每帧写 `Arc<AtomicBool>` ⇒ 实时生效。

**释放路径（五条，逐条落实）**：① `mouse_grab` 转 false ⇒ `clip_step(true,false)=Release` ⇒ 释放；② 消费线程退出 ⇒ **先解裁剪再恢复光标显示**（顺序有意，反过来光标会跳一下）；③ 线程 panic ⇒ `ClipGuard::drop`；④ 进程被强杀 ⇒ OS 归还（写进注释，无需代码）；⑤ `Capture::drop` 无条件释放。

**A 级验证（Lead 在冻结树上亲跑）**：`cargo fmt --check` = 0；`cargo test --bin scrcpy-pad --locked` = **414 passed; 0 failed**（409 → +5）；`python ci/clippy_diff.py` = **36:36 零差异**；`cargo check --locked` = 0 warning；四文件 `grep -c CR` 全 0；`diagnostics.log` md5 `c4d1f4652f20a7dd79cff6526afa3124` **未被触碰**。
**越界审计**：仅 `app.rs`/`capture.rs`/`settings.rs`/`help.md` 变动；`theme.rs`/`keymap.rs`/`engine.rs` 逐位未变。
**终局 md5（收尾修正后，Lead 亲跑 414 passed 复核）**：`app.rs 95dc1f4a368f0d72b8b0ada097a79a14`（22825 行）/ `capture.rs c48d4d9594d87367fde4024cad3ff3fa`（6507 行）/ `settings.rs 12b43d836be057662134f10b230e9e55`（805 行）/ `help.md 040868d9562480cdacc497a608d855cb`（270 行）/ `README.md e0a085245067250543c75a960ab518db`。
**唯一一处"只加不改"之外的改动（已审计通过）**：`consumer()` 删掉形参 `_start: Instant`——加 `clip_cursor` 后参数 8→9，clippy 立即 `too many arguments (9/7)` vs 基线 `(8/7)` ⇒ 新增告警即门禁红。`_start` 在函数体**零引用**（`grep` 复核），删它零行为变化。

**变异验证（执行者 5 条 + Lead 亲验 2 条）**
- 执行者：M1 去掉 `should_clip` 的 `(false,true)=>false` ⇒ 红于 `:1470`；M2 给 `clip_rect_for` 加 `max(0)` 钳制 ⇒ 红于 `:1420`（负坐标副屏）；M3 删 Release 分支的 `clip_guard.release()` ⇒ **第一轮是假绿**（原断言只判分支头存在，空分支照样绿，正是 V2-4 那类假守护），改用 `block_after` 取分支体逐行判后 **正确变红** 于 `:1576`；M4 `clip_step` 把 `(true,false)=>Nothing` ⇒ 红于 `:1527`；M5 `default_clip_cursor()` 改 false ⇒ 红于 `settings.rs:601`。
- **Lead 亲验**：删 Release 分支的 `clip_guard.release();` ⇒ 三条测试同红（`:1490` / `:1527` / 结构断言），报出「(false,true,true) 只消隐+开关开 ⇒ 不许钉」。

**⚠️ 事故登记：Lead 自己的变异脚本把变体残留进生产代码（2026-10-18 05:4x）**
- **现象**：Lead 用 Python 脚本连做两条变异（M-L1 删 `clip_guard.release()`、M-L2 把 `(false,true)=>false` 改成 `=>true`）。脚本对 M-L1 用 `txt`（原文）做基底、对 M-L2 也用**同一份 `txt`** 做基底，还原时又都写回 `orig`——两次写入/还原交错，最终 **M-L2 的变体留在了 `src/capture.rs:296` 的生产代码里**（`should_clip` 的 `(false,true)` 分支变成 `true`）。
- **检出**：脚本的"还原后复绿"步骤**报红**（2 failed），而不是绿。这正是变异验证的价值——**如果跳过复绿步骤，这个变体会被当成"已还原"静默交付**。随后逐行读 `should_clip` 源码，确认第 296 行是 `true`。
- **修复**：按字节把 `(false, true) => true,` 改回 `=> false,` ⇒ md5 精确回到执行者交付值 `c48d4d9594d87367fde4024cad3ff3fa`、CRLF=0、全量复跑 **414 passed / fmt 0 / clippy 36:36**。
- **两条新红线（已并入本节纪律）**：① **每次变异必须是"单变异 → 立即还原 → 复跑确认绿"的独立回合**，不许把多条变异串在同一个脚本里共用同一份基底文本；② **复绿必须真变绿**——复跑报红时禁止当作"环境抖动"忽略，必须逐行读被抓的生产代码。
- **另一条同类记录**：同一次操作中脚本还因 Windows 控制台 GBK 编码在 `print(⇒)` 处抛 `UnicodeEncodeError` **中断在变异状态**（文件留在变异版）。处置：改用「日志写 UTF-8 文件 + 读文件」而不是 `print` 中文，并先跑一次"按字节还原"确认 md5 回到交付值再继续。

**已知问题 / 未取得**
1. **B 级未验（一次运行四步可判）**：① 进 FPS 后 `diagnostics.log` 出现「已钉住光标」；② 快速甩鼠标，光标应**停在屏幕上不动**；③ 退出 FPS 出现「已释放光标裁剪」且光标立刻能动；④ 瞄准中用记事本等覆盖钉子、再用回中兜底验证。
2. **`clip_rect_for` 不钳制边界**是有意取舍（见函数注释），但**多显示器/DPI/旋转**下的实际表现属 B 级，未真机验。
3. UI 勾选框只在 `#[cfg(windows)]` 下渲染；Linux 上字段仍在（默认 true）但捕获层不读——**未在 Linux 实跑验证**。
### ⚠️ B21-3/G-c 真机验收反馈：ClipCursor 钉不住 + 隔膜在遮板模式下被让位（2026-10-18 06:4x，用户实测）

用户实测（06:26 那次运行，trace `trace-1792275996.jsonl` 仍然在）反馈三条，Lead 已逐条取证：

| # | 用户反馈 | Lead 取证结论 |
|---|---|---|
| 1 | 勾选「钉住光标」后**鼠标指针仍能移动**，未达到"不动"的效果（设置本身确实生效） | **未能从日志判定**：`settings.json` 的 `log_level` 是 **warn**，而钉住/释放/被拒绝三条日志分别是 `diag_info!`（:4970/:4988/:5193）与 `diag_debug!`（:4979）⇒ **全部被压掉**。trace 证明 FPS 瞄准态确实进入过（`mode=1` 294 条样本，t=23.6 s→288.6 s；FPS 窗口内 `in/motion` 5764 条），故 `mouse_grab` 应为真、`should_clip` 应为真。**⇒ 这是本批次最严重的方法缺陷：把验收证据写在了会被用户日志级别压掉的等级上（G-a 时已经踩过同一个坑，G-c 又犯一次）。** 另：方案 §4 的 **P2（ClipCursor 会不会被目标程序覆盖）明写"需你在场"，从未做就上了 G-c** ⇒ 违反协议 §4「未测假设不许承重」。 |
| 2 | **鼠标独占（隔膜）不生效**：仍能点击其它界面的按钮 | **设计使然，不是 bug**。用户 `settings.json` 的 `mouse_capture = "Mask"`（遮板），而 `theme::shield_effective()`（:406-416）规定：**遮板真的在工作时，隔膜必须被强制关掉**（"两者只能有一个生效"）。日志逐字印证：`遮板已启用:贴在投屏窗口 (949,84) 661x1432;鼠标事件由它接管(隔膜已让位)`。遮板只覆盖投屏窗口矩形，**窗口外的点击当然照常落到别的窗口**。 |
| 3 | 多显示器 / DPI 下的裁剪行为 | 用户目前无第三显示器，**已列为待办**，等用户通知后补测。 |

**根因候选（按可能性排序，均未证伪，不许据此改码）**：
1. **P2 未测**：目标程序（scrcpy/SDL 全屏窗）或其它前台程序可能用 `ClipCursor(NULL)` 覆盖我们的裁剪；也可能是"前台窗口变化时系统重置裁剪"。
2. **与回中逻辑打架**：`capture.rs:5004-5017` 每 16 ms 在 `mouse_grab` 期间调 `move_cursor(center)`；裁剪生效时该调用会被系统钳回裁剪矩形内，但**这一对调用的真实交互从未实测**（方案原本要求"先并存、后简化"，并存是对的，但没验过并存时谁赢）。
3. **跨线程调用**：`ClipCursor` 由捕获层**消费线程**调用，而其语义与"调用线程/前台窗口"的关系未在本机实测过。

**必须的下一步（顺序不可颠倒）**：① 把三条裁剪日志提到 `diag_warn!`（用户 `log_level=warn` 也要看得见）+ 把「被系统拒绝」带上 `GetLastError`；② FPS 页加一行**实时状态**（钉住中 / 未钉住 / 被拒绝 / 开关关），与既有「鼠标独占」状态行同形态，让"钉没钉住"不用翻日志就能判；③ 补做 **P2 探测**（目标程序在前台时 `ClipCursor` 是否被覆盖）——**P2 不成立就必须回滚 G-c 或重新设计**。

**同时登记的已知问题（用户裁定"不是大问题，标记即可"）**：遮板模式下隔膜被让位 ⇒ 用户在遮板模式下永远看不到「鼠标独占」生效。这是既有设计（V2 时代"两者只能有一个生效"），**不是 B21 引入的**；但 `help.md` 与界面文案**从未向用户说明"选了遮板就没有隔膜"**，属文案缺口，随下一批文案一并补。
### B21 真机反馈修复 + 四项 UI 改动（2026-10-18 06:4x–07:1x）

**一、可观测性修复（用户 06:4x 反馈驱动，必须先做）**
| 项 | 改动 | 证据 |
|---|---|---|
| 裁剪日志被日志级别压掉 | 「已钉住光标 / 已释放光标裁剪 / 捕获线程退出释放」由 `diag_info!` → **`diag_warn!`**；「被系统拒绝」由 `diag_debug!` → `diag_warn!` 并带 `GetLastError()` + 累计拒绝次数 | 用户 `settings.json` 的 `log_level=warn`，此前三条证据**全部不可见** ⇒ 该功能**无法验收**（G-a 已踩过同类坑，G-c 又犯一次） |
| 没有界面判据 | FPS 页「鼠标独占与自救」新增 `clip_status_line` **实时状态行**（纯函数 + 守护测试 `clip_status_line_has_exactly_one_green_state`，四态互不重叠、只有"钉住中"是绿、阴对照防撞文案） | 不翻日志即可判"钉没钉住、以及为什么没钉住" |

**二、四项 UI 改动（用户 2026-10-18 06:4x 提出）**
| # | 用户原话 | 实现 | 守护测试 |
|---|---|---|---|
| 1 | 「让录制功能换个行…换个行方便阅读」 | `swipe_controls` 的取起点/取终点/时长改成**独立一行**；轨迹段另起一行并加「轨迹:」小标题 | **见下方事故登记**（第一版是假的） |
| 2 | 「按键后轮换鼠标键位集合…增加勾选项，只有勾住的键位才能在轮换的集合里。顺便提供全选」 | `SwitchKey.rotate_set: Vec<usize>`（**空 = 全部**，与旧配置逐字节一致）；`engine::rotate_target` 纯函数（去重/去越界/单向绕回）；`normalize` 清洗脏下标；UI 在「其他功能 → 切换键位」按 `Next/Prev` 显示每套组合的勾选框 + 「全选」 | `rotate_set_controls_which_schemes_take_part_in_cycling`（15 条断言，含"空集合 = 全选"阴对照） |
| 3 | 「诊断的框默认太大了，让框边界根据文本输出长度智能调整」 | 纯函数 `log_window_size(条数, 最长行字符数)`：宽 = 字符×7.2+24 钳 [420,1100]、高 = 条数×18+50 钳 [240,620]、空日志 ⇒ (420,240)；只用于 `default_size`（egui 天然只在首次出现时采纳，不与手动拉伸打架） | `log_window_size_follows_text_length_not_a_fixed_box`（递增 + 钳制 + 极端输入 + "三者不全等"阴对照） |
| 4 | 「默认截图太小，让默认截图与…左右界相切而非上下界」 | 新纯函数 `shot_default_base`：**只看宽度** `by_w.clamp(0.05, 1.0)`，高度交给画布自己的滚动区；非有限输入退回旧的等比内接 | `shot_default_scale_fits_width_and_never_upscales`（含"必须比旧口径更大"的对照断言 + NaN/0 退化） |

**A 级验证（Lead 亲跑）**：`cargo fmt --check` = 0；`cargo test --bin scrcpy-pad --locked` = **418 passed / 0 failed**（开工 415 → +3）；`ci/clippy_diff.py` = 36:36；`cargo check` = 0 warning；`src/app.rs` md5 `e27c3d28a0f301976d760516a90727e7`、**CRLF = 0**。
**变异验证（3/3 亲跑，逐条红并报位）**：① 断 `rotate_set` 生效点 ⇒ 红于 `engine.rs:8157`（`从 1 正向应到 3（跳过 2）`）；② 把 `shot_default_base` 退回旧的等比内接 ⇒ 红于 `app.rs:22123`（`期望 0.7407，实际 0.25`）；③ 把 `log_window_size` 改成恒返回 760×460 ⇒ 红于 `app.rs:22213`。三条均字节级还原、md5 精确回到交付值、复绿。

**⚠️ 事故登记：Lead 的"换行"改动是假的（门禁绿但症状仍在），由子智能体检出**
- **现象**：第一版把 `swipe_controls` 里的轨迹段包进 `ui.horizontal`，以为这样就换行了。**实际没有任何换行效果。**
- **检出者**：同批执行者（`861bdccd`）在只读审计中提出，理由精确：`swipe_controls` 的调用点在 `ui_bind_row` 的 `ui.horizontal`（:7599 开 → :7865 关）**闭包内部**，在水平布局里再开 `horizontal` 只是开一个**水平子布局**。Lead 复核并追加核实：`egui 0.36.1` 的 `Ui::end_row()` 只对 **grid / wrapping** 布局生效，**对普通 horizontal 无效** ⇒ 没有任何"内部换行"的旁门。
- **修复**：把滑动细项**移出水平闭包** —— `ui_bind_row` 里先在闭包外取一次 `is_swipe_outer`，闭包只留"键型 + 键位按钮 + 仅FPS + 删除"；闭包结束后用 `ui.indent(...)` 在**垂直布局**里渲染 `swipe_controls` + 录制卡片。此时 `swipe_controls` 内部的 `ui.horizontal` 才是**真正的新行**。
- **教训（与 §0.1 同型）**：纯布局改动的"完成"标准**不是门禁绿**，而是**布局结构本身**（父容器方向）。本轮因此新增一条自检口径：**任何"换行/换列"类改动，先在代码里确认父容器是不是水平布局**；是的话，`ui.horizontal`/`end_row` 都不解决问题，必须改父容器。

**⚠️ 事故登记：Lead 与子智能体并发写同一文件（§8.11 违规）**
- 派发 `861bdccd` 后，Lead **未等待**就用同一份 `src/app.rs` 落了自己的改动（B21-C/D/E/F）⇒ 子智能体开工时拿到的基线（`323242d2…`/22922 行）在 6 分钟后失效。它按纪律停下报告而不是覆盖，**避免了互相踩踏**。
- **定性**：违反 §8.11「同一文件同一时刻只许一个写者」。后果是子智能体的任务面被我先行做了，它只能转做审计 —— 本轮它的**最大产出恰恰来自这次审计**（上面那条假换行的缺陷）。
- **口径**：文件面冲突时，**先派发者不得再写同一文件**；或明确"由 Lead 自己做，不派"。两者必须事先说死，不许"边派边自己写"。
### B21-E 补登记：录制换行的入口面矩阵（4 入口）+ 两处文案裁决（2026-10-18 07:0x–07:2x）

**背景**：B21-E 第一版是**假换行**（嵌套 `ui.horizontal` 在水平父布局里不换行），由执行子智能体只读审计抓出。修正后用户需求仍未在**全部入口**满足 —— 又是同一批子智能体做了**入口面矩阵实测**（§5 要求的那个走查，Lead 自己漏做了）。

**入口面矩阵（子智能体实测 + Lead 独立探针复核）**

| # | 入口 | 修正前父布局 | 修正后 | 探针 dy |
|---|---|---|---|---|
| 1 | `ui_bind_row`（已有键位） | `ui.horizontal` 闭包内 | ⚠️ 移出闭包、改走 `ui.indent`（**此改法当天即导致启动崩溃，见下文 🚨 事故登记**） | **21.0**（新行） |
| 2 | `combo_action_editor`（已有组合键） | `Frame::group` 垂直 | ✅ 本来就对 | **21.0** |
| 3a | `ui_draft_fields` / VisualStyle（可视化草稿） | 垂直 | ✅ 本来就对 | **21.0** |
| 3b | `ui_draft_fields` / **DefaultStyle**（默认风格草稿） | **`ui.horizontal` 闭包内** | ✅ 本次修正：`ui_draft_fields` 移出闭包 | **21.0** |

- **3b 是真实用户路径**：`RightTab::Keys → ui_binds → ui_binds_new → ui_draft_card(DefaultStyle) → ui_draft_fields`。用户在默认风格新建滑动时看到的仍是长串 ⇒ 与本任务要解决的反馈直接相关。
- **Lead 独立验证**：自建 headless 探针（`scrcpy-pad-bench/tools/egui-layout-probe/`，egui 0.36.1 + `context.run_ui`），复刻四个入口的真实父布局并测量 `dy`，输出 `ALL_ENTRIES_PASS`（四入口 dy 全为 21.0 ⇒ 均新行）。**另测**：水平父布局内嵌 horizontal 的 dy = **0.0**（同 y，仅 x 偏移）⇒ 证实旧结构确实不换行。
- **⚠️ 这次探针验证的方法论缺陷（必须记住）**：我在探针里把"`ui_bind_row` 的父布局"**写成 vertical + indent**，因为我以为移出闭包后就是垂直的。但**真实调用点是 `ui.horizontal(|ui| self.ui_bind_row(ui, i))`**，`ui_bind_row` 拿到的仍是水平 Ui。⇒ **探针验证的是我的假设，不是真实结构**，于是 dy=21.0 全绿，而真机启动即崩。**教训：验证"结构"时必须从真实调用点逐层追父容器，探针里的父布局必须是从生产代码抄下来的，不能是推断的。** dy 这种"结果指标"测不出"父布局对不对"。
- 附带修正：`ui_draft_fields` 的 `0|1` 分支原来按外壳区分"要不要自己开水平行"，现在两个外壳都是纵向布局 ⇒ 统一自己开行（原注释已同步更新）。

**两处文案裁决（子智能体提出，Lead 裁定）**
| 问题 | 裁决 | 落地 |
|---|---|---|
| 新增小标题 `ui.label("轨迹:")` 与下方**原有**的路径下拉标签 `ui.label("轨迹:")` **同名**（用户会看到两个「轨迹:」，一个说状态一个选形状） | **改名** —— 重名比原样更难读 | 状态小标题改为 **「录制:」**；下方原有的路径下拉保持「轨迹:」不动（它是既有文案，不在本任务范围） |
| 状态文案被我改写（`"轨迹已启用:…"` → `"已启用 —— …"`；`"轨迹:未录制(…)"` → `"未录制(…)"`），超出"纯布局重排"的约束 | **保留并登记**（不回退）。理由：① 拆分后小标题已承担"轨迹"二字，正文再重复是冗余；② 原文案里 `"轨迹:"` 前缀在**水平行内**才需要自我介绍，换行后不再需要；③ 无测试钉这两串 ⇒ 不会假绿 | 现状即最终；本条如实记录为"超出任务约束的文案调整" |

**A 级验证（Lead 亲跑，定稿树）**：`cargo fmt --check` = 0；`cargo test --bin scrcpy-pad --locked` = **418 passed / 0 failed**；`ci/clippy_diff.py` = 36:36 零差异；`cargo check --locked` = 0 warning；`src/app.rs` md5 `20e9f0297b1482a813ae85ff1558f12f` / 23,168 行 / **CRLF = 0**。

**任务 B 变异（子智能体在稳定树上完成，本次采信其原始输出）**：把 `log_window_size` 的宽度上限 `1100.0` 改 `420.0`（伪"固定宽度"）⇒ `test FAILED`，panic 精确报点 `src/app.rs:22243: 长文本必须比短文本宽:(420.0, 620.0) vs (420.0, 266.0)`；还原后 touch + 重跑 `1 passed`。**⇒ 该测试是真守护，不是恒绿假测试。**

**执行者纪律记录（正面）**：`861bdccd` 全程**未写 `src/app.rs` 一个字节**，在发现并发写者后按 §8.2/§8.11 停下报告，转而做只读审计 + 探针实测 —— 这一轮它贡献了三项：① 抓出 Lead 的假换行；② 抓出 3b 入口遗漏；③ 抓出文案重名。**三条都是 Lead 自查没发现的。**
### P2 探测结论（2026-10-18 07:3x–15:5x）：ClipCursor 机制有效，问题在触发条件

用户 06:4x 反馈「勾选钉住光标后鼠标指针仍能移动」。按方案 §4 的硬前置补齐 **P2** —— 这是 B21-3 当初违反协议 §4「未测假设不许承重」跳过的、明确要求「需你在场」的一步。

**探针**：`scrcpy-pad-bench/tools/clip-probe/`（独立 crate，工作区外；windows-sys 0.59；硬超时 25s；结束**无条件** `ClipCursor(NULL)`）。**主仓 src/ 零改动。**

**A 级原始输出（Lead 亲跑两次）**
```
# 第一版：±400px 框 + 切前台
cursor_start = (923,789)   clip_before = None
apply +-400px: ok=1        clip_after_apply = Some((523,389,1323,1066))
SetCursorPos(+2000) -> (1323,1066)   CLAMPED = true (dx=400)   # 光标被真实钳在框内
SetForegroundWindow(notepad) ret=0                            # 前台切换被系统拒绝 ⇒ 该子项 SKIP（如实登记）
release: ok=1              clip_after_release = None          # 释放干净

# 第二版：按**生产口径**逐尺寸压测 + 复刻"回中每 16ms 拉光标"的干扰
side= 1 apply_ok=1 accepted=Some((1019,735,1020,736)) after_push=(1020,736) moved=false
side= 2 apply_ok=1 accepted=Some((1019,735,1021,737)) after_push=(1021,737) moved=false
side= 8 apply_ok=1 ... moved=true
side=32 apply_ok=1 ... moved=true
recenter_escaped_1x1 = false      # 裁剪生效时连调 10 次 SetCursorPos 到别处，一次都没逃出
released clip=None cursor=(1019,735)
```

**结论三条（全部实测支撑）**
1. **`ClipCursor` 在本机完全可用**：施加成功、`GetClipCursor` 读回正确矩形、光标被真实钳住、释放干净 ⇒ **核心机制不是失效原因**。
2. **生产用的 1×1 矩形是四个尺寸里最严格的**（side=1/2 时 `SetCursorPos` 推不动；side=8/32 反而能推出框）⇒ 生产取值**正确**，不是"框太小才失效"。
3. **回中逻辑不与裁剪打架**：裁剪生效期间连续 `SetCursorPos` 十次，光标一次都没逃出 ⇒ B21-3 方案里担心的"回中每 16ms 把光标拽走"**不成立**。

**⇒ 失效只剩"触发条件"一侧。** `mouse_grab`（钉住的**唯一**判据）在 `engine.rs:4018` 由 `enabled && connected && pointer_should_be_hidden(...)` 决定，而 `pointer_should_be_hidden`（`engine.rs:1661`）= `connected && aim.capture_mouse && fps_mode_active`。

**从用户那次真机 trace 反查（`trace-1792275996.jsonl`）**
- FPS 瞄准态**确实进入过**：`mode=1` 共 294 条样本，t=23.6s → 288.6s；
- 但同一次运行里 `diagnostics.log` 出现**两次** `控制通道对端关闭(EOF),连接标记为断开`（t≈186s、t≈294s）；
- ⇒ **控制通道断开 ⇒ `connected=false` ⇒ `mouse_grab=false` ⇒ `should_clip=false` ⇒ 永不钉住** —— 与用户症状完全吻合的一条解释；
- 另：用户当时的日志级别是 **info**（日志头"级别 info (内置默认)"），而钉住日志当时走 `diag_info!` ⇒ 理论上该看得见却一条都没有 ⇒ 说明那次运行**根本没走到 Apply 分支**，而不是"走到了但被日志级别压掉"。

**⚠️ 另一条必须先解决的事实：用户验收时跑的 exe 不包含本批修复**
`release/windows-x86_64/scrcpy-pad.exe` 构建时间 **06:19:13**，而 B21 四批改动（G-a/G-b/G-c + 四项 UI）在 **06:5x–07:1x** 才落盘 ⇒ **用户那次验收跑的是旧二进制**。这一条单独就足以解释"改了没效果"，与上面那条并列为本轮两大解释。

**处置**：已 `cargo build --release --locked`（1m11s）并部署到发布目录：`a0923a5b…` → **`f20244d4a33363dbacb6abff07279b62`**（11,750,400 B，15:53:47）。**C9 锚点随之更新**。（**更正**：此前本节写"zip 未重建"是**错的** —— 复核时发现 `release/scrcpy-pad-0.2.0-windows-x86_64.zip` 已于 22:38:55 被重建，md5 `6b6500d01da1645e41c775a01a756984` / 4,970,481 B；**解包核验内层 `windows-x86_64/scrcpy-pad.exe` = `f20244d4a33363dbacb6abff07279b62`，与部署的 exe 逐位一致** ⇒ 该包**不是过期包**，内含本批全部修复。`release/scrcpy-pad-0.2.0-linux-x86_64.tar.gz` 仍是 10-13 的 `00812f9c…`，**显式标注：已知过期，停留于 V2-3**（§6.3））。

**未做 / 待验**
1. **P2 的"切前台后裁剪是否被系统清掉"子项未测成**：`SetForegroundWindow` 被系统拒绝（返回 0、前台未变），如实登记；要测需用户手动切窗口配合。
2. **跨进程覆盖**（别的程序调 `ClipCursor(NULL)`）未测：原理上可能，但本场景（scrcpy 全屏）大概率无其它程序主动改裁剪。
3. **B 级待用户验**：用**新 exe**、**保持控制通道在线**，进 FPS 后看状态行是否显示「钉住中」、甩鼠标光标是否不动。若显示"瞄准中但投屏窗口不在前台"，则按提示查前台归属。

### 🚨 事故登记（最高级）：`ui.indent` 用在水平布局 ⇒ **程序启动即崩溃**（2026-10-18 23:19 用户实测）

| 项 | 内容 |
|---|---|
| **现象** | 用户双击启动即崩：`thread main panicked at egui-0.36.1/src/ui.rs:2246: You can only indent vertical layouts, found Layout { main_dir: LeftToRight, ... }`。**程序完全无法使用。** |
| **根因** | B21-E 修"录制换行"时，我在 `ui_bind_row` 里加了 `ui.indent(("swipe_detail", i), ...)`。`ui_bind_row` 的**两个调用点**当时都写成 `ui.horizontal(|ui| self.ui_bind_row(ui, i))` ⇒ 它拿到的 Ui 是**水平**布局 ⇒ `ui.indent` 直接 panic。我此前只审查了"`swipe_controls` 内部"的父布局，**没有审查 `ui_bind_row` 自己的调用点**。 |
| **为什么门禁全绿却照样崩** | 418 passed / fmt 0 / clippy 36:36 全绿 —— 因为该路径只在**真的渲染出某条键位**时才走到，而单测不渲染 GUI。⇒ **§0.1「门禁绿 ≠ 问题解决」的又一次实证**，也说明"布局类改动"用单测根本挡不住。 |
| **修复** | ① 去掉两处调用点的 `ui.horizontal` 包裹（`ui_bind_row` 内部本来就自带水平行）；② **防御性**：把 `ui.indent` 外层显式套 `ui.vertical(...)`，这样即便将来又有人包水平层，也只是布局变差而**不会崩**。 |
| **机制实证** | 独立探针 `tools/egui-layout-probe`（catch_unwind + headless）：`A_horizontal_then_indent` ⇒ **PANIC**(`You can only indent vertical layouts, found Layout { main_dir: LeftToRight`)；`B_vertical_wrapper_then_indent`（修复写法）⇒ **OK**；`C_vertical_then_indent` ⇒ OK。 |
| **端到端验证** | 部署修复版后实跑 12 秒（`timeout 12 ./scrcpy-pad.exe`）：**无 panic**、无 `You can only indent`，正常进入运行期日志（钩子自愈等 WARN 属既有行为）。退出后 `tasklist` 无残留进程。 |
| **新增守护测试** | `indent_for_swipe_detail_is_never_placed_in_a_horizontal_parent`（app.rs）：源码结构扫描 —— ① 生产代码区内所有 `self.ui_bind_row(` 调用点不得含 `ui.horizontal`；② `fn ui_bind_row` 体内必须有 `ui.vertical(` 兜底；含阳/阴对照。**变异验证**：把水平包裹加回去 ⇒ 红于 `app.rs:22315` 并点名 `:7590`，还原后 md5 逐位复原、复绿。 |
| **教训（可执行口径）** | ① **布局类改动的验收标准是"结构"而非"门禁"**：新增 `ui.indent` / `ui.horizontal` / `end_row` 这类**对父布局敏感**的调用前，必须逐个调用点追到"父容器方向"；② 对布局敏感的 API，**优先自带保护容器**（如 `ui.indent` 外套 `ui.vertical`），把"调用方布局"变成非致命因素；③ **启动路径的崩溃单测挡不住** ⇒ 交付前应至少有一次**真实启动**（本轮已补做）。 |

**修复后终态**：`cargo fmt --check` = 0；`cargo test --bin scrcpy-pad --locked` = **419 passed / 0 failed**（414→418→**419**，新增那条崩溃守护测试）；cli 36:36；`cargo check` = 0 warning；`src/app.rs` md5 `ccfd765fc25069c4963bea9db60bed5b` / CRLF 0；`release/windows-x86_64/scrcpy-pad.exe` = **`df2ab724bb24150f57e80d651f4a58b0`**（23:39:08 部署，已端到端验证可启动）。
**同型后续（2026-10-23）**：又一次「门禁全绿但 UI 路径必死」—— **配置锁自锁**（点 [FPS 功能] 页立刻冻结、CPU 0%），见下方 `### 🚨 事故登记 + 修复交付：配置锁自锁` 节。

### ⚠️ 事故登记：测试污染用户现场（2026-10-12 23:0x）

| 项 | 内容 |
|---|---|
| **现象** | 用户让我看的 21:47 会话日志**被整份覆盖**：`diagnostics.log` 起始时间变成 23:01:59，内容全是 `diag::tests::…` 与 `sim::tests` 的输出 |
| **根因** | 上一轮我新加的"缝①"测试调用了**生产** `diag::init()`，而 `init()` 对 `config_dir()/diagnostics.log` 做 `File::create`。`cargo test` 跑在**用户机器**上 ⇒ 直接覆盖用户现场 |
| **性质** | 与 F1 同类但更糟：F1 是"承诺的日志没落盘"，这次是"**测试把用户唯一的取证介质删了**" |
| **修复** | ① `init()` 在 `cfg(test)` 下强制落到**临时目录**；② 缝①测试改为把 `Sink` 指向临时文件、并在末尾断言"生产日志未被改动"；③ 新增守护测试 `tests_never_touch_the_production_log_path` |
| **验证（A 级）** | 跑测试前后 `diagnostics.log` 的 md5 **完全一致**（`94ffb23fecc65276a9a36a7fd3385d38`） |
| **新增红线** | **测试绝不许写用户配置目录**。任何"用生产同一条通路验证"的想法，都必须先把目标路径指向临时目录 |


**② 已证伪假设清单**

| ID | 假设 | 证伪证据 |
|---|---|---|
| D1 | 归中抖动导致锁死 | 修掉后症状仍在 |
| D2 | Raw Input stall（DF-9） | 看门狗修掉后症状仍在 |
| D3 | 换手/重叠 re-arm 导致锁死 | 锁死期间 13 次重叠换手无一奏效 |
| D4 | 触摸坐标空间过期 | 用户 30 秒实验否掉；周期重查上线后症状仍在 |
| D5 | 控制通道死亡 | 锁死态与正常态 `/proc/net/unix`、`ps` 一致 |
| D6 | 组合键窗口吞掉 `leftshift+leftbutton` | 真根因是 `recoil.trigger_key=BTN_LEFT` 提前消费（已定性，用户决定不改） |
| A1 | `getevent -lt <单设备>` 输出带设备名前缀 | 两种形态同一次触摸对比：单设备形态**无**前缀 |
| A2 | `TouchStates` 统计注入触点 | 瞄准指按下时读 `dumpsys`：账单仍为 0 |
| A3 | adb 在管道下块缓冲输出 | 管道读首行 32ms 到达 |
| A4 | 键盘钩子真的被系统摘除 | 同一秒鼠标钩子回调 300 次/5s |

**③ 未测假设清单（机制正承重）**

| ID | 假设 | 承重机制 | 状态 |
|---|---|---|---|
| U1 | "手离开手机 ⇒ 抬指重建 ⇒ 恢复跟手" 在**真锁死**状态下成立 | 卡死自愈主触发 | **已实测**（真机 3 次，用户确认有效） |
| U2 | 自愈抬指后"下一次位移自动重新落指"在真机上无副作用 | 自愈主机制 | 部分实测（日志/轨迹有落指，手感未专项确认）→ B 级 |
| U3 | F5 钩子探针"无回音"的真实原因 | 钩子自愈是否该永久停用 | **已实测-已修**（见下 H3） |
| U4 | F6 参数重复的来源 | scrcpy 启动参数正确性 | **已实测-已修**（见下 H4） |
| U5 | `getevent` 在**多设备**形态下是否仍会出现（影响解析回归面） | 解析器容错 | 未测（当前生产只用单设备形态） |
| U6 | `SendInput` 相对位移会以 `WM_INPUT` 派发到**生产 Raw 通道**（B4 承重前提） | R6/R7/R8/R9/R10 全部 bench 实验的手段有效性 | 未测；判定实验 = B4 专会第一段（30s 单档注入，判据见"B4 会前登记"节的 P 一证一假） |

**⑤ 本轮新增假设（2026-10-12,F5/F6 取证）**

| ID | 假设 | 状态 | 实测命令 / 证据 |
|---|---|---|---|
| H1 | 环境/安全软件吞掉注入事件（原"探针无回音"的解释） | **已测-证伪** | 手写同形态探测：4 枚探针 `SendInput=2/2`，钩子认出 **16/16** |
| H2 | 同线程 `SendInput` 导致自己的 LL 钩子看不到 | **已测-证伪** | A 阶段（钩子线程自己发）同样 **16/16** |
| H3 | **钩子回调在 `SendInput` 期间重入** ⇒ 回调先写 `seen`、返回后才写 `sent` ⇒ 时间戳比较**每一拍**都 `seen < sent` ⇒ 恒判无回音 ⇒ 永久停用自愈 | **已测-成立** | 序号实验：`sent_seq=5 seen_seq=4 → probe_missed=true`（4 拍全中）；探针其实 4 枚认出 14 个事件 |
| H4 | `加入 scrcpy 参数` 按钮**无条件追加** ⇒ 同键堆多份；老配置已累积 | **已测-成立** | 用户 `settings.json`：`--max-size` 三份、`--max-fps` 两份 |

| H5 | 投屏窗口(`SDL_app`)在**最小化**时 `GetWindowRect` 返回可用坐标 | **已测-证伪** | 真机实测:`iconic=1` 且坐标为哨兵值 `(-21333,-21333)` ⇒ 最小化时**不贴**,退回隔膜(用户 2026-10-13 选定) |
| H6 | 遮板能定位到投屏窗口 | **已测-成立** | 生产同形只读探测:可见窗口 `(300,200) 640x480` 定位逐位一致;最小化/未启动 ⇒ `None` ⇒ 退回隔膜 |

| H7 | 遮板矩形 = 画面显示区域（可直接拉伸映射） | **已测-证伪** | 用户实测"圆的位置确实歪了"：scrcpy 窗口有标题栏/边框 + letterbox，必须**等比居中**适配 |
| H8 | 遮板会跟随投屏窗口移动 | **已测-证伪** | 用户实测"最小化/移动窗口遮板都不变化，直接盖在屏幕表面不走了"（原实现 500ms 节流 + 只在未起窗时定位） |
| H9 | `mouse_capture` 放 `Look` 里能稳定生效 | **已测-证伪** | `look.json`=`Mask` 而生效方案 `profile.yaml`=`Shield` ⇒ 选择串了（Look 是每方案一份，且启动被 look.json 覆盖） |

| H10 | 中文排序可以手写拼音表搞定 | **已测-证伪**（不值得） | `CompareStringEx(zh-CN)` 只读探测：`[一,丁,万,上,不]` → `[不,丁,上,万,一]`（b/d/s/w/y 拼音序），与 Unicode 码点序完全不同 ⇒ 用系统 API，零维护 |
| H11 | 遮板矩形（整窗）就是画面区域 | **已测-证伪** | 用户反馈"圆的位置仍然不太正确"：整窗矩形含标题栏，必须改用 `GetClientRect`+`ClientToScreen`（客户区） |
| H12 | 客户区变矮 ⇒ 黑边变大 | **已测-证伪**（我的断言错） | 单测当场抓出：宽度约束下客户区 1067→1000，黑边 139.4→105.9（**变小**）|

| H13 | 遮板窗口贴"投屏窗口矩形"就能对齐 | **已测-证伪** | 用户第四轮反馈"普通缩放根本无法显示蒙版,蒙版都缩到窗口外面了":GDI 用窗口客户区坐标,传屏幕坐标必然整体偏移 |
| H14 | 遮板只要"整窗等比居中"就够 | **已测-证伪** | 真机只读探测:客户区 1706×1066、手机比例 2.16563 ⇒ 画面只有 **1706×788**、上下黑边 **139px**;按整窗算会把图形纵向拉大 ⇒ 摇杆圆心明显偏上 |
| H15 | 摇杆偏得多是"另一套 bug" | **已测-证伪** | 真机数据:摇杆半径 0.0553 = 按键 0.0127 的 **4.4 倍** ⇒ 同一个坐标系偏差在大圆上更显眼,是同一根因 |

| H16 | 连点可以复用 `SchedAct::MacroClick` | **已测-证伪** | sim 实测 `down 1 up 0`：`MacroClick` 走宏的 lane 与运行表（`macro_down`），普通键位的抬起被拒 ⇒ 改用普通 lane 的 `ReDown`/`Up` |
| H17 | 连点松键后会自动停 | **已测-证伪** | Tap 的 `duration_ms != 0` 分支只处理 `fresh_press`，松键不做任何事 ⇒ 必须显式在 `!ev.pressed` 时 `cancel_up_for`（否则"松手后还在自己点"） |
| H18 | 测试键 `30`(A) 可用于普通绑定 | **已测-证伪** | `Profile::default()` 的默认轮盘把 17/31/30/32(W/S/A/D) 绑成方向键，方向键事件被轮盘**消费**（`dispatch_input` 提前 `return true`）⇒ 测试改用 57(空格) |
| H19 | `SendInput` 注入的相对位移会以 `WM_INPUT` 派发到生产 Raw 通道（bench 注入手段成立的前提） | 未测（承重前提，判据不在此重复） | 见 ③ 表 **U6**；判定实验 = B4 专会第一段，全文见"B4 会前登记"节 |

**F5 修复**：`probe_sent_kb`/`probe_seen_kb` 从**时间戳**改为**计数**（重入后两者相等 = 健康；钩子真死时 `seen` 落后）。
**F6 修复**：新增 `upsert_arg`（同键**字符串匹配就地替换**）+ `dedupe_args`（启动前按"保留最后一次"去重，与命令行语义一致）。


---

### Linux 端追赶 + 三缺陷修复 + 双平台发布包重建 交付登记（2026-10-19 06:1x）

**任务**：用户 m01228 四项之二 —— 「全力进行 linux 交叉编译验证，让程序 linux 端能跟 windows 端进展一样」。

**① 交付物（文件 + md5 + 大小）**

| 文件 | md5 | 大小 | 说明 |
|---|---|---|---|
| `src/diag.rs` | `4ad2c3b9967bccdc89c2eaea6c680470` | — | 新增 `open_log_file`，建父目录 |
| `src/fonts.rs` | `a244c7419dfe0286171bc0dd6d0a6ae5` | — | 新增 `fontconfig_cjk_font()`，`fc-list :lang=zh-cn` 兜底 |
| `src/app.rs` | `6b88679cbf280ecf0ab37bd3d79510b1` | 23,2xx 行 / CRLF 0 | 6 处 Windows 扩展名字面量平台化 + 新增守护测试 |
| `src/main.rs` | `2d88efee3059e49cb5e3be59478297cf` | — | （本轮无功能改动，随构建重编） |
| `release/windows-x86_64/scrcpy-pad.exe` | `777a467d791f5ded32e44ee01885c6aa` | 11,746,816 B | Windows 侧新二进制 |
| `release/scrcpy-pad-0.2.0-windows-x86_64.zip` | `e3955aa3aba34da3d1f13cb3d4489055` | 4,978,003 B | 解包核验内层 exe = `777a467d…` **逐位一致** |
| `release/linux-x86_64/scrcpy-pad` | `ae17e25af2763c73742c00c7586de019` | 15,729,280 B | Ubuntu-20.04 构建，`readelf` 最高符号 **GLIBC_2.30**（≤ 基线 2.31） |
| `release/scrcpy-pad-0.2.0-linux-x86_64.tar.gz` | `ba4830e228c8eef18bc47b9c1245643b` | 6,827,316 B | 解包核验内层 exe 逐位一致 |
| `文档/善后计划-2026-10-18.md` | `0322e40c3fffad4e5503ddb45b62d272` | 156 行 | m01228 第 1 项交付 |
| `文档/我一人无法解决的问题-2026-10-19.md` | `911fb9b2e6fa12280e1a40691b7e0b5d` | 112 行 | m01228 第 3 项交付 |
| `文档/Linux优势与妙想清单-2026-10-19.md` | `60971fe1c0aff216eeb6ee882ff2d840` | 254 行 / 44,298 B | m01228 第 4 项交付。**曾有一份 246 行 / md5 `6129a4b4…` 的旧格式版本，已被 subagent d45d2939 按任务书格式整份替换**；现行版为四大类 31 条、五小标题格式。登记以现行版为准 |

**② 改动清单（每文件改了什么 / 为什么）**

| 文件 | 改了什么 | 为什么 |
|---|---|---|
| `src/diag.rs` | 新增 `pub fn open_log_file(path: &Path) -> std::io::Result<File>`（父目录非空则 `create_dir_all(dir)?` 再 `File::create`）；`init()` 内 `match File::create(&path)` → `match open_log_file(&path)` | Linux 首次启动时 `~/.config/scrcpy-pad/` 尚不存在 ⇒ `File::create` 报 `No such file or directory (os error 2)` ⇒ **唯一一份取证日志整份丢失**。对照 `src/trace.rs:75-76` / `src/settings.rs:420` / `src/instance.rs:184` 都有 `create_dir_all`，只有 diag 漏了 |
| `src/fonts.rs`（新文件） | `pub fn fontconfig_cjk_font() -> Option<PathBuf>`：跑 `fc-list :lang=zh-cn file`，按「优先 sans、排除 Bold/Serif」挑一条 | `install_cjk_font` 的 6 条硬编码候选在多数发行版上落空 ⇒ 界面汉字显示为方块。**关键坑（WSL 实测）**：`fc-match "sans-serif:lang=zh-cn"` 在**没装中文字体**的 24.04 上仍返回 `DejaVuSans.ttf`（不含汉字）⇒ 用它兜底会「成功加载假字体」并吞掉告警，**比不兜底更糟**。故判据只能是 `fc-list :lang=zh-cn`（语义 = 确实声明支持中文） |
| `src/app.rs` | 6 处生产代码里的 `scrcpy.exe` / `adb.exe` 字面量改为 `adb::scrcpy_exe_name()` / `adb::adb_exe_name()`（含两行 UI 标签 `ui.label(format!("{:<11}", …))`，`{:<11}` 保持左对齐等宽占位；顺带删掉「官方 Windows 包里」）；新增守护测试 `executablenames_in_ui_copy_are_platformized` | Linux 上不存在 `.exe` ⇒ 报错文案指错文件、用户照着做也不会成功 |
| `src/fonts.rs::pick_cjk_font` | 加 `#[cfg_attr(not(target_os = "linux"), allow(dead_code))]` | 非 Linux 非测试构建无人调用 ⇒ 否则 clippy 多一条 `dead_code`，破坏 36:36 零增量基线 |

**③ A 级验证表（命令 + 实际输出）**

| 验证 | 命令 | 实际输出 |
|---|---|---|
| 格式门 | `cargo fmt --check` | `FMT_RC=0` |
| 测试门 | `cargo test --locked --bin scrcpy-pad` | `test result: ok. 424 passed; 0 failed; 0 ignored`（419 → **424**：+4 fonts、+1 platformized） |
| clippy 门 | `python ci/clippy_diff.py` | `当前告警对: 36   基线对: 36` / `与基线零差异。` |
| 编译警告 | `cargo check --locked --bin scrcpy-pad` | `Finished dev profile`，0 warning |
| 变异验证（缺陷3 守护测试） | 把 `ui.label(format!("{:<11}", adb::adb_exe_name()))` 改回 `ui.label("adb.exe")` | **红**于 `src/app.rs:22414`，点名违规行 `src/app.rs:13464`；还原 + `touch src/app.rs` 后**复绿** |
| 20.04 交叉构建 | WSL Ubuntu-20.04：`CARGO_TARGET_DIR=…/target-linux-glibc231 cargo build --release --locked` | 1m42s 成功；产物 15,729,280 B；`readelf` 最高 `GLIBC_2.30` ≤ 基线 2.31 |
| 20.04 冒烟（**缺陷1 正面证据**） | `XDG_CONFIG_HOME=/tmp/cfg20 timeout 12 ./scrcpy-pad` | `EXIT=124`（跑满被 timeout 终止）；`~/.config/scrcpy-pad/diagnostics.log` **真的生成**（2311 B，含启动环境快照） |
| 20.04 冒烟（**缺陷3 正面证据**） | 同上，读日志原文 | 逐字出现「未找到 scrcpy,请在左栏指定 **scrcpy** 或它所在的目录」「请将 **adb** 所在目录加入 PATH」；`grep -c 'scrcpy\.exe\|adb\.exe'` 对该日志 = **0** |
| 20.04 冒烟（**缺陷2 正面证据**） | 同上，看 stderr | 无 `[font]` 告警（20.04 装了 Noto CJK，`fc-list :lang=zh-cn file` 返回 4 行） |
| 24.04 **阴性对照** | `CARGO_TARGET_DIR=/tmp/t2404`，`timeout 12` | `EXIT=124` 正常跑满；`fc-list :lang=zh-cn file \| wc -l` = **0** ⇒ stderr 逐字打出 `[font] 未找到中文字体,界面汉字可能显示为方块`；`diagnostics.log` 同时正常生成 ⇒ 兜底**不会在没字体的机器上假装成功** |
| 发布包一致性 | 解包 zip / tar.gz 后 `md5sum` 内层二进制 | 内层与 `release/` 下同名二进制**逐位一致** |
| Windows 冒烟 | `timeout 12 ./release/windows-x86_64/scrcpy-pad.exe` | `EXIT=124` 正常跑满；日志有既有 WARN（键盘钩子探针无回音 ⇒ 钩子自愈，属既有环境行为）；`tasklist \| grep scrcpy` = **NONE** 零残留 |

**④ B 级未验证（需用户在场 / 真机）**

| 项 | 为什么验不了 | 一次运行就能判定的观察点 |
|---|---|---|
| ~~Linux 输入链路（evdev 直读 + `EVIOCGRAB`）从未在真机执行过一次~~ **已于 2026-10-21 销账** | 本行写于 10-19，当时本机与 WSL 都没有 `/dev/input`。**2026-10-21 已在真 Linux 桌面（CachyOS / Wayland / input 组）跑通**：`--selftest` 全 PASS、设备枚举与 `已接管` 日志、`/dev/uinput` 注入器打通整条链路（3004 行轨迹）、`EVIOCGRAB` 外部进程 0 事件 / 396 次 EAGAIN。详见 `文档/Linux真机欠账偿还-2026-10-21.md` §2 | 仍缺的是**真手机 + 真游戏**那一层（见 B 级清单），不是捕获链路本身 |
| Linux 中文字体在**真机上的渲染观感** | WSL 无 GUI，只能证明「字体文件被选中」，不能证明「界面汉字真的不是方块」 | 启动后看左栏标题/按钮中文是否正常（不是方块、不是豆腐块） |
| Wayland vs X11 行为差异 | 需要真实 GNOME/KDE/wlroots 会话 | 同一条 B-1 轮次里切两种会话各跑一次 |
| 手感一致性（帧率/延迟/回中） | 需要真机 + 游戏 | 见 `文档/我一人无法解决的问题-2026-10-19.md` 的 B-1~B-7 |

**⑤ 已知问题（含复现条件）**

| # | 问题 | 复现条件 | 处置 |
|---|---|---|---|
| 1 | **本轮 Windows 冒烟污染了用户真实配置目录**（违反 §3.5 精神） | `timeout 12 ./release/windows-x86_64/scrcpy-pad.exe` 未重定向 `APPDATA` | 后果：`settings.json` 的 `saved_at` 被刷新、`diagnostics.log` 被重写、多出 trace 文件。**用户配置内容本身未变**（`log_level=warn`、`mouse_capture=Mask` 仍在）。**下次 Windows 冒烟必须用 `APPDATA` / `XDG_CONFIG_HOME` 重定向** |
| 2 | ~~Linux 侧系统光标消隐完全没有实现~~ **已部分销账（2026-10-21）** | 现在是**本窗口级**实现：`src/app.rs:7464-7473` 每帧按状态请求 `egui::CursorIcon::None`（egui-winit 转成 `window.set_cursor_visible(false)`）。**仍做不到全局**：Wayland 光标归客户端自己的 surface、X11 要拿到自己窗口的 XID，而 scrcpy 是另一个进程的窗口 —— 这是平台原语边界，不是欠账。`src/capture.rs:1869-1878` 的空函数只为让上层调用点不必分平台 | 开映射进 FPS 看箭头：**本程序窗口内**应消失，**scrcpy 窗口上仍会显示** —— 已在界面文案里明写。全局隐藏的候选解法见妙想清单**第 13 条**（`zwp_pointer_constraints_v1`，未实测） |
| 3 | ~~Linux 侧 `priority::boost` 实际弱于 Windows~~ **已部分销账（2026-10-21/22）** | ① 「很可能完全无效」**是推断，已被真机证伪**：`ps -Lo ni` 实测读线程 nice=-5、引擎线程 nice=-2（本机 `RLIMIT_NICE=31`，普通用户即可降）。② 失败路径已从 `let _ =` 改为 WARN（Linux 10-21，**Windows 10-22 对齐**），带补救命令。**仍然成立的部分**：Linux `setpriority` 是按进程/线程的 nice、需 `RLIMIT_NICE` 或 `CAP_SYS_NICE`，而 Windows `SetThreadPriority` 普通用户可用 ⇒ 权限受限环境里 Linux 仍可能提不上去 | 真机阴性对照已做（`nice -n 10 prlimit --nice=0` ⇒ 3 条 WARN，含 `Permission denied (os error 13)`）。更强的 `SCHED_FIFO` 仍未做（收益未知，风险是死循环锁死整机） |
| 4 | ~~Linux 侧定时精度无实测数字~~ **已销账（2026-10-21）** | 真机实测（CachyOS，自建探针 200 样本）：`recv_timeout(4ms)` p50 **4.056ms** / max 4.062ms、`16ms` → 16.054/16.070ms ⇒ **Linux 没有 Windows 那种 10.2ms 网格**，`src/timer.rs` 非 Windows 空实现的假设首次有数字支撑（Windows 侧对照：不调 `timeBeginPeriod` 时 4ms→**10.245ms**、16ms→**20.479ms**） | 已闭合，无需再测 |
| 5 | 宿主侧 uinput「已明确不做」**在仓库里找不到任何出处** | `grep -rn uinput src/ Cargo.toml` 零命中；文档里的 uinput 全指 HDC 的设备端工具 | 文档缺口，建议下次拍板后补一条书面决定，否则每次会话都会重新讨论 |

**⑥ 环境事实（供下次会话复用）**

- 正式 Linux 发布包**必须用 Ubuntu-20.04 构建**：`release/build.sh:82-95` 检测 `ldd --version` 的 glibc，**基线 2.31**，更老则 `die`（除非设 `SCRCPY_PAD_ALLOW_NEW_GLIBC=1`）。24.04（glibc 2.39）只能用于「代码能否编过 / 跑测试」。
- WSL 两个发行版都已装 Rust **1.99.1**（`~/.cargo/bin`，`rustup default stable` 必须显式设，否则 `could not choose a version of cargo`）。
- WSL 内跑脚本的可用姿势：`wsl -d X -- bash -lc "echo <base64> | base64 -d > /tmp/x.sh && bash /tmp/x.sh"`（直接 `bash /mnt/d/...` 会被 bash 当 Windows 路径解析失败）。
- git-bash 里 `tar -xzf D:/...` 会把 `D:` 当远端主机 ⇒ 必须先 `cd` 到该目录或用相对路径。
- 跨平台编译能力已确认：Windows 侧 `cargo check --target x86_64-unknown-linux-gnu --locked` **能通过**（仅 23 条 unused 警告）；`cargo build` 只在链接期失败（PATH 上的 `cc`/`ld.exe` 是 mingw 版，报 `ld.exe: unrecognized option '--eh-frame-hdr'`）⇒ **不要在 Windows 上直接交叉链接**，走 WSL。

**⑦ 妙想清单换版后的抽样复核（Lead 亲做，2026-10-19 06:2x）**

subagent d45d2939 把 `文档/Linux优势与妙想清单-2026-10-19.md` **整份替换**为 31 条新格式版本（254 行 / md5 `60971fe1c0aff216eeb6ee882ff2d840`）。这不属于并发写违规（它写的是文档、且只写这一个文件；旧版本已由 Lead 审过并留档于此登记），但**换版后必须重新抽样核对行号引用**，否则等于把未经核对的证据写进台账。抽样结果：

| 被引用 | 文档说法 | Lead 实测 | 结论 |
|---|---|---|---|
| `src/capture.rs:1823/:1828/:1833/:1840` | 光标消隐四个 cfg 分支（两个 Windows 实、两个非 Windows 空） | `grep -n set_cursor_visible_from_ui\|hide_cursor_shape_from_ui src/capture.rs` 逐行命中 1823/1828/1833/1840，上下文确认非 Windows 分支是 `{}` | ✅ 属实（**本轮新发现的功能缺口，见 ⑤ 已知问题 #2**） |
| `src/capture.rs:2700` | `let r = if want { device.grab() } else { device.ungrab() };` | 命中，:2699-2702 原文一致 | ✅ |
| `src/capture.rs:2796` | `std::thread::sleep(Duration::from_millis(2))`（WouldBlock 分支） | :2795-2797 原文一致 | ✅ |
| `src/capture.rs:447/463-465` | `bound_key_words` 在非 Windows 恒返回 `[0;4]` | 命中，:462-467 原文一致（注释「没有 vk 概念…恒空位图」） | ✅ |
| `src/settings.rs:105/148` | `mouse_capture` / `clip_cursor_in_aim` 字段位置 | 命中 105 / 148 | ✅ |
| `src/mask/other.rs:10-12` | `available()` 恒 false | 命中 :10-12 | ✅ |
| `src/control.rs:66/71` | `MOVE_BACKLOG_LIMIT=256` / `WRITE_BATCH_LIMIT=64` | 命中 66 / 71 | ✅ |
| 「全仓零命中」六组 | `by-id`/`EVIOCGABS`/`libinput|zwp_`/`sched_setscheduler|SCHED_FIFO`/`journald`/`sync_channel` | 逐组 grep（`src/` + `Cargo.toml`）**全部 = 0** | ✅ 属实 |
| 「`udev|epoll|inotify|eventfd|mlock` 零命中」 | 文档 附:252 这么说 | 实测 **= 2**（两条是注释里的词，非代码） | ⚠️ 该条的「零命中」表述不严格，但**不影响任何结论**（两条命中都在注释里，无实现）。以本行为准 |
| `uinput` | 「宿主 /dev/uinput 零实现，命中全是设备端 UHID 指令」 | `grep -rn uinput src/ Cargo.toml` **= 0 命中**（不是「全是 UHID」——连 UHID 那几处也不含 uinput 字样） | ⚠️ 表述偏差：正确说法是「`uinput` 字面量全仓零命中」，UHID 是另一回事。结论不变 |

**结论**：现行版 31 条的**行号引用抽样全部属实**，可以入账；两处措辞不严格已在上表钉正，不影响任何判定。旧版（246 行）不再作为引用源。
---

### Linux 真机欠账偿还 交付登记（2026-10-21 上午，补登记于 2026-10-22 14:2x）

> **为什么是"补登记"**：这一轮当班时**没有按 §6.8 写独立登记节**，只落了一份
> `文档/Linux真机欠账偿还-2026-10-21.md`（18,075 B，无任何交叉引用）。10-22 审计时把它列为
> 「四个台账缺口」之一 —— 一个没有登记的交付等于**下次会话的未知基线**。本节按 §5 模板补齐。

**任务**：在**真 Linux 桌面**（CachyOS / 内核 7.2.10 / Wayland / 在 `input` 组）上首次实体执行 Linux
输入链路，并按《Linux优势与妙想清单-2026-10-19》逐条偿还本地可做的欠账。

**① 交付物（文件 + md5 + 行数）**

| 文件 | md5 | 行数/大小 | 说明 |
|---|---|---|---|
| `src/capture.rs` | `261e9b1b04730a83f4556c0c1b14d5ff` | 6,792 行 | `poll` 阻塞读 + by-id 身份 + 权限文案 + Linux 光标 |
| `src/app.rs` | `70e7a65e87d366298a11a0244240afc9` | 23,362 行 | 光标消隐每帧请求 + 平台折扣文案 + `grab_enabled` 落盘 |
| `src/settings.rs` | `1656d8b69cb8d59896f1f3fce9ffd2a3` | — | 新增 `grab_enabled: bool`（默认 false） |
| `src/diag.rs` | `bf088b4cd07bfa21235bf25b9f57c6c4` | — | 权限提示改成先给 udev 规则 |
| `src/priority.rs` | `5a6d30e1d83e66a5be070d5455b271ac` | — | `setpriority` 失败改 WARN + 纠正错误推断 |
| `release/60-scrcpy-pad.rules` | `b64e59bf58a9f7f3009af7c91e77140b` | 1,538 B | 新增：`TAG+="uaccess"`，装完即生效不需重登 |
| `release/build.sh` | 见 patch `e98867f2…` | — | 启动器打印两种解法 + 规则文件进包 |

**② 本轮改了什么、为什么**

| 项 | 改了什么 | 为什么 |
|---|---|---|
| 妙想第 19 条 | `device_loop` 读前加 `wait_readable(raw_fd, POLL_TIMEOUT_MS)`（`libc::poll`），删 `WouldBlock` 分支的 `sleep(2ms)` | `sleep(2ms)` 让事件必然晚 0~2ms 才被发现；`poll` 内核一叫就醒 |
| 妙想第 15 条 | `event_paths` 按 by-id 身份排序 + `by_id_map`/`parse_by_id`/`sort_by_identity`；「已接管」日志加 `身份=` | 热插拔换号后 eventN 会变，按路径排序会让设备顺序乱跳 |
| 妙想第 16 条 | udev 规则文件 + `diag` 提示 + 启动器文案 | 加 `input` 组要注销重登，用户常漏 ⇒「程序起来了但按键全无反应」 |
| 妙想第 30 条 | Linux 光标消隐：`app.rs` 每帧 `ctx.set_cursor_icon(CursorIcon::None)` | 平台不给进程级全局原语，只能做到**本窗口**强度，且必须在文案里写明 |
| 妙想第 6 条 | `setpriority` 返回值不再 `let _ =`，失败落 WARN 并给补救办法；注释纠正"很可能完全无效" | 失败静默＝「以为修好了」，比没修更糟 |
| 新欠账 | `Settings::grab_enabled`（`#[serde(default)]`）+ default + round-trip 测试 | 这个开关以前**不落盘**，每次启动重置 ⇒ 第 1 条（EVIOCGRAB）的前提其实从来没成立 |

**③ A 级验证（原样抄自 10-21 文档，均为真机实测）**

| 验证 | 命令/观察点 | 实际输出 |
|---|---|---|
| 自检 | `XDG_CONFIG_HOME=/tmp/cfgtest ./target/debug/scrcpy-pad --selftest` | `[PASS] evdev 读取权限` / `[PASS] 鼠标设备(REL_X/REL_Y) 2 个` / `[FAIL] adb 设备在线 (0 台)`（没插手机，预期） |
| 设备枚举 | `diagnostics.log` | 接管 event3 键盘 / event6 鼠标 / event8 鼠标；**event9 触摸板被正确排除**（ABS、无相对轴） |
| uinput 端到端 | 工作区外注入器建虚拟键盘 1kHz 连打 | 2 秒内「已接管」；注入的 F8 触发总开关；轨迹 **3004 行** 无丢批 |
| EVIOCGRAB 独占 | 独立探针读被独占设备 | `events=0 wouldblock=396`（全 EAGAIN）⇒ 外部进程**收不到**，这是 Windows B21/P1 判死的能力在 Linux 上的正面证据 |
| 测试门 | `cargo test --locked --bin scrcpy-pad` | `408 passed; 0 failed`（**Linux 宿主口径**，见 ④） |
| 稳定性 | 全量连跑 30 次 | **30/30 绿**（修 flaky 前 25 次红 4 次） |
| 变异 1~5 | 见 10-21 文档 §7 | 全部**变红并点名行号**（`wait_readable`/排序/by-id 过滤/超时值/grab 落盘） |
| priority 阳/阴对照 | 默认启动 / `nice -n 10 prlimit --nice=0 -- …` | 阳性 0 条 WARN；阴性 3 条 `Permission denied (os error 13)` |
| 第 19 条收益 | 自建 A/B 压测 6 组×600 样本 | p50 `1.045ms → 0.002ms`（1000µs 档） |
| Linux 定时精度 | 自建探针 200 样本 | `recv_timeout(4ms)` p50 **4.056ms**、16ms → 16.054ms ⇒ **没有 Windows 的 10.2ms 网格** |

**④ 测试数账本（这是缺口 ④ 的答案，也纠正一处误记）**

| 日期 | 宿主 | passed | 说明 |
|---|---|---|---|
| 2026-10-19 | Windows | **424** | 双平台包重建那轮 |
| 2026-10-21 上午 | **Linux（CachyOS）** | **408** | 10-21 文档 §7 记「基线 403 → 408（+4 capture::linux、+1 settings::grab_enabled）」 |
| 2026-10-21 下午 | Linux | **408** | 备份栏位 UI 那轮登记写「与上轮持平」—— **指的就是上午这个 408**，不是 10-19 的 424 |
| 2026-10-22 | Windows | **429** | 本轮终态（见 10-22 登记节） |
| 2026-10-22 | **Linux（WSL 20.04 真跑）** | **413** | 同上 |

**为什么 408 ≠ 424 不是记账错误**：Windows 与 Linux 编译的测试集**本来就不是同一批** ——
`#[cfg(windows)]` 门控的测试在 Linux 宿主上根本不编译（反之亦然，如 `capture::linux` 的 4 条）。
⇒ **测试数必须带宿主标签才可比较**；不标宿主的「与上轮持平」是这次矛盾的根源，已在上表钉正。

**⑤ B 级未验证（真机也证不了的部分）**

| 项 | 为什么 | 一次运行怎么判定 |
|---|---|---|
| 光标消隐在 scrcpy 窗口上是否仍显示 | Wayland 光标归客户端自己的 surface | 开映射进 FPS：**本程序窗口内**应消失，scrcpy 窗口上仍会显示（设计边界，已入文案） |
| 多设备同时 grab 的相互影响 | 只测了单键盘+单鼠标 | 插第二把键盘，两把都开 grab，看是否都能独占 |
| Wayland 下 grab 与合成器是否互扰 | 本机无真实游戏/投屏窗参与 | GNOME/KDE/wlroots 各验一次 |
| uaccess 规则在别的发行版是否同样生效 | 只在本机（CachyOS）验过 | 在 Debian/Ubuntu/Fedora 各装一次规则，看权限是否到位 |

**⑥ 已知问题（含一条**代码没落盘**的严重问题）**

| # | 问题 | 复现条件 | 处置 |
|---|---|---|---|
| **1** | **10-21 文档声称的 `INSTANCE_TEST_LOCK` + `serialize()` 当时并没有进 Windows 树** | `grep -rn 'INSTANCE_TEST_LOCK\|serialize' src/instance.rs` ⇒ **零命中**（10-22 13:0x 实测） | 根因：10-21 这台机器 `/mnt/2T` 是只读挂载，改动只能用补丁交付，而 §6 的补丁清单里**根本没有 `instance.rs` 的 patch** ⇒ 那条改动只活在临时机器上。**10-22 已在真树上重做**（含结构扫描守护测试 + 变异验证，见 10-22 登记节）。教训：以补丁方式交付时，"交付物清单"必须与"声称改了哪些文件"逐条对齐，否则会静默丢改动 |
| 2 | 本机 clippy 门禁跑不完 | `src/engine.rs` 的 `0.7071` 被新版 clippy 判 `approx_constant`（error 级） | 该行在原树逐字相同、非本轮引入；10-22 已在 Windows 宿主上用 `ci/clippy_diff.py` 恢复 36:36 并新增 Linux 基线 |
| 3 | 本轮**未构建发布包** | 按用户指示只做实现-测试-反馈 | 10-21 下午那轮已重建双平台包（见下一节）；10-22 又改动过源码 ⇒ 包需再重建或显式声明过期 |

---

### 备份栏位 UI 两处微调 + 双平台发布包重建（2026-10-21 14:0x–15:3x）

**任务**：用户三条 —— ① LocalDisk 恢复可写，同步代码/文档/数据；② 备份栏"图片名过长把[更换]顶出可视区"+"左栏要能左右拖"，**明确要求极小改动**；③ 按 glibc 2.30 老规矩构建 Linux，并顺带构建 Windows。

**① 交付物（文件 + md5 + 大小）**

| 文件 | md5 | 大小 | 说明 |
|---|---|---|---|
| `src/app.rs` | `70e7a65e87d366298a11a0244240afc9` | 23,362 行 | 两项 UI 微调（见下） |
| `src/capture.rs` | `261e9b1b04730a83f4556c0c1b14d5ff` | 6,792 行 | 删掉上轮遗留的无人读 `LINUX_CURSOR_HIDDEN` |
| `src/settings.rs` / `src/diag.rs` / `src/priority.rs` | 同上轮 | — | 本轮无改动 |
| `release/60-scrcpy-pad.rules` | `b64e59bf58a9f7f3009af7c91e77140b` | 1,538 B | 已随包发布 |
| `release/linux-x86_64/scrcpy-pad` | `47151a8b98eee372ac3320abe417f8ae` | 15,745,792 B | **Ubuntu 20.04 构建，readelf 最高 `GLIBC_2.30`** |
| `release/scrcpy-pad-0.2.0-linux-x86_64.tar.gz` | `698aafdd77eacd3953a84b68138fb719` | 6,837,875 B | 内层二进制与发布目录**逐位一致** |
| `release/windows-x86_64/scrcpy-pad.exe` | `f477ab76986fb143ff1b098f70cd35dc` | 10,552,832 B | mingw-w64 交叉编译（`PE32+ x86-64`） |
| `release/scrcpy-pad-0.2.0-windows-x86_64.zip` | `56943423c789c49b7527ab0e499d6b6c` | 5,010,199 B | 内层 exe 与发布目录**逐位一致** |

**② 改动清单**

| 文件 | 改了什么 | 为什么 |
|---|---|---|
| `src/app.rs`（背景图行） | 文件名标签改为 `add_sized([name_w, h], Label::new(name).truncate())`；`name_w = available_width - reserved`，`reserved` 由 `[更换]/[清除]` 的**实际文字排版宽度**算出（不写死像素） | 用户原话"图片名字过长就截断，导致更换图片的按钮被顶到右边无法找回了"。**光加 `truncate()` 不够**：`horizontal` 默认按内容给宽，标签会先把父布局撑开，截断永远不触发 —— 必须同时给宽度上限 |
| `src/app.rs`（左栏） | `Panel::left("left")` 加 `.resizable(true)`；`min_size` 260→220；`max_size` 430→`(可用宽度 × 0.5).max(430)` | 用户要求"左边状态栏可以被左右拉动"。原来**没写** `resizable`,而且上限 430 拖不太宽。上限取窗口一半，避免把中央画布挤没 |
| `src/capture.rs` | 删掉 `LINUX_CURSOR_HIDDEN` 原子量，两个 Linux 空函数合并去重 | 该量**没有任何读者**（界面用自己的 `cursor_hide_flag`）⇒ Windows 交叉编译报 `static is never used`。少一份会失真又没人读的镜像状态 |

> 用户明确说"这条改动实际上是极其微小的，不要给代码大动干戈" —— 两项 UI 改动合计 **< 40 行**，且都没有新增结构体/配置字段/模块。

**③ A 级验证表（命令 + 实际输出）**

| 验证 | 命令 | 实际输出 |
|---|---|---|
| 格式门 | `cargo fmt --check` | 退出 0 |
| 测试门 | `cargo test --locked --bin scrcpy-pad` | `test result: ok. 408 passed; 0 failed`（**Linux 宿主口径**；与同日上午那轮 408 持平，同宿主可比。**不可与 10-19 Windows 的 424 直接比** —— `#[cfg(windows)]` 门控的测试在 Linux 宿主上不编译，见上一节 ④「测试数账本」。本轮无新增测试 —— 纯 UI 布局改动，见下方"为什么没有新测试"） |
| 构建门 | `cargo build --locked`（Linux 本机）/ `--target x86_64-pc-windows-gnu --release` | `Finished dev profile` / `Finished release profile in 45.69s` |
| **glibc 基线** | Ubuntu 20.04 容器内 `release/build.sh linux` | `GLIBC baseline check passed: max GLIBC_2.30`；`readelf` 实测最高符号 **GLIBC_2.30**（17 个版本符号） |
| **Linux 包实跑** | 20.04 产物在本机（glibc **2.44**）`./scrcpy-pad --selftest` | `[PASS] evdev 读取权限` / `[PASS] 鼠标设备(REL_X/REL_Y) 2 个` / `[PASS] 定位 adb` / `[FAIL] adb 设备在线 (0 台)`（无手机，预期） |
| **Linux 包 GUI 实跑** | 同上，XWayland 会话启动 | 窗口出现、界面正常；**长文件名截断生效**（显示 `这是_一个_极其_漫长_...`，[更换]/[清除] 完整可见）；**左栏可拖**（拖宽后背景图行仍完整） |
| **Windows 包核验** | 解包 zip 后 `md5sum` 内层 exe | `f477ab76…` 与发布目录**逐位一致**；`file` = `PE32+ executable for MS Windows 5.02 (console), x86-64` |
| 包内规则文件 | `tar -tzf …tar.gz` | 含 `linux-x86_64/60-scrcpy-pad.rules`（上轮第 16 条改动已进包） |
| 清理 | `pgrep -af scrcpy-pad` | 空（含一个误起的 Wine 进程，已 pkill） |

**为什么本轮没有新增守护测试**：两项改动都是**布局**（面板尺寸与文字截断），判据是"按钮还在不在可视区里"，无法用纯函数断言——`egui` 的布局结果依赖真实字体度量与窗口宽度。按 §5「UI 新增/修改」矩阵，这类改动要求的是**入口面走查 + GUI 观感登记为 B 级**，本轮已用 X11 `xdotool` + `import` 截图做了**实体走查**（上表第 6 行），无可变异验证的守护点。**如实登记为"无新测试"**，不拿"编译通过"冒充验证。

**④ B 级未验证（需用户肉眼确认）**

| 项 | 为什么本地只能做到这一步 | 一次运行怎么判定 |
|---|---|---|
| 截断后的**观感**（省略号位置、名字可辨认度） | 截图能证明"按钮回来了"，但"看得清是哪个文件"是主观的 | 换一张超长名字的图，看是否一眼认出是哪张；悬停能看全路径 |
| 左栏**拖动上限**是否够用 | 上限设成窗口一半，未在 4K/小窗两端实测 | 在最小窗口（900×600）与最大化各拖一次，看中央画布是否被挤坏 |
| Windows 包**实际能否启动** | 本机只能交叉编译 + `file` 核验，无法运行 PE | 双击 `启动.bat`，看是否出现界面（用户侧 5 秒） |

**⑤ 已知问题**

| # | 问题 | 复现条件 | 处置 |
|---|---|---|---|
| 1 | **clippy 门禁在本机无法归零** | `python ci/clippy_diff.py` 报 59 对 vs 基线 36（Windows 口径） | 多出的 23 对**全部是 Linux 宿主上的 Windows 专属 `dead_code`**（`LBUTTONDOWN`/`MOUSE_HOOK_MSG_CODES`/`keyboard_swallows` 等），不是本轮引入；另有 1 条既有 `approx_constant`（`src/engine.rs:9025` 的 `0.7071`，**原树逐字相同**）。基线只能在 Windows 宿主上对账 |
| 2 | `target-win/`、`target-linux-glibc231/` 遗留 | 本机构建产物 | 已在 `.gitignore` 覆盖范围内；不下发 |
| 3 | Linux 包在 **glibc 2.44** 上跑通 ≠ 在 2.31 上跑通 | 本机没有 2.31 环境 | 产物符号上限 2.30 是**编译期**证据；运行期兼容性需在 20.04/22.04 实跑（用户侧 10 分钟/台） |

**⑥ 环境事实更新（供下次会话复用）**

- **`/mnt/2T` 原为 `ro` 挂载，2026-10-21 已由 Lead `sudo mount -o remount,rw` 恢复可写** ⇒ 上一轮的"只读补丁交付"方式作废，改为直接同步；`同步补丁/` 目录已删除。
- 20.04 构建环境已固化为 **Docker 镜像 `spl-build-2004`**（Ubuntu 20.04 + glibc 2.31 + Rust 1.99.1 + X11/Wayland/GL 开发库）。复现命令：
  ```
  docker run --rm -v "$PWD":/work -v "$HOME/.cargo/registry":/root/.cargo/registry \
    spl-build-2004 bash -c 'cd /work && bash release/build.sh linux'
  ```
  **注意**：`docker run` 里直接 `rustup` 下载会卡在网络（实测两次超时），镜像是把宿主 `~/.rustup` + `~/.cargo` 拷进去构建的。
- Windows 交叉编译链：`mingw-w64-gcc`（pacman）+ `rustup target add x86_64-pc-windows-gnu`；`CARGO_TARGET_DIR=$PWD/target-win`。
- 本机 Wayland 下 `import`/`grim` 截图：`grim` 用于整屏，单个窗口用 `DISPLAY=:1 import -window <id>`（须强制 X11 后端启动才能拿到可截窗口）。
- 测试并行时若出现 `instance::tests` 相关红，先看是否因子进程 fork 复制了别的线程持有的 flock（见上轮登记，已用 `INSTANCE_TEST_LOCK` 串行化）。

---

### 2026-10-22 全轮交付登记：状态总览文档 + 四缺口补齐 + Linux 实测反哺 Windows + 双平台包重建（22:3x 定稿）

> 本轮由用户 m03397 三条指令驱动：① 新建项目状态总览文档；② 根据 Linux 实测完善 Windows 侧、交叉验证、弥补四个台账缺口；③ 此后所有修改走 Windows+Linux 双链路交叉编译（Linux 只跑不编）。

**① 交付物（终态 md5）**

| 文件 | md5 | 行/字节 | 说明 |
|---|---|---:|---|
| `src/app.rs` | `47720999c35f0e3a605c351c245f2a52` | 23,601 行 | D1 `last_serial`、`CONFIG_DIR_ENV`、`clip_changed` cfg 门控、ScrcpyForeground 置灰、Linux 死代码门控 |
| `src/capture.rs` | `3d6a9fac00e2750430fa7273304de9a0` | 6,935 行 | `reduce_by_id` 确定性、`RESCAN_TICKS 20→5`、Drop join 超时 WARN、22 条 Linux warning 门控 |
| `src/instance.rs` | `ac22cd52be1d23e008b0bf5e5c08b909` | 545 行 | `INSTANCE_TEST_LOCK` + `serialize()` + 结构扫描守护测试 |
| `src/priority.rs` | `8cac6ba372e2ffd21108c22a2f65bc69` | 87 行 | Windows `SetThreadPriority` 失败改 `diag_warn!`（与 Linux 对齐） |
| `src/timer.rs` | `05538485c1ee2bfec89c5a39bca27994` | 71 行 | `timeBeginPeriod(1)` 返回值检查 + WARN |
| `src/keymap.rs` | `b3fed9f71e4108a46f0b8c5af3872819` | 5,104 行 | `MouseShieldMode::usable_on(windows)` 纯函数 + 测试 |
| `src/main.rs` | `7049f283cee03f7369c85a114c07d9e9` | 456 行 | `--selftest` 补 Windows 鼠标存在 + Raw Input 相对位移两项 |
| `ci/clippy_diff.py` | `4e6301dbdf2cdbea0e0cd8b9dd7bd15f` | — | 新增 `--target linux` 与 `BASELINE_LINUX` |
| `ci/clippy-baseline-linux.txt` | `2ab53535f540e2e536d98ed195d19496` | 42 行（6 行头注 + 36 对） | **新文件**：Linux 交叉目标专用 clippy 基线 |
| `文档/项目状态与进度总览-2026-10-22.md` | `f8185f0ac450637face00c1e4ca859c8` | 1,193 行 | **新文件**（subagent f79f4ec9 起草，Lead 按终态刷新数字） |
| `文档/Linux真机欠账偿还-2026-10-21.md` | `f7a0cc17ebc9bfdf9074bc1b6d797cc6` | — | §4 补「2026-10-22 更正（补记）」引用块 |
| `release/windows-x86_64/scrcpy-pad.exe` | `212b899992febc91bc81c8e59cfa5b75` | 11,760,640 B | 22:24 本机原生 MSVC 构建 |
| `release/scrcpy-pad-0.2.0-windows-x86_64.zip` | `35d431104090ea2439d0f17470d5fe9d` | 4,983,515 B | 内层 exe 逐位一致 |
| `release/linux-x86_64/scrcpy-pad` | `383813ede2af521d0f4a957ad578d9f1` | 15,751,936 B | 22:27 WSL 20.04 构建，最高符号 GLIBC_2.30 |
| `release/scrcpy-pad-0.2.0-linux-x86_64.tar.gz` | `9013a61636f5d022dc4542ddd5c1c403` | 6,838,479 B | 内层二进制逐位一致 |

**② 改动清单（每条都写「改了什么 / 为什么」）**

| # | 位置 | 改了什么 | 为什么 |
|---|---|---|---|
| 1 | `src/instance.rs` | 新增 `static INSTANCE_TEST_LOCK: Mutex<()>` + `fn serialize()`，四条既有测试各加 `let _g = serialize();`（**必须第一句**，`child_holds_guard` 原先放在 early-return 之后被判违规） | 10-21 台账声称「已用串行锁根治 flaky」，实测 `grep -rn 'INSTANCE_TEST_LOCK\|serialize' src/instance.rs` **零命中** —— 文档写了、代码没落。根因是 `fork+exec` 把进程内所有线程的 fd 复制给子进程，别的测试线程持有的 flock 会到子进程手里 |
| 2 | `src/instance.rs` | 新增守护测试 `every_test_in_this_module_takes_the_serial_lock`：按**结构**扫描（`#[test]` → 函数名 → 第一句有效代码是否含 `serialize()`），含阳/阴对照，属性标记用 `["#","[","test","]"].concat()` 构造 | 行为层 flaky 是概率的（实测 25 次红 4 次），断言「不偶发」永远测不稳；「第一句有没有拿锁」是确定的。**该测试第一次跑就抓到真违规** `["child_holds_guard"]` |
| 3 | `src/priority.rs` | Windows `SetThreadPriority` 由 `let _ =` 改判返回值 + `diag_warn!`（带 `{label}` 与补救说明）；模块头改「失败必须可见」 | 同一缺陷两个平台，Linux 10-21 已改 WARN，Windows 漏了 ⇒ 「以为修好了」比没修更糟 |
| 4 | `src/timer.rs` | `timeBeginPeriod(1)` 返回值检查 + `diag_warn!`；`timeEndPeriod` 失败落 `diag_debug!` | 与 #3 完全同型；模块头自证若未生效 `recv_timeout(4ms)` 实测 10.245ms（~10.2ms 网格）、派遣误差 ±6~10ms |
| 5 | `src/capture.rs` | Windows `Drop` 里等钩子线程 join 的 500ms 超时路径补 `diag_warn!`（原来静默放弃） | 静默失败无法取证 |
| 6 | `src/capture.rs` | `RESCAN_TICKS` 20 → **5**（热插拔重扫 ≈2s → ≈500ms） | Linux 心跳 100ms × 20 太久，Windows 侧消息驱动几乎即时 ⇒ 平台手感差 |
| 7 | `src/capture.rs` | `fn parse_by_id` 抽出纯函数 `fn reduce_by_id(entries: &[(String, String)]) -> HashMap<OsString, String>`，同节点多条 by-id 别名取**字典序最小** | 原实现 `insert` 覆盖写，同一 eventN 的别名取哪条取决于 `read_dir` 顺序 ⇒ 日志里「身份=」每次启动可能变，与「稳定身份」设计目的相反。**Linux 真跑测试第一次就抓红**（期望 `-event-kbd` 拿到 `-hidraw`） |
| 8 | `src/capture.rs` | 22 处 Windows 专属项加 `#[cfg_attr(not(windows), allow(dead_code))]`（`mod wm`、`MOUSE_HOOK_MSG_CODES`、`mouse_shield_swallows`、`mouse_button_bits`、`BoundKeys::contains_vk`、`keyboard_swallows` 等） | Linux 目标 `cargo check` 有 23 条 warning（Windows 侧 0 条）——「Linux 编译链路从来没人当门禁跑过」的直接证据；只压制非 Windows 构建 |
| 9 | `src/app.rs` | 新增 `last_serial: String` + 纯函数 `remembered_serial(current,last)` + `note_current_serial()`；`refresh_devices()` 末尾记一次；`settings_snapshot` 的 `selected_serial` 走兜底 | **D1 真缺陷**：设备列表空时 `fn serial()` 返回 `""` ⇒ 一次刷新就把用户记住的设备冲成空串，读回侧要求非空且在线 ⇒ 永久失效。`selected_serial` 是 23 个设置字段里唯一没有 round-trip 测试的 |
| 10 | `src/app.rs` | 新增 `pub const CONFIG_DIR_ENV: &str = "SCRCPY_PAD_CONFIG_DIR";` + 纯函数 `config_dir_override(Option<&OsStr>)`，`config_dir()` 首句生效 | **Windows 上 `APPDATA` 无效**：`directories` 走 Known Folder API（`SHGetKnownFolderPath`）不读 `APPDATA` ⇒ 先前以为已重定向的冒烟其实一直写在用户真实配置目录。Linux 侧 `XDG_CONFIG_HOME` 有效，故同一纪律两平台可行性不同 |
| 11 | `src/app.rs` | `clip_changed` 的**声明与消费块都加** `#[cfg(windows)]` | Linux 上 `mut` 多余（写它的分支在 Windows 块里）；用 cfg 比 `#[allow(unused_mut)]` 诚实 —— Linux 本来没有「钉住光标」这回事 |
| 12 | `src/keymap.rs` + `src/app.rs` | `MouseShieldMode::usable_on(self, windows: bool) -> bool`（参数显式传而非读 `cfg!`）+ 界面用 `add_enabled_ui` 置灰 + hint 追加平台说明 | `ScrcpyForeground` 靠「投屏窗口是否前台」，而 `foreground_pid()` 非 Windows 恒 `None` ⇒ 该档在 Linux 恒等于从不独占，但界面三档照常可选、hint 一字未提 |
| 13 | `src/main.rs` | `--selftest` 补两项 Windows 检查：`GetSystemMetrics(SM_MOUSEPRESENT)`；真起捕获层等 `raw_probe` 落定后注入 +8/−8 净零位移取证 | Windows 恰恰是 Raw Input 可能降级到「光标差」的平台，而原来只有 Linux 有「鼠标设备 REL_X/REL_Y」检查 |
| 14 | `ci/clippy_diff.py` + `ci/clippy-baseline-linux.txt` | 新增 `--target linux`（认 `--target linux` / `--target=linux` / `native\|windows\|本机`）与第二份基线（36 行，由 linux 目标实测输出生成） | 一份基线管不了两边：Linux 侧独有 `manual implementation of .is_multiple_of()`（`src/capture.rs:3015` / `:3018`）、Windows 侧独有 `this function has too many arguments (8/7)`，数量巧合都是 36 对 |

**③ A 级验证（命令 + 实际输出）**

| 验证 | 命令 | 实际输出 |
|---|---|---|
| 格式门 | `cargo fmt --check` | `FMT_RC=0` |
| 测试门（Windows） | `cargo test --locked --bin scrcpy-pad` | **429 passed / 0 failed** |
| 测试门（Linux 目标真跑） | WSL Ubuntu-20.04 `CARGO_TARGET_DIR=.../target-linux-glibc231 cargo test --locked --bin scrcpy-pad` | **413 passed / 0 failed**（首跑 410 passed / 1 FAILED —— 抓到 `parse_by_id` 真缺陷） |
| 静态门（Windows） | `python ci/clippy_diff.py` | 当前 36 对 / 基线 36 对 / **与基线零差异** |
| 静态门（Linux 目标） | `python ci/clippy_diff.py --target linux` | 目标 linux(x86_64-unknown-linux-gnu) / 当前 36 / 基线 36 / **零差异** |
| 双链路编译 | `cargo check --locked --bin scrcpy-pad` 与 `--target x86_64-unknown-linux-gnu` | 两侧均 Finished，**0 warning / 0 error** |
| 测试数口径闭合 | `cargo test -- --list` + 属性计数 | 严格属性 **435** − 门控 **6**（`mod linux` 内 5 + instance 1）= **429** = 实测 passed，零差额 |
| selftest（Windows 三项） | `SCRCPY_PAD_CONFIG_DIR=/tmp/spl-smoke ./target/debug/scrcpy-pad.exe --selftest` | `[PASS] 键盘捕获` / `[PASS] 鼠标设备(系统报告存在)` / `[PASS] 鼠标相对位移(原始输入)：已接管,收到 1 条 / 5 条原始输入事件`；`[FAIL] 自动寻找 scrcpy` 为本机未配路径的预期 |
| 配置目录重定向端到端 | 同上，检查落盘位置 | trace 与 `diagnostics.log` 全落 `/tmp/spl-smoke`；用户目录 `diagnostics.log` mtime 未被触碰 |
| Windows 发布包冒烟 | `SCRCPY_PAD_CONFIG_DIR=/tmp/spl-smoke3 timeout 10 ./release/windows-x86_64/scrcpy-pad.exe` | `EXIT=124`（被 timeout 终止＝跑满）、`panic\|You can only indent` 计数 **0**、`tasklist` scrcpy 进程 **0** |
| Linux 发布包冒烟 | WSL `XDG_CONFIG_HOME=/tmp/cfg-lsmoke ./release/linux-x86_64/scrcpy-pad --selftest` | 三条 FAIL 均为 WSL 无 `/dev/input`、无 scrcpy 的预期；无 panic；trace 落 `/tmp/cfg-lsmoke/scrcpy-pad/` |
| 包内容逐位核验 | Python 读 zip/tar 算 md5 | zip 内 exe `212b8999…` / tar 内 `383813ed…`，与发布目录**逐位一致** |
| 变异验证（5 条新增守护测试） | 逐条单变异→即还原→复跑 | ① instance 锁删除 ⇒ 红于 `src/instance.rs:487` 点名 `["second_guard_with_same_name_is_rejected"]`；② `reduce_by_id` 改覆盖写 ⇒ 红于 `src/capture.rs:3380`（`同一批条目换个读取顺序结果就变了`）；③ `settings_snapshot` 改回 `self.serial()` ⇒ 红于 `src/app.rs:23491`；④ `usable_on` 的 ScrcpyForeground 改恒 `true` ⇒ 红于 `src/keymap.rs:4051`；⑤ 水平包裹回 `ui_bind_row` 调用点 ⇒ 红于 `src/app.rs:22315` 并点名 `:7590`。全部还原后复绿且 md5 逐位复原 |

**④ B 级未验证（单独列，不混入「已完成」）**

| # | 项 | 为什么本地验不了 | 一次运行就能判定的观察点 |
|---|---|---|---|
| 1 | Linux 真机 GUI / 手感（拖动、瞄准、录制） | 宿主是 Windows，WSL 无输入设备与显示器 | 真机上开一局，看「诊断」页注入轨迹与手感 |
| 2 | Linux 系统光标消隐只做到「本窗口级」 | Wayland 光标归客户端自己 surface、X11 要自己窗口 XID，scrcpy 是另一进程窗口 | 全屏游戏时看系统光标是否仍可见 |
| 3 | `ScrcpyForeground` 档在 Linux 已置灰，但文案是否够清楚 | 属 GUI 观感 | 打开该下拉看置灰与 hint 是否可读 |
| 4 | 键盘钩子探针在部分环境持续无回音（本机冒烟必现 WARN，自愈后停用） | 与本机注入事件被环境吞掉有关，非本轮引入 | 真机上若「按键/视角失灵」告警常驻，需重启程序 |
| 5 | 24 项必须用户在场的验收（善后计划 §A） | 需要真机/第二台机器/用户拍板 | 见 `文档/善后计划-2026-10-18.md` |

**⑤ 已知问题（含复现条件）**

| # | 问题 | 复现条件 | 处置 |
|---|---|---|---|
| 1 | **测试曾污染用户真实配置目录** | 先前 Windows 冒烟用 `APPDATA=...` 以为已重定向，实际无效（`directories` 走 Known Folder API）；`diagnostics.log` 被重写、`settings.json` 的 `saved_at` 被刷新 | 本轮新增 `SCRCPY_PAD_CONFIG_DIR` 并端到端验证；**今后所有冒烟/自检必须设该变量**；用户配置内容本身未变（`log_level=warn`、`mouse_capture=Mask` 仍在），`settings.json` md5 `90c939b2813be5e171f4bee71cf1940b` 未变 |
| 2 | `parse_by_id` 的**假守护测试**教训 | 第一版用真实临时目录构造，变异后仍绿 —— 判据实际依赖 `read_dir` 顺序，保护的是文件系统不是代码 | 已抽纯函数并喂乱序输入，变异必红；登记为 §3.1 的又一实例 |
| 3 | 10-21 文档声称的 `INSTANCE_TEST_LOCK` 当时不在树里 | `grep -rn 'INSTANCE_TEST_LOCK\|serialize' src/instance.rs` 零命中（10-22 11:xx 前） | 本轮重做并补结构扫描守护测试；10-21 文档与协议已加更正块。根因：10-21 机器 `/mnt/2T` 只读、只能补丁交付，而 §6 补丁清单里没有 `instance.rs` |
| 4 | 0.2.0 包与 `release/build.sh` 的耦合 | `build.sh linux` 会顺带执行 `make_archives`，用 WSL 的 `zip` **重打** Windows zip（覆盖 Python 打的产物） | 以最终 `zip` 产物 md5 为准（`35d43110…`）；Python 打的 `7c3a1ba0…` 作废 |
| 5 | 测试数必须带宿主标签 | 同一份代码 Windows 429 / Linux 413 | 已在协议与总览文档双处钉明；今后登记一律写「宿主 + passed」 |

**⑥ 环境事实（供下次会话复用）**

- **双链路口径（用户 m03397 第 3 条）**：Windows 侧跑 `cargo fmt --check` + `cargo test` + `python ci/clippy_diff.py`；Linux 目标跑 `cargo check --target x86_64-unknown-linux-gnu` + `python ci/clippy_diff.py --target linux`；**Linux 二进制只在 WSL Ubuntu-20.04 构建**（Windows 上没有 `x86_64-linux-gnu-gcc` / `zig` / `clang` / `lld`，PATH 上 `cc` 是 mingw 版，链接必失败）。
- **Docker 守护进程本轮未运行** ⇒ 10-21 登记的 `spl-build-2004` 镜像用不了；Linux 构建改走 **WSL Ubuntu-20.04**（cargo 1.99.1 / glibc 2.31 / x11+xkbcommon+wayland pkg-config 齐备，`/mnt/d/...` 可见且 md5 与 Windows 侧一致）。
- **所有冒烟/自检必须设 `SCRCPY_PAD_CONFIG_DIR`**（Windows）或 `XDG_CONFIG_HOME`（Linux），否则会写用户真实配置目录（§3.5）。
- 工具坑：`ops_job_output` 的 job id 跨连接失效（模型切换后报 `This job was not started by this session on the current connection`）⇒ 用前台 `ops_bash` + `timeoutMs`；`tools.edit` 有读取守卫（必须先用 `read` 读过才能 edit）；`tar -xzf D:/...` 在 git-bash 会被当远端主机。

---

### 🚨 事故登记 + 修复交付：配置锁自锁 ⇒ 点 [FPS 功能] 页立刻冻结（2026-10-23 02:2x–03:0x，用户实测）

> 用户原话：（m04792）「程序运行中点击fps页面立刻卡死。」（m04868）「卡死了。立刻。记录是最后一行。」（m05118，修复后）「不卡。」

| 项 | 内容 |
|---|---|
| **现象（签名）** | 点右栏 [FPS 功能] 页后界面**立刻**冻结、任务管理器 CPU **0%**（＝阻塞等待，不是忙循环）；窗口还在、只是再也不重绘。诊断日志 `02:20:05` 之后 app 模块整段静默到 `02:21:29`（84 秒空洞）；注入轨迹里 `in`（引擎收到的输入）与 `aim`（瞄准态心跳，100ms 一条）**都停在 t≈152.4s**，而捕获层 `st` 心跳（200ms 一条，发射点 `src/capture.rs:5454`）**一路写到 t≈216.2s** ⇒ 引擎先停、捕获层还活着；同时 scrcpy 读取线程仍在写日志。 |
| **根因** | `src/app.rs` 的 G-c「钉住光标」状态行：它在 `let mut g = lock_shared(&self.shared);` 打开的块**内部**调了 `self.theme()`，而 `theme()`（`src/app.rs:13175`）＝ `lock_shared(&self.shared).profile.look.theme()` —— **同一把 `std::sync::Mutex`，不可重入** ⇒ UI 线程自己把自己锁死。UI 线程带锁停住后，引擎线程下一次 `lock_shared` 也永久阻塞（引擎生产区唯一阻塞原语 `rx.recv_timeout` 在 `src/engine.rs:4366`，不在任何锁作用域内 ⇒ 只能是等锁）。 |
| **为什么门禁全绿却照样卡** | 430 passed / fmt 0 / clippy 36:36 全绿 —— 该路径只在**真的渲染 [FPS 功能] 页**时才走到，单测不渲染 GUI；而借用检查器只挡 `&mut self`（`self.log` 那种直接编译不过），`self.theme()` / `self.mapper()` / `self.screen_size()` 这类 `&self` 方法**完全静默**。⇒ §0.1 的又一次实证（第一次是 10-18 的 `ui.indent` 启动崩溃）。 |
| **修复** | 口径＝**把要用的值提到加锁之前算好**（与既有先例 `let am = self.mapper();` 同型）。① 守卫块之前加 `#[cfg(windows)] let aim_page_theme = self.theme();`（`src/app.rs:15862-15863`）；② 删掉块内的 `let th_line = self.theme();`，改为把 `&aim_page_theme` 传进 `Self::clip_status_line(...)`（`src/app.rs:16715-16721`）。 |
| **同类先例（仓库里早就写着的警告）** | `src/app.rs:12172`「坐标换算器必须**先**构造好…若先拿到配置锁再调 `self.mapper()` 会自锁,界面会直接卡死」；`src/app.rs:13168` 同义；`src/app.rs:15854-15856`「在加锁之前算好 —— `mapper()` 要读配置,持锁时再取会自锁」。 |
| **全仓持锁区审计（本轮新做）** | 判据：整行恰为 `let [mut] NAME = lock_shared(&self.shared);` 才算持锁作用域（缩进求块尾；`let n = lock_shared(..).x;` 属临时借用，语句末即释放）。生产区实测：app.rs **124** 个持锁区 / engine.rs **17** / control.rs 5 / trace.rs 2 / capture.rs 0 / diag.rs 0 / adb.rs 0。风险 token（再取配置锁、`.lock()`、`self.theme()`/`mapper()`/`screen_size()`、`recv_timeout`、`Command::new`、`.output()`、`.wait()`、`.join()`、`thread::sleep`、`process::exit`）**只命中 4 处**：`src/app.rs:14717 diag_warn!`、`src/engine.rs:4035 trace::aim_state`、`src/engine.rs:4230 diag_warn!`、`src/engine.rs:4445 diag_info!` —— 全是**日志/轨迹叶子汇流**（各自拿内部锁、绝不回调配置锁，不构成 ABBA）⇒ **不存在第二条自锁/交叉死锁路径**。 |
| **新增守护测试 + 变异验证** | `app::tests::no_lock_taking_helper_is_called_while_the_config_lock_is_held`（源码逐行扫描＋阳性/阴性/临时借用三组内嵌对照）。变异：把 `self.theme()` 塞回守卫块 ⇒ **红**并精确报 `(15867, "let _dz_th_mut = self.theme();")`；还原后 `touch src/app.rs` 强制重编 ⇒ 复绿且 md5 逐位复原（§9 mtime 坑未再犯）。 |
| **A 级验证（Lead 亲跑）** | `cargo fmt --check` = `FMT_RC=0`；`cargo test --locked --bin scrcpy-pad` = **430 passed / 0 failed**（429 → 430，+1 即本条守护测试）；`python ci/clippy_diff.py` = 当前 36 / 基线 36 零差异；`cargo check` 0 warning。 |
| **发布包冒烟（已按 §3.5 重定向）** | `SCRCPY_PAD_CONFIG_DIR=$(mktemp -d) timeout 12 ./release/windows-x86_64/scrcpy-pad.exe` ⇒ `EXIT=124`（被 timeout 终止＝跑满）、panic / `You can only indent` 匹配计数 **0**、临时目录内 `diagnostics.log` / `look.json` / `settings.json` / `trace-*.jsonl` 齐全、用户真实 `settings.json` md5 **未变**（`b277b1441e55111f959a260a0325bc`）、退出后 `tasklist` 零残留。 |
| **用户复核（B 级转 A 级的关键一步）** | m05118「不卡。」（启动投屏后再点 [FPS 功能] 页）⇒ 端到端闭合。 |
| **交付物（本轮）** | `src/app.rs` `009cc2edd29bee0b97e27158b572eb5a`（23,738 行）；`release/windows-x86_64/scrcpy-pad.exe` **`a30376a15434866133c80d46c0df4cad`**（11,760,640 B）；`release/scrcpy-pad-0.2.0-windows-x86_64.zip` **`b1a8218429fcbd4ad87cf827ee58c5e8`**（4,954,635 B，内层 exe 逐位一致）。⇒ 上文 10-22 登记的 `212b8999…` / `35d43110…` **已废**。 |
| **全平台包一致性（§6.3 显式声明）** | Linux 包**未同批重建**：`release/linux-x86_64/scrcpy-pad` `383813ede2af521d0f4a957ad578d9f1`、`release/scrcpy-pad-0.2.0-linux-x86_64.tar.gz` `9013a61636f5d022dc4542ddd5c1c403`，仍停留 10-22 22:27 交付。本次两处改动**全部在 `#[cfg(windows)]` 门控内**（非 Windows 目标的编译产物应等效），但**未重编验证** ⇒ 显式列为「未同批重建」。 |
| **测试数账本（宿主口径）** | Windows **429 → 430**（+1 守护测试，0 删除）。Linux 宿主本轮**未重跑**（该测试与平台无关，预期 413 → 414，**未实测**）。 |
| **教训（可执行口径）** | ① **`&self` 方法能悄悄再取锁，借用检查器管不着**：凡内部会 `lock_shared` 的方法（`theme` / `mapper` / `screen_size`），在持锁块里调用一律按自锁处理；② 这类缺陷**只能源码扫描**（§3.2 逐行、禁转义串），已入守护测试；③ **交付必须同时给出 exe 的 md5 与构建时刻，并明确要求「先关掉旧实例」** —— 本次用户测的是 22:25 的旧 exe（修复 02:39 才落盘），一度被判「怎么修不好」，属交付信息缺失（§0.3：别让用户当测试 Oracle）；④ 渲染路径的验收标准是**真实启动 + 真实翻页**，门禁绿不构成证据（与 10-18 `ui.indent` 同型）。 |

---
### 交付登记：0.2.1 —— 组合键/宏「奇妙联动」修复 + 五项用户需求 + 双平台包重建（2026-10-23 04:2x）

> **本节的发布物 md5 已被更晚的「交付登记：0.2.1 追加 —— 日志一键导出 + 日志卡片折叠限行（2026-10-23 21:1x）」取代**；以最新一节为准（exe `3d9241c7…` / zip `ad321b53…` / linux `d48437aa…` / tar `d2d3b489…`）。

**用户口径（m05145：六项 + 交叉编译）**：① 组合键 bug（「Ctrl+Z 设完变成 Ctrl+X」「点完组合键位置突然瞬移」）② 组合键/轨迹录制 UI 太丑太长；轨迹录制快捷键与宏录制**共用一对开始/结束 + 一个启用勾选** ③ 左栏每套组合加轮换勾选 + 一键全勾/全不勾 ④ 启动预设加全屏启动与三四个有价值的 scrcpy 命令 ⑤ README 双轨制 + AGENTS.md 加规则 ⑥ 版本号 0.2.1 ⑦ 交叉编译（Linux）。

**① 交付物 md5（Lead 在冻结树上亲算）**

| 交付物 | 大小 | md5 |
|---|---|---|
| `target/release/scrcpy-pad.exe` | 11,784,192 B | `2f1c01f49d81a1b7cbb6870c895488cb` |
| `release/windows-x86_64/scrcpy-pad.exe` | 11,784,192 B | `2f1c01f49d81a1b7cbb6870c895488cb`（同源） |
| `release/scrcpy-pad-0.2.1-windows-x86_64.zip` | 4,968,438 B | `38c51ff4996fb2dfbead88a5f59c58d2`（内层 exe 逐位一致；`启动.bat` 以 UTF-8 名标志 0x800 存储） |
| `release/linux-x86_64/scrcpy-pad` | 15,768,704 B | `dc6ed4932bfb172c5a99fcd7091afda1`（GLIBC 最高 2.30 ≤ 基线 2.31） |
| `release/scrcpy-pad-0.2.1-linux-x86_64.tar.gz` | 6,820,784 B | `2c275d7d08aba6ac6ef72c7d55cae7e1`（内层逐位一致） |
| `src/app.rs` | 24,707 行 | `61d528e81f4ec8bc4c9b5a17978d867e`（含 20:0x 追加的取证仪表） |
| `src/help.md` | 272 行 | `7f6579771b7d5bc9245839a3ae3d5d92` |
| `README.md`（新用户版） | 609 行 / 50,306 B | `a576bf26f65bb0191c6d804e315214ab` |
| `旧时代完整版Readme.md`（旧 README 存档） | 670 行 | `e0a085245067250543c75a960ab518db` |
| `AGENTS.md` | 195 行 | `5c67967c803865ad217c10def27e0e7c` |
| `Cargo.toml` / `Cargo.lock` | 0.2.1 | `977d45e0c00dbf3144cd34570f677cd2` / `6f57b94ada25442149f073a9d8503b65` |

已删除的过期包（0.2.0 一套，已被 0.2.1 取代）：`scrcpy-pad-0.2.0-windows-x86_64.zip`（`b1a8218429fcbd4ad87cf827ee58c5e8`）、`scrcpy-pad-0.2.0-linux-x86_64.tar.gz`（`9013a61636f5d022dc4542ddd5c1c403`）。

**② 改动清单（每文件改了什么、为什么）**

- `Cargo.toml:3` + `Cargo.lock:2552`：版本 0.2.0 → **0.2.1**（两处必须同步，否则 `--locked` 直接失败）。
- `src/app.rs`（本轮五类）：**(a) item1 互斥与作废** —— 新增 `is_capturing_input()`（waiting_key/waiting_keys/picking/macro_recording 四态唯一判据）、`arm_key_capture()`/`arm_keys_capture()` 两个唯一武装点、`forget_stale_target()`；13 处裸赋值全部改走武装点（自己踩到 replace_all 把 helper 自身也替换成递归调用，已回改）；全局快捷键让路条件改走 `is_capturing_input()`；8 处列表增删/撤销/重做之后收口 `forget_stale_target()`。**(b) 和弦写入** —— `COMBO_CELLS_MAX=4`、`write_chord_into_combo()`、`assign_keys` 的 `KeySlot::ComboKey` 分支、`write_combo_keys()` 修正（slot>0 只改那一格，不再顶掉后面几格）。**(c) 帧内写回** —— `commit_combo_field_edits()`（长度守卫：列表刚变过就连一个字节都不写 + 只写字段不整份覆盖）、`frame_local_take()`（帧首取点快照不许复活已消费的取点）、`ui_combos` 的写回块整体上移到结构性编辑之前。**(d) item2** —— 轨迹录制「启用录制 / 开始 / 结束」一行（与宏录制共用同一套布局）；组合键成员格键钮宽度 110px → 62px。**(e) item3/4** —— `ui_schemes` 每套组合 `[轮换]` 勾选框 + `[轮换全选]`/`[轮换全不选]` + 写回；`StartPreset` 新增 Fullscreen/TurnScreenOff/MaxFps60/AlwaysOnTop/HighBitrate 与 `add_scrcpy_arg()`（只 upsert 自己那条，不覆盖分辨率设置）。
- `src/keymap.rs`：`SwitchKey.rotate_set` 由 `Vec<usize>` 改 **`Option<Vec<usize>>`**（`None`=全部参与=旧配置逐字节一致；`Some([])`=一个都不参与）；新增 `rotate_members()`（引擎与 UI 的**唯一口径**）；`normalize` 三态区分（清洗后为空 ⇒ 置 `None`，与用户主动选空集区别开）。
- `src/engine.rs`：`rotate_target` 改走 `rotate_members`；空集 ⇒ `None`（不再退化成模运算）。
- `src/help.md`：触发配置段改「启用录制 / 开始 / 结束」+ 四态说明；按键组合页加 `[轮换]`/`[轮换全选]`/`[轮换全不选]` 条目；scrcpy 管理段加启动预设五个新项与「只追加自己那一条参数」说明。
- `README.md`：全新用户版（609 行）；本轮把 §8.3 小标题与正文改成新 UI、轮换条目补勾选框说明。
- `旧时代完整版Readme.md`：旧 README 原样存档（同 md5）。`AGENTS.md` §6 新增第 9 条 README 双轨制规则。
- 测试账本：Windows **430 → 440 → 441 passed**（+10：item1 首半 4 条 + 组合键成员 2 条 + 缺陷2 4 条；+1：20:0x 追加的取证仪表守护测试）；Linux（WSL 20.04）**420 → 424 → 425 passed**（+4：Linux 侧不编译 Windows 专属守护测试；+1：同一守护测试在 Linux 侧同样编译运行）。

**③ A 级验证（Lead 亲跑，命令 + 实际输出）**

| 验证 | 命令 | 实际输出 |
|---|---|---|
| Windows 三道门 | `cargo fmt --check` / `cargo test --locked --bin scrcpy-pad` / `python ci/clippy_diff.py` | `FMT_RC=0`；**441 passed / 0 failed**（20:0x 追加后重跑）；与基线零差异（36:36） |
| Linux 链路门禁 | `cargo check --locked --target x86_64-unknown-linux-gnu --bin scrcpy-pad` / `python ci/clippy_diff.py --target linux` | 通过（1.88s）；与基线零差异 |
| Linux 单测 | WSL Ubuntu-20.04 `cargo test --locked --bin scrcpy-pad` | **425 passed / 0 failed**（20:0x 追加后重跑） |
| 变异验证 M1–M11 | 单变异 → 立即还原 → `touch` → 复跑 | 十一次全部变红并点名精确行号（如 `src/app.rs:24561 keys 被整份覆盖了` / `:24680 捕获路径少了原始码日志` / `:24695 "键位已绑定" 那行没带原始码`）；每次还原后 md5 逐位复原并复绿（M1–M9 复原 `2557f9bf…` 复绿 440；M10/M11 复原 `61d528e8…` 复绿 441） |
| Windows 冒烟 | `SCRCPY_PAD_CONFIG_DIR=$(mktemp -d) timeout 12 ./target/release/scrcpy-pad.exe` | `EXIT=124`（跑满被 timeout 终止）；panic/indent 命中 0；临时目录四件套齐全；用户真实 `settings.json` md5 **未变**（`9a783e47526573edbc158df2f94d9bca`）；退出后零残留 |
| Linux 冒烟 | `XDG_CONFIG_HOME=/tmp/cfg21 timeout 12 ./release/linux-x86_64/scrcpy-pad` | `EXIT=124`；panic 0；`diagnostics.log` 2,651 B；`scrcpy.exe|adb.exe` 字面量 **0**；无字体告警（20.04 自带 Noto CJK）；stderr 仅有已知的 nice EPERM 警告 |
| 包核验 | `python zipfile/tarfile` 解包比对 | 两个 0.2.1 包内层二进制与磁盘产物**逐位一致**；Linux 产物 `readelf` 最高 `GLIBC_2.30` |

**④ B 级未验证（须用户在场/真机）**

- item1 的三条残余通路（外部只读审计给出）：(a) 组合键捕获只有全局钩子一条路，焦点不在本窗口时会漏键（egui 回退只在 `waiting_key.is_some()` 时工作）；(b) 第 3 个键只进 `capture_down` 不进 `capture_seen`（设计如此）；(c) 那一刻 OS 是否真报 `VK 0x58` 无法从现有 trace 确证（钩子只读 vkCode/LLKHF_EXTENDED，不落 scanCode/LLKHF_INJECTED）。
- item2 的「不紧不松」观感、item3 勾选框观感、item4 五个新预设的真机效果：均属 GUI 观感 B 级。
- Linux 侧本轮只跑了 12 秒冒烟；evdev 真机输入链路本轮**未**复测（上轮 CachyOS 已测）。

**⑤ 已知问题（含复现条件）**

- 组合键页同一帧内既改字段又删/加成员时：字段写回先落地、结构性编辑照常；长度守卫会保守跳过本轮字段写回并打日志。这是**刻意取舍**（宁可丢一次字段编辑，不可改到邻居身上）。
- 两份外部审计报告 `combo-audit.md`（`527ef0b217fe2efbc1f034a3bd0b7ec2`）/ `combo-audit-2.md`（`4aa59da5a051250e7044dff0237a26cd`）位于工作区外 `D:/GitHub/scrcpy-pad-bench/`，未纳入仓库，仅供追溯。
- Linux 优先级提升仍因缺 `CAP_SYS_NICE` 静默失效（本轮 WSL 冒烟 stderr 逐字复现：`提高线程优先级失败(nice=-2): Permission denied`）。

**⑥ 本轮教训与环境事实**

- `tools.edit` 的 replace_all 会连**目标函数体自身**一起替换（把 `self.waiting_key = Some(slot);` 换成调用自身 ⇒ 无限递归，编译器与三道门都发现不了）；replace_all 前必须先排除目标定义处。
- 变异脚本的替换文本不能用数组 `slice+concat` 拼接（会静默丢行，本轮实测丢掉一行导致测试假红）；一律整段数组 `join`。
- 审计报告的定位会随会话内并发改动漂移（报告基线 `src/app.rs` `029dced3` vs 改后 `2557f9bf`），引用时必须带 grep 锚点。
- 交付给用户时必须同时给 **exe 的 md5 + 构建时刻**并提醒「先关掉旧实例」，否则用户会拿旧二进制判「没修好」（10-23 02:5x 已发生过一次）。

**⑦ 追加（2026-10-23 20:0x）：取证仪表 —— 用户三条回复的落地 + 变异 M10/M11 + 双平台包按新 exe 重建**

**用户三答（m06015，原文）**：「三条仍未验证：1.无法得知。但VK0x58之前碰到过，你不妨开最大警告档位。2.组合键的那个按钮，不是左栏。3,观感先这样了不管了。你先随便验证验证，」

- **第 1 条（无法得知 vt 层真相）⇒ 改为「下次复现必须一眼可判」**：新增 `fn log_captured_key(&mut self, code: u16)`（`src/app.rs` 约 `:4713`，紧跟 `is_capturing_input`），打印 `捕获按键:<键名> (码 <原始 evdev 码>)`；**组合键捕获**（约 `:7141`）与**单键捕获**（约 `:7155`）两条路径各调一次；`assign_keys` 的落定日志从 `键位已绑定: {label}` 改为 `键位已绑定: {label} [码 [44]]`（空集仍打「未绑定」）。⇒ 复现时只要看这两行：**捕获行报 45 ⇒ 键名/映射表一侧有问题；捕获行报 44 而落定行变 45 ⇒ 值在后续步骤被覆盖（undo/写回）**。
- **第 2 条（用户点的是组合键卡片里的成员格，不是左栏）⇒ 正是我已按「和弦捕获」修过的那条路**：`ui_combos` 成员行改走 `begin_combo_member_capture()` → `arm_keys_capture(KeySlot::ComboKey{..})`，落库走 `write_chord_into_combo()`，已有 `combo_member_chord_replaces_from_the_armed_cell_only` 与 `combo_member_cells_are_armed_as_chords_not_single_keys` 两条守护测试（M4/M5 变异验证过）。本轮无需再改该路径，仅新增上面的日志让它可判。
- **第 3 条（观感不管）**：不再追 UI 观感，登记为「用户已裁定」不再列为待办。

**新增守护测试（1 条）**：`captured_keys_are_logged_with_their_raw_code`（`src/app.rs` mod tests 末尾）—— ① 生产区 `self.log_captured_key(code);` 计数 ≥ 2；② `fn_body(src, "fn log_captured_key(")` 必须含 `码 {}` 与 `key_name(code)`；③ 生产区必须含 `[码 {:?}]`；④ 阳性对照写明「不带码的写法过不了 ②」。

**变异验证（本轮新增两条，均「单变异 → 立即还原 → touch → 复跑确认绿」）**

| 编号 | 变异 | 变红位置与消息 | 还原结果 |
|---|---|---|---|
| M10 | 删掉组合键捕获路径那一行 `log_captured_key` | 红于 `src/app.rs:24680`「捕获路径少了原始码日志(组合键与单键各要一条),实际 1 条」 | 还原后 md5 `61d528e81f4ec8bc4c9b5a17978d867e` 逐位复原、复绿 441 |
| M11 | 落定日志去掉 `[码 {:?}]` | 红于 `src/app.rs:24695`「"键位已绑定" 那行没带原始码,复现时无法与捕获行对账」 | 同上 |

**本轮新教训（已写入 ⑥ 同族清单，此处单列最关键一条）**：`tools.edit` 的 `old_string` 若不够长会**命中多处**而抛错，**且抛错时文件停留在变异态**（变异未还原）。本轮 M10/M11 各失败一次：组合键块的 `}` 换行 `} else {` 命中 2 处；`self.log(format!("键位已绑定: {}", keys.label()));` 在 `:4446` 另有一份同形代码。⇒ 还原锚点必须带足够上下文（前后各 2 行且含唯一标识），且**每次变异后必须用 md5 对账**确认真的还原了。

**双平台包重建（按带仪表的新 exe）**：`cargo build --release --locked`（1m06s）⇒ `2f1c01f49d81a1b7cbb6870c895488cb` / 11,784,192 B；WSL `bash release/build.sh linux`（1m00s，GLIBC max 2.30 ≤ 2.31）⇒ windows zip `38c51ff4996fb2dfbead88a5f59c58d2`（4,968,438 B，内层 exe 同 md5）、linux tar.gz `2c275d7d08aba6ac6ef72c7d55cae7e1`（6,820,784 B，内层 `dc6ed493…` / 15,768,704 B）。两个包内 README 均 `a576bf26…`。冒烟：Windows `EXIT=124` / panic 0 / 用户真实 `settings.json` md5 未变 / 零残留；Linux `EXIT=124` / panic 0 / `diagnostics.log` 2,654 B / `scrcpy.exe|adb.exe` 字面量 0（无交互输入故 `捕获按键` 命中 0，符合预期）。

**B 级（仍需用户在场才能判）**：仪表只在「真的按了一次组合键」时才有输出 ⇒ 端到端有效性只能由用户下次复现证明；「组合键捕获焦点不在本窗口时漏键」这一条仍开放。
### 交付登记：0.2.1 追加 —— 日志一键导出 + 日志卡折叠限行（2026-10-23 21:1x）

> 用户 m06113：「更新程序里的控件的功能，可以一键导出日志，现在状态日志覆盖太多，滚动太多。」

#### ① 交付物 md5
| 文件 | md5 | 大小/行数 |
|---|---|---|
| `release/windows-x86_64/scrcpy-pad.exe`（= `target/release/scrcpy-pad.exe`） | `3d9241c73b01cad8e74bd91edd66f4bd` | 11,798,016 B |
| `release/scrcpy-pad-0.2.1-windows-x86_64.zip` | `ad321b536b5d08e1d5381e4ddda6e4e5` | 4,974,049 B（内层 exe 逐位一致，README `a576bf26…`） |
| `release/linux-x86_64/scrcpy-pad` | `d48437aab88a2dd8af5aef2cd65e65be` | 15,778,560 B |
| `release/scrcpy-pad-0.2.1-linux-x86_64.tar.gz` | `d2d3b489a43f127749a098a307dbc06b` | 6,825,251 B（内层 exe 逐位一致，README `a576bf26…`） |
| `src/app.rs` | `e513e03202177f608ed995d5f92ea56d` | 24,981 行 |
| `src/main.rs` | `7049f283cee03f7369c85a114c07d9e9` | 未改（探针补丁从未落盘） |
| `README.md` / `src/help.md` | `a576bf26…` / `7f657977…` | 609 / 272 行 |

#### ② 改动清单（全部在 `src/app.rs`）
- 新增纯函数：`LOG_CARD_ROWS = 12`、`log_export_button()`、`log_copy_all_button()`、`log_card_hint(shown,total)`、`fold_log_rows(rows)`（**只折叠紧邻重复**，顺序本身是证据）、`log_export_file_name(stamp)` = `log-export-{stamp}.txt`、`log_export_body(version,stamp,level,level_source,ui_lines,diag_text)`（两节：界面日志按时间顺序 + 诊断文件全文）。
- `ui_log_list` 增第 4 参 `rows_cap`：卡片传 `LOG_CARD_ROWS`（12 行），浮窗传 `usize::MAX`；重复行显示 `(xN)`。
- `ui_log_card`：标题行加 `[导出日志]`，列表只留 12 行并显示 `log_card_hint`。`ui_log_window`：标题行 `[导出日志]` + `[复制全部]`。
- 新增 `fn export_ui_log(&mut self)`（**导出两份都写**：界面日志缓冲 + 诊断文件全文 —— 只导诊断文件会漏掉被 `log_level` 过滤的 info 行）。
- 新增 `fn write_log_export(dir, stamp, text) -> io::Result<PathBuf>`：把 IO 拆出来，好让单测在临时目录**真写一次**（按钮点击需要 `&mut PadApp`，单测造不出来）。

#### ③ A 级验证（Lead 亲跑，输出已读全）
| 项 | 命令 | 实际输出 |
|---|---|---|
| Windows fmt | `cargo fmt --check` | `FMT_RC=0` |
| Windows 测试 | `cargo test --locked --bin scrcpy-pad` | **442 passed / 0 failed**（441→442，新增 `log_export_and_folded_card_rows_keep_the_panel_readable`） |
| Windows clippy | `python ci/clippy_diff.py` | 与基线零差异（36:36） |
| Linux 检查 | `cargo check --locked --target x86_64-unknown-linux-gnu --bin scrcpy-pad` | Finished dev profile（1.62s） |
| Linux clippy | `python ci/clippy_diff.py --target linux` | 与基线零差异（36:36） |
| Linux 测试 | WSL Ubuntu-20.04 `cargo test --locked --bin scrcpy-pad`（`CARGO_TARGET_DIR=…/target-linux-glibc231`） | **426 passed / 0 failed** |
| Linux 包 | WSL `bash release/build.sh linux` | `GLIBC baseline check passed: max GLIBC_2.30`，两包产出 |
| **真启动冒烟** | `SCRCPY_PAD_CONFIG_DIR=$(mktemp -d) timeout 12 ./release/windows-x86_64/scrcpy-pad.exe` | `SMOKE_EXIT=124`（跑满被 timeout 终止）、`panicked|You can only indent` 匹配 **0**、临时目录内 `diagnostics.log`/`look.json`/`settings.json`/`trace-*.jsonl` 齐全、用户真实 `settings.json` md5 `9a783e47526573edbc158df2f94d9bca` **未变**、退出后 `tasklist` 零残留 |

#### ④ 变异验证（§3.1，单变异→立即还原→touch→复跑绿→md5 逐位复原）
| 变异 | 内容 | 结果 |
|---|---|---|
| M12 | `fold_log_rows` 判等加 `&& false` | 红于 `src/app.rs:24038`「紧邻重复没折对」 |
| M13 | 卡片列表调用改回 `usize::MAX`（第一次**没变红**） | 首次为**假守护**：断言只查函数体是否出现 `LOG_CARD_ROWS`，而提示语那行也含它 ⇒ 收紧为「含 `"log_card"` 的那一行本身必须含 `LOG_CARD_ROWS`」，重跑红于 `src/app.rs:24102`「日志卡的列表调用没用限行数」 |
| M14 | `write_log_export` 里不写盘（`let _ = text;`） | 红于 `src/app.rs:24145`「读回导出文件失败: NotFound」 |

#### ⑤ B 级未验证（需用户在场）
1. **点击 [导出日志] 按钮 → 文件真的出现在诊断目录** 这一段端到端没验过：单测验的是「正文构造 + 写盘 + 文件名」三段，按钮接线靠源码扫描（`ui_log_card` / `ui_log_window` 必须含 `log_export_button()`）。观察点：点一下，日志末尾应出现 `日志已导出: <路径>`。
2. 浮窗里 `[复制全部]` 没真点过（`ui.ctx().copy_text(...)` 一行）。观察点：粘贴到记事本应有 200 行。
3. 折叠后的观感（`(xN)` 后缀、12 行是否够）属 GUI 观感。

#### ⑥ 已知问题
- 折叠只针对**紧邻**重复：顺序本身是证据，禁止全局去重（不同时刻的同一条日志必须分行）。
- `write_atomic` 会自建父目录，因此「目录不存在」不是错误路径；阴性对照改成「父路径是文件」才判得出来（第一版测试就栽在这里）。
- 日志卡只显示 12 行是硬编码 `LOG_CARD_ROWS`，未做用户可调。

### 交付登记：0.2.1 再追加 —— 日志卡「不自动撑高 + 可拖动 + 跟随最新 + 可复制」（2026-10-23 22:0x）

> 用户 m06262：「你这个日志，消息越多，框框涨的越高，把我上面的都挡住了。做的基础不可自动拉伸 + 我手动可调节 + 自动滚动 + 可自如复制有那么难吗，迅速改。」

#### ① 改动（`src/app.rs` + `src/help.md`）
- `ui_log_list`：`ScrollArea` 加 `auto_shrink([false,false])` + 内层 `set_min_height/set_max_height`（高度只由调用方给的 `h` 决定）；每行改 `egui::Label::new(RichText::monospace()).truncate().selectable(true)` ⇒ **一行一条**（长行不再折行把卡片撑高、可选中复制），超 72 字符的行悬停显示全文。
- `ui_log_card`：整个卡片包进 `ui.allocate_ui_with_layout(vec2(w, h), Layout::top_down(...))` 并 `set_min_height(h)/set_max_height(h)` ⇒ **卡片高度被钉死等于底部面板高度**，内容再多也不会往上顶；列表高度 = `h - 58`；标题行新增 **[复制全部]**。
- `log_card_hint` 文案改为「最近 {shown}/{total} 行 · 自动跟随最新 · 拖上沿可改高度 · [复制全部] 整份拷走 · [弹窗查看] 看全部」。
- 手动调高度沿用既有的 `egui::Panel::bottom("left_log_panel").resizable(true)`（`log_height` 默认 170）。
- `src/help.md` 诊断节新增一条 bullet，说明卡片高度、可拖、跟随最新、悬停看全文、复制全部、导出。

#### ② A 级验证
| 项 | 命令 | 输出 |
|---|---|---|
| Windows fmt | `cargo fmt --check` | `FMT_RC=0` |
| Windows 测试 | `cargo test --locked --bin scrcpy-pad` | **442 passed / 0 failed**（未新增测试函数，新断言并入 `log_export_and_folded_card_rows_keep_the_panel_readable`） |
| Windows clippy | `python ci/clippy_diff.py` | 零差异（36:36） |
| Linux 检查/clippy | `cargo check --target x86_64-unknown-linux-gnu` / `clippy_diff.py --target linux` | 通过 / 零差异 |
| 真启动冒烟 | `SCRCPY_PAD_CONFIG_DIR=$(mktemp -d) timeout 12 ./release/…/scrcpy-pad.exe` | `SMOKE_EXIT=124`、panic 匹配 **0**、退出后零残留 |

#### ③ 变异验证（新断言，§3.1）
| 变异 | 内容 | 结果 |
|---|---|---|
| M15 | 去掉行上的 `.truncate()` | 红于 `src/app.rs:24173`「日志行没做单行截断」 |
| M16 | 去掉卡片的 `ui.set_max_height(h)` | 红于 `src/app.rs:24186`「卡片没把自己的高度钉死在面板高度上」 |
| — | 两轮还原后 `src/app.rs` md5 逐位复原 `60e9f1309169f483bfc2cbfe6e03f84b`（25,044 行） | 复绿 442 |

#### ④ 交付物 md5（**取代**上一节的 0.2.1 包）
| 文件 | md5 | 大小 |
|---|---|---|
| `release/windows-x86_64/scrcpy-pad.exe`（= `target/release/`） | `7d4b5f0cdd5d2ef61a522a0869f1d513` | 11,803,136 B |
| `release/scrcpy-pad-0.2.1-windows-x86_64.zip` | `0329e5ade8a3acb4e9b2008844fc4376` | 4,972,625 B（内层 exe 逐位一致） |
| `release/linux-x86_64/scrcpy-pad` | `c1d8bdf43ebf27a0734515ff26cf8545` | 15,782,528 B |
| `release/scrcpy-pad-0.2.1-linux-x86_64.tar.gz` | `346c1f3c9d725115cb7986b2f4f660f2` | 6,823,761 B（内层 exe 逐位一致） |
| `src/help.md` | `b5100f8e0d7dee5998db9ccf85572d74` | 273 行 |

#### ⑤ B 级未验证（需用户在场）
1. 拖动上沿改高度的**手感**，以及拖动后是否在下次启动保持（取决于 egui 内存持久化）。
2. 长行悬停显示全文、行选中复制的实际操作体验。
3. 「自动跟随最新」在用户往上翻看历史时的取巧行为（`stick_to_bottom(true)` 只在已贴底时跟随，这是刻意选择）。

#### ⑥ 已知问题
- 卡片固定显示 `LOG_CARD_ROWS = 12` 行，未做用户可调行数（改高度只是看更多未折叠行）。
- 行被 `truncate()` 截断后，选中复制拿到的是**屏幕上的截断文本**；要完整原文请用 [复制全部] 或 [导出日志]。

### 交付登记：日志卡「空日志自动上升」修正（2026-10-23 22:2x，用户实测报障）

> 用户 m06448：「你个蠢货，现在日志栏什么都没有的情况下自动上升，拉一下上升一下，什么鬼玩意」。

#### 根因（棘轮反馈环）
上一版为了"卡片不随内容涨"用了 `ui.allocate_ui_with_layout(vec2(w, h))` + `set_min_height(h)`，而 `h` 取自**面板内部**的 `ui.available_height()`；卡片的最小高度又把底部面板撑大 ⇒ 下一帧 `available_height` 更大 ⇒ 卡片更大 ⇒ **一帧比一帧往上长**（空日志也一样，拖一下立刻又涨一截）。方向错了：高度必须**面板 → 卡片**单向传递。

#### 修复
- 调用点（`src/app.rs:7566` 区）：`.show(ui, |ui| self.ui_log_card(ui))`，随后 `self.log_height = log_resp.response.rect.height();` —— 面板实际高度**回写**成下一帧的内容上限；注释写明「方向只能是面板 → 卡片」。
- `ui_log_card(&mut self, ui)`：去掉 `allocate_ui_with_layout` / `set_min_height` / `set_max_height`，去掉 `max_h` 形参；列表高度 = `(self.log_height - 58.0).max(40.0)`。
- `ui_log_list`：去掉内层 `set_min_height/set_max_height`，只留 `auto_shrink([false,false]) + max_height(h)`。
- 保留上一版的：行 `truncate()` 单行、`selectable(true)`、悬停看全文、`stick_to_bottom(true)`、标题行 `[复制全部]`。

#### A 级验证
| 项 | 命令 | 输出 |
|---|---|---|
| fmt | `cargo fmt --check` | `FMT_RC=0` |
| Windows 测试 | `cargo test --locked --bin scrcpy-pad` | **442 passed / 0 failed** |
| clippy（双目标） | `python ci/clippy_diff.py` / `--target linux` | 均「与基线零差异」 |
| Linux 检查 | `cargo check --target x86_64-unknown-linux-gnu` | Finished dev profile |
| 真启动冒烟 | `SCRCPY_PAD_CONFIG_DIR=$(mktemp -d) timeout 12 ./release/…/scrcpy-pad.exe` | `SMOKE=124`、panic 匹配 **0**、零残留 |

#### 变异验证
| 变异 | 内容 | 结果 |
|---|---|---|
| M17 | 卡片列表高度改回 `ui.available_height() - 58.0` | 红于 `src/app.rs:24184`「卡片高度不能取自 available_height —— 会和底部面板互相撑高(空日志也会自动上升)」；还原后 md5 逐位复原 `527e38f5e208b6703e88667b6e0641fa`，复绿 442 |

#### 交付物 md5（取代 22:0x 那一版）
| 文件 | md5 | 大小 |
|---|---|---|
| `release/windows-x86_64/scrcpy-pad.exe`（= `target/release/`） | `ca249dfb6e2981e9533c6368e2226dd5` | 11,801,600 B |
| `release/scrcpy-pad-0.2.1-windows-x86_64.zip` | `db60a95b767eb4b134015baa86c3e4db` | 4,972,526 B（内层 exe 逐位一致） |
| `release/linux-x86_64/scrcpy-pad` | `a55595df780851271df945fd4920370a` | 15,781,760 B |
| `release/scrcpy-pad-0.2.1-linux-x86_64.tar.gz` | `e469f896565b6df617871a4aeb4655ef` | 6,823,654 B（内层 exe 逐位一致） |
| `src/app.rs` | `527e38f5e208b6703e88667b6e0641fa` | 25,048 行 |

#### B 级未验证
1. 真实窗口里拖上沿后是否**立刻稳定**（不再一帧一涨）—— 结构上已消除反馈环，但拖拽手感只能你实机看。
2. 若上一次运行已把面板记忆成大高度，本次启动可能先显得偏高（egui 记忆残留）；拖一次即固定。

#### 教训（写进纪律）
「给容器设最小高度」前必须先问：这个数字是否来自**容器自己**的可用空间。若是，就是反馈环；容器尺寸只能由**外部来源**（父布局/用户拖拽/固定常量）决定，再由它派生内容高度。

### 交付登记：日志卡「空日志自动上升」**第二次修正** —— 「回写面板高度」是伪修复（2026-10-23 22:2x–22:4x，用户两次报障）

> 用户 m06448：「现在日志栏什么都没有的情况下自动上升，拉一下上升一下」；上一节修完（回写 `log_resp.response.rect.height()`）用户 m06489：「根本没修好，还是一打开，日志就往上窜」。

#### 为什么上一版必然无效（读 egui 源码定死）
`egui-0.36.1/src/containers/panel.rs`：
- `outer_size()` 取自上一帧存的 `PanelState`；
- 帧尾 `PanelState { outer_rect: shifted_outer_rect }.store(..)`，其中 `shifted_outer_rect = inner_response.response.rect` = **`Frame::show` 返回的内容矩形**。

⇒ 底部面板的尺寸**就是上一帧内容的高度**。所以「面板 → 卡片」的回写同样成环：卡片内容（细条+标题+列表+提示）比 `log_height` 高一点点 ⇒ 面板变大 ⇒ 回写更大 ⇒ 内容再大。上一版 `list_h = (self.log_height - 58.0)` 正是这种成分：58 < 真实装饰高度，差值就是每帧涨幅。

#### 这次的做法：三个量钉成相等，反馈链剪断
- 卡片高度**只**由 `self.log_height` 决定（新语义：用户拖出来的固定高度）；调用点不再读任何 `available_height`，`self.log_height = rect.height()` **整行删除**。
- 面板侧（`src/app.rs` 左栏调用点）：`Panel::bottom("left_log_panel").resizable(false).min_size(card_h).max_size(card_h)` ⇒ `outer_size` 被 `clamp_to_range` 夹成 `card_h`，与内容无关。
- 卡片侧 `fn ui_log_card(&mut self, ui, card_h: f32)`：`allocate_ui_with_layout(vec2(w, card_h))` + `set_min_height(card_h)` + `set_max_height(card_h)` ⇒ 内容矩形恰好 `card_h` ⇒ 面板存盘值恒为 `card_h`。
- 新常量：`Self::LOG_CARD_MIN_H = 96.0`、`Self::LOG_CARD_GRIP_H = 8.0`、`Self::LOG_CARD_CHROME_H = 100.0`（装饰余量必须**大于**真实装饰高度，否则又成环）；`list_h = (card_h - LOG_CARD_CHROME_H).max(40.0)`。
- 手动可调：卡片上沿 8px 拖动条（`Sense::click_and_drag()` + `CursorIcon::ResizeVertical`），`self.log_height = (self.log_height - grip.drag_delta().y).clamp(Self::LOG_CARD_MIN_H, 560.0)`。

#### A 级验证 —— 新仪表：headless 连跑 5 帧量面板高度（阴阳对照）
`app::tests::pinned_bottom_panel_height_does_not_drift_across_frames`：`egui::Context::default()` + `ctx.run_ui(RawInput{screen_rect:1200×800}, …)` 连跑 5 帧，直接量 `Panel` 返回的 `response.rect.height()`；每帧 `out.textures_delta.clear()`（headless 无人消费纹理增量，不 clear 会在 drop 时 panic）。

| 路 | 写法 | 断言 |
|---|---|---|
| 阳性对照 | `default_size(150)` + 内容高度 = 面板高度 + 20 | 逐帧上涨（`w[1] > w[0] + 1.0` 成立）——证明本仪表量得到棘轮 |
| 阴性对照（现写法） | `min_size == max_size == 150` + 内容按固定 150 分配 | 5 帧高度全等（`(h-first).abs() < 0.5`）且 ≈150 |

> 这是本项目**第一条能量化"棘轮"的仪表**：前两次修复只有读码推断 + 用户实测，各消耗一轮真机信任。

#### 变异验证（M18–M24 全部按预期变红，逐位复原）
| 变异 | 内容 | 结果 |
|---|---|---|
| M18 | 卡片内 `let card_h = ui.available_height();` | 红「卡片高度不能取自 available_height」 |
| M19 | `.max_size(card_h)` → `.max_size(card_h * 2.0)` | 红「底部日志面板没被钉成固定高度」 |
| M20 | `ui.set_max_height(card_h)` → `* 1.5` | 红「日志卡没把自己的高度封在 card_h」 |
| M21 | `self.ui_log_card(ui, card_h)` → `(ui, ui.available_height())` | 红「日志卡没拿到固定高度参数」 |
| M22 | `.resizable(false)` → `true` | 红「面板还开着自带拖拽」 |
| M23 | `if grip.dragged() {` → `if false {` | 红「拖动手柄没真的改高度」 |
| M24 | 拖动分支 `self.log_height =` → `let _unused =` | 红（同上） |

**M19/M21 首轮是假绿**：`include_str!("app.rs")` 把测试自己那几行期望字符串也带进来，`src.contains(".max_size(card_h)")` 被自满足 ⇒ 改为「切出 `Panel::bottom("left_log_panel")` 到 `});` 之间的**调用点切片**再断言」。M23 同类：`grip.hovered() || grip.dragged()` 那行会满足 `contains("grip.dragged()")` ⇒ 改为取 `if grip.dragged()` 之后的**分支体**，要求里面真的出现 `self.log_height =`。

#### 门禁（Windows 宿主 + Linux 目标）
| 项 | 命令 | 输出 |
|---|---|---|
| fmt | `cargo fmt --check` | `FMT_RC=0` |
| Windows 测试 | `cargo test --locked --bin scrcpy-pad` | **443 passed / 0 failed**（442→443，+1 棘轮仪表） |
| clippy | `python ci/clippy_diff.py` | 与基线零差异（36:36） |
| Linux 检查 | `cargo check --locked --target x86_64-unknown-linux-gnu` | Finished dev profile |
| Linux clippy | `python ci/clippy_diff.py --target linux` | 与基线零差异 |

#### 交付物 md5（新 exe 已放进发布目录，取代 22:2x 版）
| 文件 | md5 | 大小 |
|---|---|---|
| `release/windows-x86_64/scrcpy-pad.exe`（= `target/release/`） | `badb7017a44842d87f93e6d6e09829a9` | 11,803,648 B |
| `release/scrcpy-pad-0.2.1-windows-x86_64.zip` | `2a377f73d8f80e590f4ffb7519dc9d8c` | 4,973,779 B（内层 exe 逐位一致） |
| `release/linux-x86_64/scrcpy-pad`（WSL Ubuntu-20.04 重建） | `d5635188d52362a1cf5686f96a233289` | 15,783,040 B |
| `release/scrcpy-pad-0.2.1-linux-x86_64.tar.gz` | `1297b5d9fa6a376d232535fbbee4bd95` | 6,823,854 B（内层逐位一致） |
| `src/app.rs` | `f18970fa19f6decf3cd94825b8a72981` | 25,194 行 |

#### B 级未验证（需用户在场，1 分钟）
1. 重新启动后**空日志时**面板高度应**纹丝不动**（放着不动看 10 秒即可判定）。
2. 拖卡片上沿 8px 细条：上拖变高、下拖变矮；松手后停住，不再自动长。
3. 若上次运行已把高度记成大值，本次可能一开始偏高 —— 拖一次即固定（egui 记忆残留，不是棘轮）。

#### 教训（补进纪律）
「回写容器实际高度」是**伪修复**：当容器实际高度本身由内容决定时，回写只是把内容高度改名成状态再喂回去。判据：这个数字的**来源**是否最终来自内容/可用空间 —— 若是，必须换成**外部固定值**（常量或用户拖拽），并令 `min == max == 该值`、内容按该值精确分配。


## 9. 交付前自检清单（逐项打勾才能交付）

- [ ] 每条承诺已登记并销账；未销账的已如实列出
- [ ] 每条完成性声明已分 A/B 级；B 级单独列、不混入"已完成"
- [ ] 本会话新机制依赖的外部假设，全部已实测（假设登记册无"未测"承重项）
- [ ] 所有验证命令与生产代码逐字同形
- [ ] 跨模块的缝有接线层测试；真机原文输出有 golden test
- [ ] debug 连续两轮无效的问题，已先建仪表再继续修
- [ ] 已知问题（含复现条件）已诚实列出
- [ ] 交付物：文件 + md5 + 改动清单 + A 级验证输出 + B 级验收观察点，四件齐全

---

## 10. 团队协作纪律（Lead 与队友）

- **队友自报的数字一律不信**：Lead 在冻结树上亲自复跑 fmt/test/clippy，并审 diff。
- **任务下发必须带验收观察点**（不只是写范围）：队友完成的标准是"观察点可见"，不是"代码写完了"。
- **队友中途失败/掉线**：立即冻结工作树，Lead 审计全部落盘改动（尤其越界改动）后再继续。
- **并发纪律**：多个 agent 不得同时跑 `cargo test`（会互相抢锁产生假红灯）；测试与生产锁名必须隔离。
