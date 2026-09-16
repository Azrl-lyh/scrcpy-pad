use serde::{Deserialize, Deserializer, Serialize};

/// 点按的默认触点持续时间(ms);设为 0 表示"按下不松手,直到再按一次"
pub const DEFAULT_TAP_DURATION_MS: u32 = 40;

/// 键位圆圈的默认响应范围(截图像素半径)
pub const DEFAULT_RADIUS: f32 = 35.2;

/// 响应范围缩放的每步乘性因子(放大 *=, 缩小 /=)
pub const RADIUS_ZOOM_FACTOR: f32 = 1.12;

fn default_tap_duration_ms() -> u32 {
    DEFAULT_TAP_DURATION_MS
}
fn default_radius() -> f32 {
    DEFAULT_RADIUS
}

// ============================ 滑动曲线(缓动函数) ============================

/// 滑动曲线:决定滑动过程中"时间 -> 沿轨迹的进度"的映射,即速度分布。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum Easing {
    /// 默认(匀速):f(t) = t
    Linear,
    /// 加速曲线:慢起快止,power 越大越陡(默认 2,即 t^2)
    EaseIn { power: f32 },
    /// 减速曲线:快起慢止(默认 2,即 1-(1-t)^2)
    EaseOut { power: f32 },
    /// 钟形曲线(缓入缓出):起止都慢、中间快(默认 3,即 smoothstep)
    Smooth { power: f32 },
    /// 贝塞尔曲线:由两个控制点决定形状(默认 0.42,0,0.58,1 = ease-in-out)
    Bezier {
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
    },
}

impl Default for Easing {
    fn default() -> Self {
        Easing::Linear
    }
}

impl Easing {
    pub fn label(&self) -> &'static str {
        match self {
            Easing::Linear => "默认(匀速)",
            Easing::EaseIn { .. } => "加速曲线",
            Easing::EaseOut { .. } => "减速曲线",
            Easing::Smooth { .. } => "钟形曲线",
            Easing::Bezier { .. } => "贝塞尔曲线",
        }
    }
}

/// 缓动函数:t∈[0,1] -> 进度∈[0,1]
pub fn easing_apply(e: Easing, t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    match e {
        Easing::Linear => t,
        Easing::EaseIn { power } => t.powf(power.max(0.05)),
        Easing::EaseOut { power } => 1.0 - (1.0 - t).powf(power.max(0.05)),
        Easing::Smooth { power } => {
            let p = power.max(0.05);
            let tn = t.powf(p);
            let dn = (1.0 - t).powf(p);
            tn / (tn + dn)
        }
        Easing::Bezier { x1, y1, x2, y2 } => bezier_progress(t, x1, y1, x2, y2),
    }
}

/// 三次贝塞尔(x1,y1,x2,y2)把输入时间 t 映射为输出进度。
/// x1/x2 为时间轴控制点(夹在 [0,1]),y1/y2 可为任意(允许过冲)。
fn bezier_progress(t: f32, x1: f32, y1: f32, x2: f32, y2: f32) -> f32 {
    let x1 = x1.clamp(0.0, 1.0);
    let x2 = x2.clamp(0.0, 1.0);
    // 牛顿迭代求 u 使 x(u)=t
    let mut u = t;
    for _ in 0..8 {
        let x = bezier_x(u, x1, x2);
        let dx = bezier_dx(u, x1, x2);
        if dx.abs() < 1e-6 {
            break;
        }
        u -= (x - t) / dx;
    }
    let u = u.clamp(0.0, 1.0);
    let w = 1.0 - u;
    3.0 * w * w * u * y1 + 3.0 * w * u * u * y2 + u * u * u
}

fn bezier_x(u: f32, x1: f32, x2: f32) -> f32 {
    let w = 1.0 - u;
    3.0 * w * w * u * x1 + 3.0 * w * u * u * x2 + u * u * u
}

fn bezier_dx(u: f32, x1: f32, x2: f32) -> f32 {
    let w = 1.0 - u;
    3.0 * w * w * x1 + 6.0 * w * u * (x2 - x1) + 3.0 * u * u * (1.0 - x2)
}

// ============================ 滑动轨迹 ============================

/// 滑动轨迹:决定滑动路径的空间形状(由起点/终点派生采样点)。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum SwipePath {
    /// 默认(条形):起点到终点的直线
    Line,
    /// 方形:起点=左上角,终点=右下角,沿矩形边界顺时针一圈回到起点
    Rect,
    /// 圆形:as_diameter=true 起点终点为直径两端;false 起点为圆心、终点到起点为半径
    /// start_angle 为划圈出发点相对圆心的角度(弧度)
    Circle {
        as_diameter: bool,
        start_angle: f32,
    },
}

impl Default for SwipePath {
    fn default() -> Self {
        SwipePath::Line
    }
}

impl SwipePath {
    pub fn label(&self) -> &'static str {
        match self {
            SwipePath::Line => "默认(条形)",
            SwipePath::Rect => "方形",
            SwipePath::Circle { .. } => "圆形",
        }
    }
}

