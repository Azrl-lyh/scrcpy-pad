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
/// 左键=272、中键=274;这里列出瞄准门控(右键)与压枪触发键(V2-1,常用左键)
/// 两处用到的常量 —— 其余鼠标键已可直接当普通键绑定,不必具名。
pub const BTN_LEFT: u16 = 272;
pub const BTN_RIGHT: u16 = 273;
/// 鼠标中键(滚轮按下)。列出来是因为界面要判"这一下是不是鼠标按键"。
pub const BTN_MIDDLE: u16 = 274;
/// 鼠标滚轮的四个方向使用统一码空间的合成键码。
///
/// Windows 低级钩子与 Linux evdev 都没有把滚轮当成普通 Key;为了让它能像
/// 其它鼠标键一样参与“改键”,这里把每个滚轮刻度转成一次瞬时按下+抬起。
pub const BTN_WHEEL_UP: u16 = 277;
pub const BTN_WHEEL_DOWN: u16 = 278;
pub const BTN_WHEEL_LEFT: u16 = 279;
pub const BTN_WHEEL_RIGHT: u16 = 280;

/// 是不是滚轮键码(上/下/左/右)。
///
/// 用户 2026-10-09(第 3 条"滚动"):滚轮**只能**在[鼠标映射]那份下拉里设定。
/// 在别处(键位设置、轮盘方向、宏指令键…)用"按任意键"接滚轮,会把用户在清单上
/// 滚一下鼠标的动作当成一次绑定 —— 不是本意,而且改完还很难看出是怎么改的。
/// 所以那条捕获路径见到滚轮一律不写(见 `app.rs` 里等待按键的分支)。
pub const fn is_wheel_code(code: u16) -> bool {
    matches!(
        code,
        BTN_WHEEL_UP | BTN_WHEEL_DOWN | BTN_WHEEL_LEFT | BTN_WHEEL_RIGHT
    )
}

/// 是不是鼠标按键码(左 272 / 右 273 / 中 274;滚轮不算,见 [`is_wheel_code`])。
///
/// 用途:界面上的「就地取消 / 停止」控件被按下时要把这一下**整体丢掉**
/// (用户 2026-10-10 第 1 条)—— 能被"按在按钮上"的只有鼠标键,键盘键不会。
pub const fn is_mouse_button(code: u16) -> bool {
    matches!(code, BTN_LEFT | BTN_RIGHT | BTN_MIDDLE)
}

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

/// 「按后延迟」的默认值(毫秒)。用户 2026-10-10 第 2 条。
///
/// 一条键位 / 组合键 / 宏**上一次按下结束之后**,至少再等这么久才接受它的下一次
/// 按下 —— 快速连点、连续划动时"上一动作还没抬起、下一动作已经按下"会互相干扰。
///
/// **默认 `0` = 不等**:这是一个**可选功能**(用户 2026-10-10 晚追加要求
/// "按后延迟改为可选功能"),要**两层都愿意**才生效 ——
///   ① 总开关 [`Profile::tail_delay_enabled`] 打开(**默认关闭**);
///   ② 这一条自己的数值 > 0(**默认 0**)。
/// 所以老配置、新建的键位在任何情况下都**不会**被悄悄加上冷却。30ms 约等于两帧:
/// 想用时这是个好起点,但它只是"建议值",不再当默认值。
pub const DEFAULT_TAIL_DELAY_MS: u32 = 0;

/// 「按后延迟」的上限(毫秒)。界面数字框用它定范围。
///
/// 2 秒已经远超"防手抖连点"的语义:再长就是刻意的节流,该用别的手段(宏里的等待
/// 步骤)而不是把一条键位变成半残。上限同时挡住了"手滑多打一位数 → 这个键再也不
/// 响应"这种只能靠改 YAML 才能恢复的坑。
pub const MAX_TAIL_DELAY_MS: u32 = 2000;

fn default_tail_delay_ms() -> u32 {
    DEFAULT_TAIL_DELAY_MS
}

/// 序列化时省略"就是默认值"的那一份(= `0` = 不用这个功能) —— 老 YAML 读进来
/// 仍按默认值补齐,而没动过这一项的条目不会被写出一堆 `tail_delay_ms: 0` 噪音。
fn tail_delay_is_default(v: &u32) -> bool {
    *v == DEFAULT_TAIL_DELAY_MS
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

fn default_wheel_zoom() -> bool {
    true
}

fn default_wheel_zoom_step() -> f32 {
    20.0
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
    // ⚠️【已废弃 deprecated · 2026-10-08】虚拟手柄右摇杆(连续/分段回中)不再维护:
    // 只保留现有行为以兼容既有配置,不做修复、不做扩展、不加新功能;
    // 新功能一律不要依赖这条通道(engine.rs 的 GAMEPAD_* / gamepad_* 整块同理)。
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
/// 旧配置(v1)没有这个字段,反序列化后按 [`CoordUnit::Pixel`] 处理:像素值
/// **原样直通**使用。v1→v2 的自动换算已删除(O-7=B,2026-10-07):YAML 加载
/// 时版本字段一律写死为当前版本,不存在需要升级的配置。
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

/// 圆形轨迹的采样点数:UI 预览(`draw_swipe_track`)与引擎实际路径
/// (滑动展开)统一用这一份,免得两边密度漂移成"预览圆、实机多边形"。
pub const SWIPE_SAMPLES: usize = 64;

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
    /// 按一下键:按下 → 持续 `duration_ms` → 抬起(自包含,不跨步骤保持)。
    ///
    /// 历史:`mode: tap/hold` 字段已于 2026-10-06(W2-8)删除——用户拍板
    /// "宏不需要长按,录制什么释放什么";旧配置里的 `mode` 会被 serde
    /// 忽略(全仓没有 deny_unknown_fields),行为统一为"按下-持续-抬起"。
    Key {
        code: u16,
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
    /// 本步骤相对上一事件的等待毫秒数(排程用;`Delay` 自身即纯等待)。
    pub fn delay_ms(&self) -> u32 {
        match self {
            Self::Delay { ms } => *ms,
            Self::Key { delay_ms, .. }
            | Self::Combo { delay_ms, .. }
            | Self::Wheel { delay_ms, .. }
            | Self::Fps { delay_ms, .. }
            | Self::Click { delay_ms, .. }
            | Self::Swipe { delay_ms, .. }
            | Self::Macro { delay_ms, .. } => *delay_ms,
        }
    }

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
/// 坐标一律是相对值(0..1);旧配置(像素坐标)按 [`CoordUnit::Pixel`] 直通使用,
/// 不做换算(升级路径已删,见坐标单位说明)。
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
    /// 宏:录制回放,或按语义步骤执行,可选虚拟键位层(见 [`MacroAction`])。
    Macro(MacroAction),
}

impl Action {
    pub fn kind_name(&self) -> &'static str {
        match self {
            Action::Tap { .. } => "点按",
            Action::Hold { .. } => "长按",
            Action::Swipe(_) => "滑动",
            Action::AndroidKey { .. } => "系统键",
            Action::Macro(_) => "宏",
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
    /// 「按后延迟」(ms,用户 2026-10-10 第 2 条):这条键位的上一次按下**结束之后**,
    /// 至少再等这么久才接受下一次按下。0 = 不等(旧行为)。
    #[serde(
        default = "default_tail_delay_ms",
        skip_serializing_if = "tail_delay_is_default"
    )]
    pub tail_delay_ms: u32,
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
    /// 「按后延迟」(ms,用户 2026-10-10 第 2 条):同 [`KeyBind::tail_delay_ms`]。
    #[serde(
        default = "default_tail_delay_ms",
        skip_serializing_if = "tail_delay_is_default"
    )]
    pub tail_delay_ms: u32,
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

