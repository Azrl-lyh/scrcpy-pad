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

/// 鼠标按键的 evdev 码:两平台统一(Windows 侧由低级钩子映射到同一码空间)。
/// 左键=272、中键=274 未在此列出,因为鼠标按键已可直接当普通键绑定,
/// 这里只保留瞄准门控最常用的右键。
pub const BTN_RIGHT: u16 = 273;
/// 鼠标滚轮的四个方向使用统一码空间的合成键码。
///
/// Windows 低级钩子与 Linux evdev 都没有把滚轮当成普通 Key;为了让它能像
/// 其它鼠标键一样参与“改键”,这里把每个滚轮刻度转成一次瞬时按下+抬起。
pub const BTN_WHEEL_UP: u16 = 277;
pub const BTN_WHEEL_DOWN: u16 = 278;
pub const BTN_WHEEL_LEFT: u16 = 279;
pub const BTN_WHEEL_RIGHT: u16 = 280;

/// 合成鼠标键码的可读名(跨平台共用)
pub fn mouse_aux_name(code: u16) -> Option<&'static str> {
    match code {
        BTN_WHEEL_UP => Some("BTN_WHEEL_UP"),
        BTN_WHEEL_DOWN => Some("BTN_WHEEL_DOWN"),
        BTN_WHEEL_LEFT => Some("BTN_WHEEL_LEFT"),
        BTN_WHEEL_RIGHT => Some("BTN_WHEEL_RIGHT"),
        _ => None,
    }
}

fn default_tap_duration_ms() -> u32 {
    DEFAULT_TAP_DURATION_MS
}
fn default_swipe_duration_ms() -> u32 {
    300
}
fn default_radius() -> f32 {
    DEFAULT_RADIUS
}
fn default_capture_mouse() -> bool {
    true
}

fn default_move_speed() -> f32 {
    1.0
}

fn default_open_world_radius() -> f32 {
    0.22
}

fn default_open_world_smoothing() -> f32 {
    0.85
}

fn default_drag_deadzone() -> f32 {
    0.0
}

fn default_boundary() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ViewInputMode {
    /// Universal compatibility: relative mouse values drive a touch drag.
    #[default]
    TouchDrag,
    /// Legacy scrcpy UHID mouse value; automatically migrated to TouchDrag.
    UhidMouse,
    /// Legacy scrcpy AOA mouse value; automatically migrated to TouchDrag.
    AoaMouse,
    /// Inject a virtual HID gamepad and drive its right stick from mouse motion.
    VirtualGamepadContinuous,
    /// Like continuous gamepad mode, but recenter at the stick limit and carry
    /// the excess, producing human-swipe-like segmented camera turns.
    VirtualGamepadSegmented,
}

impl ViewInputMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::TouchDrag => "触摸拖动（通用）",
            Self::UhidMouse => "UHID 相对鼠标（已移除，旧配置自动迁移）",
            Self::AoaMouse => "AOA 相对鼠标（已移除，旧配置自动迁移）",
            Self::VirtualGamepadContinuous => "虚拟手柄右摇杆（连续）",
            Self::VirtualGamepadSegmented => "虚拟手柄右摇杆（分段回中）",
        }
    }
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
        if self.legacy { v } else { v * self.w }
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
        if self.legacy { px } else { px / self.w }
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
    Bezier { x1: f32, y1: f32, x2: f32, y2: f32 },
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
    Circle { as_diameter: bool, start_angle: f32 },
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
    vec![(x0, y0), (x1, y0), (x1, y1), (x0, y1), (x0, y0)]
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

/// 宏中的一个按键事件。`delay_ms` 是相对上一个事件的等待时间。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MacroStep {
    pub code: u16,
    pub pressed: bool,
    #[serde(default)]
    pub delay_ms: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum MacroKeyMode {
    #[default]
    Tap,
    Hold,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum MacroWheelPart {
    #[default]
    Up,
    Down,
    Left,
    Right,
    Custom(usize),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MacroInstruction {
    Delay {
        #[serde(default)]
        ms: u32,
    },
    Key {
        code: u16,
        #[serde(default)]
        mode: MacroKeyMode,
        #[serde(default = "default_tap_duration_ms")]
        duration_ms: u32,
        #[serde(default)]
        delay_ms: u32,
    },
    Combo {
        keys: Vec<u16>,
        #[serde(default = "default_tap_duration_ms")]
        duration_ms: u32,
        #[serde(default)]
        delay_ms: u32,
    },
    Wheel {
        wheel: usize,
        #[serde(default)]
        part: MacroWheelPart,
        #[serde(default = "default_tap_duration_ms")]
        duration_ms: u32,
        #[serde(default)]
        delay_ms: u32,
    },
    Fps {
        on: bool,
        #[serde(default)]
        delay_ms: u32,
    },
    Click {
        x: f32,
        y: f32,
        #[serde(default = "default_tap_duration_ms")]
        duration_ms: u32,
        #[serde(default)]
        delay_ms: u32,
    },
    Swipe {
        start_x: f32,
        start_y: f32,
        end_x: f32,
        end_y: f32,
        #[serde(default = "default_swipe_duration_ms")]
        duration_ms: u32,
        #[serde(default)]
        delay_ms: u32,
    },
    Macro {
        action: Box<MacroAction>,
        #[serde(default)]
        delay_ms: u32,
    },
}

impl MacroInstruction {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Delay { .. } => "间隔",
            Self::Key { .. } => "按键",
            Self::Combo { .. } => "组合键",
            Self::Wheel { .. } => "轮盘",
            Self::Fps { .. } => "FPS",
            Self::Click { .. } => "点击",
            Self::Swipe { .. } => "滑动",
            Self::Macro { .. } => "宏",
        }
    }
}

/// 录制宏保存原始事件；设置宏保存语义操作组合。两者可单独使用。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct MacroAction {
    /// 用户录制得到的原始按键事件（已合并自动重复）。
    #[serde(default)]
    pub steps: Vec<MacroStep>,
    /// 用户设置的语义操作组合。
    #[serde(default)]
    pub instructions: Vec<MacroInstruction>,
    /// 扩展宏：可选的虚拟键位层。设置后，录制步骤和按键/组合键/轮盘
    /// 设置步骤都按这套虚拟映射解析后再注入。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub virtual_profile: Option<Box<Profile>>,
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
    /// 录制并回放的宏（开发中）。第一版依赖已有键位。
    Macro(MacroAction),
}

