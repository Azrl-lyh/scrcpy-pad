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

/// 「卡死自愈」的缺省值:**开**(见 [`Aim::self_heal`])。
///
/// 为什么这里可以"默认开"而其它新功能都默认关:它**只在出问题(手离开手机 /
/// 设备端账本与本地对不上)时才动作**,正常滑动时一次都不动 —— 手感逐位不变。
fn default_self_heal() -> bool {
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

/// 折线总长(像素;界面显示与方向标间距用它)。
///
/// 只提供像素口径:轨迹的"多长"是给用户看的数,而用户看的是屏幕像素。
/// 相对坐标口径(0~1)的长度对用户没有意义(换个分辨率数字就变),
/// 所以不提供 —— 少一个 API 就少一处口径漂移。
pub fn polyline_len_px(pts: &[(i32, i32)]) -> f32 {
    pts.windows(2)
        .map(|w| {
            let dx = (w[1].0 - w[0].0) as f32;
            let dy = (w[1].1 - w[0].1) as f32;
            (dx * dx + dy * dy).sqrt()
        })
        .sum()
}

/// 沿折线按**弧长等距**取方向标锚点(用户 2026-10-13,V2-3)。
///
/// 返回 `(位置, 单位方向)`;`count` 个锚点均匀落在折线内部(不取两端 ——
/// 两端各有自己的"起/终"标记,箭头挤在那里只会糊成一团)。
///
/// 为什么必须有方向标:只有一条折线时,一条**来回折返**的轨迹与一条直线
/// 在截图上看不出区别;加上箭头才读得出"从哪划到哪、在哪折返"。
///
/// 边界:点数 < 2、总长为 0(所有点重合)、`count == 0` ⇒ 返回空
/// (没有方向可言,调用方据此不画箭头)。
pub fn track_arrow_marks(points: &[(i32, i32)], count: usize) -> Vec<((i32, i32), (f32, f32))> {
    if points.len() < 2 || count == 0 {
        return Vec::new();
    }
    let total = polyline_len_px(points);
    if total <= f32::EPSILON {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(count);
    // 沿折线走过的累计长度,便于"按弧长定位"
    let mut seg = 0usize;
    let mut seg_start = 0.0f32;
    for k in 1..=count {
        // 目标弧长:均分在 (0, total) 开区间内
        let want = total * k as f32 / (count + 1) as f32;
        // 前进到覆盖 `want` 的那一段
        while seg + 1 < points.len() {
            let dx = (points[seg + 1].0 - points[seg].0) as f32;
            let dy = (points[seg + 1].1 - points[seg].1) as f32;
            let d = (dx * dx + dy * dy).sqrt();
            if seg_start + d >= want && d > f32::EPSILON {
                let t = (want - seg_start) / d;
                let px = points[seg].0 as f32 + dx * t;
                let py = points[seg].1 as f32 + dy * t;
                out.push(((px.round() as i32, py.round() as i32), (dx / d, dy / d)));
                break;
            }
            seg_start += d;
            seg += 1;
        }
        if seg + 1 >= points.len() {
            // 兜底:累计误差导致走到末尾仍未取满 —— 用最后一段的方向收尾
            let a = points[points.len() - 2];
            let b = points[points.len() - 1];
            let dx = (b.0 - a.0) as f32;
            let dy = (b.1 - a.1) as f32;
            let d = (dx * dx + dy * dy).sqrt();
            if d > f32::EPSILON {
                out.push((b, (dx / d, dy / d)));
            }
            break;
        }
    }
    out
}

/// 抽稀折线到至多 `max` 个点(**保留首末点与顺序**)。
///
/// 为什么需要:录制上限 2000 点,而一帧里可能同时要画好几条轨迹;直接把
/// 2000 个屏幕点交给绘制层既费时又看不出更多细节(屏幕上一像素内挤几十个点)。
/// 抽稀到 256 点后曲线肉眼无差,绘制开销有界。
///
/// `max < 2` 或本来就够短 ⇒ 原样返回。
pub fn decimate(points: &[(i32, i32)], max: usize) -> Vec<(i32, i32)> {
    if max < 2 || points.len() <= max {
        return points.to_vec();
    }
    let mut out: Vec<(i32, i32)> = Vec::with_capacity(max);
    let step = (points.len() - 1) as f32 / (max - 1) as f32;
    for i in 0..max {
        let idx = (i as f32 * step).round() as usize;
        let p = points[idx.min(points.len() - 1)];
        // 取整可能产生重复下标,去重(末点必须留)
        if out.last() != Some(&p) {
            out.push(p);
        }
    }
    let last = points[points.len() - 1];
    if out.last() != Some(&last) {
        out.push(last);
    }
    out
}

/// 轨迹录制的点数上限。
///
/// 2000 点足够表达任何手绘路径(即使每 4px 一个点也覆盖 8000px 的路径),
/// 又保证有界:不会因为长时间录制撑爆内存,也不会把注入与绘制拖慢。
pub const TRACK_MAX_POINTS: usize = 2000;

/// 录制时相邻两点的最小间距(曼哈顿距离,像素)。
///
/// 抽稀用:鼠标静止时会不停上报同一个位置,不抽稀就会堆出一串重合点
/// (画出来看不出,却白占内存与注入时间)。4px 是"肉眼已看不出折角"的量级。
pub const TRACK_MIN_STEP_PX: i32 = 4;

/// 往录制缓冲里追加一个采样点,返回是否真的记下了。
///
/// # 为什么抽成纯函数
///
/// 抽稀与上限是**录制功能唯一的采样规则**,草稿/已有键位/已有组合键三种目标
/// 必须完全一致(否则同一只手划出的轨迹会因为录到哪个目标而疏密不同)。
/// 做成纯函数后,这条规则可以被单测直接钉住,而不必构造整个 `App`。
///
/// 规则:
/// ① 与前一点的曼哈顿距离 < [`TRACK_MIN_STEP_PX`] ⇒ 不记(鼠标静止不堆点);
/// ② 已达 [`TRACK_MAX_POINTS`] ⇒ 不记(有界,但**不清空** —— 用户看得见
///    点数停在 2000,不会发现前半段凭空消失);
/// ③ 空缓冲 ⇒ 第一个点无条件记下(否则永远起不了头)。
pub fn push_track_sample(points: &mut Vec<(i32, i32)>, px: i32, py: i32) -> bool {
    if points.len() >= TRACK_MAX_POINTS {
        return false;
    }
    let far_enough = points
        .last()
        .is_none_or(|&(lx, ly)| (px - lx).abs() + (py - ly).abs() >= TRACK_MIN_STEP_PX);
    if far_enough {
        points.push((px, py));
        true
    } else {
        false
    }
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
    /// 「长按重复」(用户 2026-10-11 第 2 条,默认关闭):按住触发键时一轮接一轮
    /// 重放整条宏,直到抬起。**松手后正在跑的那一轮跑完即停** —— 不排下一轮,
    /// 设备端不会留下"只按不抬"的触点。
    ///
    /// 两轮之间的间隔 = 触发处(键位 / 组合键 / 宏页)自己的「按后延迟」
    /// `tail_delay_ms`,且**受 [`Profile::tail_delay_enabled`] 总开关管**:
    /// 总开关关着时一律按 0 处理,一轮接一轮不等待。
    #[serde(default)]
    pub repeat_while_held: bool,
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
        /// **连点间隔**(ms,用户 2026-10-13,V2-2):按住期间每隔这么久点一次。
        ///
        /// `0`(缺省)= 关闭 = 与从前**逐位一致**(按住只点一次)。
        /// 语义照原计划文档:由引擎本地排程**一次性展开**,不逐击过网 ——
        /// 于是每击之间的抖动只受本地定时器影响(目标 <5ms),不受 adb 往返影响。
        /// 松键立刻停,不留"只按不抬"的触点。
        #[serde(default, skip_serializing_if = "is_zero_u32")]
        repeat_interval_ms: u32,
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
    /// **录制的轨迹点**(相对坐标,用户 2026-10-13,V2-2)。
    ///
    /// 空(缺省)= 走现在的"起点→终点 + 曲线/路径"行为 —— **默认逐位不变**。
    /// 非空 = 按下即按这些点依次注入,时间轴按**点间隔距离占比**分配总
    /// `duration_ms`:距离越长的段耗时越多,于是手势的"速度感"与录制时接近
    /// (等分会让慢速段看起来突然加速)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recorded_points: Vec<(f32, f32)>,
}

/// serde 辅助:`0` 时不写进 YAML(老配置/未启用时文件保持干净)
fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}

impl Swipe {
    /// 这条滑动实际要走的点列(用户 2026-10-13)。
    ///
    /// 有录制轨迹就用它;否则退回"起点→终点"(与既有实现完全一致)。
    /// 抽成方法是为了让引擎与界面**共用同一个口径**,避免两处各写一份判断。
    pub fn point_path(&self) -> Vec<(f32, f32)> {
        if self.recorded_points.len() >= 2 {
            self.recorded_points.clone()
        } else {
            vec![self.start, self.end]
        }
    }

    /// 按点间距离把总时长分段(用户 2026-10-13)。
    ///
    /// 返回每个点应处的**累计时刻**(ms),长度与 `point_path()` 相同,
    /// 首元素 0、末元素 = `duration_ms`。
    ///
    /// 为什么按距离而不是等分:用户录制的轨迹里"快划"与"慢移"混在一起,
    /// 等分会让慢速段被压缩、看起来突然加速。按距离分配能让速度感接近录制。
    /// 全部点重合(总距离 0)时退化为等分,避免除零。
    pub fn point_timeline_ms(&self) -> Vec<u32> {
        let pts = self.point_path();
        let n = pts.len();
        if n == 0 {
            return Vec::new();
        }
        if n == 1 {
            return vec![0];
        }
        let total = self.duration_ms as f32;
        // 逐段距离
        let mut seg: Vec<f32> = Vec::with_capacity(n - 1);
        let mut sum = 0.0f32;
        for w in pts.windows(2) {
            let d = ((w[1].0 - w[0].0).powi(2) + (w[1].1 - w[0].1).powi(2)).sqrt();
            seg.push(d);
            sum += d;
        }
        let mut out: Vec<u32> = Vec::with_capacity(n);
        out.push(0);
        let mut acc = 0.0f32;
        for (i, d) in seg.iter().enumerate() {
            // 总距离为 0(所有点重合)⇒ 等分兜底
            let frac = if sum > f32::EPSILON {
                d / sum
            } else {
                1.0 / (n - 1) as f32
            };
            acc += frac;
            let t = (total * acc).round() as u32;
            // 末点强制 = duration_ms(吸收取整误差)
            out.push(if i + 1 == n - 1 { self.duration_ms } else { t });
        }
        out
    }
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
            /// 录制的轨迹点(用户 2026-10-13,V2-2);老配置没有 ⇒ 空 ⇒ 走原路径
            recorded_points: Option<Vec<(f32, f32)>>,
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
            recorded_points: r.recorded_points.unwrap_or_default(),
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
    /// 「长按重复」打开时,它同时是**两轮之间**的间隔。
    #[serde(
        default = "default_tail_delay_ms",
        skip_serializing_if = "tail_delay_is_default"
    )]
    pub tail_delay_ms: u32,
    /// 「长按重复」(用户 2026-10-11 第 2 条,默认关闭):整组键都按着时循环重放
    /// 这一组合的动作,任一成员抬起即不再排下一轮(正在跑的那一轮跑完)。
    /// 间隔用组合自己的 [`KeySet`] 之外的 `tail_delay_ms`,同受
    /// [`Profile::tail_delay_enabled`] 总开关管。
    #[serde(default)]
    pub repeat_while_held: bool,
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

/// 「鼠标独占(隔膜)」的作用范围(S①,用户 2026-10-11 第二个问题:
/// "真的不能 scrcpy 运行时切换可否使用鼠标点击吗?")。
///
/// 背景:隔膜本身是**运行时可切换**的 —— 它只是捕获层里的一枚原子开关,
/// 钩子回调每个事件现读它(见 [`crate::capture::mouse_shield_swallows`]),
/// 翻转即刻生效、**不需要重启 scrcpy**。以前它只由"已连接 × FPS 生效"驱动,
/// 现在把"什么时候独占"这件事交给用户选。
///
/// 判据统一由纯函数 [`mouse_shield_on`] 给出(便于单测)。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum MouseShieldMode {
    /// **默认**:只在 FPS 模式生效期间独占 —— 与今天的行为逐位一致
    /// (不改变任何既有体验)。
    #[default]
    FpsOnly,
    /// 只在**投屏窗口(scrcpy)处于前台**时独占:其他窗口里鼠标照常
    /// (用户明确说这条"特别推荐" —— 打游戏时独占,查攻略/聊天时不挡)。
    ScrcpyForeground,
    /// 映射开启且已连接就一直独占。
    Always,
}

impl MouseShieldMode {
    pub fn label(self) -> &'static str {
        match self {
            MouseShieldMode::FpsOnly => "仅 FPS 模式",
            MouseShieldMode::ScrcpyForeground => "仅投屏窗口在前台",
            MouseShieldMode::Always => "始终独占",
        }
    }

    /// 界面下拉用的全部取值(顺序 = 从最保守到最激进)。
    pub const ALL: [MouseShieldMode; 3] = [
        MouseShieldMode::FpsOnly,
        MouseShieldMode::ScrcpyForeground,
        MouseShieldMode::Always,
    ];

    /// 这一档在当前平台**是否真的能生效**(2026-10-22,平台不对等修复)。
    ///
    /// `ScrcpyForeground` 依赖"投屏窗口是否在前台",而前台进程查询
    /// (`capture::foreground_pid`)只有 Windows 有实现:非 Windows 恒返回
    /// `None` ⇒ `scrcpy_foreground` 恒 `false` ⇒ 这一档**恒等于从不独占**。
    ///
    /// 以前界面上三档一律可选、提示语一字未提平台,Linux 用户选了会发现
    /// "没生效"却查不出原因(只读审计 2026-10-22 发现)。现在由本函数给出
    /// 可用性,界面据此置灰并说明 —— 不静默降级成别的档,也不删掉这一档
    /// (Windows 上它是最有用的那档)。
    ///
    /// 参数显式传 `windows` 而不是在函数里读 `cfg!`:跨平台分支才能在
    /// **同一个测试进程**里两种取值都验一遍。
    pub fn usable_on(self, windows: bool) -> bool {
        match self {
            MouseShieldMode::FpsOnly | MouseShieldMode::Always => true,
            MouseShieldMode::ScrcpyForeground => windows,
        }
    }
}