/// 轮盘类型：标准四向 / 多向轮盘 / 执行轮盘（先点中心再滑到方向）。
///
/// `Custom` 的**显示名是「多向轮盘」**（R3，2026-10-08；此前的显示名是「自定义方向」）。
/// YAML 里的取值仍然写 `custom`，所以旧配置一字不用改、行为也一字未改 ——
/// 只换牌子不改内核：
///
/// * **效果≈执行轮盘**：方向集合与执行轮盘完全一样 —— 任意个方向（2–8）、
///   每个方向可以设任意角度，还能点「设置位置」在截图上直接指定终点；
///   方向键的落点计算也走同一套（`wheel_combo_offset`：双键取两键方向的中点、
///   距离规整到基准圆）。
/// * **行为=普通轮盘**：按下方向键后触点**一直推在目标上**（长久指引），
///   松开才回中/抬指；**不是**执行轮盘那种"按下→拖动到目标→撤去"的一次性手势。
///
/// 于是它和 `Execute` 的差别只在"撤不撤"：`Execute` 每次按下跑一段
/// 中心→目标的插值滑动然后抬手（`execute_direction_ms` 决定快慢），
/// `Custom` 则把触点停在目标上等你松手。
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
            // 多向轮盘（旧显示名「自定义方向」）：效果同执行轮盘、行为同普通轮盘。
            Self::Custom => "多向轮盘",
            Self::Execute => "执行轮盘",
        }
    }
}

/// 自定义轮盘的一个方向。0°=右，-90°=上，顺时针为正。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WheelDirection {
    pub angle_deg: f32,
    /// 这个方向的触发键(用户 2026-10-10 第 2 条:与其它槽位一样支持组合键,见 [`KeySet`])。
    ///
    /// 单键配置序列化成裸整数(老 YAML 一字不改),匹配用
    /// `KeySet::contains` + "整个集合都按着" —— 于是 `Ctrl+W` 与 `W+Ctrl` 等价。
    pub key: KeySet,
    /// 手动指定的**终点**(相对坐标,与轮盘圆心同一坐标系)。
    ///
    /// - `Some` = 用户点「设置位置」后在截图上直接点的那个点:触点就推到这里,
    ///   可以比"影响范围"圆更远、也可以更近 —— 角度和影响范围都不再管它;
    /// - `None`(默认) = 跟随基准圆:方向由 `angle_deg` 决定、距离由
    ///   `radius × scope`(影响范围)决定,与旧版行为完全一致。
    ///
    /// 只有点该方向的「重置」才会退回 `None`(重新贴回基准圆),
    /// 之后改影响范围才会再次影响到这个方向 —— 这正是区分"基准"与"手改"的关键。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual: Option<(f32, f32)>,
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

/// 几何长度(轮盘半径/中心半径这类"相对屏幕宽度的长度")的加载期收敛(W0-8)。
///
/// 手改 YAML 写进来的 `NaN`/`inf`/负数/超大值都在这里归一:非有限值回默认,
/// 其余钳进 `(0, 1]`。不收敛的话,`radius × 屏幕宽` 会算出巨大的推出距离,
/// 注入点直接飞出屏幕(出屏触摸被设备端整条丢弃,表现为"这个方向推不动"),
/// debug 构建下 i32 加法还会溢出 panic —— 引擎线程一死,之后全部按键静默失效。
fn clamp_unit_span(v: f32, fallback: f32) -> f32 {
    if v.is_finite() {
        v.clamp(0.001, 1.0)
    } else {
        fallback
    }
}

/// 相对坐标位置(轮盘中心 cx/cy 这类 0..1 的点)的加载期收敛(W0-8)。
fn clamp_unit_pos(v: f32, fallback: f32) -> f32 {
    if v.is_finite() {
        v.clamp(0.0, 1.0)
    } else {
        fallback
    }
}

/// 临时摇杆:设置启用键后,方向键仅在启用期间归摇杆,期间同键位的其它绑定失效
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TempWheel {
    /// 启用键(用户 2026-10-10 第 2 条:支持组合键,见 [`KeySet`]。
    /// 空集合 = 没设启用键,此时 `temp` 本不该存在,`Wheel` 会把它当永久轮盘处理)。
    pub key: KeySet,
    pub mode: TempMode,
}

/// 虚拟轮盘(KMT_STEER_WHEEL):四个方向键控制一个以 (cx, cy) 为中心、radius 为半径的虚拟摇杆。
/// 坐标为相对值(0..1),半径相对屏幕宽度。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Wheel {
    /// 标准轮盘四向的触发键(用户 2026-10-10 第 2 条:与其它槽位一样支持组合键,
    /// 见 [`KeySet`]。单键配置与旧 YAML 完全互通)。
    pub up: KeySet,
    pub down: KeySet,
    pub left: KeySet,
    pub right: KeySet,
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
    buf: [(f32, KeySet); 8],
    len: usize,
}

impl ActiveDirs {
    fn new() -> Self {
        Self {
            buf: [(0.0, KeySet::new()); 8],
            len: 0,
        }
    }

    fn push(&mut self, dir: (f32, KeySet)) {
        if self.len < self.buf.len() {
            self.buf[self.len] = dir;
            self.len += 1;
        }
    }

    pub fn as_slice(&self) -> &[(f32, KeySet)] {
        &self.buf[..self.len]
    }

    pub fn iter(&self) -> std::slice::Iter<'_, (f32, KeySet)> {
        self.as_slice().iter()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn get(&self, index: usize) -> Option<&(f32, KeySet)> {
        self.as_slice().get(index)
    }

