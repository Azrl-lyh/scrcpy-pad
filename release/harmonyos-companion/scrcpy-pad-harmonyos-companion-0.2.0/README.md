# scrcpy-pad HarmonyOS 6+ 手机端辅助应用

本目录是 **手机端辅助应用**，不是电脑端主程序。它面向 HarmonyOS 6.0.2 / API 22 及以上，适合手机连接蓝牙/USB 键盘后，在手机本地把按键映射为触摸。电脑通过 HDC 控制手机的主产品线位于 `harmonyos-pc/`。

## 当前实现

- Stage 模型、ArkTS/ArkUI、`phone` 设备类型。
- 无障碍扩展 `ScrcpyPadAccessibilityAbility`，声明 `gesture` 与 `keyEventObserver` 能力。
- 无障碍扩展通过 `onKeyEvent` 接收实体键，通过异步 `injectGesture` 注入触摸路径。
- 已迁移轮盘算法：
  - `经典（标准）`：同轴反向同时按下时抵消。
  - `灵敏（后按覆盖）`：同轴最后按下的方向覆盖；覆盖键先抬起后，较早仍按住的键自动恢复。
  - 左右轴与上下轴独立，斜向不会互相抵消。
- 已迁移引擎稳定性原则：
  - `Held` 保存物理按键镜像。
  - 配置、临时轮盘、FPS 模式切换后按当前物理状态全量对账。
  - 忽略按键自动重复产生的“伪上升沿”。
  - 漏抬事件会在下一次权威按键快照到达时被清理。
  - 无障碍服务断开时释放全部活动手势。
- FPS 配置已具备独立开关、独立开关键、临时退出键和“进入 FPS 隐藏指针”配置，不依赖映射总开关。
- `fpsOnly` 键位模型已保留；FPS 退出时自动停止 FPS 专用键。
- 手机端可新增/删除点按绑定，并标记“仅 FPS”；绑定和开关配置写入 HarmonyOS Preferences，重启保留。
- Preferences 使用跨进程变更监听；UIAbility 与无障碍扩展即使不在同一 ArkTS 进程，运行中的配置也会同步。
- 无障碍手势注入串行排队，避免长按窗口重入把系统手势注入搅乱。
- 页面内置 `核心自检`，覆盖自动重复、FPS 独立开关、灵敏轮盘覆盖/恢复和漏 KeyRelease 自愈。

## 构建

推荐直接用 DevEco Studio 6.0.2 打开本目录。

命令行构建（Windows PowerShell）已在本机验证：

```powershell
$env:DEVECO_SDK_HOME = 'C:\Huawei\DevEco Studio\sdk'
$env:JAVA_HOME = 'C:\Huawei\DevEco Studio\jbr'
$env:NODE_HOME = 'C:\Huawei\DevEco Studio\tools\node'
$env:Path = "C:\Huawei\DevEco Studio\jbr\bin;C:\Huawei\DevEco Studio\tools\node;C:\Huawei\DevEco Studio\tools\ohpm\bin;C:\Huawei\DevEco Studio\tools\hvigor\bin;$env:Path"

& 'C:\Huawei\DevEco Studio\tools\hvigor\bin\hvigorw.bat' `
  --mode module `
  -p product=default `
  -p module=entry@default `
  assembleHap `
  --no-daemon
```

也可以直接运行仓库内脚本：

```powershell
.\build.ps1
.\build.ps1 -Mode release
```

未配置签名时产物为：

`entry/build/default/outputs/default/entry-default-unsigned.hap`

安装真机前需在 DevEco Studio 中配置自动签名，或使用已有 HarmonyOS 6+ 签名资料。

## 首次使用

1. 安装并打开应用。
2. 打开系统 `设置 > 辅助功能`。
3. 启用 `scrcpy-pad 键位引擎`。
4. 返回应用，确认页面显示“无障碍扩展已连接”。
5. 用映射总开关或默认 `F8` 启停普通映射；FPS 模式可绑定自己的开关和临时退出键。
6. 排查外部鼠标/键盘时，可在 DevEco Log 中过滤 `scrcpy-pad`；每条物理键事件会打印 `code/action/pressed`，可直接确认设备实际上报的鼠标键码和滚轮码。

## 常用 HarmonyOS 键码

| 按键 | 键码 |
|---|---:|
| A | 2017 |
| D | 2020 |
| K | 2027 |
| S | 2035 |
| U | 2037 |
| W | 2039 |
| Space | 2050 |
| Enter | 2054 |
| F8 | 2097 |
| F9 | 2098 |
| 方向键上/下/左/右 | 2012 / 2013 / 2014 / 2015 |
| 滚轮上/下 | 2638 / 2639 |

## 平台边界

普通第三方 HarmonyOS 应用不能申请系统级输入注入权限。当前分支使用的是用户授权后的无障碍手势注入，因此：

- 只保证单指 `GesturePath` 注入。
- `GesturePath` 是完整手势，系统不提供公开的独立 DOWN/MOVE/UP 接口；无限时长长按由短时手势窗口循环维持，不同游戏的手感可能存在差异。
- 真正的多指同时注入、全局悬浮控件和任意应用上层的系统光标控制需要系统级权限，普通应用无法完整复刻桌面版。
- 鼠标按键是否能进入 `AccessibilityExtensionAbility.onKeyEvent` 取决于 HarmonyOS 6 设备与输入设备实现，需要在目标真机上验证。
- 当前第一版先打通键事件、轮盘/FPS 状态机和触摸注入链路；鼠标相对位移与完整布局编辑器将在真机验证后继续迁移；滚轮若被设备以普通键事件上报，可直接填写键码进行映射。

## 目录

```text
harmonyos/
  AppScope/
  entry/src/main/ets/
    accessibilityability/  无障碍扩展入口
    engine/                映射引擎与手势接口
    model/                 配置、动作、轮盘和 FPS 模型
    pages/                 手机端控制台
    service/               无障碍桥接与手势注入适配器
```

记录时间：2026-09-28 11:52:24 +0800