/// 「鼠标独占(隔膜)」此刻是否该开(S①,纯函数便于单测)。
///
/// 判据(顺序即优先级):
/// 1. **硬闸**:映射关闭、或控制通道没连上 → 一律 `false`。理由与既有实现同:
///    断连时引擎一个触摸都注入不出去,隔膜留着只会让用户点不了任何窗口;
///    关闭映射更是"我要正常用鼠标了"的明确信号。
/// 2. 手动开关([`Aim::shield_key`],S②)为真 → 开(无视 `mode`)。
/// 3. 否则按 `mode`:
///    - [`MouseShieldMode::FpsOnly`] → `fps_active`(今天的行为);
///    - [`MouseShieldMode::ScrcpyForeground`] → `scrcpy_foreground`;
///    - [`MouseShieldMode::Always`] → 一直开。
///
/// `scrcpy_foreground` 的判据是"前台窗口的进程 pid == 我们启动的 scrcpy
/// 子进程 pid";拿不到 pid(没启动 scrcpy / 查不到)时必须传 `false`
/// —— **保守地不独占**。
///
/// ⚠ 与任务书里那个字面公式的一处**故意不同**:字面写成
/// `映射开启 && 已连接 && 手动 || 模式条件`,那样在"映射关闭 + Always"
/// 下会算出 `true`(鼠标被吞、映射却是关的)。这里把"映射开启 && 已连接"
/// 提到最外层当**硬闸**,任何模式都过不去 —— 与既有注释"映射关闭:隔膜
/// 绝不能留着"以及 `Always` 自己的定义("映射开启且已连接就一直独占")一致。
pub fn mouse_shield_on(
    mode: MouseShieldMode,
    mapping_enabled: bool,
    connected: bool,
    fps_active: bool,
    scrcpy_foreground: bool,
    manual_override: bool,
) -> bool {
    if !mapping_enabled || !connected {
        return false;
    }
    if manual_override {
        return true;
    }
    match mode {
        MouseShieldMode::FpsOnly => fps_active,
        MouseShieldMode::ScrcpyForeground => scrcpy_foreground,
        MouseShieldMode::Always => true,
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
    /// 「鼠标独占(隔膜)」的作用范围(S①)。默认 = 仅 FPS 模式生效期间
    /// (与今天的行为逐位一致)。判据见 [`mouse_shield_on`]。
    #[serde(default)]
    pub mouse_shield: MouseShieldMode,
    /// 「鼠标独占」的**手动开关**热键(S②;空 = 未绑定)。
    ///
    /// 按一下翻转"手动独占"(只认物理上升沿、宏事件不算 —— 与 FPS 开关键同一
    /// 条纪律),再按一下关闭。手动为真时无视 [`Self::mouse_shield`] 的模式
    /// **一律独占**。
    ///
    /// 为什么必须有这样一个键:隔膜开着时**连本程序界面都点不了**(鼠标消息不
    /// 投递给任何窗口),唯一还通的路是键盘事件 —— 钩子对键盘是"先转发后吞",
    /// 所以热键不受隔膜影响(见 `capture::mouse_shield_swallows`)。界面左栏会
    /// 把当前键名与当前状态一并写出来,告诉用户"怎么关掉它"。
    #[serde(default)]
    pub shield_key: KeySet,
    /// 「唤醒视角」热键(空 = 未绑定)。
    ///
    /// 真机定性(2026-10-11):锁死时我们的通道与进程都还活着,而**鼠标点不了
    /// 任何界面**,唯一还通的路是键盘 —— 用户按 F8(关映射 → `release_all`
    /// 抬掉全部触点)能立刻恢复。两种恢复手段唯一共同的动作就是**把我们按着的
    /// 那根瞄准指抬起来**。这个键就把那一下单独摘出来:按一次 → 抬起瞄准指,
    /// **不关映射、不退出 FPS**;下一次鼠标位移会自动重新落指(`aim_lift` 已把
    /// 偏移与计时清零,见它的实现)。
    ///
    /// 与 [`Self::shield_key`] 的区别:那个键翻转"鼠标独占"的手动开关,这个键
    /// **只抬指、一下就好**。默认未绑定 = 行为与从前逐位一致(不动默认手感)。
    #[serde(default)]
    pub wake_key: KeySet,
    /// 「强制重连控制通道」热键(空 = 未绑定)。
    ///
    /// 备用逃生门:锁死时若"抬指"救不回来,坏的可能是我们这条 control-only
    /// 通道(server 的进程与 socket 都还在,设备端读线程却已经死了 —— 存活自检
    /// 抓不到这一半)。按一下等同诊断面板里的[强制重连控制通道]:**只重启我们
    /// 自己的控制通道,不动 scrcpy 投屏**。按键处理在界面侧(工作线程 + 请求),
    /// 不在持锁状态里做 adb/网络。
    ///
    /// 默认未绑定 = 行为与从前逐位一致。
    #[serde(default)]
    pub reconnect_key: KeySet,
    /// 「卡死自愈」总开关(2026-10-12,**默认开**)。
    ///
    /// 用户第 6 条 bug(手碰一下手机屏 ⇒ 注入输入全失效,只有按键自救能夺回)的
    /// 真修复:把用户手动按的那一下自动化。它**只在出问题时才动作**,因此默认开
    /// 也不改变正常滑动的手感(正常滑动仍然是既有的"重叠换手"DOWN→UP,
    /// 自愈一个字都不碰它)。
    ///
    /// 两个触发条件,任一成立即"抬指重建":
    /// 1. **手离开手机**(只读监听物理触摸屏的 Up 边沿,见
    ///    [`crate::adb::watch_physical_touch`]);
    /// 2. **设备端账本与本地对不上**(`dumpsys input` 的 `TouchStates` 触点数
    ///    是 0 而我们本地认为瞄准指还按着;或账本触点数比本地多 = 我们发出的
    ///    抬起没到设备端/手势被丢)。
    ///
    /// "抬指重建"= [`crate::engine`] 里「唤醒视角」键的**同一个动作**:抬起此刻
    /// 按着的那根瞄准指(fn `aim_lift`),**不关映射、不退出 FPS、不改 mode/挂起/
    /// 门控/总映射**,下一次鼠标位移会自动重新落指。限流 300ms;宏步进 / 绑定捕获
    /// / 截图取点期间不自愈。
    ///
    /// 关掉 = 回到**纯手动自救键**(唤醒视角 / 强制重连两个热键照旧可用),
    /// 与从前逐位一致。老配置里根本没有这一项 ⇒ `serde(default)` 读成 `true`。
    #[serde(default = "default_self_heal")]
    pub self_heal: bool,
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
    /// 档位名字(与 `strengths` 一一对应的**平行向量**;缺省/越界 = 无名字)。
    ///
    /// 用户 2026-10-11:"档位可以加一个名字"。为什么是平行向量而不是把
    /// `strengths` 换成结构体数组:`strengths` 是老配置里就有的字段,类型与语义
    /// 都不许动(见 `recoil_defaults_off_and_strength_clamps`);平行向量 + 长度
    /// 不足按规则补齐,老文件读进来才与从前逐位一致。
    #[serde(default)]
    pub gear_names: Vec<String>,
    /// 该档是否参与"滚轮换档"(与 `strengths` 一一对应的平行向量)。
    ///
    /// **缺省/越界一律按 `true`**:老配置里根本没有这一项,读进来必须与
    /// "全部档位都参与"的老行为逐位一致(见 [`Self::gear_wheel_at`])。
    #[serde(default)]
    pub gear_wheel: Vec<bool>,
    /// 每档的**水平偏置**(与 `strengths` 一一对应的平行向量,H②)。
    ///
    /// 单位与语义:每次控制的水平位移(设备像素),**正 = 向右**(见
    /// [`Self::gear_horizontal_at`] 与界面 hover)。缺省/越界 = `0.0`
    /// —— 老配置里没有这一项,读进来必须表现为"每档水平偏置都是 0"。
    ///
    /// 与 `shake_px` 的区别(用户曾把两者混为一谈):
    /// - `shake_px` = **打散**(每次控制叠加 ±随机量,模拟散布);
    /// - `gear_horizontal` = **抵消**(常量偏置,对付"总往一边偏")。
    #[serde(default)]
    pub gear_horizontal: Vec<f32>,
    /// 每档的**水平交替补偿幅度**(与 `strengths` 一一对应的平行向量,H①)。
    ///
    /// 用户 2026-10-11(第 3 轮):"让水平补偿和垂直补偿在一个组里,切换的时候
    /// 让水平补偿和垂直补偿一起切换" —— H① 的幅度/周期/波形从**全局**搬成
    /// **每档一套**。这里就是幅度:设备像素,正 = 波形从 0 向上(先左后右),
    /// 负 = 反向,0 = 这一档不振荡。
    ///
    /// 与全局字段([`Self::horizontal_px`] 等)的关系:那三个全局字段现在只是
    /// **迁移种子** —— 平行向量为空(老配置文件)时读值回落到全局值,
    /// [`Self::sync_gears`] 规整时也把全局值复制到每一档(老配置的每档手感
    /// 一字不变);一旦平行向量非空,一律以每档值为准。
    #[serde(default)]
    pub gear_horizontal_px: Vec<f32>,
    /// 每档的水平交替补偿**周期**(毫秒;平行向量,见 [`Self::gear_horizontal_px`])。
    /// 0 = 这一档不振荡。
    #[serde(default)]
    pub gear_horizontal_period_ms: Vec<u32>,
    /// 每档的水平交替补偿**波形**(平行向量,见 [`Self::gear_horizontal_px`])。
    #[serde(default)]
    pub gear_horizontal_wave: Vec<HorizontalWave>,
    /// 鼠标滚轮改变强度:**勾选即换档** —— FPS 模式内滚轮直接用于切换控制强度
    /// 档位,不再要求按住触发键/挡位切换键(用户 2026-10-11 口径纠正);
    /// 未勾选时滚轮仍是 FPS 缩放。两项同时开时换档优先。
    #[serde(default)]
    pub wheel_switch: bool,
    /// 挡位切换键(evdev 码或两键组合,顺序无关;空 = 未绑定)。
    ///
    /// 用户 2026-10-10(第 3 条):按一下换一档(环绕)。
    /// (2026-10-11 起滚轮换档不再要求按住它 —— 见 `wheel_switch`。)
    #[serde(default)]
    pub switch_key: KeySet,
    /// **老配置的全局水平交替补偿幅度**(H①,设备像素;0 = 关,**允许负值 = 反向**)。
    ///
    /// ⚠ 2026-10-11 第 3 轮起,幅度/周期/波形已**并入档位表**
    /// ([`Self::gear_horizontal_px`] / [`Self::gear_horizontal_period_ms`] /
    /// [`Self::gear_horizontal_wave`]);这三个全局字段只剩一个职责:
    /// **老配置的迁移种子** —— 平行向量为空时按它读值、规整时按它补齐,
    /// 之后一律以每档值为准(界面上已没有独立的「水平补偿」一块)。
    /// 字段保留不删,是为了"老配置文件必须能直接读进来";序列化照旧写出,
    /// 读回时被平行向量覆盖。
    ///
    /// 用户 2026-10-11:"消除抖动除了上下消除抖动,有些枪械还有左右的抖动。
    /// 你要想办法消除掉。" —— 这条与 `shake_px` 是**相反**的两件事:
    /// - `shake_px` = 每次控制叠加 **±随机** 水平量 = **打散**(模拟散布);
    /// - 水平交替补偿 = 按波形把水平偏移**增量**并进去 = **抵消**(确定性)。
    ///
    /// 总位移是**有界**的:`幅度 × w(t)`,`w ∈ [-1, 1]` 由波形决定
    /// (见 engine 的 `recoil_tick`)。
    #[serde(default)]
    pub horizontal_px: f32,
    /// **老配置的全局**水平交替补偿周期(毫秒;0 = 关)。
    ///
    /// 一个完整来回的时间 —— 用于对付"左右摆"的节拍:枪械左右摆多快,
    /// 这里就填多快(600ms 是个中庸的起步值)。
    /// 语义与去向同 [`Self::horizontal_px`](现在只是迁移种子)。
    #[serde(default = "default_horizontal_period_ms")]
    pub horizontal_period_ms: u32,
    /// **老配置的全局**水平交替补偿波形(见 [`HorizontalWave`]);
    /// 语义与去向同 [`Self::horizontal_px`](现在只是迁移种子)。
    #[serde(default)]
    pub horizontal_wave: HorizontalWave,
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
            // 平行向量的默认值**与默认强度表等长**:`Profile::default()` 必须是
            // 自洽的 —— 否则"刚建好的默认配置"会被 `normalize` 报告成"改过了"
            // (那个布尔值代表的是"用户的文件需要被修",不是"我们想补默认值")。
            gear_names: vec![String::new()],
            gear_wheel: vec![true],
            gear_horizontal: vec![0.0],
            gear_horizontal_px: vec![0.0],
            gear_horizontal_period_ms: vec![default_horizontal_period_ms()],
            gear_horizontal_wave: vec![HorizontalWave::default()],
            wheel_switch: false,
            switch_key: KeySet::new(),
            horizontal_px: 0.0,
            horizontal_period_ms: default_horizontal_period_ms(),
            horizontal_wave: HorizontalWave::default(),
            shake_px: 0.0,
            sensitivity: 0.0,
        }
    }
}

/// 水平交替补偿的默认周期(毫秒)。见 [`Recoil::horizontal_period_ms`]。
fn default_horizontal_period_ms() -> u32 {
    600
}

/// 水平交替补偿(H①)的波形。
///
/// 两种都从 `phase = 0` 的 **0 点向上**出发(正弦与三角同相),于是
/// "每次按下触发键都从同一点开始"(见 engine 的相位复位)。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum HorizontalWave {
    /// 正弦:起停柔和,过零点速度最快 —— 大多数"左右摆"的近似。
    #[default]
    Sine,
    /// 三角:幅值线性往返(匀速摆),过零点与顶点速度一致。
    Triangle,
}

impl HorizontalWave {
    /// 界面下拉用的文案(与 serde 名一一对应,便于用户对着 YAML 看)。
    pub fn label(self) -> &'static str {
        match self {
            HorizontalWave::Sine => "正弦",
            HorizontalWave::Triangle => "三角",
        }
    }

    pub const ALL: [HorizontalWave; 2] = [HorizontalWave::Sine, HorizontalWave::Triangle];
}

fn default_recoil_rate() -> f32 {
    60.0
}

/// 压枪单档默认强度(设备像素/次)。
const DEFAULT_RECOIL_STRENGTH: f32 = 6.0;