impl Action {
    pub fn kind_name(&self) -> &'static str {
        match self {
            Action::Tap { .. } => "点按",
            Action::Hold { .. } => "长按",
            Action::Swipe(_) => "滑动",
            Action::AndroidKey { .. } => "系统键",
            Action::Macro(_) => "宏（开发中）",
        }
    }

    /// 描述文字(相对坐标,便于排查;精确像素见截图浮层)
    pub fn describe(&self) -> String {
        match self {
            Action::Tap {
                x, y, duration_ms, ..
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
            Action::Macro(mac) => {
                if !mac.steps.is_empty() && !mac.instructions.is_empty() {
                    format!(
                        "宏 / {} 个录制事件 + {} 个设置操作",
                        mac.steps.len(),
                        mac.instructions.len()
                    )
                } else if !mac.instructions.is_empty() {
                    format!("设置宏 / {} 个操作", mac.instructions.len())
                } else {
                    let total: u32 = mac.steps.iter().map(|s| s.delay_ms).sum();
                    format!("录制宏 / {} 个事件 / 约 {}ms", mac.steps.len(), total)
                }
            }
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
    pub key: u16,
    pub action: Action,
    /// 仅在 FPS 模式开启时生效/显示;普通模式下完全让位。
    #[serde(default)]
    pub fps_only: bool,
}

/// A simultaneous chord such as Ctrl+R.  The action fires when all `keys`
/// are held at the same time, and a Hold/System action releases when any
/// component key is released.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KeyCombo {
    /// Physical evdev codes; at least two keys are required.
    pub keys: Vec<u16>,
    pub action: Action,
    /// Only active while FPS/open-world view mode is running.
    #[serde(default)]
    pub fps_only: bool,
}

/// 轮盘方向冲突时的处理方式。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum WheelMode {
    /// 经典方向叠加:左右同按会互相抵消,上下同理。
    #[default]
    Classic,
    /// 灵敏模式:同一轴上最后按下的方向覆盖较早方向;释放覆盖键后,
    /// 若反方向仍按着则自动恢复。左右与上下分别计算,斜向不冲突。
    Sensitive,
}

impl WheelMode {
    pub fn label(self) -> &'static str {
        match self {
            WheelMode::Classic => "经典(标准)",
            WheelMode::Sensitive => "灵敏(后按覆盖)",
        }
    }
}

/// 轮盘类型：标准四向 / 自定义方向 / 执行轮盘（先点中心再滑到方向）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum WheelKind {
    #[default]
    Standard,
    Custom,
    Execute,
}

impl WheelKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Standard => "标准四向",
            Self::Custom => "自定义方向",
            Self::Execute => "执行轮盘（开发中）",
        }
    }
}

/// 自定义轮盘的一个方向。0°=右，-90°=上，顺时针为正。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WheelDirection {
    pub angle_deg: f32,
    pub key: u16,
}
/// 临时摇杆的启用模式
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TempMode {
    /// 按住启用键期间生效,松开即失效
    Hold,
    /// 按一下启用,再按一下失效
    Toggle,
}

/// 摇杆"影响范围"系数的取值范围。
///
/// 取值乘在半径上得到触点实际推出的距离:1.0 = 与半径一致(旧行为)。
/// 下限不为 0:方向键按下却推不动会让摇杆彻底失效,是纯粹的配置事故;
/// 上限 4.0:再大就推到屏幕外了(超出部分会被系统丢弃),没有意义。
pub const SCOPE_MIN: f32 = 0.2;
pub const SCOPE_MAX: f32 = 4.0;

/// 影响范围系数的默认值(=1.0,与半径一致,保证老配置行为完全不变)
pub const DEFAULT_WHEEL_SCOPE: f32 = 1.0;

/// 新建摇杆的默认半径(**像素**,按当前触摸坐标空间换算成相对值后入配置)。
///
/// 为什么用像素而不是写死一个比例:界面上"半径:"一栏显示的就是这个像素值,
/// 用户是拿它去对照游戏里真实摇杆的判定圈的(用户实测反馈"摇杆初始范围过大,
/// 请修改到150")。写死比例的话,同一个比例在不同宽度的屏幕上会变成完全不同的
/// 像素值 —— 用户缩小过的摇杆与新建的摇杆就会差出一大截。
/// 存进配置的仍然是相对值(见 [`Wheel::radius`]),换手机/换方向时照旧自适应。
pub const NEW_WHEEL_RADIUS_PX: f32 = 150.0;

/// 新建摇杆时的参考屏宽:没有屏幕尺寸可用时按这个宽度把像素换成比例,
/// 保证"没有屏幕信息"这一路也不会造出一个荒唐的圈。
const REFERENCE_SCREEN_W: f32 = 1080.0;

/// 新建摇杆的默认半径(相对值,按参考屏宽换算)—— 供 [`Profile::default`] 这类
/// 拿不到屏幕尺寸的场合使用
pub const DEFAULT_WHEEL_RADIUS: f32 = NEW_WHEEL_RADIUS_PX / REFERENCE_SCREEN_W;

fn default_wheel_radius() -> f32 {
    DEFAULT_WHEEL_RADIUS
}

fn default_wheel_scope() -> f32 {
    DEFAULT_WHEEL_SCOPE
}

fn default_center_radius() -> f32 {
    NEW_WHEEL_RADIUS_PX * 0.45 / REFERENCE_SCREEN_W
}

fn default_execute_duration() -> u32 {
    90
}
/// 把任意输入(含手工编辑的 json)收敛到合法范围
pub fn clamp_scope(v: f32) -> f32 {
    if v.is_finite() {
        v.clamp(SCOPE_MIN, SCOPE_MAX)
    } else {
        DEFAULT_WHEEL_SCOPE
    }
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
    /// 同轴反向键同时按下时的处理模式。缺省为经典,保证老配置行为不变。
    #[serde(default)]
    pub mode: WheelMode,
    /// 影响范围系数:触点实际推出的距离 = radius × scope。
    /// 1.0(默认)= 推出距离等于半径;>1 更大幅度、<1 更精细。
    /// 老配置没有这个字段,反序列化后为 1.0,行为与旧版逐一致。
    #[serde(default = "default_wheel_scope")]
    pub scope: f32,
    /// 轮盘类型。缺省为标准，保证旧配置行为不变。
    #[serde(default)]
    pub kind: WheelKind,
    /// 自定义/执行轮盘的方向，最多 8 个。标准轮盘继续使用 up/down/left/right。
    #[serde(default)]
    pub directions: Vec<WheelDirection>,
    /// 执行轮盘的中心有效区域半径（相对屏宽）。
    #[serde(default = "default_center_radius")]
    pub center_radius: f32,
    /// 执行轮盘从中心到目标位置的滑动时间。
    #[serde(default = "default_execute_duration")]
    pub execute_duration_ms: u32,
    /// None=永久摇杆;Some=临时摇杆(按启用键期间方向键归摇杆)
    #[serde(default)]
    pub temp: Option<TempWheel>,
}

