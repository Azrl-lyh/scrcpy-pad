use serde::{Deserialize, Deserializer, Serialize};

/// 配置格式版本。
/// 1 = 坐标存屏幕像素(旧格式);2 = 坐标存相对比例(0..1,自适应分辨率与横竖屏)。
pub const PROFILE_VERSION: u32 = 2;

/// 点按的默认触点持续时间(ms);设为 0 表示"按下不松手,直到再按一次"
pub const DEFAULT_TAP_DURATION_MS: u32 = 40;

/// 键位圆圈的默认响应范围(相对屏幕宽度的比例)。
/// 35.2px / 1080 ≈ 0.0326:与旧版默认视觉大小一致,但不再绑定具体分辨率。
pub const DEFAULT_RADIUS: f32 = 0.0326;

/// 响应范围缩放的每步乘性因子(放大 *=, 缩小 /=)
pub const RADIUS_ZOOM_FACTOR: f32 = 1.12;

/// 响应范围缩放一步(乘性),并在放大回默认值附近时精确复位为默认半径。
///
/// 单独抽成函数是为了能用单元测试锁死一个曾经的 bug:半径是**相对值**
/// (0.03 上下),若用绝对差判断"是否接近默认值",条件恒成立,每按一步都会
/// 立刻被复位,表现为 `Ctrl++ / Ctrl+-` 完全没反应。
pub fn zoom_radius(radius: f32, factor: f32) -> f32 {
    let mut r = (radius * factor).max(0.01);
    if (r / DEFAULT_RADIUS - 1.0).abs() < 0.02 {
        r = DEFAULT_RADIUS;
    }
    r
}

/// 鼠标按键的 evdev 码:两平台统一(Windows 侧由 rdev 映射到同一码空间)。
/// 左键=272、中键=274 未在此列出,因为鼠标按键已可直接当普通键绑定,
/// 这里只保留瞄准门控最常用的右键。
pub const BTN_RIGHT: u16 = 273;

fn default_tap_duration_ms() -> u32 {
    DEFAULT_TAP_DURATION_MS
}
fn default_radius() -> f32 {
    DEFAULT_RADIUS
}
fn default_version() -> u32 {
    1
}

/// 配置里坐标的单位。
/// 旧配置(v1)没有这个字段,反序列化后按 [`CoordUnit::Pixel`] 处理,
/// 由 [`upgrade_profile`] 在得知屏幕尺寸后换算成相对比例。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CoordUnit {
    /// 屏幕像素(仅旧配置使用)
    Pixel,
    /// 相对比例 0..1(当前格式)
    #[default]
    Rel,
}

/// 坐标换算器:配置坐标 <-> 当前屏幕像素。
///
/// 这是**唯一**做坐标换算的地方:注入、绘制、取点、界面显示都走它。
/// 旧配置(像素)在升级前 `legacy` 为真,此时直通(与升级前行为完全一致)。
///
/// 注意:构造它需要读取配置,调用方若已持有配置锁,必须先取好 Mapper 再加锁,
/// 否则会自锁(同一把锁在同线程二次 lock)。
#[derive(Debug, Clone, Copy)]
pub struct Mapper {
    pub w: f32,
    pub h: f32,
    legacy: bool,
}

impl Mapper {
    /// `space` 为当前屏幕(触摸坐标空间)尺寸
    pub fn new(unit: CoordUnit, space: (u32, u32)) -> Self {
        Self {
            w: space.0.max(1) as f32,
            h: space.1.max(1) as f32,
            legacy: unit == CoordUnit::Pixel,
        }
    }

    /// 横坐标:配置值 -> 像素
    pub fn x(&self, v: f32) -> i32 {
        self.axis(v, self.w)
    }

    /// 纵坐标:配置值 -> 像素
    pub fn y(&self, v: f32) -> i32 {
        self.axis(v, self.h)
    }

