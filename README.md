本项目基于scrcpy，是我用AI生成的，为了实现类似模拟器的功能，可以添加键位和轮盘操控手机。主要是因为目前没有好用的基于scrcpy的相关项目，所以我做了他。
我很乐意分享它，也欢迎大家前来修改，完善代码。

# scrcpy-pad

一个基于 [scrcpy](https://github.com/Genymobile/scrcpy) 的键鼠映射游戏控制台 —— 通过键盘控制手机的rust程序。

在电脑上给任意键位绑定手机屏幕坐标，用键盘操控手机游戏：虚拟摇杆、点按、长按、滑动，样样都有。

```
键盘 WASD ──► ┌─────────────┐      ┌──────────────┐      ┌──────────┐
              │  scrcpy-pad │ ───► │ scrcpy-server│ ───► │   手机    │
其它按键 ──►  │  (映射引擎)  │ TCP  │ (仅控制通道)  │ adb  │  游戏画面 │
              └─────────────┘      └──────────────┘      └──────────┘
                                      视频由 scrcpy 本体窗口显示,互不干扰
```

## 功能

- **点按**：按键 → 点击一次（单击技能、按钮）
- **长按**：按住多久，手机触点就按多久，实时跟随（蓄力、持续触发）
- **滑动**：按键 → 沿自定义轨迹滑动一次（技能滑动、翻页）
- **轮盘（虚拟摇杆）**：四个方向键（默认 WASD）驱动一个虚拟摇杆，支持八方向，松开自动回中
- **系统键**：注入 Android 返回（4）/主页（3）等
- **截图取点**：截取手机屏幕，点击图像即可完成坐标设置
- **键位浮层**：所有键位、摇杆以圆圈/轨迹形式叠加显示在手机截图上，一目了然
- **长短按一键切换**：单按钮互转，坐标保留
- **总开关键**（默认 F8）：随时开关映射，关闭时自动释放所有触点，不会卡键
- 多点触控支持：轮盘与按键各有独立触点，可同时进行

## 环境要求

| 依赖 | 说明 |
|---|---|
| [Rust](https://rustup.rs) | 编译用 |
| adb | 系统包管理器安装（`dnf install android-tools` / `choco install adb`） |
| [scrcpy](https://github.com/Genymobile/scrcpy) | 提供 `scrcpy-server` 文件与画面窗口 |
| 手机 | 已开启 USB 调试 |

**Linux 额外要求**（全局键盘捕获需要读 `/dev/input` 权限）：

```bash
sudo usermod -aG input $USER
# 然后注销重新登录(必须!)
```

Windows 无需任何特殊权限。

## 编译

```bash
git clone https://github.com/你的用户名/scrcpy-pad.git
cd scrcpy-pad
cargo build --release
```

产物：`target/release/scrcpy-pad`（Linux）/ `target/release/scrcpy-pad.exe`（Windows）。

之后直接运行项目根目录的 `启动.sh`（Linux）/ `启动.bat`（Windows）即可。

## 使用

1. 手机开启 USB 调试，连接电脑
2. 启动 scrcpy-pad，顶栏下拉选择设备
3. 点 **连接控制**（建立注入通道，显示绿点）
4. 点 **启动 scrcpy**（弹出手机画面窗口）
5. 配置键位（见下）
6. 按 **F8** 开打；再按 **F8** 收工

### 配置键位

1. **截取手机屏幕** → 游戏画面进入程序
2. **轮盘**：四个方向按钮逐个点选后按物理键（默认 WASD）；**取圆心** → 点击截图中游戏摇杆中心；拖动调整半径
3. **按键**：新增行选类型（点按/长按/滑动/系统键）→ 点"未绑定"按物理键 → **取点** → 点击截图上技能图标 → **添加**
4. 所有配置实时显示在截图浮层上（绿圈=点按，橙圈=长按，蓝线=滑动，青圈=摇杆）
5. **保存配置**（存于 `~/.config/scrcpy-pad/profile.json`，跨平台通用）

### Linux：建议开启"映射时屏蔽原键(grab)"

勾选后映射开启期间键盘只对本程序生效，避免游戏时误触发系统输入。
Windows 上此选项暂不生效。

## 工作原理

- 启动一个 **control-only** 的 scrcpy-server（`video=false audio=false control=true`，独立 scid），只作触控注入通道，与 scrcpy 画面窗口互不干扰
- 直接实现 scrcpy 控制协议（触控消息 32 字节大端序列化），延迟与官方客户端同级（事件驱动，全程 <10ms）
- 全局键盘捕获：Linux 走 evdev（`/dev/input`），Windows 走 rdev 低级钩子
- 键码在内部统一为 Linux evdev 码空间，配置文件两平台通用

## 自检

无界面环境快速验证全链路（权限 → adb → 控制通道 → 协议注入，注入使用无害的 hover 事件）：

```bash
./target/release/scrcpy-pad --selftest
```

## 常见问题

**Q：提示"输入捕获不可用 / 无权限"**
A：`sudo usermod -aG input $USER` 后没有重新登录。注销重登，或用 `sg input -c './启动.sh'` 临时借组运行。

**Q：连接控制失败**
A：确认 adb 能看到设备（`adb devices`），且 scrcpy-server 路径存在（Linux 默认 `/usr/share/scrcpy/scrcpy-server`；Windows 默认与本程序同目录，名字为 `scrcpy-server`，可在界面左栏修改）。

**Q：开了映射但按键没反应**
A：按总开关键（F8）确认状态为"开"；确认配置里该键已绑定；Linux 下确认程序以 input 组权限运行。

**Q：游戏时想打字**
A：按 F8 关闭映射即可正常打字（grab 模式下键盘会还给系统）。

## 跨平台

| 平台 | 键盘捕获 | grab 屏蔽原键 | 状态 |
|---|---|---|---|
| Linux (Wayland/X11) | evdev | 支持 | 已实测 |
| Windows | rdev | 暂不支持 | 已通过交叉编译检查，待真机验证 |

## 许可

[MIT](LICENSE) © Azrl, 2026