    pub fn first(&self) -> Option<&(f32, KeySet)> {
        self.as_slice().first()
    }

    /// 需要独立 `Vec` 的冷路径(界面、测试)使用;热路径请直接用上面的借用接口。
    pub fn to_vec(&self) -> Vec<(f32, KeySet)> {
        self.as_slice().to_vec()
    }
}

impl<'a> IntoIterator for &'a ActiveDirs {
    type Item = &'a (f32, KeySet);
    type IntoIter = std::slice::Iter<'a, (f32, KeySet)>;

    fn into_iter(self) -> Self::IntoIter {
        self.as_slice().iter()
    }
}

impl IntoIterator for ActiveDirs {
    type Item = (f32, KeySet);
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
    type Item = (f32, KeySet);

    fn next(&mut self) -> Option<(f32, KeySet)> {
        if self.pos < self.dirs.len {
            let item = self.dirs.buf[self.pos];
            self.pos += 1;
            Some(item)
        } else {
            None
        }
    }
}

/// 一个槽位上绑定的按键集合(最多 2 个,`0` 表示空位,顺序不影响匹配)。
///
/// 用户 2026-10-09(第 4 条):"系统键(映射开关、FPS 开关、快睡切换等)可作组合键"。
/// 这些槽位原来是一个 `u16`,现在统一换成这个集合:
///   * 匹配规则统一成"事件是这个集合的成员 **且** 集合此刻整个按着"(`engine` 里
///     `key_set_hit`/切换键那套),因此 `Ctrl+X` 与 `X+Ctrl` 都能触发 —— 顺序无关;
///   * 界面统一成一个按钮捕获 + `Ctrl+X` 这样的显示;
///   * **不**给它们加时长/间隔之类的参数(用户同一条要求里的限制)。
///
/// 切换键早先已经有这套(旧名 `EffectiveKeys`),现在提成通用类型,一处定义。
///
/// 定长栈上存储(零分配):引擎的按键热路径会频繁构造与比较它 ——
/// 组合键门控对每个按键事件都要扫一遍切换键,旧实现每条一次 `Vec` 分配,
/// 实测 +38.8ns/事件。
#[derive(Debug, Clone, Copy, Default)]
pub struct KeySet {
    buf: [u16; 2],
    len: usize,
}

impl KeySet {
    pub fn new() -> Self {
        Self::default()
    }

    /// 单个键(0 = 空)。
    pub fn single(key: u16) -> Self {
        let mut out = Self::new();
        if key != 0 {
            out.push(key);
        }
        out
    }

    /// 由若干键码构造:滤掉 0、去重、最多留 2 个,**保留给定顺序**。
    pub fn from_keys(keys: impl IntoIterator<Item = u16>) -> Self {
        let mut out = Self::new();
        for k in keys {
            if k != 0 && !out.as_slice().contains(&k) {
                out.push(k);
            }
        }
        out
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

    /// 集合里的**最后一个**成员(`None` = 空集合)。
    ///
    /// 捕获经 `canonical_chord` 排序后,修饰键在前,所以"最后一个"就是那个普通键
    /// —— 正好是 `Ctrl+X` 里的 `X`。宏回放把方向键挂到时间轴上时需要一个**唯一**
    /// 把手(否则 `Ctrl+W` 两个成员会各挂一次、抬起时只解开一个),就用它。
    pub fn last(&self) -> Option<u16> {
        self.as_slice().last().copied()
    }

    /// 单个键的槽位(未绑定返回 None)。老配置与"只允许单键"的槽位都走这里。
    pub fn only(&self) -> Option<u16> {
        (self.len == 1).then(|| self.buf[0])
    }

    pub fn contains(&self, key: &u16) -> bool {
        self.as_slice().contains(key)
    }

    /// `Ctrl+X` 这样的显示文本(空集合返回空串,由调用方给"未绑定")。
    pub fn label(&self) -> String {
        self.as_slice()
            .iter()
            .map(|k| key_name(*k))
            .collect::<Vec<_>>()
            .join("+")
    }

    /// 集合里的键**此刻全部按着**(空集合恒为 `false`)。
    ///
    /// 这是组合键成立的另一半条件:事件只是"集合成员之一"还不够,
    /// 必须整个集合都按着 —— 于是 `Ctrl+X` 与 `X+Ctrl` 都能触发。
    pub fn all_held_by(&self, mut down: impl FnMut(u16) -> bool) -> bool {
        !self.is_empty() && self.iter().all(|k| down(*k))
    }

    /// 需要排序/去重/存储的冷路径(配置规范化、界面)使用。
    pub fn to_vec(&self) -> Vec<u16> {
        self.as_slice().to_vec()
    }
}

impl From<u16> for KeySet {
    fn from(key: u16) -> Self {
        Self::single(key)
    }
}

/// 与单个键码比较 = "这个槽位**正好**绑定了这一个键"(空集合 ↔ `0`)。
///
/// 有意这么做:全程序原来到处是 `ev.code == profile.toggle_key` 这类单键比较,
/// 有了这层实现,老配置(单键)的语义一字不变、调用点也不用全改;
/// 而组合键的槽位必须显式用 `contains` + "整个集合都按着" 去匹配
/// (见 `engine::key_set_hit`),不会因为这里"看起来相等"就误触发。
impl PartialEq<u16> for KeySet {
    fn eq(&self, other: &u16) -> bool {
        if *other == 0 {
            self.is_empty()
        } else {
            self.len == 1 && self.buf[0] == *other
        }
    }
}

impl PartialEq<KeySet> for u16 {
    fn eq(&self, other: &KeySet) -> bool {
        other == self
    }
}

/// 集合相等 = 成员相同(顺序无关),与匹配语义一致。
impl PartialEq for KeySet {
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len && self.as_slice().iter().all(|k| other.contains(k))
    }
}

impl Eq for KeySet {}

impl IntoIterator for KeySet {
    type Item = u16;
    type IntoIter = KeySetIter;

    fn into_iter(self) -> Self::IntoIter {
        KeySetIter { keys: self, pos: 0 }
    }
}

/// [`KeySet`] 的按值迭代器(栈上,零分配)。
pub struct KeySetIter {
    keys: KeySet,
    pos: usize,
}

impl Iterator for KeySetIter {
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

/// 序列化:单键(含空)写成裸整数 —— 老配置原样往返、人看着也清爽;
/// 两键写成数组(如 `[29, 45]`)。
impl Serialize for KeySet {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self.len {
            0 => s.serialize_u16(0),
            1 => s.serialize_u16(self.buf[0]),
            _ => self.as_slice().serialize(s),
        }
    }
}