/// 轮盘"生效方向"的定长容器(最多 8 个)——栈上,零堆分配。
///
/// 为什么不用 `Vec`:引擎对**每个按键事件 × 每个生效轮盘**都要做一次方向匹配
/// (`engine.rs` 的方向键扫描与对账),而 `active_dirs()` 的结果最长只有 8 项。
/// 旧实现每次调用都 `vec![]/collect()` 一次堆分配,实测每事件多花 ~42ns
/// (2 个永久轮盘,`hotpath-bench.md`);换成定长拷贝后归零。
#[derive(Debug, Clone, Copy)]
pub struct ActiveDirs {
    buf: [(f32, u16); 8],
    len: usize,
}

impl ActiveDirs {
    fn new() -> Self {
        Self {
            buf: [(0.0, 0); 8],
            len: 0,
        }
    }

    fn push(&mut self, dir: (f32, u16)) {
        if self.len < self.buf.len() {
            self.buf[self.len] = dir;
            self.len += 1;
        }
    }

    pub fn as_slice(&self) -> &[(f32, u16)] {
        &self.buf[..self.len]
    }

    pub fn iter(&self) -> std::slice::Iter<'_, (f32, u16)> {
        self.as_slice().iter()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn get(&self, index: usize) -> Option<&(f32, u16)> {
        self.as_slice().get(index)
    }

    pub fn first(&self) -> Option<&(f32, u16)> {
        self.as_slice().first()
    }

    /// 需要独立 `Vec` 的冷路径(界面、测试)使用;热路径请直接用上面的借用接口。
    pub fn to_vec(&self) -> Vec<(f32, u16)> {
        self.as_slice().to_vec()
    }
}

impl<'a> IntoIterator for &'a ActiveDirs {
    type Item = &'a (f32, u16);
    type IntoIter = std::slice::Iter<'a, (f32, u16)>;

    fn into_iter(self) -> Self::IntoIter {
        self.as_slice().iter()
    }
}

impl IntoIterator for ActiveDirs {
    type Item = (f32, u16);
    type IntoIter = ActiveDirsIter;

    fn into_iter(self) -> Self::IntoIter {
        ActiveDirsIter { dirs: self, pos: 0 }
    }
}

/// [`ActiveDirs`] 的按值迭代器(栈上,零分配)。
pub struct ActiveDirsIter {
    dirs: ActiveDirs,
    pos: usize,
}

impl Iterator for ActiveDirsIter {
    type Item = (f32, u16);

    fn next(&mut self) -> Option<(f32, u16)> {
        if self.pos < self.dirs.len {
            let item = self.dirs.buf[self.pos];
            self.pos += 1;
            Some(item)
        } else {
            None
        }
    }
}

/// 切换键"实际按键"的定长集合(最多 2 个)——来源同 [`ActiveDirs`]:
/// 组合键门控对每个按键事件都要扫描 `switch_keys`(`engine.rs` ingest_button),
/// 旧实现每条切换键一次 `Vec` 分配,实测 +38.8ns/事件。
#[derive(Debug, Clone, Copy)]
pub struct EffectiveKeys {
    buf: [u16; 2],
    len: usize,
}

impl EffectiveKeys {
    fn new() -> Self {
        Self { buf: [0; 2], len: 0 }
    }

    fn push(&mut self, key: u16) {
        if self.len < self.buf.len() {
            self.buf[self.len] = key;
            self.len += 1;
        }
    }

    pub fn as_slice(&self) -> &[u16] {
        &self.buf[..self.len]
    }

    pub fn iter(&self) -> std::slice::Iter<'_, u16> {
        self.as_slice().iter()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn get(&self, index: usize) -> Option<&u16> {
        self.as_slice().get(index)
    }

    pub fn first(&self) -> Option<&u16> {
        self.as_slice().first()
    }

    pub fn contains(&self, key: &u16) -> bool {
        self.as_slice().contains(key)
    }

    /// 需要排序/去重/存储的冷路径(配置规范化、界面)使用。
    pub fn to_vec(&self) -> Vec<u16> {
        self.as_slice().to_vec()
    }
}

impl IntoIterator for EffectiveKeys {
    type Item = u16;
    type IntoIter = EffectiveKeysIter;

    fn into_iter(self) -> Self::IntoIter {
        EffectiveKeysIter { keys: self, pos: 0 }
    }
}

/// [`EffectiveKeys`] 的按值迭代器(栈上,零分配)。
pub struct EffectiveKeysIter {
    keys: EffectiveKeys,
    pos: usize,
}

impl Iterator for EffectiveKeysIter {
    type Item = u16;

    fn next(&mut self) -> Option<u16> {
        if self.pos < self.keys.len {
            let item = self.keys.buf[self.pos];
            self.pos += 1;
            Some(item)
        } else {
            None
        }
    }
}

impl Wheel {
    /// 触点推出距离(像素)—— 半径与影响范围的乘积,一处定义、全链路使用。
    ///
    /// 所有"按方向后手指推多远"的计算都必须走这里,避免某处漏乘 scope
    /// 导致界面显示的圈与实际手感对不上。
    pub fn push_px(&self, m: &Mapper) -> f32 {
        m.len(self.radius * clamp_scope(self.scope))
    }

    /// 影响范围系数(已收敛到合法范围)
    pub fn scope(&self) -> f32 {
        clamp_scope(self.scope)
    }

    /// 当前生效的方向集合。标准轮盘返回四向；其它类型返回自定义方向。
    /// 返回定长栈拷贝(零分配),见 [`ActiveDirs`]。
    pub fn active_dirs(&self) -> ActiveDirs {
        let mut out = ActiveDirs::new();
        match self.kind {
            WheelKind::Standard => {
                out.push((-90.0, self.up));
                out.push((90.0, self.down));
                out.push((180.0, self.left));
                out.push((0.0, self.right));
            }
            WheelKind::Custom | WheelKind::Execute => {
                if self.directions.is_empty() {
                    out.push((-90.0, self.up));
                    out.push((0.0, self.right));
                    out.push((90.0, self.down));
                    out.push((180.0, self.left));
                } else {
                    for d in self.directions.iter().take(8) {
                        out.push((d.angle_deg, d.key));
                    }
                }
            }
        }
        out
    }

    pub fn owns_key(&self, code: u16) -> bool {
        self.temp.as_ref().is_some_and(|t| t.key == code)
            || self.active_dirs().iter().any(|(_, key)| *key == code)
    }

    /// 切到自定义/执行轮盘时，用标准四向初始化自定义方向，保留旧配置语义。
    pub fn ensure_custom_directions(&mut self) {
        if self.directions.is_empty() {
            self.directions = vec![
                WheelDirection {
                    angle_deg: -90.0,
                    key: self.up,
                },
                WheelDirection {
                    angle_deg: 0.0,
                    key: self.right,
                },
                WheelDirection {
                    angle_deg: 90.0,
                    key: self.down,
                },
                WheelDirection {
                    angle_deg: 180.0,
                    key: self.left,
                },
            ];
        }
    }