fn default_recoil_strengths() -> Vec<f32> {
    vec![DEFAULT_RECOIL_STRENGTH]
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

    /// 第 `index` 档的**水平偏置**(设备像素;正 = 向右)。
    ///
    /// 缺省/越界一律 `0.0` —— 这是"老配置行为不变"的落点之二(之一是
    /// `gear_wheel` 缺省 `true`)。`index` 不像 `strength_at` 那样收敛到末档:
    /// 水平偏置是**每档各自的量**,越界只能当"没有这一档"。
    pub fn gear_horizontal_at(&self, index: usize) -> f32 {
        self.gear_horizontal.get(index).copied().unwrap_or(0.0)
    }

    /// 第 `index` 档的**水平交替补偿幅度**(H①,设备像素;正负都有效)。
    ///
    /// 取值口径(与 [`Self::sync_gears`] 的补齐规则同一条):
    /// - 平行向量里有这一档 → 用它(切档时"水平和垂直一起切"的落点);
    /// - 下标越界(手改坏的文件 / 刚换完档)→ 末档,与 [`Self::strength_at`] 同;
    /// - 平行向量**整个为空**(老配置,还没规整)→ 回落全局种子
    ///   [`Self::horizontal_px`],于是"读进来就是老手感"。
    pub fn horizontal_px_at(&self, index: usize) -> f32 {
        self.gear_horizontal_px
            .get(index)
            .or_else(|| self.gear_horizontal_px.last())
            .copied()
            .unwrap_or(self.horizontal_px)
    }

    /// 第 `index` 档的水平交替补偿周期(毫秒;口径同 [`Self::horizontal_px_at`])。
    pub fn horizontal_period_ms_at(&self, index: usize) -> u32 {
        self.gear_horizontal_period_ms
            .get(index)
            .or_else(|| self.gear_horizontal_period_ms.last())
            .copied()
            .unwrap_or(self.horizontal_period_ms)
    }

    /// 第 `index` 档的水平交替补偿波形(口径同 [`Self::horizontal_px_at`])。
    pub fn horizontal_wave_at(&self, index: usize) -> HorizontalWave {
        self.gear_horizontal_wave
            .get(index)
            .or_else(|| self.gear_horizontal_wave.last())
            .copied()
            .unwrap_or(self.horizontal_wave)
    }

    /// 第 `index` 档的水平交替补偿(H①)是否具备生效前提:幅度非 0 **且**周期非 0。
    ///
    /// 幅度允许为负(反向):判断"开没开"看的是"非零",不是"大于零"。
    /// 每档各自判定 —— 这正是"去掉独立水平补偿、并入档位表"的落点:
    /// 一档开着不影响另一档,切档时它跟着垂直强度一起切。
    pub fn horizontal_wave_armed_at(&self, index: usize) -> bool {
        self.horizontal_px_at(index) != 0.0 && self.horizontal_period_ms_at(index) > 0
    }

    /// 第 `index` 档的水平交替补偿周期(秒;调用方保证
    /// [`Self::horizontal_wave_armed_at`] 成立)。
    pub fn horizontal_period_secs_at(&self, index: usize) -> f32 {
        self.horizontal_period_ms_at(index).max(1) as f32 / 1000.0
    }

    /// 七个平行向量(强度 / 名字 / 参与滚轮 / 水平偏置 / 水平幅度 / 水平周期 /
    /// 水平波形)长度必须一致。
    ///
    /// 补齐规则(**老配置兼容的关键**):
    /// - 名字不够 → 补空名字(显示上 = 无名字);
    /// - 参与位不够 → 补 `true`(老配置的文件里没有这一项,读进来必须与
    ///   "全部档位都参与"完全一致);
    /// - 水平偏置(H②)不够 → 补 `0.0`(老配置没有这一项,读进来必须与"不偏"一致);
    /// - 水平幅度/周期/波形(H①)不够 → 补**全局种子值**
    ///   ([`Self::horizontal_px`] / [`Self::horizontal_period_ms`] /
    ///   [`Self::horizontal_wave`]):老配置只有那三个全局字段,这一补就等价于
    ///   "把当时的全局水平补偿复制到每一档" —— 用户每一个档位的手感都不变,
    ///   而字段从此归每一档自己管(切档时垂直与水平一起切);
    ///   多出来的尾部一律截掉(手改坏的文件)。
    ///
    /// 返回是否真的动过(交给 `normalize` 决定要不要记"配置已规范化")。
    pub fn sync_gears(&mut self) -> bool {
        let n = self.strengths.len();
        let mut changed = false;
        if self.gear_names.len() != n {
            self.gear_names.resize(n, String::new());
            changed = true;
        }
        if self.gear_wheel.len() != n {
            self.gear_wheel.resize(n, true);
            changed = true;
        }
        if self.gear_horizontal.len() != n {
            self.gear_horizontal.resize(n, 0.0);
            changed = true;
        }
        // H① 的迁移种子:先取出来,免得边改向量边借全局字段。
        let legacy_px = self.horizontal_px;
        let legacy_period_ms = self.horizontal_period_ms;
        let legacy_wave = self.horizontal_wave;
        if self.gear_horizontal_px.len() != n {
            self.gear_horizontal_px.resize(n, legacy_px);
            changed = true;
        }
        if self.gear_horizontal_period_ms.len() != n {
            self.gear_horizontal_period_ms.resize(n, legacy_period_ms);
            changed = true;
        }
        if self.gear_horizontal_wave.len() != n {
            self.gear_horizontal_wave.resize(n, legacy_wave);
            changed = true;
        }
        changed
    }

    /// 第 `index` 档的名字(缺省/越界 = 空串;两侧空白已去掉)。
    pub fn gear_name_at(&self, index: usize) -> &str {
        self.gear_names
            .get(index)
            .map(|s| s.trim())
            .unwrap_or_default()
    }

    /// 第 `index` 档是否参与滚轮换档。**缺省/越界一律 `true`** ——
    /// 这一条就是"老配置行为不变"的落点。
    pub fn gear_wheel_at(&self, index: usize) -> bool {
        self.gear_wheel.get(index).copied().unwrap_or(true)
    }

    /// 参与滚轮换档的档位数(换档要至少两个才有意义)。
    pub fn wheel_gear_count(&self) -> usize {
        (0..self.strengths.len())
            .filter(|i| self.gear_wheel_at(*i))
            .count()
    }

    /// 从 `from` 档沿 `dir`(+1 向后 / −1 向前)找**下一个参与换档**的档位下标,
    /// 环绕;一个都没勾选(或没有档位)时返回 `None` —— 调用方据此让滚轮
    /// 退回原本的缩放行为,而不是卡住。
    pub fn step_gear(&self, from: usize, dir: i32) -> Option<usize> {
        let n = self.strengths.len();
        if n == 0 || dir == 0 {
            return None;
        }
        let start = from.min(n - 1);
        for k in 1..=n {
            let i = (start as i32 + dir * k as i32).rem_euclid(n as i32) as usize;
            if self.gear_wheel_at(i) {
                return Some(i);
            }
        }
        None
    }

    /// 档位的显示文案(界面与引擎通知**共用这一处口径**):
    /// 有名字 → `第 2 档「名字」(9)`,没名字 → `第 2 档(9)`。
    pub fn gear_display(&self, index: usize) -> String {
        let strength = self.strength_at(index);
        let name = self.gear_name_at(index);
        if name.is_empty() {
            format!("第 {} 档({strength})", index + 1)
        } else {
            format!("第 {} 档「{name}」({strength})", index + 1)
        }
    }

    /// 加一档:强度沿用最后一档(没有档位时用默认值),名字为空、参与换档 = true、
    /// 水平偏置 = 0;H①(水平幅度/周期/波形)也**沿用最后一档** —— 新档先与
    /// 上一档手感一致,再让用户单独调。七个平行向量一起长,长度永远一致。
    pub fn add_gear(&mut self) {
        let last = self
            .strengths
            .last()
            .copied()
            .unwrap_or(DEFAULT_RECOIL_STRENGTH);
        // 平行向量为空(老配置还没规整)时落到全局种子 —— 与
        // `horizontal_*_at` 的回落、`sync_gears` 的补齐同一条口径。
        let h_px = self
            .gear_horizontal_px
            .last()
            .copied()
            .unwrap_or(self.horizontal_px);
        let h_period = self
            .gear_horizontal_period_ms
            .last()
            .copied()
            .unwrap_or(self.horizontal_period_ms);
        let h_wave = self
            .gear_horizontal_wave
            .last()
            .copied()
            .unwrap_or(self.horizontal_wave);
        self.strengths.push(last);
        self.gear_names.push(String::new());
        self.gear_wheel.push(true);
        self.gear_horizontal.push(0.0);
        self.gear_horizontal_px.push(h_px);
        self.gear_horizontal_period_ms.push(h_period);
        self.gear_horizontal_wave.push(h_wave);
    }

    /// 删掉第 `index` 档(连同它的名字、参与位、水平偏置与 H① 三项)。
    /// 最后一档删不掉(档位表不允许为空,返回 `false` = 没动)。
    pub fn remove_gear(&mut self, index: usize) -> bool {
        if self.strengths.len() <= 1 || index >= self.strengths.len() {
            return false;
        }
        self.sync_gears();
        self.strengths.remove(index);
        self.gear_names.remove(index);
        self.gear_wheel.remove(index);
        self.gear_horizontal.remove(index);
        self.gear_horizontal_px.remove(index);
        self.gear_horizontal_period_ms.remove(index);
        self.gear_horizontal_wave.remove(index);
        true
    }

    /// 把第 `from` 档挪到 `to` 这个**插入边界**(0..=档数;`to` = "插到第 to 档
    /// 之前"),七个平行向量一起挪 —— 名字/水平偏置/H① 都**跟着自己那一档走**,
    /// 不会错档。返回它挪完后的新下标;没挪动(或越界)返回 `None`
    /// —— 界面靠这个返回值决定要不要记撤销、要不要跟着挪"当前档"。
    pub fn move_gear(&mut self, from: usize, to: usize) -> Option<usize> {
        let n = self.strengths.len();
        if n < 2 || from >= n || to > n {
            return None;
        }
        let insert = if to > from { to - 1 } else { to };
        if insert == from {
            return None;
        }
        // 手改坏的文件/老配置里后几个平行向量可能是空的:挪之前先拉齐长度,
        // 否则 `insert` 会落到一个长度不足的向量上(与 `remove_gear` 同一纪律)。
        self.sync_gears();
        let s = self.strengths.remove(from);
        let name = self.gear_names.remove(from);
        let wheel = self.gear_wheel.remove(from);
        let horizontal = self.gear_horizontal.remove(from);
        let h_px = self.gear_horizontal_px.remove(from);
        let h_period = self.gear_horizontal_period_ms.remove(from);
        let h_wave = self.gear_horizontal_wave.remove(from);
        self.strengths.insert(insert, s);
        self.gear_names.insert(insert, name);
        self.gear_wheel.insert(insert, wheel);
        self.gear_horizontal.insert(insert, horizontal);
        self.gear_horizontal_px.insert(insert, h_px);
        self.gear_horizontal_period_ms.insert(insert, h_period);
        self.gear_horizontal_wave.insert(insert, h_wave);
        Some(insert)
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
            mouse_shield: MouseShieldMode::default(),
            shield_key: KeySet::new(),
            wake_key: KeySet::new(),
            reconnect_key: KeySet::new(),
            self_heal: default_self_heal(),
            recoil: Recoil::default(),
        }
    }
}

impl Aim {
    /// 锚点是否已经设置过
    pub fn anchor_set(&self) -> bool {
        self.anchor_x.abs() > 1e-6 || self.anchor_y.abs() > 1e-6
    }