/// 反序列化:既吃裸整数(老配置/单键),也吃数组(组合键)。
impl<'de> Deserialize<'de> for KeySet {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Def {
            One(u16),
            Many(Vec<u16>),
        }
        Ok(match Def::deserialize(d)? {
            Def::One(k) => KeySet::single(k),
            Def::Many(v) => KeySet::from_keys(v),
        })
    }
}

/// 哪些键码算"修饰键"(Ctrl/Shift/Alt/Win 左右共 8 个)。
///
/// 只用于把捕获到的组合键排成 `Ctrl+X` 这种**好认的顺序** ——
/// 匹配与存储都不依赖顺序(见 [`KeySet`])。
pub fn is_modifier_key(code: u16) -> bool {
    matches!(
        code,
        29 | 97   // KEY_LEFTCTRL / KEY_RIGHTCTRL
        | 42 | 54 // KEY_LEFTSHIFT / KEY_RIGHTSHIFT
        | 56 | 100 // KEY_LEFTALT / KEY_RIGHTALT
        | 125 | 126 // KEY_LEFTMETA / KEY_RIGHTMETA
    )
}

/// 把捕获到的一组键排成规范顺序:修饰键在前,其余按码值。
/// (捕获是"按下顺序",直接存会把 `X 后 Ctrl` 显示成 `X+Ctrl`。)
pub fn canonical_chord(keys: &[u16]) -> KeySet {
    let mut v: Vec<u16> = keys.iter().copied().filter(|k| *k != 0).collect();
    v.sort_by_key(|k| (!is_modifier_key(*k), *k));
    v.dedup();
    KeySet::from_keys(v)
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
    /// 当前生效的方向集合。标准轮盘返回四向；多向轮盘([`WheelKind::Custom`])
    /// 与执行轮盘返回自定义方向集合(两者逐一致 —— 见 [`WheelKind`] 的说明)。
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

    /// 这个物理键是否"归本轮盘"(任意生效方向的触发键之一,或临时轮盘的启用键之一)。
    ///
    /// 组合键槽位按**成员**判定(与 `vk_find_target` 同口径):`Ctrl+W` 为某方向时,
    /// `Ctrl` 与 `W` 都算被本轮盘占着 —— 否则那个普通绑定会与组合方向键抢触点。
    pub fn owns_key(&self, code: u16) -> bool {
        self.temp.as_ref().is_some_and(|t| t.key.contains(&code))
            || self
                .active_dirs()
                .iter()
                .any(|(_, key)| key.contains(&code))
    }

    /// 切到多向/执行轮盘时，用标准四向初始化自定义方向，保留旧配置语义。
    /// (「多向轮盘」= [`WheelKind::Custom`]，显示名见 [`WheelKind::label`]。)
    pub fn ensure_custom_directions(&mut self) {
        if self.directions.is_empty() {
            self.directions = vec![
                WheelDirection {
                    angle_deg: -90.0,
                    key: self.up,
                    manual: None,
                },
                WheelDirection {
                    angle_deg: 0.0,
                    key: self.right,
                    manual: None,
                },
                WheelDirection {
                    angle_deg: 90.0,
                    key: self.down,
                    manual: None,
                },
                WheelDirection {
                    angle_deg: 180.0,
                    key: self.left,
                    manual: None,
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
                key: KeySet::new(),
                manual: None,
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
            up: KeySet::single(17),    // W
            down: KeySet::single(31),  // S
            left: KeySet::single(30),  // A
            right: KeySet::single(32), // D
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
    /// 映射总开关的切换键,默认 F8 = 66。可写成组合键(如 `[29, 66]` = Ctrl+F8)。
    pub toggle_key: KeySet,
    /// 全局鼠标消隐切换键：按一下隐藏系统光标，再按一下恢复；不依赖 FPS。
    /// 可写成组合键(见 [`KeySet`])。
    #[serde(default)]
    pub cursor_toggle_key: KeySet,
    pub binds: Vec<KeyBind>,
    /// Optional chord recognition.  When disabled, `combos` stays gray and
    /// has no effect on the normal single-key path.
    #[serde(default)]
    pub combos_enabled: bool,
    /// 「按后延迟」的**总开关**(用户 2026-10-10 晚追加要求:把该功能改成可选)。
    ///
    /// 默认 **false** = 整个功能不生效:每条自己的 `tail_delay_ms` 一律按 `0` 处理
    /// (数值原样保留,便于随时打开)。打开后,才逐条按各自的毫秒值生效。
    /// 与逐条数值构成"两层都愿意才生效" —— 见 [`DEFAULT_TAIL_DELAY_MS`]。
    #[serde(default)]
    pub tail_delay_enabled: bool,
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
/// - `hold_key`:仅当该鼠标键(evdev 码)按住时才瞄准;空表示始终瞄准
/// - `toggle_key`:独立启停 FPS 模式;空表示未绑定
/// - `suspend_key`:按住时暂时退出 FPS 并把光标还给鼠标;空表示未绑定
///
/// 这三个(以及压枪 `trigger_key`)都是**系统键**,自 2026-10-09 起可以是组合键
/// (见 [`KeySet`]),即"这几个键一起按住"才生效,顺序无关。
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
    /// 需要按住才瞄准的鼠标键(evdev 码;空 = 始终瞄准)。可写成组合键。
    pub hold_key: KeySet,
    /// FPS 模式指针消隐:是否捕获鼠标(隐藏/冻结系统光标),默认开启
    #[serde(default = "default_capture_mouse")]
    pub capture_mouse: bool,
    /// 进入/退出 FPS 模式的独立切换键(空 = 未绑定,可用界面按钮启停)。可写成组合键。
    #[serde(default)]
    pub toggle_key: KeySet,
    /// “按住才退出”:按住时暂时退出 FPS、恢复普通映射并显示鼠标(空 = 未绑定)。可写成组合键。
    #[serde(default)]
    pub suspend_key: KeySet,
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
    ///
    /// [已废弃 2026-10-08] 其中的 `VirtualGamepadContinuous` / `VirtualGamepadSegmented`
    /// (虚拟手柄右摇杆 连续 / 分段回中)不再维护:配置照旧可读、行为照旧不变,
    /// 但不再修复、不再扩展。其它三个取值(触摸拖动等)不受影响。
    #[serde(default)]
    pub input_mode: ViewInputMode,
    /// FPS 模式内:鼠标滚轮 = 双指缩放(向游戏注入两指张开/捏合手势;
    /// 上滚=放大两指张开,下滚=缩小两指捏合)。2026-10-07 用户要求。
    /// 默认开:FPS 模式下滚轮此前本来没有用武之地,开了不改变任何既有行为。
    #[serde(default = "default_wheel_zoom")]
    pub wheel_zoom: bool,
    /// 每齿缩放比例(%):两指间距按 (1±step%) 指数变化 —— 缩放是比例量,
    /// 固定像素步进在"贴近/拉远"两端的颗数会严重失衡。
    #[serde(default = "default_wheel_zoom_step")]
    pub wheel_zoom_step: f32,
    /// 压枪/后坐力补偿(V2-1)。默认全关;参数语义对齐 K2er《鼠标宏》专页。
    #[serde(default)]
    pub recoil: Recoil,
}

/// 压枪 / 后坐力补偿(V2-1,原 W3-3)。参数语义**直接对齐 K2er 官方《鼠标宏》专页**,
/// 先把原文抄在这里(教训见方案文档 S-12:参数名 ≠ 语义,抄原文再写代码):
///
/// > 鼠标宏需要配合瞄准模式或者瞄准（摇杆）一起使用，绑定触发快捷键触发时，控制视角向下移动。
/// > 参数:
/// > - 绑定触发快捷键: 一般是鼠标左键，如果是手柄的话，就是R2。
/// > - 控制频率: 每秒控制的次数。
/// > - 控制强度: 可以增加多个强度，每按一次快捷键，就会切换一个强度。也可以开启鼠标滚轮改变强度
/// > - 鼠标滚轮改变强度: 用鼠标滚轮的滚动来快速切换当前的强度
/// > - 摇晃: 随机左右摇晃
/// > - 覆盖灵敏度: 当触发鼠标宏时，覆盖瞄准的灵敏度
///
/// (原文出处 https://doc.k2er.com/mappings/recoil_zh.html ,2026-10-07 抓取)
///
/// 本程序的落法与对原文的对照:
/// - **控制频率** = 每秒向下"控制"的次数:引擎按 1/频率 的节拍把增量并进瞄准偏移,
///   合并成每拍最多一条 touch_move —— **不逐帧发命令**(方案 §4.13 V2-1 的实现要求);
/// - **控制强度** = 每次控制的向下位移(设备像素,多档)。档位存放在配置里
///   (`strengths`),"当前用哪一档"是**引擎侧运行状态**(配置只由界面写,引擎不改配置);
/// - **鼠标滚轮改变强度** = 勾选后,触发键按住期间滚轮从"缩放"改为"换档"(松开触发键,
///   滚轮仍是缩放)——这样不勾选/没按住时 FPS 滚轮缩放行为完全不变;
/// - **摇晃** = 每次控制在水平方向叠加 ±shake_px 内的随机量(0 = 不打散);
/// - **覆盖灵敏度** = 触发期间鼠标位移改用该灵敏度(<=0 = 不覆盖,仍用左右各自的值);
/// - 原文"需要配合瞄准模式使用"→ 瞄准未激活(不满足 `aim_active`)时整个子系统不生效;
/// - 本项目为**默认关闭**:开启后界面上明示"后坐力补偿"(产品立场,见方案 §3.2.2)。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Recoil {
    /// 总开关。默认关闭(WASD+ 一类竞品明确"永不做自动压枪",本项属可选增强)。
    #[serde(default)]
    pub enabled: bool,
    /// 绑定触发快捷键(evdev 码;0 = 未绑定)。K2er 原文:一般是鼠标左键。
    #[serde(default)]
    pub trigger_key: KeySet,
    /// 控制频率:每秒控制的次数(收敛到 1..=240;默认 60)。
    #[serde(default = "default_recoil_rate")]
    pub rate_hz: f32,
    /// 控制强度:多档,每档 = 每次控制的向下像素位移。空表按单档默认值处理。
    #[serde(default = "default_recoil_strengths")]
    pub strengths: Vec<f32>,
    /// 鼠标滚轮改变强度:触发键(或下面的挡位切换键)按住期间,滚轮优先用于换档
    /// (否则仍是 FPS 缩放)。
    #[serde(default)]
    pub wheel_switch: bool,
    /// 挡位切换键(evdev 码或两键组合,顺序无关;空 = 未绑定)。
    ///
    /// 用户 2026-10-10(第 3 条):按一下换一档(环绕);按住它时滚轮也能换档
    /// (上滚 +1 / 下滚 −1),并且这种时候补偿优先于 FPS 滚轮缩放。
    #[serde(default)]
    pub switch_key: KeySet,
    /// 摇晃:每次控制在左右方向的随机抖动上限(设备像素;0 = 不摇晃)。
    #[serde(default)]
    pub shake_px: f32,
    /// 覆盖灵敏度:触发期间鼠标位移改用该值(<= 0 表示不覆盖)。
    #[serde(default)]
    pub sensitivity: f32,
}

impl Default for Recoil {
    fn default() -> Self {
        Self {
            enabled: false,
            trigger_key: KeySet::new(),
            rate_hz: default_recoil_rate(),
            strengths: default_recoil_strengths(),
            wheel_switch: false,
            switch_key: KeySet::new(),
            shake_px: 0.0,
            sensitivity: 0.0,
        }
    }
}

fn default_recoil_rate() -> f32 {
    60.0
}

fn default_recoil_strengths() -> Vec<f32> {
    vec![6.0]
}

impl Recoil {
    /// 是否具备生效前提(开启 + 绑了触发键)。
    pub fn armed(&self) -> bool {
        self.enabled && self.trigger_key != 0
    }

    /// 第 `index` 档的强度(设备像素;越界收敛,空表返回 0 = 不产生位移)。
    pub fn strength_at(&self, index: usize) -> f32 {
        if self.strengths.is_empty() {
            return 0.0;
        }
        self.strengths[index.min(self.strengths.len() - 1)].max(0.0)
    }
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
            hold_key: KeySet::new(),
            capture_mouse: true,
            toggle_key: KeySet::new(),
            suspend_key: KeySet::new(),
            open_world: false,
            open_world_radius: default_open_world_radius(),
            open_world_smoothing: default_open_world_smoothing(),
            drag_deadzone: default_drag_deadzone(),
            boundary: default_boundary(),
            input_mode: ViewInputMode::TouchDrag,
            wheel_zoom: default_wheel_zoom(),
            wheel_zoom_step: default_wheel_zoom_step(),
            recoil: Recoil::default(),
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
            toggle_key: KeySet::single(66), // KEY_F8
            cursor_toggle_key: KeySet::new(),
            binds: Vec::new(),
            combos_enabled: false,
            tail_delay_enabled: false,
            combos: Vec::new(),
            wheels: vec![Wheel {
                up: KeySet::single(17),    // W
                down: KeySet::single(31),  // S
                left: KeySet::single(30),  // A
                right: KeySet::single(32), // D
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
    /// 切换组合键，最多两个；顺序不影响匹配(见 [`KeySet`])。
    #[serde(default)]
    pub keys: KeySet,
    /// 目标组合在 `schemes` 里的下标
    #[serde(default)]
    pub target: usize,
    /// Target=切到指定组合；Next/Prev=按组合表循环。
    #[serde(default)]
    pub direction: SwitchDirection,
}

impl SwitchKey {
    /// 实际生效的切换按键(旧字段单键或新字段组合,最多 2 个)。
    /// 返回定长栈拷贝(零分配),见 [`KeySet`]。
    pub fn effective_keys(&self) -> KeySet {
        if self.keys.is_empty() {
            KeySet::single(self.key)
        } else {
            self.keys
        }
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
            // W0-8:轮盘几何量一律按相对坐标收敛。v1 像素值不再有换算者
            // (升级路径已按 O-7=B 删除,2026-10-07):v1 标记现在只可能来自
            // 手改 YAML 漏写版本字段,数值无从解释 —— 不收敛的话,1e30 级的
            // 半径会算出巨大推出距离,把引擎(debug 构建)直接算崩。
            for w in &mut p.wheels {
                let radius = clamp_unit_span(w.radius, default_wheel_radius());
                if radius != w.radius {
                    w.radius = radius;
                    changed = true;
                }
                let cx = clamp_unit_pos(w.cx, 0.5);
                let cy = clamp_unit_pos(w.cy, 0.5);
                if cx != w.cx || cy != w.cy {
                    w.cx = cx;
                    w.cy = cy;
                    changed = true;
                }
                let cr = clamp_unit_span(w.center_radius, default_center_radius());
                if cr != w.center_radius {
                    w.center_radius = cr;
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
#   cursor_toggle_key : 鼠标消隐切换键(按一下隐藏系统光标,再按一下恢复;0=未绑定)
#   系统键的"组合键"写法(2026-10-09 起;2026-10-10 扩到滚轮):
#     本文件里以下槽位既可写**单个整数**，也可写**两个键的数组**（顺序无关）:
#       toggle_key / cursor_toggle_key / aim.hold_key / aim.toggle_key /
#       aim.suspend_key / aim.recoil.trigger_key / switch_keys[].keys /
#       wheels[].up / down / left / right / wheels[].temp.key /
#       wheels[].directions[].key
#     例: toggle_key: [29, 66]  表示 Ctrl+F8 同时按住才切换。
#     单键与 0 仍写成裸整数 —— 老版本读得懂，升级不会把配置读坏。
#   screen       : 设计这套布局时的屏幕尺寸 [宽, 高](仅作参考,可不填)
#   binds        : 键位绑定列表
#     - key      : 物理键(evdev 码;鼠标左/右/中=272/273/274,滚轮=277上/278下/279左/280右)
#       action   : 动作,取值见下
#       fps_only : true 时仅在 FPS 模式生效/显示(鼠标技能键建议开启)
#       tail_delay_ms : 「按后延迟」(毫秒,默认 0 = 不等,不写就是 0)。
#                  这一条**上一次抬起之后**至少再等这么久才接受下一次按下 ——
#                  快速连点 / 连续划动时,防止上一动作还没抬起、下一动作已经按下。
#                  冷却没过就按下来的那一次**不会丢**,会被推迟到冷却结束再执行。
#                  这是**可选功能**,要两层都愿意才生效:本字段 > 0,且下面的
#                  `tail_delay_enabled: true`。宏按"整条跑完"再算冷却。
#   combos_enabled : 是否启用组合键(默认 false)。关闭时 combos 保留但全部失效。
#   tail_delay_enabled : 「按后延迟」总开关(默认 false)。关闭时每条 tail_delay_ms
#                  一律按 0 处理(数值原样保留,随时可以打开)。
#   combos        : 组合键列表。keys[0] 是前缀键,后续 keys 与前缀同时按住才触发:
#     - keys      : 物理键 evdev 码数组,至少两个;例如 [29, 19] = Ctrl + R
#       action    : 组合触发时执行的动作(与 binds.action 相同)
#       fps_only  : true 时只在 FPS 模式运行期间生效
#       tail_delay_ms : 同上,这一条组合键自己的「按后延迟」(默认 0 = 不等)
#   wheels       : 虚拟摇杆(轮盘)列表
#     up/down/left/right : 四个方向的物理键(evdev 码,或最多两键的组合)
#     cx, cy     : 摇杆中心(相对坐标 0..1)
#     radius     : 视觉半径(相对屏幕宽度的比例)
#     scope      : 影响范围倍数(手指实际被推离中心的距离 = radius × scope)
#     mode       : classic(经典)/sensitive(灵敏,同轴后按覆盖)
#     directions : 自定义/执行轮盘的方向列表(angle_deg 角度 + key 物理键,
#                  key 同样可写最多两键的组合) —— standard 轮盘不用它
#     temp       : 可选。临时轮盘:key = 启用键(evdev 码,或最多两键的组合),
#                  mode = Hold|Toggle
#                  (与文件里其它枚举一样,取值首字母大写 —— 这是实际序列化写法)
#   aim          : 鼠标视角(FPS / 开放世界)
#     enabled / anchor_x / anchor_y : 是否启用 + 手指落下的锚点(相对坐标)
#     sensitivity_x / sensitivity_y : 每 1 个鼠标计数对应的设备像素
#     move_speed : view speed multiplier (0.2..3.0, default 1.0; touch/open-world/gamepad)
#     invert_y   : 是否反转纵向
#     recenter   : 归中策略 Idle|Threshold|Never(首字母大写,同上)
#     recenter_idle_ms / recenter_threshold : 静止归中时长 / 阈值归中的偏移阈值
#     hold_key   : 仅当该鼠标键按住时才瞄准(evdev 码或两键数组;0 = 始终瞄准)
#     capture_mouse : FPS 模式指针消隐(默认 true)
#     toggle_key : 独立启停 FPS 模式的按键或组合键(0 = 未绑定)
#     suspend_key: “按住才退出”,按住时暂时退出 FPS、恢复普通映射并显示光标(0 = 未绑定)
#     recoil.trigger_key : 后坐力补偿触发键或组合键(0 = 未绑定)
#     open_world : true 时启用开放世界模式(不要求射击/开镜,无限水平转向),
#                  FPS 模式的一种变体 —— 界面统一叫「FPS 模式」
#     open_world_radius : 水平触摸拖动带的回中半径(相对屏幕宽度,默认 0.22)
#     open_world_smoothing : 位移平滑系数(0.15~1.0,默认 0.85)
#     input_mode : touch_drag(universal touch drag) / virtual_gamepad_continuous /
#                  virtual_gamepad_segmented (Xbox 360 HID right stick; segmented recenters at limit)
#                  legacy uhid_mouse / aoa_mouse are migrated to touch_drag.
#                  [已废弃 2026-10-08] virtual_gamepad_continuous / virtual_gamepad_segmented
#                  不再维护:仍可读入并照旧生效,但不会被修复或扩展。
#   look         : 外观(配色/密度/背景图),随组合一起保存
#
# 动作(action)五种写法(注意类型用 YAML 标签标出,即 !Tap 这种写法;
# 手改时要连感叹号一起写,否则解析会失败):
#   !Tap       : 点按。x, y 为落点(相对坐标),duration_ms=0 表示按住不松手
#                直到再次按下同一键才抬起;radius 为响应范围
#   !Hold      : 长按。键盘按下即落指、松开即抬指,x, y, radius 同上
#   !Swipe     : 滑动。start/end 为起终点,duration_ms 为时长,
#                easing 为缓动,path 为轨迹(取值见界面里的下拉选项)
#   !AndroidKey: 注入 Android 系统键。keycode 例:4=返回, 3=主页, 187=最近任务
#   !Macro     : 宏。steps = 录制得到的按键步骤(自动合并自动重复);
#                instructions = 设置宏的语义操作,每项用 type 区分:
#                delay / key / combo / wheel / fps / click / swipe / macro;
#                virtual_profile 存在时为"扩展宏"的虚拟键位层。
#                (两项都由界面生成;手改容易与界面状态对不上)
# ============================================================================
"#;

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

    /// KeySet(用户 2026-10-09 第 4 条):组合键槽位的读写必须与老配置**互通**。
    ///
    /// 未绑定与单键写成裸整数(老版本读得懂、文件看着也清爽),两键才写数组。
    #[test]
    fn key_set_serializes_single_and_empty_as_plain_integer() {
        assert_eq!(serde_json::to_string(&KeySet::new()).unwrap(), "0");
        assert_eq!(serde_json::to_string(&KeySet::single(66)).unwrap(), "66");
        assert_eq!(
            serde_json::to_string(&KeySet::from_keys([29, 45])).unwrap(),
            "[29,45]"
        );
        assert_eq!(serde_json::from_str::<KeySet>("0").unwrap(), KeySet::new());
        assert_eq!(
            serde_json::from_str::<KeySet>("66").unwrap(),
            KeySet::single(66)
        );
        assert_eq!(
            serde_json::from_str::<KeySet>("[45,29]").unwrap(),
            KeySet::from_keys([29, 45]),
            "顺序无关:读回来的集合与写出去的等价"
        );
        // 手写的 YAML 里多塞了键也不该报错:只留前两个(与界面捕获的上限一致)
        assert_eq!(
            serde_json::from_str::<KeySet>("[1,2,3]").unwrap(),
            KeySet::from_keys([1, 2])
        );
    }

    /// KeySet 的相等与"与单个键比较"的语义(全程序大量沿用单键写法)。
    #[test]
    fn key_set_equality_ignores_order_and_is_exact_against_a_single_code() {
        assert_eq!(KeySet::from_keys([29, 45]), KeySet::from_keys([45, 29]));
        assert_eq!(KeySet::from_keys([0, 45, 45]), KeySet::single(45));
        // 与单个键码比较 = "这个槽位**正好**只有这一个键"
        assert_eq!(KeySet::single(66), 66u16);
        assert_eq!(KeySet::new(), 0u16);
        assert_ne!(KeySet::from_keys([29, 45]), 29u16);
        assert_ne!(KeySet::from_keys([29, 45]), KeySet::single(29));
        // 按持判定:整个集合都按着才算(空集合恒 false)
        assert!(KeySet::from_keys([29, 45]).all_held_by(|k| k == 29 || k == 45));
        assert!(!KeySet::from_keys([29, 45]).all_held_by(|k| k == 29));
        assert!(!KeySet::new().all_held_by(|_| true));
    }

    /// 捕获后的显示顺序:`Ctrl+X` 而不是用户真实的按下顺序(`X+Ctrl`)。
    #[test]
    fn canonical_chord_puts_modifiers_first() {
        let c = canonical_chord(&[45, 29]);
        assert_eq!(c.as_slice(), &[29, 45], "修饰键必须排在前面");
        assert_eq!(canonical_chord(&[29, 45]).as_slice(), &[29, 45]);
        assert_eq!(
            canonical_chord(&[29, 0, 29]).as_slice(),
            &[29],
            "去重并滤 0"
        );
        assert!(canonical_chord(&[29, 45]).label().contains('+'));
        assert_eq!(KeySet::new().label(), "");
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

    /// 旧版(0.1.2 及以前)写出的 json 仍能原样读入,并按像素直通使用:
    /// 坐标是整数、没有 format_version / screen / look。
    /// v1→v2 自动升级已删(2026-10-07 O-7=B)——"直通、不换算"是唯一行为。
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
        let p: Profile = serde_json::from_str(legacy).expect("旧配置应仍可解析");
        assert_eq!(p.format_version, 1);
        assert_eq!(p.coord_unit(), CoordUnit::Pixel);
        assert_eq!(
            p.aim.move_speed, 1.0,
            "old aim configs default to 1.0x speed"
        );

        // 像素直通:与旧版行为逐一相同
        let m = p.mapper((1080, 2400));
        match &p.binds[0].action {
            Action::Tap { x, y, .. } => assert_eq!((m.x(*x), m.y(*y)), (540, 1200)),
            _ => unreachable!(),
        }
        match &p.binds[1].action {
            Action::Swipe(s) => assert_eq!(m.point(s.start.0, s.start.1), (100, 200)),
            _ => unreachable!(),
        }
        assert_eq!((m.x(p.aim.anchor_x), m.y(p.aim.anchor_y)), (810, 1200));
        assert_eq!(m.point(p.wheels[0].cx, p.wheels[0].cy), (300, 900));
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

    /// 方向的"手动终点"(2026-10-08「设置位置」)读写兼容:
    /// 老配置没有这个字段 → None(跟随基准圆,行为与旧版逐一一致);
    /// 手改点原样往返;None 时不写字段(老版本读新配置不会看到陌生键)。
    #[test]
    fn wheel_direction_manual_round_trips_and_defaults_to_none() {
        let old = r#"{"angle_deg": 0.0, "key": 32}"#;
        let d: WheelDirection = serde_json::from_str(old).expect("老方向应可解析");
        assert_eq!(d.manual, None, "缺省必须跟随基准圆");
        assert_eq!(d.angle_deg, 0.0);
        assert_eq!(d.key, 32);

        let with = WheelDirection {
            angle_deg: 33.0,
            key: KeySet::single(55),
            manual: Some((0.9, 0.25)),
        };
        let text = serde_json::to_string(&with).unwrap();
        assert!(text.contains("manual"), "手改点必须写进配置:{text}");
        let back: WheelDirection = serde_json::from_str(&text).unwrap();
        assert_eq!(back, with);

        let none = WheelDirection {
            manual: None,
            ..with
        };
        let text = serde_json::to_string(&none).unwrap();
        assert!(!text.contains("manual"), "None 不该写出字段:{text}");
        assert_eq!(serde_json::from_str::<WheelDirection>(&text).unwrap(), none);
    }

    /// 斜向推出距离不应超过 scope 描述的范围(归一化后半径分量 < 半径)
    #[test]
    fn wheel_push_px_is_a_radius_scale() {
        let w = Wheel {
            up: KeySet::single(17),
            down: KeySet::single(31),
            left: KeySet::single(30),
            right: KeySet::single(32),
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
                (
                    KeySet::single(17),
                    KeySet::single(31),
                    KeySet::single(30),
                    KeySet::single(32)
                ),
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
                p.toggle_key = KeySet::single(65); // F7
                p.binds.push(KeyBind {
                    fps_only: false,
                    key: 22, // U
                    action: Action::Tap {
                        x: 0.42,
                        y: 0.31,
                        duration_ms: 40,
                        radius: 0.05,
                    },
                    tail_delay_ms: 0,
                });
                p.binds.push(KeyBind {
                    fps_only: false,
                    key: 23,
                    action: Action::AndroidKey { keycode: 4 },
                    tail_delay_ms: 0,
                });
                p.wheels[0].temp = Some(TempWheel {
                    key: KeySet::single(57),
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

    /// V2-1 压枪:老配置(aim 里没有 `recoil` 字段)读入后必须"关闭 + 默认参数";
    /// 档位取值越界收敛与 armed 前提一并锁死。
    #[test]
    fn recoil_defaults_off_and_strength_clamps() {
        // 老配置:没有 recoil 字段(其余字段按当年 `Aim` 的必填项给全)
        let old = r#"{
            "enabled": true, "anchor_x": 0.5, "anchor_y": 0.5,
            "sensitivity_x": 2.0, "sensitivity_y": 2.0, "invert_y": false,
            "recenter": "Idle", "recenter_idle_ms": 120, "recenter_threshold": 400,
            "hold_key": 0
        }"#;
        let a: Aim = serde_json::from_str(old).expect("老配置应可解析");
        assert!(!a.recoil.enabled, "缺省默认必须是关闭");
        assert_eq!(a.recoil.rate_hz, 60.0);
        assert_eq!(a.recoil.trigger_key, 0);
        assert_eq!(a.recoil.strengths, vec![6.0]);

        // armed:开启 + 绑了触发键才算具备前提(只开不绑 = 无处触发)
        let armed = Recoil {
            enabled: true,
            trigger_key: KeySet::single(BTN_LEFT),
            ..Recoil::default()
        };
        assert!(armed.armed());
        let unbound = Recoil {
            enabled: true,
            trigger_key: KeySet::new(),
            ..Recoil::default()
        };
        assert!(!unbound.armed(), "没绑触发键不算 armed");

        // strength_at:越界收敛到最后一档;空表返回 0(不产生位移)
        let r = Recoil {
            strengths: vec![3.0, 9.0],
            ..Recoil::default()
        };
        assert_eq!(r.strength_at(0), 3.0);
        assert_eq!(r.strength_at(9), 9.0, "越界必须收敛到最后一档");
        let empty = Recoil {
            strengths: vec![],
            ..Recoil::default()
        };
        assert_eq!(empty.strength_at(0), 0.0);
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

    /// W0-8 回归(P0-8):手改 YAML 把轮盘几何量写坏(负数/NaN/超大/出屏),
    /// 加载后必须收敛成合法值。
    ///
    /// 不收敛的后果:半径 × 屏宽算出巨大的推出距离 → 注入点飞出屏幕被设备端
    /// 整条丢弃("这个方向推不动");debug 构建下 i32 加法还会溢出 panic,
    /// 引擎线程一死,之后所有按键静默失效。
    #[test]
    fn normalize_clamps_wheel_geometry() {
        let mut doc = ConfigFile::default();
        let profile = doc.schemes.first_mut().expect("默认组合");
        profile.wheels[0].radius = 1.0e30; // 超大
        profile.wheels[0].cx = f32::NAN; // 非有限
        profile.wheels[0].cy = -3.0; // 负数(出屏)
        profile.wheels[0].center_radius = f32::INFINITY;

        assert!(doc.normalize(), "写坏的几何量必须报告改动");
        let w = &doc.schemes[0].wheels[0];
        assert!(
            w.radius.is_finite() && w.radius > 0.0 && w.radius <= 1.0,
            "半径必须收敛到 (0,1],实际 {}",
            w.radius
        );
        assert!(
            (0.0..=1.0).contains(&w.cx) && (0.0..=1.0).contains(&w.cy),
            "中心必须落在屏内,实际 ({}, {})",
            w.cx,
            w.cy
        );
        assert!(
            w.center_radius.is_finite() && w.center_radius > 0.0 && w.center_radius <= 1.0,
            "中心半径必须收敛到 (0,1],实际 {}",
            w.center_radius
        );
        assert!(!doc.normalize(), "规整过一次之后就该自洽了");

        // v1 标记(手改 YAML 漏写版本字段)同样一律收敛:升级路径已删
        // (2026-10-07 O-7=B),像素值已无换算者,原样留着只会让 1e30 级
        // 坏值绕过保护。radius=150 收敛到上限 1.0,cx=300 收敛到屏内 1.0。
        let mut legacy = ConfigFile::default();
        legacy.schemes[0].format_version = 1;
        legacy.schemes[0].wheels[0].radius = 150.0;
        legacy.schemes[0].wheels[0].cx = 300.0;
        assert!(legacy.normalize(), "v1 标记也要收敛并改写成当前版本");
        assert_eq!(
            (
                legacy.schemes[0].wheels[0].radius,
                legacy.schemes[0].wheels[0].cx,
                legacy.schemes[0].format_version,
            ),
            (1.0, 1.0, PROFILE_VERSION),
            "v1 像素值不再有豁免:一律按相对坐标收敛"
        );
    }
}