/// 由轨迹类型与起/终点生成滑动路径采样点(空间折线)。
pub fn swipe_points(
    path: SwipePath,
    start: (i32, i32),
    end: (i32, i32),
    samples: usize,
) -> Vec<(i32, i32)> {
    let samples = samples.max(2);
    match path {
        SwipePath::Line => vec![start, end],
        SwipePath::Rect => rect_points(start, end),
        SwipePath::Circle {
            as_diameter,
            start_angle,
        } => circle_points(as_diameter, start_angle, start, end, samples),
    }
}

/// 方形轨迹:起点=左上,终点=右下,沿边界顺时针闭合(左上→右上→右下→左下→左上)
fn rect_points(start: (i32, i32), end: (i32, i32)) -> Vec<(i32, i32)> {
    let (x0, y0) = start;
    let (x1, y1) = end;
    vec![
        (x0, y0),
        (x1, y0),
        (x1, y1),
        (x0, y1),
        (x0, y0),
    ]
}

/// 圆形轨迹采样。返回 samples 个点(含闭合回出发点)。
fn circle_points(
    as_diameter: bool,
    start_angle: f32,
    start: (i32, i32),
    end: (i32, i32),
    samples: usize,
) -> Vec<(i32, i32)> {
    let (cx, cy, r) = if as_diameter {
        let cx = (start.0 + end.0) as f32 / 2.0;
        let cy = (start.1 + end.1) as f32 / 2.0;
        let r = ((end.0 - start.0) as f32).hypot((end.1 - start.1) as f32) / 2.0;
        (cx, cy, r)
    } else {
        let cx = start.0 as f32;
        let cy = start.1 as f32;
        let r = ((end.0 - start.0) as f32).hypot((end.1 - start.1) as f32);
        (cx, cy, r)
    };
    if r < 1.0 {
        return vec![start, start];
    }
    (0..samples)
        .map(|i| {
            let a = start_angle + i as f32 * std::f32::consts::TAU / (samples - 1) as f32;
            (cx + a.cos() * r, cy + a.sin() * r)
        })
        .map(|(x, y)| (x.round() as i32, y.round() as i32))
        .collect()
}

// ============================ 动作 ============================

/// 单个按键绑定的动作
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Action {
    /// 点按:按下时触点落下,持续 duration_ms 后抬起。
    /// duration_ms=0 表示按住不松手,直到再次按下同一键才抬起。
    Tap {
        x: i32,
        y: i32,
        #[serde(default = "default_tap_duration_ms")]
        duration_ms: u32,
        /// 响应范围(截图像素半径)
        #[serde(default = "default_radius")]
        radius: f32,
    },
    /// 长按:按下键盘的瞬间触点落下,抬起键盘的瞬间触点抬起(全程实时跟随)
    Hold {
        x: i32,
        y: i32,
        #[serde(default = "default_radius")]
        radius: f32,
    },
    /// 滑动:按下时按轨迹与曲线滑动一次
    Swipe(Swipe),
    /// 注入 Android 系统键(如返回=4, 主页=3)
    AndroidKey { keycode: u32 },
}

impl Action {
    pub fn kind_name(&self) -> &'static str {
        match self {
            Action::Tap { .. } => "点按",
            Action::Hold { .. } => "长按",
            Action::Swipe(_) => "滑动",
            Action::AndroidKey { .. } => "系统键",
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Action::Tap {
                x,
                y,
                duration_ms,
                ..
            } => {
                if *duration_ms == 0 {
                    format!("点按 ({x}, {y}) / 按住切换")
                } else {
                    format!("点按 ({x}, {y}) / {}ms", duration_ms)
                }
            }
            Action::Hold { x, y, .. } => format!("长按 ({x}, {y})"),
            Action::Swipe(s) => format!(
                "滑动 {}→{} / {}ms / {} / {}",
                fmt_pt(s.start),
                fmt_pt(s.end),
                s.duration_ms,
                s.easing.label(),
                s.path.label()
            ),
            Action::AndroidKey { keycode } => format!("系统键 keycode={keycode}"),
        }
    }
}

fn fmt_pt((x, y): (i32, i32)) -> String {
    format!("({x},{y})")
}

/// 滑动动作(独立结构,以便兼容旧版 points 折线配置)
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Swipe {
    pub start: (i32, i32),
    pub end: (i32, i32),
    pub duration_ms: u32,
    pub easing: Easing,
    pub path: SwipePath,
}

// 兼容旧配置 { points: [...], duration_ms } -> 取首末点为起终点
impl<'de> Deserialize<'de> for Swipe {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            start: Option<(i32, i32)>,
            end: Option<(i32, i32)>,
            points: Option<Vec<(i32, i32)>>,
            duration_ms: Option<u32>,
            easing: Option<Easing>,
            path: Option<SwipePath>,
        }
        let r = Raw::deserialize(d)?;
        let (start, end) = match (r.start, r.end) {
            (Some(a), Some(b)) => (a, b),
            _ => match r.points {
                Some(p) => {
                    let a = p.first().copied().unwrap_or((0, 0));
                    let b = p.last().copied().unwrap_or(a);
                    (a, b)
                }
                None => ((0, 0), (0, 0)),
            },
        };
        Ok(Swipe {
            start,
            end,
            duration_ms: r.duration_ms.unwrap_or(300),
            easing: r.easing.unwrap_or(Easing::Linear),
            path: r.path.unwrap_or(SwipePath::Line),
        })
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