    /// 设置自定义方向数量，保留已存在的前几个方向。
    pub fn set_direction_count(&mut self, count: usize) {
        self.ensure_custom_directions();
        let count = count.clamp(2, 8);
        while self.directions.len() > count {
            self.directions.pop();
        }
        while self.directions.len() < count {
            let k = self.directions.len() as f32;
            let angle = -90.0 + k * 360.0 / count as f32;
            self.directions.push(WheelDirection {
                angle_deg: angle,
                key: 0,
            });
        }
    }

    /// 新建摇杆:圆心落在给定位置上,半径固定为
    /// [`NEW_WHEEL_RADIUS_PX`] 像素(按当前坐标空间换算)。
    ///
    /// 抽成函数是为了让"新建摇杆"只有一处定义 —— 半径、scope、方向键默认值
    /// 全在这里,界面与默认配置不会再各写一份而慢慢跑偏。
    pub fn new_default(m: &Mapper, cx: f32, cy: f32) -> Self {
        Self {
            up: 17,    // W
            down: 31,  // S
            left: 30,  // A
            right: 32, // D
            cx,
            cy,
            radius: m.rel_len(NEW_WHEEL_RADIUS_PX),
            scope: DEFAULT_WHEEL_SCOPE,
            mode: WheelMode::Classic,
            kind: WheelKind::Standard,
            directions: Vec::new(),
            center_radius: default_center_radius(),
            execute_duration_ms: default_execute_duration(),
            temp: None,
        }
    }
}

/// 两个摇杆圆心近到这个距离(占屏宽的比例)以内就算"叠在一起"了 ——
/// 150px 的响应圈叠起来就是 300px,再近就分不清哪个是哪个。
const WHEEL_MIN_GAP: f32 = 0.12;

/// 新建摇杆的候选落点(相对坐标,按屏幕比例分布,避开四角与边缘)
const WHEEL_SPOTS: &[(f32, f32)] = &[
    (0.278, 0.375),
    (0.78, 0.375),
    (0.278, 0.72),
    (0.78, 0.72),
    (0.5, 0.5),
    (0.5, 0.25),
    (0.5, 0.75),
    (0.22, 0.25),
    (0.8, 0.25),
    (0.22, 0.8),
    (0.8, 0.8),
];