    /// 写入一组"FPS 起手参数"(2026-10-11 用户批准)。
    /// 只改这七项;灵敏度/移速/消隐/门控键等**用户的个人调校一律不动**。
    ///
    /// 纯函数(只借用 `Mapper` 换算锚点):界面上那个一键按钮与单测共用同一份
    /// 语义 —— 按钮只负责"调用 + 记撤销 + 攒日志"。
    pub fn apply_fps_preset(&mut self, m: &Mapper) {
        self.boundary = false; // 无边界 → 走无缝换手跑步机
        self.open_world = false; // 那条路径没有跑步机(换手会清零垂直累计量)
        self.recenter_threshold = 200; // 回转半径
        self.drag_deadzone = 0.0;
        self.recenter = RecenterMode::Idle;
        self.recenter_idle_ms = 80;
        self.anchor_x = m.rel_x((m.w / 2.0) as i32);
        self.anchor_y = m.rel_y((m.h * 0.42) as i32); // 居中偏上
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
    /// B21-C(2026-10-18,用户第 2 条):轮换时**只考虑勾选过的组合**。
    ///
    /// 三态(用户 2026-10-23 第 3 条要求"一键全勾 / 一键取消",两者必须可区分):
    /// - `None`(字段缺失)**= 全部参与** —— 这是老配置的语义,逐字节一致,不用迁移;
    /// - `Some([])` **= 一个都不参与**(等于把这个切换键关掉,但配置还在);
    /// - `Some([i, j, …])` = 只在这些下标之间 Next/Prev。
    ///
    /// ⚠️ 手改配置文件时注意:以前写 `rotate_set: []` 表示"全部参与",现在表示
    /// "一个都不参与";要"全部参与"就直接**删掉这一行**。
    ///
    /// 只对 `direction` 为 Next/Prev 有效 —— Target 是"直接切到某一套",与轮换无关。
    /// 用下标而不是名字:名字可改,下标是本文件的既有主键(见 `normalize` 里
    /// "指向不存在的组合直接删掉"那条规则)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotate_set: Option<Vec<usize>>,
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

/// 「哪些组合参与轮换」的**唯一口径** —— 引擎与界面都调它,免得两处各写一遍
/// 过滤/排序,日后改一处忘一处。
///
/// - `None`(字段缺失)⇒ 全部参与,`0..scheme_count`;
/// - `Some([])` ⇒ **一个都不参与**(返回空 vec,引擎据此判定"没有可切的目标");
/// - `Some([…])` ⇒ 去掉越界、去重、升序后的下标。
///
/// 越界一律**忽略而不是夹取**:手改坏了的配置只该少勾几套组合,不该让引擎跳到
/// 不存在的组合上去(见 `rotate_target` 的口径 2)。
pub fn rotate_members(rotate_set: Option<&[usize]>, scheme_count: usize) -> Vec<usize> {
    match rotate_set {
        None => (0..scheme_count).collect(),
        Some(list) => {
            let mut set: Vec<usize> = list.iter().copied().filter(|&i| i < scheme_count).collect();
            set.sort_unstable();
            set.dedup();
            set
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
            // 压枪的三个平行向量(强度/名字/参与滚轮)在这里收敛到等长:
            // 老配置只有 `strengths`,读进来后名字补空、参与位补 true,
            // 于是"老配置的行为"与从前逐位一致;手改坏的长度也在这里被拉齐。
            if p.aim.recoil.sync_gears() {
                changed = true;
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
            // B21-C:轮换集合里的越界/重复下标一并修掉(手改配置的容忍,见模块头
            // "用户会手改 YAML"那条)。注意这里只清洗,不删条目 —— 轮换集合坏了
            // 不该让整条切换键失效,退化成"全部参与"更安全。
            if s.direction == SwitchDirection::Target && s.target >= n {
                return false;
            }
            if seen.contains(&keys) {
                return false;
            }
            seen.push(keys);
            true
        });
        // B21-C:轮换集合清洗(去越界、去重、排序)。放在 retain 之后单独跑一遍,
        // 因为 retain 的闭包拿到的是 `&SwitchKey`(不可变),改不了集合。
        for s in self.switch_keys.iter_mut() {
            // 三态(用户 2026-10-23 第 3 条):None=全部参与、Some([])=一个都不参与,
            // 两者都不需要清洗;只有 Some(非空) 才去越界/去重/排序。
            // 注意 rotate_members 会把 None 展开成 0..n,这里**不能**用它 ——
            // 展开写回就把"缺失字段"固化成显式列表了,老配置文件会因此被改写。
            let Some(mut list) = s.rotate_set.take() else {
                continue;
            };
            if !list.is_empty() {
                let mut set: Vec<usize> = list.iter().copied().filter(|&i| i < n).collect();
                set.sort_unstable();
                set.dedup();
                if set.is_empty() {
                    // 原集合非空、却全被越界滤掉(典型:用户删掉了那几套组合)⇒
                    // 退化成 None=全部参与,别让整条切换键因此失效(这是旧版
                    // "空集合=全部"那条兜底的延续)。注意与 Some([]) 区分开:
                    // 后者是用户**主动**点了"全不选",必须保持"一个都不参与"。
                    changed = true;
                    continue; // s.rotate_set 已是 None
                }
                if set != list {
                    list = set;
                    changed = true;
                }
            }
            s.rotate_set = Some(list);
        }
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
#       aim.suspend_key / aim.shield_key / aim.wake_key / aim.reconnect_key /
#       aim.recoil.trigger_key / switch_keys[].keys /
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
#     recoil.strengths   : 控制强度档位表(每档 = 每次控制的向下像素位移)
#     recoil.gear_names  : 与 strengths 一一对应的档位名字(空串 = 无名字)
#     recoil.gear_wheel  : 与 strengths 一一对应的"是否参与滚轮换档"
#                          (缺省或越界一律按 true —— 老的配置文件里没有这一项,
#                           读进来与"全部档位都参与"完全一致)
#     recoil.wheel_switch: true = FPS 模式内滚轮用来换档(勾选即换档,不要求按住按键;
#                          未勾选时滚轮仍是 FPS 缩放)
#     recoil.switch_key  : 档位切换键(按一下换一档,环绕;0 = 未绑定)
#     recoil.gear_horizontal : 与 strengths 一一对应的每档水平**常量**偏置(设备像素/次,
#                          正 = 向右、负 = 向左;缺省或越界一律按 0)
#     recoil.gear_horizontal_px : 与 strengths 一一对应的每档**水平交替补偿**幅度
#                          (设备像素;正 = 波形从 0 向上先左后右、负 = 反向、
#                           0 = 这一档不振荡。它按下面的波形/周期在 ±幅度 之间
#                           往返 = 确定性抵消「左右摆」,与 shake_px 的随机打散相反)
#     recoil.gear_horizontal_period_ms : 与 strengths 一一对应的每档周期
#                          (一个完整来回的毫秒数;0 = 这一档不振荡;缺省 600)
#     recoil.gear_horizontal_wave : 与 strengths 一一对应的每档波形
#                          sine|triangle(小写;缺省 sine)
#     recoil.horizontal_px / recoil.horizontal_period_ms / recoil.horizontal_wave :
#                          **老配置的全局水平补偿**(幅度/周期/波形),现已并入档位表。
#                          只在 gear_horizontal_px 等三个平行向量为空时作为
#                          **迁移种子**被读取:规整时把这份全局值复制到每一档
#                          (老手感不变),之后一律以每档值为准。
#     recoil.shake_px / recoil.sensitivity : 随机摇晃上限 / 覆盖灵敏度
#     mouse_shield : 鼠标独占(隔膜)的作用范围 FpsOnly|ScrcpyForeground|Always
#                          (首字母大写;默认 FpsOnly = 只在 FPS 模式生效期间独占。
#                           隔膜开着时鼠标消息不投递给任何窗口,投屏窗口也就不会
#                           自己再转发一套;它是运行时可切换的,不需要重启 scrcpy)
#     shield_key : 「鼠标独占」的手动开关热键(按一下翻转、再按一下关闭;0 = 未绑定。
#                          隔膜开着时连本程序界面都点不了,这个键是唯一还通的路)
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

    /// DF-7:FPS 一键预设只写"起手那七项",且锚点落在屏幕**居中偏上**。
    ///
    /// 断言里特意先把每一项都设成非预设值(含 `anchor_x = 0.9`),再确认调用后
    /// 全部被覆盖 —— 漏写任何一项都会在这里露出来。
    #[test]
    fn fps_preset_writes_agreed_values() {
        let m = Mapper::new(CoordUnit::Rel, (1000, 2000));
        let mut a = Aim::default();
        a.open_world = true;
        a.boundary = true;
        a.recenter = RecenterMode::Never;
        a.recenter_idle_ms = 999;
        a.recenter_threshold = 1234;
        a.drag_deadzone = 55.0;
        a.anchor_x = 0.9;
        a.anchor_y = 0.9;

        a.apply_fps_preset(&m);

        assert!(!a.boundary, "必须关掉边界限制(否则没有无缝换手)");
        assert!(!a.open_world, "必须走普通 FPS 那条路(开放世界没有跑步机)");
        assert_eq!(a.recenter_threshold, 200);
        assert_eq!(a.drag_deadzone, 0.0);
        assert!(
            matches!(a.recenter, RecenterMode::Idle),
            "必须是静止归中,实际 {:?}",
            a.recenter
        );
        assert_eq!(a.recenter_idle_ms, 80);
        assert!(a.anchor_set(), "预设必须把锚点设上(0,0 视为未设置)");
        // 居中偏上:宽度取一半,高度取 42% —— 换算回像素必须**高于**屏幕中心
        let (ax, ay) = m.point(a.anchor_x, a.anchor_y);
        assert_eq!(ax, 500, "锚点 x 应为屏幕宽度的一半");
        assert!(
            (ay as f32) < m.h / 2.0,
            "锚点应居中偏上:y={ay} 必须小于屏幕中心 {}",
            m.h / 2.0
        );
    }

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
                        repeat_interval_ms: 0,
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

    /// 「长按重复」(用户 2026-10-11 第 2 条):宏与组合键各有一个
    /// `repeat_while_held` 开关,锁三件事 ——
    ///   ① 打开后 round-trip(JSON 与 YAML 两条落盘路径)一字不丢;
    ///   ② 老配置(根本没有这个字段)读成 `false` = 老行为一字不改;
    ///   ③ 关着时按 `false`(而不是别的)序列化出来。
    #[test]
    fn repeat_while_held_round_trips_and_defaults_off_for_old_configs() {
        // ---- ① 打开:JSON round-trip ----
        let mac = MacroAction {
            steps: vec![MacroStep {
                code: 30,
                pressed: true,
                delay_ms: 0,
            }],
            instructions: Vec::new(),
            virtual_profile: None,
            repeat_while_held: true,
        };
        let text = serde_json::to_string(&mac).unwrap();
        assert!(
            text.contains(r#""repeat_while_held":true"#),
            "打开时必须真的写出来: {text}"
        );
        let back: MacroAction = serde_json::from_str(&text).unwrap();
        assert_eq!(back, mac, "宏的长按重复必须原样往返");
        assert!(back.repeat_while_held);

        let combo = KeyCombo {
            keys: vec![29, 19],
            action: Action::Tap {
                x: 0.5,
                y: 0.5,
                duration_ms: DEFAULT_TAP_DURATION_MS,
                radius: DEFAULT_RADIUS,
                repeat_interval_ms: 0,
            },
            fps_only: false,
            tail_delay_ms: 0,
            repeat_while_held: true,
        };
        let text = serde_json::to_string(&combo).unwrap();
        assert!(
            text.contains(r#""repeat_while_held":true"#),
            "组合键打开时也必须真的写出来: {text}"
        );
        let back: KeyCombo = serde_json::from_str(&text).unwrap();
        assert_eq!(back, combo, "组合键的长按重复必须原样往返");
        assert!(back.repeat_while_held);

        // ---- ①b YAML 是配置的唯一落盘路径:同一份 Profile 必须过一遍一样 ----
        let mut p = Profile::default();
        p.binds.push(KeyBind {
            key: 22,
            action: Action::Macro(mac.clone()),
            fps_only: false,
            tail_delay_ms: 0,
        });
        p.combos.push(combo.clone());
        let yaml = serde_norway::to_string(&p).unwrap();
        let back: Profile = serde_norway::from_str(&yaml).unwrap();
        assert_eq!(back, p, "YAML round-trip 必须一模一样");

        // ---- ② 老配置:字段整个不存在 -> false ----
        let old_mac: MacroAction = serde_json::from_str(
            r#"{"steps":[{"code":30,"pressed":true,"delay_ms":0}],"instructions":[]}"#,
        )
        .expect("老宏配置(无 repeat_while_held)必须仍可解析");
        assert!(
            !old_mac.repeat_while_held,
            "老宏配置必须读成长按重复关闭(老行为一字不改)"
        );
        let old_combo: KeyCombo =
            serde_json::from_str(r#"{"keys":[29,19],"action":{"AndroidKey":{"keycode":4}}}"#)
                .expect("老组合键配置(无 repeat_while_held)必须仍可解析");
        assert!(
            !old_combo.repeat_while_held,
            "老组合键配置必须读成长按重复关闭"
        );

        // ---- ③ 关着时序列化出 false(不是省略成别的形状) ----
        let off = MacroAction {
            repeat_while_held: false,
            ..mac
        };
        let text = serde_json::to_string(&off).unwrap();
        assert!(
            text.contains(r#""repeat_while_held":false"#),
            "关闭时也要写成明确的 false: {text}"
        );
        let off = KeyCombo {
            repeat_while_held: false,
            ..combo
        };
        let text = serde_json::to_string(&off).unwrap();
        assert!(
            text.contains(r#""repeat_while_held":false"#),
            "组合键关闭时也要写成明确的 false: {text}"
        );
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

    /// 2026-10-11 档位增强:名字与"参与滚轮换档"是**平行向量**,老配置里没有
    /// 这两项 —— 读进来必须表现为"没有名字 + 全部参与",而且往返稳定
    /// (写出去再读回来还是同一份)。
    #[test]
    fn recoil_gear_vectors_default_to_nameless_and_all_participating() {
        // 老配置形状:recoil 里只有 strengths(今天落盘的老文件正是这样)
        let old = r#"{
            "enabled": true, "trigger_key": 272, "rate_hz": 60.0,
            "strengths": [0.4, 9.0, 12.0], "wheel_switch": true,
            "switch_key": 0, "shake_px": 0.0, "sensitivity": 0.0
        }"#;
        let r: Recoil = serde_json::from_str(old).expect("老配置应可解析");
        assert!(r.gear_names.is_empty(), "缺省 = 没有名字");
        assert!(r.gear_wheel.is_empty(), "缺省 = 向量为空");
        for i in 0..r.strengths.len() {
            assert!(r.gear_wheel_at(i), "老配置的每一档都必须按「参与」处理");
            assert_eq!(r.gear_name_at(i), "", "老配置的每一档都没有名字");
        }
        assert!(
            r.gear_wheel_at(99),
            "越界的参与位同样按 true(缺省口径只有一条)"
        );
        assert_eq!(r.gear_display(1), "第 2 档(9)", "没名字时不带名字");

        // 往返稳定:原样写回再读回来必须逐位相等
        let text = serde_norway::to_string(&r).unwrap();
        let back: Recoil = serde_norway::from_str(&text).unwrap();
        assert_eq!(back, r);

        // 规范化之后的落盘形状(界面/文档都按这个口径描述):
        // 三个平行向量等长 —— 强度是数字序列,名字是等长字符串序列(空串 = 没名字),
        // 参与位是等长的 true/false 序列。
        let mut synced = r.clone();
        synced.sync_gears();
        let text = serde_norway::to_string(&synced).unwrap();
        assert!(
            text.contains("gear_names:\n- ''\n- ''\n- ''"),
            "名字列必须与强度表等长(空名字写成空串):\n{text}"
        );
        assert!(
            text.contains("gear_wheel:\n- true\n- true\n- true"),
            "参与位必须与强度表等长(老配置补 true):\n{text}"
        );

        // 显式写了名字/参与位的配置也要原样往返
        let full = Recoil {
            enabled: true,
            strengths: vec![3.0, 8.0],
            gear_names: vec!["近".into(), "远".into()],
            gear_wheel: vec![true, false],
            ..Recoil::default()
        };
        let text = serde_norway::to_string(&full).unwrap();
        let back: Recoil = serde_norway::from_str(&text).unwrap();
        assert_eq!(back, full);
        assert_eq!(full.gear_display(1), "第 2 档「远」(8)");
    }

    /// 七个平行向量在增/删/挪档之后长度必须一致(`normalize` 里的收敛也靠它)。
    #[test]
    fn sync_gears_keeps_the_parallel_vectors_aligned() {
        // 手工构造"长度不齐"的配置(手改 YAML / 老版本写的文件都可能这样)
        let mut r = Recoil {
            strengths: vec![1.0, 2.0, 3.0],
            gear_names: vec!["a".into()],
            gear_wheel: vec![false],
            gear_horizontal: vec![-1.5],
            ..Recoil::default()
        };
        assert!(r.sync_gears(), "长度不齐必须报告「改动」");
        assert_eq!(r.gear_names.len(), 3);
        assert_eq!(r.gear_wheel.len(), 3);
        assert_eq!(r.gear_horizontal.len(), 3);
        assert_eq!(r.gear_names[0], "a", "已有的名字不能被覆盖");
        assert_eq!(r.gear_name_at(1), "", "补出来的是空名字");
        assert!(!r.gear_wheel[0], "已有的参与位不能被覆盖");
        assert!(r.gear_wheel_at(1), "补出来的是 true");
        assert_eq!(r.gear_horizontal[0], -1.5, "已有的水平偏置不能被覆盖");
        assert_eq!(r.gear_horizontal_at(1), 0.0, "补出来的是 0");
        assert_eq!(r.gear_horizontal_px.len(), 3, "H① 幅度向量也要拉齐");
        assert_eq!(r.gear_horizontal_period_ms.len(), 3, "H① 周期向量也要拉齐");
        assert_eq!(r.gear_horizontal_wave.len(), 3, "H① 波形向量也要拉齐");
        assert!(!r.sync_gears(), "已经齐了就不该再报改动(幂等)");

        // 多出来的尾巴要截掉(手改坏的文件)
        let mut long = Recoil {
            strengths: vec![1.0],
            gear_names: vec!["a".into(), "b".into()],
            gear_wheel: vec![true, false],
            gear_horizontal: vec![1.0, 2.0],
            ..Recoil::default()
        };
        assert!(long.sync_gears());
        assert_eq!(long.gear_names.len(), 1);
        assert_eq!(long.gear_wheel.len(), 1);
        assert_eq!(long.gear_horizontal.len(), 1);

        // 增一档:七个向量一起长
        long.add_gear();
        assert_eq!(long.strengths.len(), 2);
        assert_eq!(long.gear_names.len(), 2);
        assert_eq!(long.gear_wheel.len(), 2);
        assert_eq!(long.gear_horizontal.len(), 2);
        assert_eq!(long.gear_horizontal_px.len(), 2);
        assert_eq!(long.gear_horizontal_period_ms.len(), 2);
        assert_eq!(long.gear_horizontal_wave.len(), 2);
        assert_eq!(long.strengths, vec![1.0, 1.0], "新档沿用最后一档的强度");
        assert!(long.gear_wheel_at(1), "新档默认参与换档");
        assert_eq!(long.gear_horizontal_at(1), 0.0, "新档水平偏置默认 0");

        // 挪一档:七个向量一起挪,名字与水平偏置都跟着走
        let mut moved = Recoil {
            strengths: vec![1.0, 2.0, 3.0],
            gear_names: vec!["一".into(), "二".into(), "三".into()],
            gear_wheel: vec![true, false, true],
            gear_horizontal: vec![0.5, -1.0, 2.0],
            ..Recoil::default()
        };
        // 把第 0 档挪到最后(插入边界 = 3)
        assert_eq!(moved.move_gear(0, 3), Some(2));
        assert_eq!(moved.strengths, vec![2.0, 3.0, 1.0]);
        assert_eq!(moved.gear_names, vec!["二", "三", "一"]);
        assert_eq!(moved.gear_wheel, vec![false, true, true]);
        assert_eq!(moved.gear_horizontal, vec![-1.0, 2.0, 0.5]);
        assert_eq!(moved.gear_names.len(), moved.strengths.len());
        assert_eq!(moved.gear_wheel.len(), moved.strengths.len());
        assert_eq!(moved.gear_horizontal.len(), moved.strengths.len());
        assert_eq!(moved.move_gear(1, 1), None, "原地放下 = 不动");
        assert_eq!(moved.move_gear(9, 0), None, "越界 = 不动");

        // 删一档:四个向量一起短;最后一档删不掉
        assert!(moved.remove_gear(0));
        assert_eq!(moved.strengths.len(), 2);
        assert_eq!(moved.gear_names.len(), 2);
        assert_eq!(moved.gear_wheel.len(), 2);
        assert_eq!(moved.gear_horizontal.len(), 2);
        assert!(moved.remove_gear(0));
        assert!(!moved.remove_gear(0), "最后一档不允许删");
        assert_eq!(moved.strengths.len(), 1);
        assert_eq!(moved.gear_names.len(), 1);
        assert_eq!(moved.gear_wheel.len(), 1);
        assert_eq!(moved.gear_horizontal.len(), 1);
    }

    /// H⑤1(用户 2026-10-11:"有些枪械还有左右的抖动"):老配置里没有水平补偿的
    /// 任何字段 —— 读进来必须表现为"全关 + 每档偏置 0",而且往返稳定。
    #[test]
    fn recoil_horizontal_defaults_off_and_round_trips() {
        // 老配置形状:recoil 里没有 horizontal_px / horizontal_period_ms /
        // horizontal_wave / gear_horizontal(今天落盘的老文件正是这样)
        let old = r#"{
            "enabled": true, "trigger_key": 272, "rate_hz": 60.0,
            "strengths": [0.4, 9.0, 12.0], "wheel_switch": true,
            "switch_key": 0, "shake_px": 0.0, "sensitivity": 0.0
        }"#;
        let r: Recoil = serde_json::from_str(old).expect("老配置应可解析");
        assert_eq!(r.horizontal_px, 0.0, "缺省 = 水平交替补偿关闭");
        assert!(r.gear_horizontal.is_empty(), "缺省 = 平行向量为空");
        assert!(
            r.gear_horizontal_px.is_empty()
                && r.gear_horizontal_period_ms.is_empty()
                && r.gear_horizontal_wave.is_empty(),
            "缺省 = H① 的三个每档向量同样为空(读值回落全局种子)"
        );
        for i in 0..r.strengths.len() {
            assert_eq!(
                r.gear_horizontal_at(i),
                0.0,
                "老配置的第 {i} 档水平偏置必须按 0 处理"
            );
            assert_eq!(
                r.horizontal_px_at(i),
                0.0,
                "老配置(全局幅度 0)的第 {i} 档水平幅度必须按 0 处理"
            );
        }
        assert_eq!(r.gear_horizontal_at(99), 0.0, "越界的偏置同样按 0");
        assert_eq!(r.horizontal_period_ms, 600, "周期有合理默认");
        assert_eq!(r.horizontal_wave, HorizontalWave::Sine, "默认正弦");
        assert!(!r.horizontal_wave_armed_at(0), "老配置不振荡");

        // 往返稳定:原样写回再读回来必须逐位相等
        let text = serde_norway::to_string(&r).unwrap();
        let back: Recoil = serde_norway::from_str(&text).unwrap();
        assert_eq!(back, r);

        // 规范化之后落盘:七个平行向量等长(水平偏置补 0;H① 三项补全局种子)
        let mut synced = r.clone();
        assert!(synced.sync_gears(), "长度不齐必须报告「改动」");
        assert_eq!(synced.gear_horizontal, vec![0.0, 0.0, 0.0]);
        assert_eq!(synced.gear_horizontal_px, vec![0.0, 0.0, 0.0]);
        assert_eq!(synced.gear_horizontal_period_ms, vec![600, 600, 600]);
        assert_eq!(
            synced.gear_horizontal_wave,
            vec![
                HorizontalWave::Sine,
                HorizontalWave::Sine,
                HorizontalWave::Sine
            ]
        );
        for i in 0..synced.strengths.len() {
            assert!(
                !synced.horizontal_wave_armed_at(i),
                "全局幅度 0 = 迁到每档后第 {i} 档同样不振荡"
            );
        }

        // 显式写了水平项的配置也要原样往返(幅度允许负值)
        let full = Recoil {
            horizontal_px: -12.5,
            horizontal_period_ms: 333,
            horizontal_wave: HorizontalWave::Triangle,
            gear_horizontal: vec![0.0, -2.5, 4.0],
            ..r
        };
        let text = serde_norway::to_string(&full).unwrap();
        assert!(
            text.contains("horizontal_wave: triangle"),
            "波形落盘名必须是 snake_case:\n{text}"
        );
        let back: Recoil = serde_norway::from_str(&text).unwrap();
        assert_eq!(back, full);
        assert!(
            back.horizontal_wave_armed_at(0),
            "幅度非 0 + 周期非 0 = 开(平行向量为空时按全局种子读)"
        );
        assert_eq!(back.gear_horizontal_at(1), -2.5, "负偏置(向左)照旧");
        assert_eq!(back.gear_horizontal_at(2), 4.0, "正偏置(向右)照旧");

        // 0 = 关:幅度为 0 或周期为 0 都不振荡(负幅度不算关)
        assert!(
            !Recoil {
                horizontal_period_ms: 0,
                ..full.clone()
            }
            .horizontal_wave_armed_at(0)
        );
        assert!(
            !Recoil {
                horizontal_px: 0.0,
                ..full
            }
            .horizontal_wave_armed_at(0)
        );
    }

    /// H⑥(用户 2026-10-11 第 3 轮:"让水平补偿和垂直补偿在一个组里,切换的时候
    /// 让水平补偿和垂直补偿一起切换。也就是去掉单独的水平补偿的部分,合并至垂直
    /// 补偿内,都可以独立分别设置。"):**老配置迁移不变量** —— 只有全局
    /// `horizontal_px / horizontal_period_ms / horizontal_wave`、三个 per-gear
    /// 向量为空的老文件,加载/规整之后**每一个档位**的水平参数都必须等于那份
    /// 全局值(用户任何档位的手感都不变),而且迁移是幂等的。
    #[test]
    fn recoil_horizontal_per_gear_migrates_from_the_legacy_globals() {
        // 老配置形状:recoil 里只有全局水平三件套,没有任何 per-gear 向量
        // (今天落盘的老文件正是这样);三档强度各不相同。
        let legacy = r#"{
            "enabled": true, "trigger_key": 272, "rate_hz": 60.0,
            "strengths": [0.4, 9.0, 12.0], "wheel_switch": true,
            "horizontal_px": 7.5, "horizontal_period_ms": 450,
            "horizontal_wave": "triangle"
        }"#;
        let r: Recoil = serde_json::from_str(legacy).expect("老配置应可解析");
        assert!(
            r.gear_horizontal_px.is_empty()
                && r.gear_horizontal_period_ms.is_empty()
                && r.gear_horizontal_wave.is_empty(),
            "老文件里三个 per-gear 水平向量必须是空的"
        );
        // 规整之前:读值回落到全局种子(老配置"读进来就是老手感")
        for i in 0..r.strengths.len() {
            assert_eq!(r.horizontal_px_at(i), 7.5);
            assert_eq!(r.horizontal_period_ms_at(i), 450);
            assert_eq!(r.horizontal_wave_at(i), HorizontalWave::Triangle);
        }

        // 真正的加载路径:塞进 ConfigFile,再 `normalize` 走一遍规整
        let mut doc = ConfigFile::default();
        doc.schemes[0].aim.recoil = r;
        assert!(doc.normalize(), "老配置的每档水平参数必须由迁移补齐");
        let r = &doc.schemes[0].aim.recoil;
        assert_eq!(r.strengths.len(), 3);
        assert_eq!(r.gear_horizontal_px.len(), 3, "每档向量与强度表等长");
        assert_eq!(r.gear_horizontal_period_ms.len(), 3);
        assert_eq!(r.gear_horizontal_wave.len(), 3);
        for i in 0..r.strengths.len() {
            assert_eq!(r.gear_horizontal_px[i], 7.5, "第 {i} 档 = 老全局幅度");
            assert_eq!(
                r.gear_horizontal_period_ms[i], 450,
                "第 {i} 档 = 老全局周期"
            );
            assert_eq!(
                r.gear_horizontal_wave[i],
                HorizontalWave::Triangle,
                "第 {i} 档 = 老全局波形"
            );
            // 迁移后的**每档读值**也必须逐档等于全局值(不是只有第 0 档)
            assert_eq!(r.horizontal_px_at(i), 7.5);
            assert_eq!(r.horizontal_period_ms_at(i), 450);
            assert_eq!(r.horizontal_wave_at(i), HorizontalWave::Triangle);
            assert!(r.horizontal_wave_armed_at(i), "非 0 幅度 + 非 0 周期 = 开");
            assert!(
                (r.horizontal_period_secs_at(i) - 0.45).abs() < 1e-6,
                "周期 450ms = 0.45s"
            );
        }
        // 全局种子字段原样留着(下一次 normalize 不会再来一遍迁移)
        assert_eq!(r.horizontal_px, 7.5);
        assert!(!doc.normalize(), "迁移必须幂等:第二次规整无改动");
    }

    /// H⑥:七个平行向量在**增 / 删 / 重排**档位时必须一起动、且**不错档**
    /// (水平幅度/周期/波形跟着自己那一档走)。
    #[test]
    fn recoil_horizontal_per_gear_vectors_follow_gear_edits() {
        let mut r = Recoil {
            strengths: vec![1.0, 2.0, 3.0],
            gear_names: vec!["一".into(), "二".into(), "三".into()],
            gear_wheel: vec![true, true, true],
            gear_horizontal: vec![0.5, -1.0, 2.0],
            gear_horizontal_px: vec![10.0, 20.0, 30.0],
            gear_horizontal_period_ms: vec![300, 600, 900],
            gear_horizontal_wave: vec![
                HorizontalWave::Sine,
                HorizontalWave::Triangle,
                HorizontalWave::Sine,
            ],
            ..Recoil::default()
        };
        assert!(!r.sync_gears(), "构造时就该是齐的");

        // ---- 重排:第 0 档挪到最后,三组量一起挪、不错档 ----
        assert_eq!(r.move_gear(0, 3), Some(2));
        assert_eq!(r.strengths, vec![2.0, 3.0, 1.0]);
        assert_eq!(r.gear_horizontal_px, vec![20.0, 30.0, 10.0]);
        assert_eq!(r.gear_horizontal_period_ms, vec![600, 900, 300]);
        assert_eq!(
            r.gear_horizontal_wave,
            vec![
                HorizontalWave::Triangle,
                HorizontalWave::Sine,
                HorizontalWave::Sine
            ],
            "第 0 档的 Triangle 必须跟着它自己挪到最后"
        );
        // 每档"整套参数"的对应关系不放错:名字/偏置/幅度/周期/波形指同一档
        assert_eq!(r.gear_name_at(2), "一");
        assert_eq!(r.gear_horizontal_at(2), 0.5);
        assert_eq!(r.horizontal_px_at(2), 10.0);
        assert_eq!(r.horizontal_period_ms_at(2), 300);

        // ---- 删档:只掉被删那一档,其余不错位 ----
        assert!(r.remove_gear(1));
        assert_eq!(r.strengths, vec![2.0, 1.0]);
        assert_eq!(r.gear_horizontal_px, vec![20.0, 10.0]);
        assert_eq!(r.gear_horizontal_period_ms, vec![600, 300]);
        assert_eq!(
            r.gear_horizontal_wave,
            vec![HorizontalWave::Triangle, HorizontalWave::Sine]
        );
        assert_eq!(r.gear_name_at(1), "一");
        assert_eq!(r.horizontal_wave_at(1), HorizontalWave::Sine);

        // ---- 加档:七个向量一起长,新档沿用最后一档的 H①(先与上一档同手感)----
        assert_eq!(r.horizontal_px_at(1), 10.0);
        let text = serde_norway::to_string(&r).unwrap();
        assert!(
            text.contains("gear_horizontal_px:") && text.contains("gear_horizontal_wave:"),
            "三个 per-gear 水平向量都必须落盘:\n{text}"
        );
        let mut back: Recoil = serde_norway::from_str(&text).unwrap();
        assert_eq!(back, r, "七个平行向量都要原样往返(不丢字段)");
        back.add_gear();
        assert_eq!(back.strengths.len(), 3);
        assert_eq!(back.gear_horizontal_px.len(), 3);
        assert_eq!(back.gear_horizontal_period_ms.len(), 3);
        assert_eq!(back.gear_horizontal_wave.len(), 3);
        assert_eq!(back.horizontal_px_at(2), 10.0, "新档沿用最后一档的幅度");
        assert_eq!(back.horizontal_period_ms_at(2), 300, "周期也沿用最后一档");
        assert_eq!(back.horizontal_wave_at(2), HorizontalWave::Sine);

        // ---- 越界读值:非空的平行向量按末档(与 strength_at 同一条纪律)----
        assert_eq!(back.horizontal_px_at(99), 10.0);
        assert_eq!(back.horizontal_period_ms_at(99), 300);
    }

    /// S④:隔膜判据的表驱动用例 —— 三档 × (映射开/关、连接/断连、FPS 开/关、
    /// 投屏前台/后台、手动开/关)的代表组合。
    ///
    /// 最重要的一行是第一组:**映射关闭 / 断连时任何模式、任何手动都不得独占**
    /// (否则用户关了映射还得跟一个吞鼠标的隔膜较劲)。
    #[test]
    fn mouse_shield_on_table() {
        use MouseShieldMode::{Always, FpsOnly, ScrcpyForeground};
        // (模式, 映射开, 已连接, FPS 生效, 投屏前台, 手动, 期望)
        let cases: &[(MouseShieldMode, bool, bool, bool, bool, bool, bool)] = &[
            // ---- 硬闸:映射关闭 → 一律不独占 ----
            (FpsOnly, false, true, true, true, true, false),
            (ScrcpyForeground, false, true, true, true, true, false),
            (Always, false, true, true, true, true, false),
            (Always, false, false, false, false, false, false),
            // ---- 硬闸:断连 → 一律不独占(注入了也是白注入) ----
            (FpsOnly, true, false, true, true, true, false),
            (ScrcpyForeground, true, false, true, true, false, false),
            (Always, true, false, false, false, false, false),
            // ---- FpsOnly(默认 = 今天的行为):只看 FPS 是否生效 ----
            (FpsOnly, true, true, true, false, false, true),
            (FpsOnly, true, true, false, true, false, false),
            (FpsOnly, true, true, false, false, false, false),
            // ---- ScrcpyForeground:只看投屏窗口是否在前台 ----
            (ScrcpyForeground, true, true, false, true, false, true),
            (ScrcpyForeground, true, true, true, false, false, false),
            (ScrcpyForeground, true, true, false, false, false, false),
            // ---- Always:映射开着 + 连着就独占(与 FPS/前台无关) ----
            (Always, true, true, false, false, false, true),
            (Always, true, true, true, true, false, true),
            // ---- 手动开关:无视模式,一律独占 ----
            (FpsOnly, true, true, false, false, true, true),
            (ScrcpyForeground, true, true, false, false, true, true),
            (Always, true, true, false, false, true, true),
        ];
        for &(mode, mapping, connected, fps, foreground, manual, want) in cases {
            assert_eq!(
                mouse_shield_on(mode, mapping, connected, fps, foreground, manual),
                want,
                "mode={mode:?} 映射={mapping} 连接={connected} \
                 FPS={fps} 前台={foreground} 手动={manual}"
            );
        }
    }

    /// S①/S②:老配置没有 `mouse_shield` / `shield_key` 两项 —— 读进来必须是
    /// "仅 FPS 模式(默认)+ 未绑定热键",三档都能往返稳定。
    #[test]
    fn mouse_shield_config_defaults_to_fps_only_and_round_trips() {
        let old = r#"{
            "enabled": true, "anchor_x": 0.5, "anchor_y": 0.5,
            "sensitivity_x": 2.0, "sensitivity_y": 2.0, "invert_y": false,
            "recenter": "Idle", "recenter_idle_ms": 120, "recenter_threshold": 400,
            "hold_key": 0
        }"#;
        let a: Aim = serde_json::from_str(old).expect("老配置应可解析");
        assert_eq!(
            a.mouse_shield,
            MouseShieldMode::FpsOnly,
            "缺省 = 今天的行为"
        );
        assert_eq!(a.shield_key, KeySet::new(), "缺省 = 未绑定手动开关热键");

        // 往返稳定(整份 Aim 写回再读回来)
        let text = serde_norway::to_string(&a).unwrap();
        let back: Aim = serde_norway::from_str(&text).unwrap();
        assert_eq!(back, a);

        // 三档都能往返,且标签与取值一一对应
        for m in MouseShieldMode::ALL {
            let cfg = Aim {
                mouse_shield: m,
                shield_key: KeySet::single(BTN_MIDDLE),
                ..a.clone()
            };
            let text = serde_norway::to_string(&cfg).unwrap();
            let back: Aim = serde_norway::from_str(&text).unwrap();
            assert_eq!(back.mouse_shield, m, "{m:?} 往返后必须还是它");
            assert_eq!(back.shield_key, KeySet::single(BTN_MIDDLE));
            assert!(!m.label().is_empty(), "{m:?} 必须有界面文案");
        }
        assert_eq!(MouseShieldMode::default(), MouseShieldMode::FpsOnly);
    }

    /// 平台可用性(2026-10-22):`ScrcpyForeground` 在 Linux 上恒失效,必须
    /// **显式报告不可用**,而不是让用户选了以后发现"没生效"。
    ///
    /// 判据是纯函数 `usable_on(windows)`,同一进程里两种平台取值都验一遍
    /// —— 否则这条只能在 CI 的某一个平台上跑到一半。
    #[test]
    fn scrcpy_foreground_shield_mode_is_unavailable_off_windows() {
        use MouseShieldMode::{Always, FpsOnly, ScrcpyForeground};

        // Windows:三档全部可用(它是最有用的那一档,不许误伤)
        for m in MouseShieldMode::ALL {
            assert!(m.usable_on(true), "Windows 上 {m:?} 必须可用");
        }
        // 非 Windows:只有依赖"前台窗口"的那一档不可用
        assert!(
            FpsOnly.usable_on(false),
            "FpsOnly 不依赖前台窗口,Linux 可用"
        );
        assert!(Always.usable_on(false), "Always 不依赖前台窗口,Linux 可用");
        assert!(
            !ScrcpyForeground.usable_on(false),
            "ScrcpyForeground 依赖前台进程查询,而它只有 Windows 实现 \
             ⇒ Linux 上恒等于从不独占,必须报告不可用"
        );

        // 阳性对照(§3.3):判据必须真的在区分平台 —— 若两种取值给出同一答案,
        // 上面的断言全是空的。
        assert_ne!(
            ScrcpyForeground.usable_on(true),
            ScrcpyForeground.usable_on(false),
            "阳性对照失败:判据对平台不敏感 ⇒ 它没在判平台"
        );
        // 阴性对照:可用性不许把"当前选中"这件事一并改掉(选了就是选了)
        assert!(
            ScrcpyForeground.usable_on(true),
            "Windows 上这一档必须保持可选"
        );
    }

    /// A/B(2026-10-11):新增的两个自救热键(`wake_key` / `reconnect_key`)在
    /// **老配置里根本没有这两项** —— 读进来必须是"未绑定"。默认不绑 = 行为与
    /// 从前逐位一致(用户明确要求"改体验前先问",所以默认手感不许变);
    /// 绑上之后要能原样往返(单键与两键组合两种写法都吃)。
    #[test]
    fn rescue_hotkeys_default_to_unbound_and_round_trip() {
        let old = r#"{
            "enabled": true, "anchor_x": 0.5, "anchor_y": 0.5,
            "sensitivity_x": 2.0, "sensitivity_y": 2.0, "invert_y": false,
            "recenter": "Idle", "recenter_idle_ms": 120, "recenter_threshold": 400,
            "hold_key": 0
        }"#;
        let a: Aim = serde_json::from_str(old).expect("老配置应可解析");
        assert_eq!(a.wake_key, KeySet::new(), "缺省 = 未绑定「唤醒视角」");
        assert_eq!(a.reconnect_key, KeySet::new(), "缺省 = 未绑定「强制重连」");

        // 单键与两键组合都能往返;空值也照旧写成 0
        let cfg = Aim {
            wake_key: KeySet::single(88),
            reconnect_key: KeySet::from_keys([29, 90]),
            ..a.clone()
        };
        let text = serde_norway::to_string(&cfg).unwrap();
        let back: Aim = serde_norway::from_str(&text).unwrap();
        assert_eq!(back.wake_key, KeySet::single(88), "唤醒键往返后必须还是它");
        assert_eq!(
            back.reconnect_key,
            KeySet::from_keys([29, 90]),
            "重连键(组合写法)往返后必须还是它"
        );
        assert_eq!(back, cfg, "整份 Aim 往返稳定");
    }

    /// D(2026-10-12):「卡死自愈」`Aim::self_heal` 在老配置里**根本没有这一项**
    /// —— 读进来必须是 **true**(用户要求默认开,因为它只在出问题时才动作)。
    ///
    /// 两个方向都要钉死:
    /// - 老 JSON/YAML 缺字段 ⇒ `true`(而不是 serde 给的 `false`);
    /// - 显式写了 `false` ⇒ 保持 `false`(关掉 = 回到纯手动自救键),并能往返。
    #[test]
    fn self_heal_defaults_to_true_for_old_configs() {
        // 逐字照抄"上一轮的老配置":里面没有 self_heal
        let old = r#"{
            "enabled": true, "anchor_x": 0.5, "anchor_y": 0.5,
            "sensitivity_x": 2.0, "sensitivity_y": 2.0, "invert_y": false,
            "recenter": "Idle", "recenter_idle_ms": 120, "recenter_threshold": 400,
            "hold_key": 0
        }"#;
        let a: Aim = serde_json::from_str(old).expect("老配置应可解析");
        assert!(a.self_heal, "老配置缺 self_heal 字段 ⇒ 必须读成 true");

        // 省略整个 aim 段的老配置(Profile 级)同样要拿到 true:
        // `aim` 本身是 `#[serde(default)]`,里面的 `self_heal` 又缺省 true。
        let p: Profile = serde_json::from_str(r#"{"toggle_key": 66, "binds": [], "wheels": []}"#)
            .expect("最小老配置应可解析(其余字段都有 serde 缺省)");
        assert!(p.aim.self_heal, "整段 aim 缺省 ⇒ self_heal 也要是 true");

        // 显式关掉要能原样往返(关掉 = 纯手动自救键)
        let off = Aim {
            self_heal: false,
            ..a.clone()
        };
        let text = serde_norway::to_string(&off).unwrap();
        let back: Aim = serde_norway::from_str(&text).unwrap();
        assert!(!back.self_heal, "显式 false 往返后必须还是 false");
        assert_eq!(back, off, "整份 Aim 往返稳定");
    }

    /// 滚轮换档只在"参与"的档位之间跳:上滚往后、下滚往前,环绕;
    /// 一个都没勾选 → 没有落点(`None`,引擎据此让滚轮退回缩放)。
    #[test]
    fn step_gear_only_lands_on_participating_gears() {
        // 4 档,只有第 0 / 第 2 档参与
        let r = Recoil {
            strengths: vec![1.0, 2.0, 3.0, 4.0],
            gear_wheel: vec![true, false, true, false],
            ..Recoil::default()
        };
        assert_eq!(r.wheel_gear_count(), 2);
        assert_eq!(r.step_gear(0, 1), Some(2), "向后跳过不参与的 1 档");
        assert_eq!(r.step_gear(2, 1), Some(0), "环绕回第一个参与档");
        assert_eq!(r.step_gear(0, -1), Some(2), "向前同样环绕");
        assert_eq!(r.step_gear(2, -1), Some(0));
        // 从"不参与"的当前档出发也要落到参与档上
        assert_eq!(r.step_gear(1, 1), Some(2));
        assert_eq!(r.step_gear(3, 1), Some(0));
        assert_eq!(r.step_gear(0, 0), None, "方向 0 = 不动");

        // 一个都没勾选 → 不换档
        let none = Recoil {
            strengths: vec![1.0, 2.0],
            gear_wheel: vec![false, false],
            ..Recoil::default()
        };
        assert_eq!(none.wheel_gear_count(), 0);
        assert_eq!(none.step_gear(0, 1), None);
        assert_eq!(none.step_gear(0, -1), None);

        // 空档位表 → 什么都不做
        let empty = Recoil {
            strengths: vec![],
            ..Recoil::default()
        };
        assert_eq!(empty.step_gear(0, 1), None);
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

    /// 轮换集合的三态规整(用户 2026-10-23 第 3 条:"一键全勾 / 一键取消"必须可区分)。
    ///
    /// 四条口径:
    /// 1. None(字段缺失)= 不限制 ⇒ 规整后**仍然是 None**(不能写成 Some(全部),
    ///    否则老配置文件会被平白改写,每次启动都整份重写一遍);
    /// 2. Some([]) = 用户主动"全不选" ⇒ 规整后保持 Some([]);
    /// 3. Some(非空) ⇒ 去越界、去重、升序;
    /// 4. Some(非空但全越界) ⇒ **退化成 None**(不限制):这多半是用户删掉了那几套
    ///    组合,不该让整条切换键因此失效 —— 这是旧版"空集合=全部"兜底的延续。
    ///    注意它与第 2 条不同:前者是坏数据,后者是用户的明确选择。
    #[test]
    fn normalize_keeps_rotate_set_three_states_apart() {
        let sw = |key: u16, rs: Option<Vec<usize>>| SwitchKey {
            key,
            keys: KeySet::single(key),
            target: 0,
            direction: SwitchDirection::Next,
            rotate_set: rs,
        };
        let mut doc = ConfigFile {
            schemes: (0..4)
                .map(|i| Profile {
                    name: format!("组合{i}"),
                    ..Profile::default()
                })
                .collect(),
            switch_keys: vec![
                sw(90, None),                   // ① 缺失 = 不限制
                sw(91, Some(Vec::new())),       // ② 全不选
                sw(92, Some(vec![3, 1, 1, 0])), // ③ 脏值:乱序 + 重复
                sw(93, Some(vec![9, 99])),      // ④ 全越界
            ],
            ..ConfigFile::default()
        };
        assert!(doc.normalize());
        assert_eq!(
            doc.switch_keys[0].rotate_set, None,
            "缺失字段不许被写成显式列表(否则老配置被平白改写)"
        );
        assert_eq!(
            doc.switch_keys[1].rotate_set,
            Some(Vec::new()),
            "用户主动全不选必须保留"
        );
        assert_eq!(
            doc.switch_keys[2].rotate_set,
            Some(vec![0, 1, 3]),
            "乱序/重复要整成升序去重"
        );
        assert_eq!(doc.switch_keys[3].rotate_set, None, "全越界 ⇒ 退化成不限制");
        // 幂等:再规整一次不许报改动(否则每次保存都在改写配置)
        assert!(!doc.normalize(), "规整过一次之后就该自洽了");

        // rotate_members 是引擎与界面共用的**唯一口径**,单独钉一遍
        assert_eq!(rotate_members(None, 3), vec![0, 1, 2], "None = 全部参与");
        assert_eq!(
            rotate_members(Some(&[]), 3),
            Vec::<usize>::new(),
            "Some([]) = 一个都不参与"
        );
        assert_eq!(
            rotate_members(Some(&[2, 0, 2, 7]), 3),
            vec![0, 2],
            "去越界 / 去重 / 升序"
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

    /// 2026-10-11 档位增强:老配置(只有 `strengths`)过一次 `normalize` 之后
    /// 三个平行向量等长、"没有名字 + 全部参与"的口径不变;再规范化一次无改动。
    #[test]
    fn normalize_pads_recoil_gear_vectors() {
        let mut doc = ConfigFile::default();
        doc.schemes[0].aim.recoil.strengths = vec![0.4, 9.0, 12.0];
        doc.schemes[0].aim.recoil.gear_names.clear();
        doc.schemes[0].aim.recoil.gear_wheel.clear();
        assert!(doc.normalize(), "补齐平行向量必须报告改动");
        let r = &doc.schemes[0].aim.recoil;
        assert_eq!(r.gear_names.len(), r.strengths.len());
        assert_eq!(r.gear_wheel.len(), r.strengths.len());
        assert!(r.gear_names.iter().all(|n| n.is_empty()));
        assert!(r.gear_wheel.iter().all(|w| *w), "补出来的一律参与换档");
        assert!(!doc.normalize(), "规整过一次之后就该自洽了");
    }

    /// **V2-2 连点 / 轨迹录制的 serde 兼容**(用户 2026-10-13)。
    ///
    /// 两条硬要求:
    /// ① 老配置**没有**这两个字段 ⇒ 读成"关闭/空" ⇒ 行为与从前**逐位一致**;
    /// ② 写出去再读回来必须一模一样(往返稳定,不丢字段)。
    #[test]
    fn tap_repeat_and_swipe_track_round_trip_with_legacy_defaults() {
        // ① 老配置:Tap 没有 repeat_interval_ms
        let legacy = r#"{
            "key": 30,
            "action": { "Tap": { "x": 0.5, "y": 0.5, "duration_ms": 40, "radius": 0.01 } },
            "fps_only": false
        }"#;
        let b: KeyBind = serde_json::from_str(legacy).expect("老配置应能解析");
        match &b.action {
            Action::Tap {
                repeat_interval_ms, ..
            } => assert_eq!(*repeat_interval_ms, 0, "老配置缺字段 ⇒ 连点必须默认关闭"),
            other => panic!("应为 Tap,实际 {other:?}"),
        }
        // 老 Swipe(用 points 形态)
        let legacy_swipe = r#"{
            "key": 30,
            "action": { "Swipe": { "points": [[0.1,0.2],[0.3,0.4]], "duration_ms": 300 } },
            "fps_only": false
        }"#;
        let bs: KeyBind = serde_json::from_str(legacy_swipe).expect("老滑动配置应能解析");
        match &bs.action {
            Action::Swipe(s) => {
                assert!(
                    s.recorded_points.is_empty(),
                    "老配置缺 recorded_points ⇒ 必须为空(走起点→终点)"
                );
                assert_eq!(s.start, (0.1, 0.2), "老的 points 仍要正确落成起点");
                assert_eq!(s.end, (0.3, 0.4));
            }
            other => panic!("应为 Swipe,实际 {other:?}"),
        }

        // ② 新配置往返:两个字段都要能存下来再读回
        let new = KeyBind {
            key: 30,
            action: Action::Tap {
                x: 0.5,
                y: 0.5,
                duration_ms: 40,
                radius: 0.01,
                repeat_interval_ms: 120,
            },
            fps_only: false,
            tail_delay_ms: 0,
        };
        let text = serde_json::to_string(&new).expect("应能序列化");
        assert!(
            text.contains("repeat_interval_ms"),
            "非 0 的连点间隔必须写进配置:{text}"
        );
        let back: KeyBind = serde_json::from_str(&text).expect("应能反序列化");
        assert_eq!(back, new, "连点往返必须稳定");

        let swipe = Swipe {
            start: (0.1, 0.1),
            end: (0.9, 0.9),
            duration_ms: 300,
            easing: Easing::Linear,
            path: SwipePath::Line,
            recorded_points: vec![(0.1, 0.1), (0.5, 0.2), (0.9, 0.9)],
        };
        let text = serde_json::to_string(&swipe).expect("应能序列化");
        assert!(text.contains("recorded_points"), "轨迹必须写进配置:{text}");
        let back: Swipe = serde_json::from_str(&text).expect("应能反序列化");
        assert_eq!(back, swipe, "轨迹往返必须稳定");

        // ③ 关闭状态(0)不写进配置 ⇒ 老配置文件保持干净
        let off = KeyBind {
            key: 30,
            action: Action::Tap {
                x: 0.5,
                y: 0.5,
                duration_ms: 40,
                radius: 0.01,
                repeat_interval_ms: 0,
            },
            fps_only: false,
            tail_delay_ms: 0,
        };
        let text = serde_json::to_string(&off).expect("应能序列化");
        assert!(
            !text.contains("repeat_interval_ms"),
            "0 不该写进配置(保持文件干净):{text}"
        );
    }

    /// **轨迹时间轴按距离分配**(用户 2026-10-13,V2-2)。
    ///
    /// 为什么必须按距离:等分会让"慢移"的段被压缩、看起来突然加速。
    /// 这里钉住四条:
    /// ① 首点 0、末点 = duration_ms(边界精确);
    /// ② 时间轴单调不降(否则注入顺序会乱);
    /// ③ **距离更长的段分到更多时间**;
    /// ④ 全部点重合(总距离 0)时不除零,退化为等分。
    #[test]
    fn swipe_track_timeline_follows_distance() {
        let base = Swipe {
            start: (0.0, 0.0),
            end: (0.0, 0.0),
            duration_ms: 1000,
            easing: Easing::Linear,
            path: SwipePath::Line,
            recorded_points: vec![(0.0, 0.0), (0.01, 0.0), (1.0, 0.0)],
        };
        let t = base.point_timeline_ms();
        assert_eq!(t.len(), 3, "时间轴长度必须与点数一致");
        assert_eq!(t[0], 0, "首点必须 0");
        assert_eq!(t[2], 1000, "末点必须 = duration_ms(吸收取整误差)");
        assert!(t[1] < 500, "短段(0.01)应分到远少于一半的时间:{t:?}");
        assert!(t[0] <= t[1] && t[1] <= t[2], "时间轴必须单调不降:{t:?}");
        // 反过来:第一段很长 ⇒ 它应分到更多
        let long_first = Swipe {
            recorded_points: vec![(0.0, 0.0), (1.0, 0.0), (1.01, 0.0)],
            ..base.clone()
        };
        let t2 = long_first.point_timeline_ms();
        assert!(t2[1] > 500, "长段应分到远多于一半的时间:{t2:?}");
        // 全部点重合 ⇒ 等分,不许 NaN/除零
        let same = Swipe {
            recorded_points: vec![(0.5, 0.5), (0.5, 0.5), (0.5, 0.5)],
            ..base.clone()
        };
        assert_eq!(
            same.point_timeline_ms(),
            vec![0, 500, 1000],
            "重合点应退化为等分"
        );
        // 没有轨迹(空)⇒ point_path 退回起点→终点
        let no_track = Swipe {
            start: (0.1, 0.2),
            end: (0.8, 0.9),
            recorded_points: Vec::new(),
            ..base.clone()
        };
        assert_eq!(
            no_track.point_path(),
            vec![(0.1, 0.2), (0.8, 0.9)],
            "空轨迹必须退回起点→终点(默认行为不变)"
        );
        assert_eq!(base.point_path().len(), 3, "有轨迹时用轨迹");
    }

    /// 折线长度:两段 3-4-5 直角边 ⇒ 总长 10(像素口径)。
    #[test]
    fn polyline_len_sums_segments() {
        let pts = [(0, 0), (3, 4), (3, 4 + 5)];
        assert_eq!(polyline_len_px(&pts), 10.0, "两段 5 + 5");
        // 单点/空折线没有长度,不许 NaN
        assert_eq!(polyline_len_px(&[]), 0.0);
        assert_eq!(polyline_len_px(&[(7, 9)]), 0.0);
    }

    /// 方向标:沿弧长**等距**取点,方向取自所在段;数量与位置都必须对得上。
    #[test]
    fn track_arrow_marks_are_evenly_spaced_with_segment_directions() {
        // 一条 100px 直线,取 3 个锚点 ⇒ 应落在 25 / 50 / 75 处,方向都朝 +x
        let line = [(0, 0), (100, 0)];
        let marks = track_arrow_marks(&line, 3);
        assert_eq!(marks.len(), 3, "取满 3 个锚点");
        let xs: Vec<i32> = marks.iter().map(|(p, _)| p.0).collect();
        assert_eq!(xs, vec![25, 50, 75], "必须等距落在折线内部:{xs:?}");
        for (_, d) in &marks {
            assert!((d.0 - 1.0).abs() < 1e-6 && d.1.abs() < 1e-6, "方向应朝 +x");
        }
        // 折返轨迹:方向标必须跟着段的方向翻过来(这正是加箭头的理由)
        let back = track_arrow_marks(&[(0, 0), (100, 0), (0, 0)], 2);
        assert_eq!(back.len(), 2);
        assert!(
            (back[0].1).0 > 0.0 && (back[1].1).0 < 0.0,
            "去程朝 +x、回程朝 -x,折返必须看得出来:{back:?}"
        );
        // 退化输入:点数不足 / 全部重合 / 要 0 个 ⇒ 空(没有方向可言)
        assert!(track_arrow_marks(&[(5, 5)], 3).is_empty(), "单点无方向");
        assert!(
            track_arrow_marks(&[(5, 5), (5, 5)], 3).is_empty(),
            "全部重合(总长 0)必须返回空,不许除零"
        );
        assert!(track_arrow_marks(&line, 0).is_empty(), "要 0 个就给 0 个");
    }

    /// 抽稀:首末点必须保留、顺序不变、点数有界;够短时原样返回。
    #[test]
    fn decimate_keeps_ends_and_bounds_the_count() {
        let long: Vec<(i32, i32)> = (0..1000).map(|i| (i, i * 2)).collect();
        let d = decimate(&long, 64);
        assert_eq!(d.len(), 64, "抽到上限");
        assert_eq!(d.first(), long.first(), "首点必须保留(起点标记画在它上面)");
        assert_eq!(d.last(), long.last(), "末点必须保留(终点标记画在它上面)");
        assert!(
            d.windows(2).all(|w| w[0].0 <= w[1].0 && w[0].1 <= w[1].1),
            "抽稀后顺序不能乱"
        );
        // 本来就短 ⇒ 原样(不做任何改动,避免无谓的坐标取整)
        let short = [(1, 1), (2, 2), (3, 3)];
        assert_eq!(decimate(&short, 64), short.to_vec());
        // 上限小于 2 时不该 panic(调用方传错也只是不抽稀)
        assert_eq!(decimate(&short, 1).len(), 3);
    }

    /// 录制采样:第一个点无条件记下、静止不堆点、上限到顶后**不清空**。
    ///
    /// 这三条是录制功能唯一的采样规则,三种录制目标(草稿/键位/组合键)共用,
    /// 所以必须钉死 —— 规则漂了,同一只手划出的轨迹会因为录到哪个目标而疏密不同。
    #[test]
    fn track_sampling_dedupes_and_stops_at_the_cap() {
        let mut pts: Vec<(i32, i32)> = Vec::new();
        // ① 空缓冲:第一个点必须记下(否则永远起不了头)
        assert!(push_track_sample(&mut pts, 100, 100), "首点必须记下");
        assert_eq!(pts.len(), 1);
        // ② 太近(曼哈顿距离 3 < 4)不记;正好 4 记
        assert!(!push_track_sample(&mut pts, 103, 100), "距离 3 < 4 不记");
        assert_eq!(pts.len(), 1, "静止/微动不该堆点");
        assert!(push_track_sample(&mut pts, 104, 100), "距离 4 应记下");
        assert_eq!(pts.len(), 2);
        // ③ 上限:到顶后不记,而且**已录的点一个都不许丢**
        let mut full: Vec<(i32, i32)> = (0..TRACK_MAX_POINTS as i32)
            .map(|i| (i * TRACK_MIN_STEP_PX, 0))
            .collect();
        let before = full.clone();
        assert!(
            !push_track_sample(&mut full, 999_999, 999_999),
            "到上限后不该再记"
        );
        assert_eq!(full, before, "到上限只是不再记,绝不能清空已录部分");
        assert_eq!(full.len(), TRACK_MAX_POINTS);
    }

    /// **接缝测试(用户 2026-10-13,V2-3)**:录制的像素点走完
    /// "像素 → 相对坐标 → YAML → 引擎回放路径"整条链之后,必须还是同一只手划的
    /// 那条路径。
    ///
    /// 为什么必须测这条:录制功能的**全部价值**都在这个转换上 ——
    /// 用户在截图浮层上按像素划(`m.rel_x/rel_y`),配置里存相对值(换分辨率/
    /// 换设备要能自适应),引擎再换回像素注入(`Mapper::point`)。任何一环口径
    /// 不一致(比如存了像素却按相对读),表现都是"录了但走偏/走样",
    /// 而三道门(编译/单测/clippy)全绿也照样漏过去。
    #[test]
    fn recorded_pixels_survive_rel_yaml_and_replay() {
        use crate::keymap::{CoordUnit, Mapper, Profile};
        let screen = (1280u32, 2772u32);
        let m = Mapper::new(CoordUnit::Rel, screen);
        // 用户在浮层上划出的像素轨迹(带折返)
        let drawn: Vec<(i32, i32)> = vec![(256, 1386), (1024, 1386), (256, 1386), (1024, 2000)];
        // ① 界面侧:像素 → 相对(与 `finish_track_recording` 同一算法)
        let rel: Vec<(f32, f32)> = drawn
            .iter()
            .map(|&(x, y)| (m.rel_x(x), m.rel_y(y)))
            .collect();
        let swipe = Swipe {
            start: rel[0],
            end: rel[rel.len() - 1],
            duration_ms: 400,
            easing: Easing::Linear,
            path: SwipePath::Line,
            recorded_points: rel.clone(),
        };
        // ② 落盘再读回:配置里的轨迹一个点都不能少、也不能串位
        let text = serde_norway::to_string(&swipe).expect("轨迹必须能序列化");
        let back: Swipe = serde_norway::from_str(&text).expect("必须能读回");
        assert_eq!(back.recorded_points, rel, "YAML 往返后轨迹必须逐点一致");
        // ③ 引擎侧:相对 → 像素(注入用的那条路),必须回到用户划的那些点
        let replayed: Vec<(i32, i32)> = back
            .point_path()
            .iter()
            .map(|&(x, y)| m.point(x, y))
            .collect();
        assert_eq!(
            replayed, drawn,
            "像素→相对→YAML→像素 走完一圈必须回到原轨迹(否则录了会走偏)"
        );
        // ④ 时间轴:首 0、末 = duration_ms,且单调(注入顺序不能乱)
        let tl = back.point_timeline_ms();
        assert_eq!(tl.len(), drawn.len(), "时间轴长度必须与点数一致");
        assert_eq!(tl[0], 0);
        assert_eq!(*tl.last().unwrap(), 400);
        assert!(tl.windows(2).all(|w| w[0] <= w[1]), "时间轴必须单调:{tl:?}");
        // ⑤ 换了分辨率(相对坐标的意义所在):同一份配置仍落在**相同的相对位置**
        let m2 = Mapper::new(CoordUnit::Rel, (720, 1560));
        let scaled: Vec<(i32, i32)> = back
            .point_path()
            .iter()
            .map(|&(x, y)| m2.point(x, y))
            .collect();
        // 每个点的相对位置必须与原来一致(允许 ±1px 取整)
        for (a, b) in scaled.iter().zip(rel.iter()) {
            assert!(
                (a.0 as f32 - b.0 * 720.0).abs() <= 1.0 && (a.1 as f32 - b.1 * 1560.0).abs() <= 1.0,
                "换分辨率后相对位置必须不变:{a:?} vs {b:?}"
            );
        }
        // 顺带钉住:默认配置(无轨迹)仍然是"起点→终点",没有被这次改动影响
        let plain = Swipe {
            recorded_points: Vec::new(),
            ..swipe.clone()
        };
        assert_eq!(plain.point_path().len(), 2, "空轨迹必须退回两点直线");
        let _ = Profile::default();
    }

    /// #1 坐标越界 · 特征测试(用户 2026-10-15 裁定:**不 clamp、不改生产行为**,
    /// 只生成测试锁定现状)。
    ///
    /// 真机取证:方案 screen=2772x1280、`format_version: 2`(相对坐标),轨迹里出现
    /// 4 条宏触点 y 越界——(1031,1888) → 相对 (0.372, **1.475**)、(1016,2471) →
    /// (0.367, **1.931**),而 profile.yaml 里**没有任何**存储的越界坐标(全部落在
    /// 0..1 内、无像素值 > 50) ⇒ 越界是**运行时产生**,静态来源未定位(一次性)。
    ///
    /// 本测试锁定的行为(`Mapper::axis` 故意不做钳位):
    ///   * 相对坐标超出显示范围也**原样换算**:1.475 → 1888,已在屏高 1280 之外;
    ///   * `> 1` 与负值同样直通:2.0 → 5544、-0.5 → -1386;
    ///   * 正常边界精确:0.0 → 0、1.0 → 屏宽/屏高;
    ///   * legacy([`CoordUnit::Pixel`])路径同样直通,像素值只 round 不钳。
    ///
    /// ⚠ 若将来决定给 `Mapper::axis` 加 clamp(用户当时明确说"超过显示范围"且
    /// "影响没测过",故**不改**),本测试**必须故意变红** —— 红色是"该行为被改动过、
    /// 而越界坐标的真实影响仍未实测"的强制提醒;改动者须重新决策并更新本注释,
    /// 不许把断言直接改成钳位后的新值来凑绿(AGENTS §0.6:不红的测试比没有测试更糟)。
    #[test]
    fn mapper_point_passes_through_out_of_range_relative() {
        let m = Mapper::new(CoordUnit::Rel, (2772, 1280));
        // 真机那条越界:相对 (0.372, 1.475) → 像素 (1031, 1888),y 已在屏幕之外
        assert_eq!(
            m.point(0.372, 1.475),
            (1031, 1888),
            "越界相对坐标必须原样换算(passthrough),不许被钳回屏内"
        );
        // 正常边界:0 与 1 精确落在左上角与右下角
        assert_eq!(m.point(0.0, 0.0), (0, 0));
        assert_eq!(m.point(1.0, 1.0), (2772, 1280));
        // 上界之外不截断
        assert_eq!(m.point(2.0, 2.0), (5544, 2560));
        // 负向不钳到 0
        assert_eq!(m.point(-0.5, -0.5), (-1386, -640));
        // legacy(像素单位):值原样 round,与是否越屏无关
        let legacy = Mapper::new(CoordUnit::Pixel, (2772, 1280));
        assert_eq!(legacy.point(1031.0, 1888.0), (1031, 1888));
        assert_eq!(legacy.point(-20.0, 4000.0), (-20, 4000));
    }

    /// #1 坐标越界 · 特征测试(与 [`mapper_point_passes_through_out_of_range_relative`]
    /// 同一裁定:用户 2026-10-15 决定不 clamp、不改生产行为,只生成测试)。
    ///
    /// `ConfigFile::normalize` 是"手改 YAML 的兜底收敛",但它**只管轮盘几何量**
    /// (radius / cx / cy / center_radius 走 clamp_unit_pos / clamp_unit_span),
    /// **完全没有遍历 binds / 宏 instructions / aim.anchor 的坐标字段**。本测试把
    /// "一半钳、一半不钳"的现状钉住:
    ///   * wheel.cx=2.0 / cy=-1.0 ⇒ 被钳到 (1.0, 0.0),并报告 `changed = true`;
    ///   * 同一条配置里宏的 `Click { x: 2.0, y: 1.475 }` 与越界的 `Swipe` 端点,
    ///     normalize 之后**分毫不动**。
    ///
    /// ⚠ 若将来在 normalize 里对宏(或 Tap/Hold/Swipe/anchor)坐标补 clamp,本测试
    /// **必须故意变红**:那意味着"越界坐标从此被静默吞掉",而真机越界的来源与影响
    /// 都还没测过(用户裁定原样保留)。改动者须重新决策,不许改断言凑绿。
    #[test]
    fn normalize_clamps_wheel_geometry_but_leaves_macro_coordinates_untouched() {
        let mut doc = ConfigFile::default();
        let wheel = &mut doc.schemes[0].wheels[0];
        wheel.cx = 2.0;
        wheel.cy = -1.0;
        doc.schemes[0].binds.push(KeyBind {
            key: 30,
            action: Action::Macro(MacroAction {
                steps: Vec::new(),
                instructions: vec![
                    MacroInstruction::Click {
                        x: 2.0,
                        y: 1.475,
                        duration_ms: DEFAULT_TAP_DURATION_MS,
                        delay_ms: 0,
                    },
                    MacroInstruction::Swipe {
                        start_x: -0.5,
                        start_y: 1.931,
                        end_x: 2.0,
                        end_y: 3.0,
                        duration_ms: 300,
                        delay_ms: 0,
                    },
                ],
                virtual_profile: None,
                repeat_while_held: false,
            }),
            fps_only: false,
            tail_delay_ms: DEFAULT_TAIL_DELAY_MS,
        });

        assert!(doc.normalize(), "轮盘几何量被钳位必须报告改动");
        let w = &doc.schemes[0].wheels[0];
        assert_eq!((w.cx, w.cy), (1.0, 0.0), "轮盘中心必须被钳进 0..1(现状:钳)");

        let Action::Macro(mac) = &doc.schemes[0].binds[0].action else {
            panic!("测试自造的宏绑定被 normalize 改掉了,本测试的前提不成立");
        };
        assert_eq!(
            mac.instructions[0],
            MacroInstruction::Click {
                x: 2.0,
                y: 1.475,
                duration_ms: DEFAULT_TAP_DURATION_MS,
                delay_ms: 0,
            },
            "宏 Click 的越界坐标 normalize 后必须原样保留(现状:不遍历宏坐标)"
        );
        assert_eq!(
            mac.instructions[1],
            MacroInstruction::Swipe {
                start_x: -0.5,
                start_y: 1.931,
                end_x: 2.0,
                end_y: 3.0,
                duration_ms: 300,
                delay_ms: 0,
            },
            "宏 Swipe 的四个越界端点同样原样保留"
        );
        assert!(
            !doc.normalize(),
            "规整过一次之后就应自洽(宏坐标不会引发反复改动)"
        );
    }

    // ================= U5(B5r)测量型测试 =================
    //
    // 只测量、不改生产语义。数字靠 `cargo test --bin scrcpy-pad u5_ -- --nocapture` 抓取。
    // 断言刻意放得很松(单批 1000 次 < 500ms),只用来防止"某天突然慢三个数量级",
    // 不承担任何性能门禁职责 —— 真正的判据是打印出来的 p50/p95/max。

    /// 跑 `iters` 次 `run`,逐次计时,返回 (p50, p95, max) 纳秒。
    fn u5_stats<T>(iters: usize, mut run: impl FnMut() -> T) -> (u128, u128, u128) {
        // 预热:把首次分配的页错误/分支预测冷启动排除在样本外(否则 max 只反映冷启动)
        for _ in 0..32 {
            std::hint::black_box(run());
        }
        let mut samples: Vec<u128> = Vec::with_capacity(iters);
        for _ in 0..iters {
            let t = std::time::Instant::now();
            std::hint::black_box(run());
            samples.push(t.elapsed().as_nanos());
        }
        samples.sort_unstable();
        let n = samples.len();
        (samples[n / 2], samples[n * 95 / 100], samples[n - 1])
    }

    /// U5 实测:每帧 `Profile::clone` / `Look::clone` / `decimate` / `polyline_len_px` /
    /// `track_arrow_marks` 的真实单次耗时。
    ///
    /// 口径(写进报告时必须一并给):
    /// * 机器 = 本机(Windows 11,见报告);`cargo test` debug 构建(非 `--release`);
    /// * 每次迭代单独立表(`Instant` 前后夹住一次调用),排序后取 p50 / p95 / max;
    /// * 输入 = `Profile::default()` + N 条 `Action::Tap` 键位(与 bench `02-microbench`
    ///   的 10 / 42 档对齐),轨迹 = 2000 点正弦折线(等于录制上限 `TRACK_MAX_POINTS`)。
    #[test]
    fn u5_clone_and_polyline_costs_are_measured() {
        let mk_profile = |n: usize| -> Profile {
            let mut p = Profile::default();
            p.binds = (0..n)
                .map(|i| KeyBind {
                    key: 30 + (i as u16 % 60),
                    action: Action::Tap {
                        x: 0.1 + (i as f32 * 0.013) % 0.8,
                        y: 0.2 + (i as f32 * 0.007) % 0.7,
                        duration_ms: 60,
                        radius: 0.05,
                        repeat_interval_ms: 0,
                    },
                    fps_only: i % 3 == 0,
                    tail_delay_ms: if i % 4 == 0 { 30 } else { 0 },
                })
                .collect();
            p
        };

        // 2000 点正弦折线:模拟"一屏内最长的一条录制轨迹"。
        let pts: Vec<(i32, i32)> = (0..TRACK_MAX_POINTS as i32)
            .map(|i| (i * 2, (i as f32 * 0.05).sin().mul_add(300.0, 600.0) as i32))
            .collect();
        let dec = decimate(&pts, 256);

        let p10 = mk_profile(10);
        let p42 = mk_profile(42);
        let look = crate::theme::Look::default();

        // §3.3 仪表的阴性对照:空闭包 = 计时器本身的读数地板。
        // 它必须显著小于上面每一项,否则说明"测出来的都是计时开销"。
        let (nop_p50, nop_p95, nop_max) = u5_stats(1000, || ());
        let (c10_p50, c10_p95, c10_max) = u5_stats(1000, || p10.clone());
        let (c42_p50, c42_p95, c42_max) = u5_stats(1000, || p42.clone());
        let (lk_p50, lk_p95, lk_max) = u5_stats(2000, || look.clone());
        let (dc_p50, dc_p95, dc_max) = u5_stats(1000, || decimate(&pts, 256));
        let (ln_p50, ln_p95, ln_max) = u5_stats(2000, || polyline_len_px(&pts));
        let (am_p50, am_p95, am_max) = u5_stats(2000, || track_arrow_marks(&dec, 3));
        // `fill_mask_shapes` 每帧真正拷的是这三份,不是整份 Profile
        let (binds, combos, wheels) = (p42.binds.clone(), p42.combos.clone(), p42.wheels.clone());
        let (fc_p50, fc_p95, fc_max) =
            u5_stats(1000, || (binds.clone(), combos.clone(), wheels.clone()));
        // `ui_combos:11184` 每帧整表拷的那一份(20 组,每组 2 键)
        let combos20: Vec<KeyCombo> = (0..20)
            .map(|i| KeyCombo {
                keys: vec![29, 30 + i as u16],
                action: Action::Tap {
                    x: 0.5,
                    y: 0.5,
                    duration_ms: 60,
                    radius: 0.05,
                    repeat_interval_ms: 0,
                },
                fps_only: false,
                tail_delay_ms: 0,
                repeat_while_held: false,
            })
            .collect();
        let (cb_p50, cb_p95, cb_max) = u5_stats(1000, || combos20.clone());

        println!("==== U5 实测(debug 构建,单次调用纳秒)====");
        println!("基线 空闭包(计时器地板) : p50 {nop_p50} / p95 {nop_p95} / max {nop_max} ns");
        println!("Profile::clone  10 bind : p50 {c10_p50} / p95 {c10_p95} / max {c10_max} ns");
        println!("Profile::clone  42 bind : p50 {c42_p50} / p95 {c42_p95} / max {c42_max} ns");
        println!("Look::clone             : p50 {lk_p50} / p95 {lk_p95} / max {lk_max} ns");
        println!("decimate(2000->256)     : p50 {dc_p50} / p95 {dc_p95} / max {dc_max} ns");
        println!("polyline_len_px(2000)   : p50 {ln_p50} / p95 {ln_p95} / max {ln_max} ns");
        println!("track_arrow_marks(256,3): p50 {am_p50} / p95 {am_p95} / max {am_max} ns");
        println!("fill_mask 三份 clone(42): p50 {fc_p50} / p95 {fc_p95} / max {fc_max} ns");
        println!("combos.clone(20 组)     : p50 {cb_p50} / p95 {cb_p95} / max {cb_max} ns");
        println!("decimate 后点数 = {} / 输入 {}", dec.len(), pts.len());

        // 极松的 sanity(防"慢三个数量级",不是性能门禁):1000 次 42-bind clone < 500ms
        assert!(
            c42_p50 * 1000 < 500_000_000,
            "1000 次 Profile::clone(42 bind) 应远低于 500ms,实测 p50 {c42_p50}ns"
        );
        assert_eq!(dec.len(), 256, "抽稀上限必须生效,否则测的不是生产路径");
    }

    /// U5 ③ 的**可测部分**:`sync_mouse_capture` 每 100ms 调一次
    /// [`crate::mask::client_area_of_pid`],后者内部是一次 `EnumWindows` 全量枚举。
    ///
    /// 无 GUI 条件下的口径限制(如实登记):
    /// * 用一个**不存在的 pid** ⇒ 枚举照走(扫过全部顶层窗)、只是筛不出目标;
    ///   因此测到的是"枚举扫描 + 逐窗 `IsWindowVisible`/`IsIconic`/`GetWindowThreadProcessId`",
    ///   **不含**匹配成功后的 `GetClientRect` + `ClientToScreen` 与真实窗口数量分布;
    /// * 仓内无 GUI 时无法证明真实机器上的顶层窗数量,故这一项只能算"下界实测"。
    #[cfg(windows)]
    #[test]
    fn u5_enum_windows_sweep_cost_is_measured() {
        // pid 非 0 才会真的进 EnumWindows(0 会被 `client_area_of_pid` 直接短路)
        let bogus_pid = 0xFFFF_FFF0u32;
        let (p50, p95, max) = u5_stats(200, || crate::mask::client_area_of_pid(bogus_pid));
        println!("==== U5 实测 ③ EnumWindows 扫描(不存在的 pid,单次纳秒)====");
        println!("client_area_of_pid(bogus): p50 {p50} / p95 {p95} / max {max} ns");
        assert!(
            crate::mask::client_area_of_pid(bogus_pid).is_none(),
            "不存在的 pid 必须返回 None(否则本测试测的就不是枚举下界)"
        );
    }

    // ================= U13(B18)测量型测试 =================
    //
    // 只测量、不改生产语义。数字靠 `cargo test --bin scrcpy-pad u13_ -- --nocapture` 抓取。
    //
    // 口径(`deep` = 一份配置快照真正占的字节):
    //   deep(T) = size_of::<T>()                       // 栈上:含各 Vec/String 的 24 B 头
    //           + Σ 每个 Vec: capacity() × size_of::<元素>()   // 堆上:按**容量**算
    //           + Σ 每个 String: capacity()
    //           + 递归展开 Action::Swipe 的 recorded_points、
    //             Action::Macro 的 steps/instructions/Box<MacroAction>/Box<Profile>
    // 按容量而不是 len 算 ⇒ 结果是**上界**(分配器可能再向上取整,未计入)。
    // 断言只做"账本恒等式"(空配置 = 纯栈、加 N 条 = 精确加 N×元素大小),
    // 不承担任何性能/内存门禁职责 —— 判据是打印出来的数字。

    fn u13_action_heap(a: &Action) -> usize {
        match a {
            Action::Tap { .. } | Action::Hold { .. } | Action::AndroidKey { .. } => 0,
            Action::Swipe(s) => s.recorded_points.capacity() * std::mem::size_of::<(f32, f32)>(),
            Action::Macro(m) => u13_macro_heap(m),
        }
    }

    fn u13_macro_heap(m: &MacroAction) -> usize {
        let mut n = m.steps.capacity() * std::mem::size_of::<MacroStep>();
        n += m.instructions.capacity() * std::mem::size_of::<MacroInstruction>();
        for ins in &m.instructions {
            n += match ins {
                MacroInstruction::Combo { keys, .. } => {
                    keys.capacity() * std::mem::size_of::<u16>()
                }
                // Box<MacroAction> 自己那块堆:结构体 + 其内部堆
                MacroInstruction::Macro { action, .. } => {
                    std::mem::size_of::<MacroAction>() + u13_macro_heap(action)
                }
                _ => 0,
            };
        }
        if let Some(v) = &m.virtual_profile {
            // Box<Profile>:堆上那份整份配置(含它自己的 binds/combos/wheels)
            n += u13_profile_bytes(v);
        }
        n
    }

    fn u13_profile_bytes(p: &Profile) -> usize {
        let mut n = std::mem::size_of::<Profile>();
        n += p.name.capacity();
        n += p.binds.capacity() * std::mem::size_of::<KeyBind>();
        for b in &p.binds {
            n += u13_action_heap(&b.action);
        }
        n += p.combos.capacity() * std::mem::size_of::<KeyCombo>();
        for c in &p.combos {
            n += c.keys.capacity() * std::mem::size_of::<u16>();
            n += u13_action_heap(&c.action);
        }
        n += p.wheels.capacity() * std::mem::size_of::<Wheel>();
        for w in &p.wheels {
            n += w.directions.capacity() * std::mem::size_of::<WheelDirection>();
        }
        n += p.look.bg_path.capacity();
        n
    }

    /// U13 实测:撤销栈(`UNDO_DEPTH=50` × 整份 `Profile`)的内存峰值上界。
    ///
    /// 四档输入,逐档给 `deep` 与 ×50:
    /// * `empty`   —— 全空配置(仪表阴性对照:必须等于纯栈大小)
    /// * `exact`   —— `empty` + 100 条 Tap(仪表阳性对照:必须精确加 100×`size_of::<KeyBind>()`)
    /// * `user`    —— **用户真实配置里最大的那套方案**(2026-10-17 只读解析
    ///   `%APPDATA%\scrcpy-pad\config\profile.yaml`:12 binds / 1 combo / 4 wheels /
    ///   8 directions / 4 macro steps,序列化 5,086 B)
    /// * `stress42`—— U5/A3 同口径的 42 bind + 20 组组合键 + 4 轮盘
    /// * `worst`   —— 结构上界:42 条各带 2000 点录制轨迹的滑动
    #[test]
    fn u13_undo_snapshot_memory_is_measured() {
        let tap = || Action::Tap {
            x: 0.5,
            y: 0.5,
            duration_ms: 60,
            radius: 0.05,
            repeat_interval_ms: 0,
        };
        let hold = |i: u16| Action::Hold {
            x: 0.1 + (i as f32) * 0.01,
            y: 0.2 + (i as f32) * 0.01,
            radius: 0.0127,
        };
        let mk_bind = |i: u16, a: Action| KeyBind {
            key: 30 + i % 60,
            action: a,
            fps_only: i.is_multiple_of(3),
            tail_delay_ms: 0,
        };

        // 空配置:所有可变长字段都为空 ⇒ 只剩栈上结构
        let mut empty = Profile::default();
        empty.name = String::new();
        empty.binds = Vec::new();
        empty.combos = Vec::new();
        empty.wheels = Vec::new();
        empty.look.bg_path = String::new();

        // 阳性对照:精确 100 条 Tap
        let mut exact = empty.clone();
        exact.binds = Vec::with_capacity(100);
        for i in 0..100u16 {
            exact.binds.push(mk_bind(i, tap()));
        }

        // 用户真实配置里最大的那套(🔥正常 / 不正常:12 binds / 1 combo / 4 wheels / 8 dirs)
        let mut user = empty.clone();
        user.name = "🔥正常".to_string();
        user.binds = Vec::with_capacity(12);
        for i in 0..9u16 {
            user.binds.push(mk_bind(i, hold(i)));
        }
        for i in 9..12u16 {
            let mut mac = MacroAction::default();
            mac.steps = (0..4u16)
                .map(|k| MacroStep {
                    code: 30 + k,
                    pressed: k.is_multiple_of(2),
                    delay_ms: 20,
                })
                .collect();
            user.binds.push(mk_bind(i, Action::Macro(mac)));
        }
        user.combos = vec![KeyCombo {
            keys: vec![29, 19],
            action: tap(),
            fps_only: false,
            tail_delay_ms: 0,
            repeat_while_held: false,
        }];
        user.wheels = Vec::with_capacity(4);
        for i in 0..4u16 {
            let mut w = Profile::default().wheels.remove(0);
            w.cx = 0.2 + (i as f32) * 0.15;
            w.directions = (0..2)
                .map(|d| WheelDirection {
                    angle_deg: (d as f32) * 45.0,
                    key: KeySet::single(30 + d),
                    manual: None,
                })
                .collect();
            user.wheels.push(w);
        }

        // U5/A3 同口径:42 bind + 20 组合键 + 4 轮盘
        let mut stress42 = empty.clone();
        stress42.binds = Vec::with_capacity(42);
        for i in 0..42u16 {
            stress42.binds.push(mk_bind(i, tap()));
        }
        stress42.combos = Vec::with_capacity(20);
        for i in 0..20u16 {
            stress42.combos.push(KeyCombo {
                keys: vec![29, 30 + i],
                action: tap(),
                fps_only: false,
                tail_delay_ms: 0,
                repeat_while_held: false,
            });
        }
        stress42.wheels = user.wheels.clone();

        // 结构上界:42 条各 2000 点录制轨迹
        let pts: Vec<(f32, f32)> = (0..TRACK_MAX_POINTS)
            .map(|i| (i as f32 / TRACK_MAX_POINTS as f32, 0.5))
            .collect();
        let mut worst = empty.clone();
        worst.binds = Vec::with_capacity(42);
        for i in 0..42u16 {
            worst.binds.push(mk_bind(
                i,
                Action::Swipe(Swipe {
                    start: (0.1, 0.1),
                    end: (0.9, 0.9),
                    duration_ms: 300,
                    easing: Easing::Linear,
                    path: SwipePath::Line,
                    recorded_points: pts.clone(),
                }),
            ));
        }

        let d_empty = u13_profile_bytes(&empty);
        let d_exact = u13_profile_bytes(&exact);
        let d_user = u13_profile_bytes(&user);
        let d_42 = u13_profile_bytes(&stress42);
        let d_worst = u13_profile_bytes(&worst);

        println!("==== U13 实测(debug 构建,单份快照字节)====");
        println!(
            "size_of: Profile {} / KeyBind {} / Action {} / KeyCombo {} / Wheel {} / WheelDirection {}",
            std::mem::size_of::<Profile>(),
            std::mem::size_of::<KeyBind>(),
            std::mem::size_of::<Action>(),
            std::mem::size_of::<KeyCombo>(),
            std::mem::size_of::<Wheel>(),
            std::mem::size_of::<WheelDirection>(),
        );
        for (label, d) in [
            ("empty   (全空)", d_empty),
            ("exact   (100 Tap)", d_exact),
            ("user    (真实最大方案)", d_user),
            ("stress42(42bind+20combo)", d_42),
            ("worst   (42×2000 点滑动)", d_worst),
        ] {
            println!(
                "{label:<26}: {d:>8} B/份 ⇒ ×50 = {:>9} B ({:.3} MB)",
                d * UNDO_DEPTH_FOR_MEASURE,
                (d * UNDO_DEPTH_FOR_MEASURE) as f64 / (1024.0 * 1024.0)
            );
        }
        println!("参考:用户 profile.yaml 整份 5 套方案 = 32,467 B;最大单套 = 5,086 B(序列化口径)");

        // 阴性对照:全空配置的 deep 必须恰好等于栈上 size_of(没有任何幽灵堆占用)
        assert_eq!(
            d_empty,
            std::mem::size_of::<Profile>(),
            "空配置的 deep 必须等于 size_of::<Profile>()"
        );
        // 阳性对照:100 条 Tap 必须精确增加 100 × size_of::<KeyBind>()(Tap 无堆)
        assert_eq!(
            d_exact,
            d_empty + exact.binds.capacity() * std::mem::size_of::<KeyBind>(),
            "100 条 Tap 的增量必须精确等于 容量 × size_of::<KeyBind>()"
        );
        // 结构上界必须大于 user 档(否则 worst 档构造错了)
        assert!(d_worst > d_user, "worst 档必须大于 user 档");
    }

    /// `UNDO_DEPTH` 在 `app.rs`(私有常量,不在本模块作用域内)——
    /// 这里只作为测量换算系数,写死 50 并在断言里与源码口径对齐一次即可。
    /// (改 `app.rs::UNDO_DEPTH` 时本数字必须同步;见 B18 报告)
    const UNDO_DEPTH_FOR_MEASURE: usize = 50;
}