    fn axis(&self, v: f32, size: f32) -> i32 {
        if self.legacy {
            v.round() as i32
        } else {
            (v * size).round() as i32
        }
    }

    /// 点:配置值 -> 像素
    pub fn point(&self, x: f32, y: f32) -> (i32, i32) {
        (self.x(x), self.y(y))
    }

    /// 长度(半径等,按屏幕宽度换算):配置值 -> 像素
    pub fn len(&self, v: f32) -> f32 {
        if self.legacy {
            v
        } else {
            v * self.w
        }
    }

    /// 像素坐标 -> 配置值(取点时用)
    pub fn rel_x(&self, px: i32) -> f32 {
        if self.legacy {
            px as f32
        } else {
            px as f32 / self.w
        }
    }

    pub fn rel_y(&self, px: i32) -> f32 {
        if self.legacy {
            px as f32
        } else {
            px as f32 / self.h
        }
    }

    /// 像素长度 -> 配置值
    pub fn rel_len(&self, px: f32) -> f32 {
        if self.legacy {
            px
        } else {
            px / self.w
        }
    }
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

/// 单个按键绑定的动作。
///
/// 语义与 QtScrcpy 的动作类型一一对应(便于互通与后续控件化布局):
///   `Tap`/`Hold` = KMT_CLICK,KMT_DRAG = `Swipe`,
///   KMT_STEER_WHEEL = [`Wheel`],mouseMoveMap = [`Aim`],switchKey = [`Profile::toggle_key`]。
/// 坐标一律是相对值(0..1);旧配置由 [`upgrade_profile`] 自动换算。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Action {
    /// 点按:按下时触点落下,持续 duration_ms 后抬起。
    /// duration_ms=0 表示按住不松手,直到再次按下同一键才抬起。
    Tap {
        x: f32,
        y: f32,
        #[serde(default = "default_tap_duration_ms")]
        duration_ms: u32,
        /// 响应范围(相对屏幕宽度的比例)
        #[serde(default = "default_radius")]
        radius: f32,
    },
    /// 长按:按下键盘的瞬间触点落下,抬起键盘的瞬间触点抬起(全程实时跟随)
    Hold {
        x: f32,
        y: f32,
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

    /// 描述文字(相对坐标,便于排查;精确像素见截图浮层)
    pub fn describe(&self) -> String {
        match self {
            Action::Tap {
                x,
                y,
                duration_ms,
                ..
            } => {
                if *duration_ms == 0 {
                    format!("点按 {},{} / 按住切换", pct(*x), pct(*y))
                } else {
                    format!("点按 {},{} / {}ms", pct(*x), pct(*y), duration_ms)
                }
            }
            Action::Hold { x, y, .. } => format!("长按 {},{}", pct(*x), pct(*y)),
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

/// 相对坐标 -> 百分比文本(0.42 => "42%")
fn pct(v: f32) -> String {
    format!("{:.0}%", v * 100.0)
}

fn fmt_pt((x, y): (f32, f32)) -> String {
    format!("({:.0}%,{:.0}%)", x * 100.0, y * 100.0)
}

/// 滑动动作(独立结构,以便兼容旧版 points 折线配置)
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Swipe {
    pub start: (f32, f32),
    pub end: (f32, f32),
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
            start: Option<(f32, f32)>,
            end: Option<(f32, f32)>,
            points: Option<Vec<(f32, f32)>>,
            duration_ms: Option<u32>,
            easing: Option<Easing>,
            path: Option<SwipePath>,
        }
        let r = Raw::deserialize(d)?;
        let (start, end) = match (r.start, r.end) {
            (Some(a), Some(b)) => (a, b),
            _ => match r.points {
                Some(p) => {
                    let a = p.first().copied().unwrap_or((0.0, 0.0));
                    let b = p.last().copied().unwrap_or(a);
                    (a, b)
                }
                None => ((0.0, 0.0), (0.0, 0.0)),
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

/// 虚拟轮盘(KMT_STEER_WHEEL):四个方向键控制一个以 (cx, cy) 为中心、radius 为半径的虚拟摇杆。
/// 坐标为相对值(0..1),半径相对屏幕宽度。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Wheel {
    pub up: u16,
    pub down: u16,
    pub left: u16,
    pub right: u16,
    pub cx: f32,
    pub cy: f32,
    pub radius: f32,
    /// None=永久摇杆;Some=临时摇杆(按启用键期间方向键归摇杆)
    #[serde(default)]
    pub temp: Option<TempWheel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    /// 配置格式版本(1=像素坐标;2=相对坐标,见 [`PROFILE_VERSION`])
    #[serde(default = "default_version")]
    pub format_version: u32,
    /// 布局设计时所用的屏幕尺寸(仅供显示参考;相对坐标本身自适应)
    #[serde(default)]
    pub screen: Option<(u32, u32)>,
    pub name: String,
    /// 映射总开关的切换键,默认 F8 = 66
    pub toggle_key: u16,
    pub binds: Vec<KeyBind>,
    pub wheels: Vec<Wheel>,
    /// FPS 鼠标视角瞄准(旧配置缺省为未启用)
    #[serde(default)]
    pub aim: Aim,
    /// 外观(配色/密度/背景图);随配置保存
    #[serde(default)]
    pub look: crate::theme::Look,
}

/// 瞄准归中策略。
///
/// 手指拖动无法无限持续(会被屏幕边界挡住),所以偏移过大时要「抬指 + 在锚点重按」。
/// 三种策略对应不同的手感取舍。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum RecenterMode {
    /// 鼠标停止移动一小段时间后归中:顿挫最不易察觉,推荐默认
    Idle,
    /// 偏移超过阈值立即归中:适合不停转圈
    Threshold,
    /// 不归中:偏移到哪算哪
    Never,
}

impl Default for RecenterMode {
    fn default() -> Self {
        RecenterMode::Idle
    }
}

impl RecenterMode {
    pub fn label(self) -> &'static str {
        match self {
            RecenterMode::Idle => "静止归中",
            RecenterMode::Threshold => "阈值归中",
            RecenterMode::Never => "不归中",
        }
    }
}

/// FPS 鼠标视角瞄准(mouseMoveMap):把鼠标相对位移映射为手机上的手指拖动。
///
/// - `anchor_*`:手指落下的锚点(相对坐标),拖动从该点开始
/// - `sensitivity_*`:每 1 个鼠标计数对应的设备像素,越大越灵敏
/// - `hold_key`:仅当该鼠标键(evdev 码)按住时才瞄准;0 表示始终瞄准
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Aim {
    pub enabled: bool,
    /// 锚点(相对坐标);(0,0) 视为尚未设置
    pub anchor_x: f32,
    pub anchor_y: f32,
    pub sensitivity_x: f32,
    pub sensitivity_y: f32,
    pub invert_y: bool,
    pub recenter: RecenterMode,
    /// 静止归中:停止移动多久后归中(毫秒)
    pub recenter_idle_ms: u32,
    /// 阈值归中:偏移超过多少设备像素后归中
    pub recenter_threshold: i32,
    /// 需要按住才瞄准的鼠标键(evdev 码;0 = 始终瞄准)
    pub hold_key: u16,
    /// 瞄准期间是否捕获鼠标(隐藏/冻结系统光标)
    pub capture_mouse: bool,
}

impl Default for Aim {
    fn default() -> Self {
        Self {
            enabled: false,
            anchor_x: 0.0,
            anchor_y: 0.0,
            sensitivity_x: 2.0,
            sensitivity_y: 2.0,
            invert_y: false,
            recenter: RecenterMode::Idle,
            recenter_idle_ms: 120,
            recenter_threshold: 400,
            hold_key: 0,
            capture_mouse: true,
        }
    }
}

impl Aim {
    /// 锚点是否已经设置过
    pub fn anchor_set(&self) -> bool {
        self.anchor_x.abs() > 1e-6 || self.anchor_y.abs() > 1e-6
    }
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            format_version: PROFILE_VERSION,
            screen: None,
            name: "默认配置".into(),
            toggle_key: 66, // KEY_F8
            binds: Vec::new(),
            wheels: vec![Wheel {
                up: 17,    // W
                down: 31,  // S
                left: 30,  // A
                right: 32, // D
                // 相对坐标:左下角偏内,半径约为屏幕宽度的 1/9
                cx: 0.278,
                cy: 0.375,
                radius: 0.111,
                temp: None,
            }],
            aim: Aim::default(),
            look: crate::theme::Look::default(),
        }
    }
}

impl Profile {
    /// 当前坐标单位(由格式版本决定)
    pub fn coord_unit(&self) -> CoordUnit {
        if self.format_version >= PROFILE_VERSION {
            CoordUnit::Rel
        } else {
            CoordUnit::Pixel
        }
    }

