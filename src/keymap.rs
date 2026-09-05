use serde::{Deserialize, Serialize};

/// 单个按键绑定的动作
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Action {
    /// 点按:按下时快速点击一次 (x, y)
    Tap { x: i32, y: i32 },
    /// 长按:按下键盘的瞬间触点落下,抬起键盘的瞬间触点抬起(全程实时跟随)
    Hold { x: i32, y: i32 },
    /// 滑动:按下时沿轨迹滑动一次
    Swipe {
        points: Vec<(i32, i32)>,
        duration_ms: u32,
    },
    /// 注入 Android 系统键(如返回=4, 主页=3)
    AndroidKey { keycode: u32 },
}

impl Action {
    pub fn kind_name(&self) -> &'static str {
        match self {
            Action::Tap { .. } => "点按",
            Action::Hold { .. } => "长按",
            Action::Swipe { .. } => "滑动",
            Action::AndroidKey { .. } => "系统键",
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Action::Tap { x, y } => format!("点按 ({x}, {y})"),
            Action::Hold { x, y } => format!("长按 ({x}, {y})"),
            Action::Swipe { points, duration_ms } => {
                format!("滑动 {} 个点 / {}ms", points.len(), duration_ms)
            }
            Action::AndroidKey { keycode } => format!("系统键 keycode={keycode}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KeyBind {
    /// 统一键码空间(Linux=evdev 码;Windows 由 rdev 映射到同一空间)
    pub key: u16,
    pub action: Action,
}

/// 临时摇杆的启用模式
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TempMode {
    /// 按住启用键期间生效,松开即失效
    Hold,
    /// 按一下启用,再按一下失效
    Toggle,
}

/// 临时摇杆:设置启用键后,方向键仅在启用期间归摇杆,期间同键位的其它绑定失效
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TempWheel {
    /// 启用键
    pub key: u16,
    pub mode: TempMode,
}

/// 虚拟轮盘:四个方向键控制一个以 (cx, cy) 为中心、radius 为半径的虚拟摇杆
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Wheel {
    pub up: u16,
    pub down: u16,
    pub left: u16,
    pub right: u16,
    pub cx: i32,
    pub cy: i32,
    pub radius: u32,
    /// None=永久摇杆;Some=临时摇杆(按启用键期间方向键归摇杆)
    #[serde(default)]
    pub temp: Option<TempWheel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    /// 映射总开关的切换键,默认 F8 = 66
    pub toggle_key: u16,
    pub binds: Vec<KeyBind>,
    pub wheels: Vec<Wheel>,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            name: "默认配置".into(),
            toggle_key: 66, // KEY_F8
            binds: Vec::new(),
            wheels: vec![Wheel {
                up: 17,    // W
                down: 31,  // S
                left: 30,  // A
                right: 32, // D
                cx: 300,
                cy: 900,
                radius: 120,
                temp: None,
            }],
        }
    }
}

/// 键码 -> 可读名称(按平台取各自来源的名称,码空间统一)
#[cfg(target_os = "linux")]
pub fn key_name(code: u16) -> String {
    format!("{:?}", evdev::KeyCode(code))
}

#[cfg(windows)]
pub fn key_name(code: u16) -> String {
    crate::capture::win_key_name(code)
}