/// 新建摇杆的落点:在候选点里挑一个**离已有摇杆足够远**的。
///
/// 为什么不能固定写死一个点:多个摇杆圆心重合时,浮层上的圆环、方向标注与
/// 影响范围圈会糊成一团,用户根本分不清哪个是刚建的那个
/// (反馈"创建新摇杆时旧的摇杆会瞬间变大、新摇杆还可能和旧的换位")。
///
/// 候选点全被占满时(摇杆非常多),退而求其次挑"离最近的已有摇杆最远"的那个 ——
/// 至少不会完全叠在一起。
pub fn next_wheel_spot(wheels: &[Wheel]) -> (f32, f32) {
    let nearest = |cx: f32, cy: f32| {
        wheels
            .iter()
            .map(|w| (w.cx - cx).hypot(w.cy - cy))
            .fold(f32::INFINITY, f32::min)
    };
    let mut best = WHEEL_SPOTS[0];
    let mut best_d = f32::NEG_INFINITY;
    for &(cx, cy) in WHEEL_SPOTS {
        let d = nearest(cx, cy);
        if d >= WHEEL_MIN_GAP {
            return (cx, cy);
        }
        if d > best_d {
            best_d = d;
            best = (cx, cy);
        }
    }
    // 极端情况:候选点全被占住(摇杆极多)。按小网格继续往外错开,
    // 保证新建的摇杆至少不会和已有的完全重合。
    for k in 1..=64i32 {
        let cx = (best.0 + 0.05 * (k % 8) as f32).clamp(0.03, 0.97);
        let cy = (best.1 + 0.05 * (k / 8) as f32).clamp(0.03, 0.97);
        if nearest(cx, cy) > 1e-3 {
            return (cx, cy);
        }
    }
    best
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Profile {
    /// 配置格式版本(1=像素坐标;2=相对坐标,见 [`PROFILE_VERSION`])
    #[serde(default = "default_version")]
    pub format_version: u32,
    /// 布局设计时所用的屏幕尺寸(仅供显示参考;相对坐标本身自适应)
    #[serde(default)]
    pub screen: Option<(u32, u32)>,
    #[serde(default)]
    pub name: String,
    /// 映射总开关的切换键,默认 F8 = 66
    pub toggle_key: u16,
    /// 全局鼠标消隐切换键：按一下隐藏系统光标，再按一下恢复；不依赖 FPS。
    #[serde(default)]
    pub cursor_toggle_key: u16,
    pub binds: Vec<KeyBind>,
    /// Optional chord recognition.  When disabled, `combos` stays gray and
    /// has no effect on the normal single-key path.
    #[serde(default)]
    pub combos_enabled: bool,
    #[serde(default)]
    pub combos: Vec<KeyCombo>,
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
/// - `toggle_key`:独立启停 FPS 模式;0 表示未绑定
/// - `suspend_key`:按住时暂时退出 FPS 并把光标还给鼠标;0 表示未绑定
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Aim {
    pub enabled: bool,
    /// 锚点(相对坐标);(0,0) 视为尚未设置
    pub anchor_x: f32,
    pub anchor_y: f32,
    pub sensitivity_x: f32,
    pub sensitivity_y: f32,
    /// Global multiplier applied to mouse movement before any view-input mode.
    #[serde(default = "default_move_speed")]
    pub move_speed: f32,
    pub invert_y: bool,
    pub recenter: RecenterMode,
    /// 静止归中:停止移动多久后归中(毫秒)
    pub recenter_idle_ms: u32,
    /// 阈值归中:偏移超过多少设备像素后归中
    pub recenter_threshold: i32,
    /// 需要按住才瞄准的鼠标键(evdev 码;0 = 始终瞄准)
    pub hold_key: u16,
    /// FPS 模式指针消隐:是否捕获鼠标(隐藏/冻结系统光标),默认开启
    #[serde(default = "default_capture_mouse")]
    pub capture_mouse: bool,
    /// 进入/退出 FPS 模式的独立切换键(0 = 未绑定,可用界面按钮启停)
    #[serde(default)]
    pub toggle_key: u16,
    /// “按住才退出”:按住时暂时退出 FPS、恢复普通映射并显示鼠标(0 = 未绑定)
    #[serde(default)]
    pub suspend_key: u16,
    /// 开放世界模式:不需要射击/开镜,持续把相对鼠标位移映射为水平转向。
    #[serde(default)]
    pub open_world: bool,
    /// 开放世界水平回中半径(相对屏幕宽度)。越过该半径时无缝换手,保留超出量。
    #[serde(default = "default_open_world_radius")]
    pub open_world_radius: f32,
    /// 开放世界位移平滑系数:1.0 为完全直通,越小越平滑但越跟手迟。
    #[serde(default = "default_open_world_smoothing")]
    pub open_world_smoothing: f32,
    /// 鼠标拖动死区(设备像素):累计偏移在这个范围内不注入 touch_move。
    /// 目的:模拟手指按下后的小幅抖动，不让视角跟着鼠标噪声乱晃。
    #[serde(default = "default_drag_deadzone")]
    pub drag_deadzone: f32,
    /// 是否有屏幕边界。false = 无边界，到达回转半径后无缝抬指/重按并保留余量。
    #[serde(default = "default_boundary")]
    pub boundary: bool,
    /// 鼠标视角输入通道。旧 UHID/AOA 配置读取后会迁移为通用触摸拖动。
    #[serde(default)]
    pub input_mode: ViewInputMode,
}

impl Default for Aim {
    fn default() -> Self {
        Self {
            enabled: false,
            anchor_x: 0.0,
            anchor_y: 0.0,
            sensitivity_x: 2.0,
            sensitivity_y: 2.0,
            move_speed: default_move_speed(),
            invert_y: false,
            recenter: RecenterMode::Idle,
            recenter_idle_ms: 120,
            recenter_threshold: 400,
            hold_key: 0,
            capture_mouse: true,
            toggle_key: 0,
            suspend_key: 0,
            open_world: false,
            open_world_radius: default_open_world_radius(),
            open_world_smoothing: default_open_world_smoothing(),
            drag_deadzone: default_drag_deadzone(),
            boundary: default_boundary(),
            input_mode: ViewInputMode::TouchDrag,
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
            cursor_toggle_key: 0,
            binds: Vec::new(),
            combos_enabled: false,
            combos: Vec::new(),
            wheels: vec![Wheel {
                up: 17,    // W
                down: 31,  // S
                left: 30,  // A
                right: 32, // D
                // 相对坐标:左下角偏内,半径 = NEW_WHEEL_RADIUS_PX(150px)
                // 按参考屏宽换算 —— 与界面上新建摇杆得到的像素半径一致
                cx: 0.278,
                cy: 0.375,
                radius: default_wheel_radius(),
                scope: DEFAULT_WHEEL_SCOPE,
                mode: WheelMode::Classic,
                kind: WheelKind::Standard,
                directions: Vec::new(),
                center_radius: default_center_radius(),
                execute_duration_ms: default_execute_duration(),
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

/// 切换键:按下物理键 `key`,就把生效中的按键组合换成 `target` 那一套。
///
/// 与总开关键、轮盘启用键同属"功能键":按下它不会触发任何普通绑定,
/// 切换动作由引擎完成(它独占触点状态,能保证先抬起旧组合的触点再换车)。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SwitchDirection {
    #[default]
    Target,
    Next,
    Prev,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SwitchKey {
    /// 旧配置的单键触发码；新配置使用 `keys`，正常化时会把旧值搬进去。
    #[serde(default)]
    pub key: u16,
    /// 切换组合键，最多两个；顺序不影响匹配。
    #[serde(default)]
    pub keys: Vec<u16>,
    /// 目标组合在 `schemes` 里的下标
    #[serde(default)]
    pub target: usize,
    /// Target=切到指定组合；Next/Prev=按组合表循环。
    #[serde(default)]
    pub direction: SwitchDirection,
}

impl SwitchKey {
    /// 实际生效的切换按键(旧字段单键或新字段组合,最多 2 个)。
    /// 返回定长栈拷贝(零分配),见 [`EffectiveKeys`]。
    pub fn effective_keys(&self) -> EffectiveKeys {
        let mut out = EffectiveKeys::new();
        if self.keys.is_empty() {
            if self.key != 0 {
                out.push(self.key);
            }
        } else {
            for k in self.keys.iter().copied().filter(|k| *k != 0).take(2) {
                out.push(k);
            }
        }
        out
    }
}

/// 配置文件(YAML)的根:一份文件里装若干套"按键组合",外加把它们串起来的切换键。
///
/// 为什么多套组合共用一个文件,而不是一个组合一个文件:切换键要指向"哪一套",
/// 组合之间还有顺序(下标就是身份)——分开存就得额外维护一张名单和一堆路径,
/// 而名单本身又会与文件对不上。每套组合的内容与旧的单套配置逐一对应(见 [`Profile`]),
/// 因此"每套键位的设置与执行行为"与以前完全一致。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConfigFile {
    /// 配置格式版本(沿用 [`PROFILE_VERSION`])
    #[serde(default = "current_version")]
    pub format_version: u32,
    /// 当前生效的组合下标(启动时按它恢复"上次用的那一套")
    #[serde(default)]
    pub active: usize,
    /// 切换键表
    #[serde(default)]
    pub switch_keys: Vec<SwitchKey>,
    /// 只有打开时才允许 switch_keys 真正切换组合；关闭时仍可编辑。
    #[serde(default)]
    pub fast_switch_enabled: bool,
    /// 全部按键组合(至少一套)
    #[serde(default)]
    pub schemes: Vec<Profile>,
}

fn current_version() -> u32 {
    PROFILE_VERSION
}

impl Default for ConfigFile {
    fn default() -> Self {
        Self {
            format_version: PROFILE_VERSION,
            active: 0,
            switch_keys: Vec::new(),
            fast_switch_enabled: false,
            schemes: vec![Profile::default()],
        }
    }
}

impl ConfigFile {
    /// 当前生效的组合(空表或下标越界时退回第一套)
    pub fn active_profile(&self) -> Option<&Profile> {
        self.schemes
            .get(self.active)
            .or_else(|| self.schemes.first())
    }

    /// 把越界/无效的内容修回自洽状态,**返回是否发生了改动**。
    ///
    /// 用户会手改 YAML,而"指向不存在的组合""两个切换键撞在同一个键上"
    /// 这类错误不该让程序崩溃或行为诡异 —— 读进来先规整一遍:
    ///   * 至少有一套组合(空文件按默认补一套);
    ///   * `active` 越界时钳回 0;
    ///   * 切换键里 `key == 0`(未设置)或 `target` 越界的条目直接删掉;
    ///   * 同一个物理键只保留第一条(否则"按一次切到哪一套"取决于实现细节);
    ///   * 版本号一律按当前格式写死 —— YAML 是全新格式,旧版像素配置在
    ///     `profile.json` 里、本程序不再读取,因此不存在"需要升级的 YAML"。
    ///     这一条顺带堵住了"手写 YAML 漏了 format_version,被当成像素坐标"
    ///     这条最容易毁掉一份布局的路。
    pub fn normalize(&mut self) -> bool {
        let mut changed = false;
        if self.format_version != PROFILE_VERSION {
            self.format_version = PROFILE_VERSION;
            changed = true;
        }
        if self.schemes.is_empty() {
            self.schemes.push(Profile::default());
            changed = true;
        }
        for p in self.schemes.iter_mut() {
            if matches!(
                p.aim.input_mode,
                ViewInputMode::UhidMouse | ViewInputMode::AoaMouse
            ) {
                p.aim.input_mode = ViewInputMode::TouchDrag;
                changed = true;
            }
            if !p.aim.move_speed.is_finite() {
                p.aim.move_speed = 1.0;
                changed = true;
            } else {
                let speed = p.aim.move_speed.clamp(0.2, 3.0);
                if speed != p.aim.move_speed {
                    p.aim.move_speed = speed;
                    changed = true;
                }
            }
            p.combos
                .retain(|combo| combo.keys.len() >= 2 && combo.keys.iter().all(|key| *key != 0));
            for combo in &mut p.combos {
                let mut seen = Vec::new();
                combo.keys.retain(|key| {
                    if seen.contains(key) {
                        false
                    } else {
                        seen.push(*key);
                        true
                    }
                });
            }
            p.combos.retain(|combo| combo.keys.len() >= 2);
            if p.format_version != PROFILE_VERSION {
                p.format_version = PROFILE_VERSION;
                changed = true;
            }
            if p.name.trim().is_empty() {
                p.name = "默认配置".to_string();
                changed = true;
            }
        }
        if self.active >= self.schemes.len() {
            self.active = 0;
            changed = true;
        }
        let n = self.schemes.len();
        let before = self.switch_keys.len();
        let mut seen: Vec<Vec<u16>> = Vec::with_capacity(before);
        self.switch_keys.retain(|s| {
            let mut keys = s.effective_keys().to_vec(); // 冷路径(配置规范化):需要排序与去重
            if keys.is_empty() {
                return false;
            }
            keys.sort_unstable();
            if s.direction == SwitchDirection::Target && s.target >= n {
                return false;
            }
            if seen.contains(&keys) {
                return false;
            }
            seen.push(keys);
            true
        });
        if self.switch_keys.len() != before {
            changed = true;
        }
        changed
    }
}

/// 写进 YAML 文件头的字段说明。
///
/// 为什么是"文件头一段注释"而不是每个字段旁边一行:serde 的 YAML 序列化
/// (serde_norway,与 serde_yaml 同源)不输出注释,逐字段嵌注释需要自己写一遍
/// 序列化器 —— 为一份"给人看"的说明不值当。落盘时先写这段说明,再写数据,
/// 于是用户打开文件就能对照着改。
pub const YAML_HEADER: &str = r#"# ============================================================================
# scrcpy-pad 键位配置(按键组合)
# ----------------------------------------------------------------------------
# 一份文件里可以放多套"按键组合",每套的内容与行为完全一致,靠切换键在位。
# 所有坐标都是相对值(0..1),与手机分辨率/横竖屏无关。
#
# format_version : 配置格式版本(2 = 相对坐标)。不要手改。
# active         : 启动时生效的组合下标(0 起)。切换键按下后也会更新它。
# fast_switch_enabled : 是否启用快速切换(默认 false)。关闭时仍保留下面设置，但不生效。
# switch_keys    : 切换键表。支持单键或最多两个键的组合(顺序无关)。
#   - key        : 旧版单键字段，兼容旧配置。
#     keys       : 新配置的多键字段，最多两个 evdev 码。
#     direction  : target / next / prev；next=正向循环，prev=反向循环。
#     target     : direction=target 时的目标组合下标(0 起)。
# schemes        : 全部按键组合,至少有一套。每套字段如下:
#   name         : 组合名(界面左侧可改,切换时按它提示)
#                  每套里另有一份 format_version,由程序自动维护:
#                  读取时统一按当前格式处理,手改它没有作用。
#   toggle_key   : 映射总开关的切换键(evdev 码,默认 66=F8)
#   screen       : 设计这套布局时的屏幕尺寸 [宽, 高](仅作参考,可不填)
#   binds        : 键位绑定列表
#     - key      : 物理键(evdev 码;鼠标左/右/中=272/273/274,滚轮=277上/278下/279左/280右)
#       action   : 动作,取值见下
#       fps_only : true 时仅在 FPS 模式生效/显示(鼠标技能键建议开启)
#   combos_enabled : 是否启用组合键(默认 false)。关闭时 combos 保留但全部失效。
#   combos        : 组合键列表。keys[0] 是前缀键,后续 keys 与前缀同时按住才触发:
#     - keys      : 物理键 evdev 码数组,至少两个;例如 [29, 19] = Ctrl + R
#       action    : 组合触发时执行的动作(与 binds.action 相同)
#       fps_only  : true 时只在视角模式运行期间生效
#   wheels       : 虚拟摇杆(轮盘)列表
#     up/down/left/right : 四个方向的物理键(evdev 码)
#     cx, cy     : 摇杆中心(相对坐标 0..1)
#     radius     : 视觉半径(相对屏幕宽度的比例)
#     scope      : 影响范围倍数(手指实际被推离中心的距离 = radius × scope)
#     mode       : classic(经典)/sensitive(灵敏,同轴后按覆盖)
#     temp       : 可选。临时轮盘:key = 启用键(evdev 码),mode = hold|toggle
#   aim          : 鼠标视角(FPS / 开放世界)
#     enabled / anchor_x / anchor_y : 是否启用 + 手指落下的锚点(相对坐标)
#     sensitivity_x / sensitivity_y : 每 1 个鼠标计数对应的设备像素
#     move_speed : view speed multiplier (0.2..3.0, default 1.0; touch/open-world/gamepad)
#     invert_y   : 是否反转纵向
#     recenter   : 归中策略 idle|threshold|never
#     recenter_idle_ms / recenter_threshold : 静止归中时长 / 阈值归中的偏移阈值
#     hold_key   : 仅当该鼠标键按住时才瞄准(evdev 码;0 = 始终瞄准)
#     capture_mouse : FPS 模式指针消隐(默认 true)
#     toggle_key : 独立启停 FPS 模式的按键(0 = 未绑定)
#     suspend_key: “按住才退出”,按住时暂时退出 FPS、恢复普通映射并显示光标(0 = 未绑定)
#     open_world : true 时启用开放世界视角模式(不要求射击/开镜,无限水平转向)
#     open_world_radius : 水平触摸拖动带的回中半径(相对屏幕宽度,默认 0.22)
#     open_world_smoothing : 位移平滑系数(0.15~1.0,默认 0.85)
#     input_mode : touch_drag(universal touch drag) / virtual_gamepad_continuous /
#                  virtual_gamepad_segmented (Xbox 360 HID right stick; segmented recenters at limit)
#                  legacy uhid_mouse / aoa_mouse are migrated to touch_drag.
#   look         : 外观(配色/密度/背景图),随组合一起保存
#
# 动作(action)四种写法(注意类型用 YAML 标签标出,即 !Tap 这种写法;
# 手改时要连感叹号一起写,否则解析会失败):
#   !Tap       : 点按。x, y 为落点(相对坐标),duration_ms=0 表示按住不松手
#                直到再次按下同一键才抬起;radius 为响应范围
#   !Hold      : 长按。键盘按下即落指、松开即抬指,x, y, radius 同上
#   !Swipe     : 滑动。start/end 为起终点,duration_ms 为时长,
#                easing 为缓动,path 为轨迹(取值见界面里的下拉选项)
#   !AndroidKey: 注入 Android 系统键。keycode 例:4=返回, 3=主页, 187=最近任务
# ============================================================================
"#;

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
            Action::Macro(_) => {}
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
            Action::Macro(_) => {}
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
    if let Some(name) = mouse_aux_name(code) {
        return name.to_string();
    }
    format!("{:?}", evdev::KeyCode(code))
}

#[cfg(windows)]
pub fn key_name(code: u16) -> String {
    if let Some(name) = mouse_aux_name(code) {
        return name.to_string();
    }
    crate::capture::win_key_name(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归:响应范围缩放必须真的改变半径(曾经因为复位阈值写成绝对差而完全失效)
    #[test]
    fn legacy_uhid_view_mode_migrates_to_touch_drag() {
        let mut doc = ConfigFile::default();
        doc.schemes[0].aim.input_mode = ViewInputMode::UhidMouse;
        assert!(doc.normalize());
        assert_eq!(doc.schemes[0].aim.input_mode, ViewInputMode::TouchDrag);
    }

    #[test]
    fn synthetic_wheel_names_are_stable() {
        assert_eq!(mouse_aux_name(BTN_WHEEL_UP), Some("BTN_WHEEL_UP"));
        assert_eq!(mouse_aux_name(BTN_WHEEL_DOWN), Some("BTN_WHEEL_DOWN"));
        assert_eq!(mouse_aux_name(BTN_WHEEL_LEFT), Some("BTN_WHEEL_LEFT"));
        assert_eq!(mouse_aux_name(BTN_WHEEL_RIGHT), Some("BTN_WHEEL_RIGHT"));
    }

    #[test]
    fn legacy_keybind_defaults_to_global_and_wheel_defaults_to_classic() {
        let bind: KeyBind =
            serde_json::from_str(r#"{"key":30,"action":{"Hold":{"x":0.1,"y":0.2,"radius":0.03}}}"#)
                .unwrap();
        assert!(!bind.fps_only);
        let wheel: Wheel = serde_json::from_str(
            r#"{"up":17,"down":31,"left":30,"right":32,"cx":0.1,"cy":0.2,"radius":0.03}"#,
        )
        .unwrap();
        assert_eq!(wheel.mode, WheelMode::Classic);
    }

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
                fps_only: false,
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
        assert_eq!(
            p.binds[0].action,
            Action::Hold {
                x: 2462.0,
                y: 1038.0,
                radius: 35.2,
            }
        );
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
                fps_only: false,
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
                scope: DEFAULT_WHEEL_SCOPE,
                mode: WheelMode::Classic,
                kind: WheelKind::Standard,
                directions: Vec::new(),
                center_radius: default_center_radius(),
                execute_duration_ms: default_execute_duration(),
                temp: None,
            }],
            ..Profile::default()
        };
        p.aim.anchor_x = 810.0;
        p.aim.anchor_y = 1200.0;

        assert_eq!(p.coord_unit(), CoordUnit::Pixel);
        assert_eq!(
            p.aim.move_speed, 1.0,
            "old aim configs default to 1.0x speed"
        );
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

    /// 新增的"影响范围"字段:老配置(没有该字段)读入后必须是 1.0,
    /// 保证升级前后手感完全一致;越界与非法值(NaN/负数)必须被收敛。
    #[test]
    fn wheel_scope_defaults_and_clamps() {
        // 老 json(无 scope 字段)
        let old = r#"{
            "up": 17, "down": 31, "left": 30, "right": 32,
            "cx": 0.278, "cy": 0.375, "radius": 0.111
        }"#;
        let w: Wheel = serde_json::from_str(old).expect("老配置应可解析");
        assert_eq!(w.scope, DEFAULT_WHEEL_SCOPE, "缺省影响范围必须是 1.0");
        let m = Mapper::new(CoordUnit::Rel, (1000, 1000));
        assert!(
            (w.push_px(&m) - m.len(w.radius)).abs() < 1e-3,
            "scope=1.0 时推出距离必须与半径一致"
        );

        // 放大 / 缩小都体现在推出距离上
        let big = Wheel {
            scope: 2.0,
            ..w.clone()
        };
        assert!((big.push_px(&m) - m.len(w.radius) * 2.0).abs() < 1e-3);

        // 越界与非法值被 clamp(SCOPE_MIN..=SCOPE_MAX)
        assert_eq!(
            clamp_scope(0.0),
            SCOPE_MIN,
            "0 会让摇杆完全推不动,必须抬到下限"
        );
        assert_eq!(clamp_scope(-3.0), SCOPE_MIN);
        assert_eq!(clamp_scope(99.0), SCOPE_MAX);
        assert_eq!(clamp_scope(f32::NAN), DEFAULT_WHEEL_SCOPE);
        let wild = Wheel {
            scope: 100.0,
            ..w.clone()
        };
        assert!(wild.push_px(&m) <= m.len(w.radius) * SCOPE_MAX + 1e-3);
    }

    /// 斜向推出距离不应超过 scope 描述的范围(归一化后半径分量 < 半径)
    #[test]
    fn wheel_push_px_is_a_radius_scale() {
        let w = Wheel {
            up: 17,
            down: 31,
            left: 30,
            right: 32,
            cx: 0.278,
            cy: 0.375,
            radius: 0.111,
            scope: 1.5,
            mode: WheelMode::Classic,
            kind: WheelKind::Standard,
            directions: Vec::new(),
            center_radius: default_center_radius(),
            execute_duration_ms: default_execute_duration(),
            temp: None,
        };
        let m = Mapper::new(CoordUnit::Rel, (1080, 2400));
        let r = m.len(w.radius);
        assert!((w.push_px(&m) - r * 1.5).abs() < 1e-2);
        // 换算器换了屏幕尺寸,推出距离按比例跟着变(仍是同一个 scope)
        let m2 = Mapper::new(CoordUnit::Rel, (720, 1600));
        assert!((w.push_px(&m2) - m2.len(w.radius) * 1.5).abs() < 1e-2);
    }

    /// 新建摇杆的默认半径必须**在任何屏幕上都等于 150px**。
    ///
    /// 回归:旧版把默认半径写死成 0.111(宽度的 1/9),在 2772 宽的横屏手机上
    /// 就是 307px、在 1080 宽的竖屏上是 120px —— 同一个"默认"在不同设备上
    /// 差出一大截,用户在界面上看到的像素值也就跟着失控(反馈"摇杆初始范围过大")。
    #[test]
    fn new_wheel_radius_is_always_150px() {
        for space in [(1080u32, 2400u32), (2772, 1280), (720, 1600)] {
            let m = Mapper::new(CoordUnit::Rel, space);
            let w = Wheel::new_default(&m, 0.3, 0.4);
            let px = m.len(w.radius);
            assert!(
                (px - NEW_WHEEL_RADIUS_PX).abs() < 0.01,
                "{}x{} 上新建摇杆半径应为 {}px,实际 {px}",
                space.0,
                space.1,
                NEW_WHEEL_RADIUS_PX
            );
            assert_eq!(
                w.scope(),
                DEFAULT_WHEEL_SCOPE,
                "新建摇杆的影响范围默认为 1.0"
            );
            assert_eq!(
                (w.up, w.down, w.left, w.right),
                (17, 31, 30, 32),
                "方向键默认 WASD"
            );
            assert!(w.temp.is_none(), "新建摇杆默认是永久摇杆");
        }

        // 默认配置(拿不到当前屏幕尺寸)按参考屏宽换算,同样是 150px
        let m = Mapper::new(CoordUnit::Rel, (1080, 2400));
        let d = &Profile::default().wheels[0];
        assert!((m.len(d.radius) - NEW_WHEEL_RADIUS_PX).abs() < 0.01);
    }

    /// 新建轮盘的落点必须避开已有摇杆 —— 圆心重叠会让浮层上的圆环、方向标注与
    /// 影响范围圈糊成一团,用户根本分不清哪个是刚建的那个
    /// (反馈过"创建新摇杆时旧的摇杆会瞬间变大、新摇杆还可能和旧的换位")。
    #[test]
    fn new_wheels_do_not_stack_on_each_other() {
        let m = Mapper::new(CoordUnit::Rel, (1080, 2400));
        let mut wheels: Vec<Wheel> = Vec::new();
        // 常见的头几个摇杆:必须与所有已有摇杆都拉开距离
        for i in 0..8 {
            let (cx, cy) = next_wheel_spot(&wheels);
            for w in &wheels {
                let d = (w.cx - cx).hypot(w.cy - cy);
                assert!(
                    d >= WHEEL_MIN_GAP - 1e-6,
                    "第 {i} 个新建摇杆与已有摇杆只差 {d},会叠在一起"
                );
            }
            assert!((0.0..=1.0).contains(&cx) && (0.0..=1.0).contains(&cy));
            wheels.push(Wheel::new_default(&m, cx, cy));
        }
        // 候选点用尽(摇杆很多)时:退化为小网格错开,但绝不能重合
        for _ in 0..6 {
            let (cx, cy) = next_wheel_spot(&wheels);
            let d = wheels
                .iter()
                .map(|w| (w.cx - cx).hypot(w.cy - cy))
                .fold(f32::INFINITY, f32::min);
            assert!(d > 1e-3, "挤满时也不能与已有摇杆重合(最近只差 {d})");
            assert!((0.0..=1.0).contains(&cx) && (0.0..=1.0).contains(&cy));
            wheels.push(Wheel::new_default(&m, cx, cy));
        }
    }

    /// 多套"按键组合"必须能原样过一遍 YAML(序列化 -> 解析),一个字段都不丢。
    ///
    /// 这是整套配置的唯一落盘路径,round-trip 出问题就等于用户的键位被悄悄改掉。
    #[test]
    fn config_file_round_trips_through_yaml() {
        let mut doc = ConfigFile {
            format_version: PROFILE_VERSION,
            active: 1,
            fast_switch_enabled: false,
            switch_keys: vec![
                SwitchKey {
                    key: 67,
                    target: 0,
                    ..Default::default()
                },
                SwitchKey {
                    key: 68,
                    target: 1,
                    ..Default::default()
                },
            ],
            schemes: vec![Profile::default(), {
                let mut p = Profile::default();
                p.name = "按键组合2".into();
                p.toggle_key = 65; // F7
                p.binds.push(KeyBind {
                    fps_only: false,
                    key: 22, // U
                    action: Action::Tap {
                        x: 0.42,
                        y: 0.31,
                        duration_ms: 40,
                        radius: 0.05,
                    },
                });
                p.binds.push(KeyBind {
                    fps_only: false,
                    key: 23,
                    action: Action::AndroidKey { keycode: 4 },
                });
                p.wheels[0].temp = Some(TempWheel {
                    key: 57,
                    mode: TempMode::Toggle,
                });
                p
            }],
        };
        assert!(!doc.normalize(), "内容本来就自洽,不该报告改动");

        let text = serde_norway::to_string(&doc).unwrap();
        let back: ConfigFile = serde_norway::from_str(&text).unwrap();
        assert_eq!(back, doc, "YAML round-trip 必须一模一样");
    }

    /// 文件头那段说明是给人看的注释:带上它一起读必须照样解析成功
    /// (否则用户改完文件保存,程序反而读不出配置)。
    #[test]
    fn yaml_header_comments_are_ignored_by_the_parser() {
        let doc = ConfigFile::default();
        let text = format!("{YAML_HEADER}{}", serde_norway::to_string(&doc).unwrap());
        let back: ConfigFile = serde_norway::from_str(&text).unwrap();
        assert_eq!(back, doc);
    }

    /// 手改坏的 YAML 要被规整回自洽状态,而不是让程序崩掉或行为诡异
    #[test]
    fn normalize_repairs_out_of_range_switch_keys() {
        let mut doc = ConfigFile {
            active: 9, // 越界
            switch_keys: vec![
                SwitchKey {
                    key: 0,
                    target: 0,
                    ..Default::default()
                }, // 未设置
                SwitchKey {
                    key: 67,
                    target: 9,
                    ..Default::default()
                }, // 指向不存在的组合
                SwitchKey {
                    key: 68,
                    target: 0,
                    ..Default::default()
                }, // 正常
                SwitchKey {
                    key: 68,
                    target: 1,
                    ..Default::default()
                }, // 同键重复(保留第一条)
            ],
            ..ConfigFile::default()
        };
        assert!(doc.normalize(), "有越界内容时应报告改动");
        assert_eq!(doc.active, 0, "active 越界应钳回 0");
        assert_eq!(
            doc.switch_keys,
            vec![SwitchKey {
                key: 68,
                target: 0,
                ..Default::default()
            }],
            "无效/重复的切换键应被剔除"
        );
        assert!(!doc.normalize(), "规整过一次之后就该自洽了");

        // 空组合表也要能自愈(手写文件时很容易删空)
        let mut empty = ConfigFile {
            schemes: Vec::new(),
            ..ConfigFile::default()
        };
        assert!(empty.normalize());
        assert_eq!(empty.schemes.len(), 1, "至少要有一套组合");
        assert_eq!(
            empty.active_profile().map(|p| p.name.as_str()),
            Some("默认配置")
        );
    }
}