    /// 坐标换算器:配置坐标 <-> 当前屏幕像素(注入/绘制/取点/显示的唯一入口)。
    /// 调用方若已持有配置锁,请先构造好 Mapper 再加锁,避免自锁。
    pub fn mapper(&self, space: (u32, u32)) -> Mapper {
        Mapper::new(self.coord_unit(), space)
    }
}

/// 这份像素坐标布局是否"装得进"给定坐标空间(所有点都落在画面内)。
///
/// 这是防止升级毁配置的关键判据:手机竖着截屏时(宽 1080),横屏布局的像素
/// 坐标(x 最大约 2772)除以竖屏宽度会得到 >1 的相对值 —— 换算本身"成功"了,
/// 但键位全部错位。这种方向/尺寸不匹配的情况下直接不升级,保持像素模式:
/// 像素坐标是原样使用的,不该为了换个单位就把用户辛苦调好的布局算坏。
fn pixels_fit_space(profile: &Profile, space: (u32, u32)) -> bool {
    let (w, h) = (space.0 as f32, space.1 as f32);
    let mut points: Vec<(f32, f32)> = Vec::new();
    for b in &profile.binds {
        match &b.action {
            Action::Tap { x, y, .. } | Action::Hold { x, y, .. } => points.push((*x, *y)),
            Action::Swipe(s) => {
                points.push(s.start);
                points.push(s.end);
            }
            Action::AndroidKey { .. } => {}
        }
    }
    for wl in &profile.wheels {
        points.push((wl.cx, wl.cy));
    }
    points
        .iter()
        .all(|(x, y)| *x >= -1.0 && *x <= w + 1.0 && *y >= -1.0 && *y <= h + 1.0)
}

/// 把旧格式(v1,像素坐标)的配置升级为相对坐标。
/// 需要屏幕尺寸才能换算,因此在"得知当前屏幕尺寸"时调用(连接成功或截图之后),
/// 且任何注入之前调用,保证不会出现"按错误单位注入"的中间态。
/// 返回是否发生了升级。
pub fn upgrade_profile(profile: &mut Profile, space: (u32, u32)) -> bool {
    let (w, h) = space;
    if profile.format_version >= PROFILE_VERSION || w == 0 || h == 0 {
        return false;
    }
    // 像素布局"装不进"这个坐标空间时绝不做换算 —— 见 pixels_fit_space 的说明
    if !pixels_fit_space(profile, space) {
        return false;
    }
    let (fw, fh) = (w as f32, h as f32);
    let rel_x = |v: f32| v / fw;
    let rel_y = |v: f32| v / fh;
    for b in &mut profile.binds {
        match &mut b.action {
            Action::Tap { x, y, radius, .. } | Action::Hold { x, y, radius, .. } => {
                *x = rel_x(*x);
                *y = rel_y(*y);
                *radius = rel_x(*radius);
            }
            Action::Swipe(s) => {
                s.start = (rel_x(s.start.0), rel_y(s.start.1));
                s.end = (rel_x(s.end.0), rel_y(s.end.1));
            }
            Action::AndroidKey { .. } => {}
        }
    }
    for wheel in &mut profile.wheels {
        wheel.cx = rel_x(wheel.cx);
        wheel.cy = rel_y(wheel.cy);
        wheel.radius = rel_x(wheel.radius);
    }
    profile.aim.anchor_x = rel_x(profile.aim.anchor_x);
    profile.aim.anchor_y = rel_y(profile.aim.anchor_y);
    profile.format_version = PROFILE_VERSION;
    profile.screen = Some(space);
    true
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归:响应范围缩放必须真的改变半径(曾经因为复位阈值写成绝对差而完全失效)
    #[test]
    fn zoom_radius_actually_changes() {
        let d = DEFAULT_RADIUS;
        let up = zoom_radius(d, RADIUS_ZOOM_FACTOR);
        assert!(up > d * 1.05, "放大必须明显变大,实际 {up}");
        let down = zoom_radius(d, 1.0 / RADIUS_ZOOM_FACTOR);
        assert!(down < d * 0.95, "缩小必须明显变小,实际 {down}");
        // 连续放大 5 步仍然持续变大(不能被复位吃掉)
        let mut r = d;
        for _ in 0..5 {
            let next = zoom_radius(r, RADIUS_ZOOM_FACTOR);
            assert!(next > r, "连续放大必须单调变大: {r} -> {next}");
            r = next;
        }
        // 回到默认值附近时精确复位,保持精度
        assert_eq!(zoom_radius(d * 0.99, 1.0), d);
    }

    /// 回归:竖屏截图的尺寸不得用来换算横屏像素布局 —— 那会把键位整体算坏
    #[test]
    fn upgrade_refuses_mismatched_orientation() {
        let mut p = Profile {
            format_version: 1,
            binds: vec![KeyBind {
                key: 37,
                action: Action::Hold {
                    x: 2462.0,
                    y: 1038.0,
                    radius: 35.2,
                },
            }],
            ..Profile::default()
        };
        // 竖屏空间(宽 1280):横屏的 x=2462 装不进去 —— 必须拒绝升级
        assert!(!upgrade_profile(&mut p, (1280, 2772)));
        assert_eq!(p.format_version, 1, "不匹配时不得升级");
        assert_eq!(p.binds[0].action, Action::Hold {
            x: 2462.0,
            y: 1038.0,
            radius: 35.2,
        });
        // 横屏空间(宽 2772):装得下 —— 正常升级为相对坐标,且位置不变
        assert!(upgrade_profile(&mut p, (2772, 1280)));
        assert_eq!(p.format_version, PROFILE_VERSION);
        let m = p.mapper((2772, 1280));
        match &p.binds[0].action {
            Action::Hold { x, y, .. } => {
                assert!((*x - 2462.0 / 2772.0).abs() < 1e-6);
                assert_eq!((m.x(*x), m.y(*y)), (2462, 1038));
            }
            _ => unreachable!(),
        }
    }

    /// 旧配置(像素坐标)升级为相对坐标后,在同一分辨率下注入的像素必须完全一致
    /// —— 升级不得改变任何既有按键的实际位置。
    #[test]
    fn upgrade_profile_keeps_pixel_positions() {
        let mut p = Profile {
            format_version: 1,
            binds: vec![KeyBind {
                key: 17,
                action: Action::Tap {
                    x: 540.0,
                    y: 1200.0,
                    duration_ms: DEFAULT_TAP_DURATION_MS,
                    radius: 35.2,
                },
            }],
            wheels: vec![Wheel {
                up: 17,
                down: 31,
                left: 30,
                right: 32,
                cx: 300.0,
                cy: 900.0,
                radius: 120.0,
                temp: None,
            }],
            ..Profile::default()
        };
        p.aim.anchor_x = 810.0;
        p.aim.anchor_y = 1200.0;

        assert_eq!(p.coord_unit(), CoordUnit::Pixel);
        assert!(upgrade_profile(&mut p, (1080, 2400)));
        assert_eq!(p.format_version, PROFILE_VERSION);
        assert!(!upgrade_profile(&mut p, (1080, 2400)), "不应重复升级");

        // 同分辨率:像素与升级前逐一相同
        let m = p.mapper((1080, 2400));
        match &p.binds[0].action {
            Action::Tap { x, y, radius, .. } => {
                assert_eq!((m.x(*x), m.y(*y)), (540, 1200));
                assert!((m.len(*radius) - 35.2).abs() < 0.01);
            }
            _ => unreachable!(),
        }
        let w = &p.wheels[0];
        assert_eq!((m.x(w.cx), m.y(w.cy)), (300, 900));
        assert!((m.len(w.radius) - 120.0).abs() < 0.01);
        assert_eq!((m.x(p.aim.anchor_x), m.y(p.aim.anchor_y)), (810, 1200));

        // 换分辨率(如换手机):按比例自适应,不再错位
        let m2 = p.mapper((720, 1600));
        match &p.binds[0].action {
            Action::Tap { x, y, .. } => assert_eq!((m2.x(*x), m2.y(*y)), (360, 800)),
            _ => unreachable!(),
        }
    }

    /// 旧版(0.1.2 及以前)写出的 json 必须仍能原样读入:
    /// 坐标是整数、没有 format_version / screen / look。
    #[test]
    fn legacy_json_still_loads_as_pixels() {
        let legacy = r#"{
            "name": "老配置",
            "toggle_key": 66,
            "binds": [
                { "key": 17, "action": { "Tap": { "x": 540, "y": 1200, "duration_ms": 40, "radius": 35.2 } } },
                { "key": 31, "action": { "Swipe": { "start": [100, 200], "end": [300, 400], "duration_ms": 300 } } }
            ],
            "wheels": [
                { "up": 17, "down": 31, "left": 30, "right": 32, "cx": 300, "cy": 900, "radius": 120 }
            ],
            "aim": { "enabled": true, "anchor_x": 810, "anchor_y": 1200, "sensitivity_x": 2.0,
                     "sensitivity_y": 2.0, "invert_y": false, "recenter": "Idle",
                     "recenter_idle_ms": 120, "recenter_threshold": 400, "hold_key": 0,
                     "capture_mouse": true }
        }"#;
        let mut p: Profile = serde_json::from_str(legacy).expect("旧配置应仍可解析");
        assert_eq!(p.format_version, 1);
        assert_eq!(p.coord_unit(), CoordUnit::Pixel);

        // 升级前直通像素:与旧版行为逐一致
        let m = p.mapper((1080, 2400));
        match &p.binds[0].action {
            Action::Tap { x, y, .. } => assert_eq!((m.x(*x), m.y(*y)), (540, 1200)),
            _ => unreachable!(),
        }
        match &p.binds[1].action {
            Action::Swipe(s) => assert_eq!(m.point(s.start.0, s.start.1), (100, 200)),
            _ => unreachable!(),
        }

        // 升级后重取换算器(单位已变为相对值),落点仍是同一像素
        assert!(upgrade_profile(&mut p, (1080, 2400)));
        let m2 = p.mapper((1080, 2400));
        match &p.binds[0].action {
            Action::Tap { x, y, .. } => assert_eq!((m2.x(*x), m2.y(*y)), (540, 1200)),
            _ => unreachable!(),
        }
        assert_eq!((m2.x(p.aim.anchor_x), m2.y(p.aim.anchor_y)), (810, 1200));
        assert_eq!(m2.point(p.wheels[0].cx, p.wheels[0].cy), (300, 900));
    }
}
