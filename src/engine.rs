//! 映射引擎:消费 evdev 按键事件,驱动控制通道。
//! 动作语义:
//!   Tap   - 按下 -> 触点落下,40ms 后抬起
//!   Hold  - 按下 -> 触点落下,松开 -> 抬起
//!   Swipe - 按下 -> 沿折线匀速滑动
//!   Wheel - 方向键组合 -> 虚拟摇杆(圆心按下 + 向方向移动 + 松开回中抬起)
//!          永久轮盘始终生效;临时轮盘仅在启用期间生效(Hold=按住启用键,
//!          Toggle=按一下开/再按关),启用期间方向键归摇杆、同键位的其它绑定失效。
//! 并发:普通键位(点按/长按/滑动)各自独立跟踪按下/抬起(见 Fingers),最多
//!       MAX_CONCURRENT_KEYS 个键可同时按下,互不干扰,便于战斗中放组合技。
//!
//! 三条贯穿全文件的铁律(踩过的坑都在这三条上):
//!   ① **触点绝不能漏抬**:设备端同时只认 10 个触点,漏掉一个就少一个,
//!      攒够了新按键就"按了没反应"。凡是"状态被重建"的地方,必须先把
//!      还在按着的触点抬起来。
//!   ② **归属切换必须立刻对账**:键盘事件是边沿触发的,同一个物理键在
//!      "轮盘方向"与"普通绑定"之间改换门庭的那一瞬间,必须按[`Held`]
//!      里的物理状态把两边的触点重新算一遍,否则那个键要等用户松开再按
//!      才会生效(战场上这很致命)。
//!   ③ **状态一律按"当前配置"重算,不做增量假设**:用户随时可能在开打中
//!      增删键位/摇杆,索引会整体位移。与其维护一堆增量标志,不如在结构变化时
//!      释放 + 重算 —— 慢一帧无所谓,丢一个触点就是事故。

use crate::capture::CaptureEvent;
use crate::control::ControlClient;
use crate::keymap::{
    Action, Aim, KeyBind, Mapper, Profile, RecenterMode, TempMode, Wheel, easing_apply,
    swipe_points,
};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 普通键位(点按/长按/滑动)可同时按下的数量上限。
/// 设备端 PointersState.MAX_POINTERS = 10;瞄准指针与轮盘指针另计,
/// 因此这里留出余量,便于 FPS 场景同时按住开火/蹲/跳等多个键。
/// 真正的硬上限由 [`DEVICE_MAX_POINTERS`] 兜底(见 [`Fingers::try_down`])。
const MAX_CONCURRENT_KEYS: usize = 8;

/// 设备端 scrcpy-server 的 `PointersState.MAX_POINTERS`:
/// 同时按下的触点超过这个数时,**多出来的按下会被服务端直接丢弃**(不是排队),
/// 在用户侧就表现为"按下没反应"。所以客户端必须自己守住这个上限 ——
/// 这是"普通按键按下无反应"最后一层、也是最隐蔽的一层原因。
pub const DEVICE_MAX_POINTERS: usize = 10;

/// 瞄准指针的固定 id(与普通绑定 1000+、轮盘 2000+ 区分开)
const AIM_PID: u64 = 3000;

/// 事件等待的"快档"毫秒数:映射已开启且通道在线时的轮询间隔。
/// 4ms 足够让瞄准的静止归中与鼠标捕获状态保持跟手,
/// 同时把引擎线程的空转次数控制在 250 次/秒以内。
const IDLE_FAST_MS: u64 = 4;

/// 事件等待的"慢档"毫秒数:没连接设备 / 未开启映射时的轮询间隔。
/// 此时循环体不做任何事(没有事件就没有触点要维护),继续按快档空转
/// 等于白烧 CPU 与锁 —— 设备没插着的时候尤其明显。
/// 事件到达会立即唤醒等待,所以这个退避不影响任何响应延迟。
const IDLE_SLOW_MS: u64 = 16;

/// Ctrl / Alt 的 evdev 键码,用于识别"Ctrl+Alt 交还鼠标"这一组合
const KEY_LEFTCTRL: u16 = 29;
const KEY_RIGHTCTRL: u16 = 97;
const KEY_LEFTALT: u16 = 56;
const KEY_RIGHTALT: u16 = 100;

fn is_ctrl(code: u16) -> bool {
    code == KEY_LEFTCTRL || code == KEY_RIGHTCTRL
}

fn is_alt(code: u16) -> bool {
    code == KEY_LEFTALT || code == KEY_RIGHTALT
}

/// Ctrl+Alt 组合的触发判定:两者都按住时,按下的那一方触发一次切换。
/// 这样一次组合只会切换一次,按住不放也不会连发。
fn ctrl_alt_chord(code: u16, pressed: bool, ctrl_down: bool, alt_down: bool) -> bool {
    pressed && ctrl_down && alt_down && (is_ctrl(code) || is_alt(code))
}

/// 引擎内部使用的按键事件:与 capture::CaptureEvent::Button 同构,
/// 保留 code/pressed 字段名以便下方既有逻辑直接复用。
#[derive(Clone, Copy)]
struct CaptureKey {
    code: u16,
    pressed: bool,
}

/// 普通键位并发跟踪器:每个绑定索引独立记录"是否已按下",
/// 保证任意键的按下/抬起严格成对、只作用于自己的 pid,互不干扰。
///  - 同键未抬起时忽略重复按下(全局钩子自动重复/抖动),避免向 scrcpy
///    反复注入同一 pid 的 DOWN,把 server 的多点指针池搅乱;
///  - 没有按下记录的抬起一律忽略,绝不误抬其它键;
///  - 同时按下的键数不超过 MAX_CONCURRENT_KEYS(超出忽略新按下)。
#[derive(Default)]
struct Fingers {
    /// 绑定索引 -> 是否已按下
    down: Vec<bool>,
    /// 当前按下中的键数
    count: usize,
}

impl Fingers {
    /// 绑定列表长度变化时补长(只增不减,删除绑定产生的残留由 free_all 兜底)
    fn align(&mut self, n: usize) {
        if self.down.len() < n {
            self.down.resize(n, false);
        }
    }

    fn is_down(&self, idx: usize) -> bool {
        self.down.get(idx).copied().unwrap_or(false)
    }

    /// 登记一次按下;已按下、并发已达上限、或**加上轮盘/瞄准占用的触点后**
    /// 会超过设备端触点池上限([`DEVICE_MAX_POINTERS`])时返回 false
    /// (调用方据此不注入 DOWN)。
    ///
    /// `others` 是此刻被轮盘与瞄准子系统占用的触点数 —— 它们和普通绑定抢同一个
    /// 设备端指针池,必须一起算,否则"按住好几个键 + 摇杆一直推着"时,
    /// 超出的那次按下会被服务端悄悄丢掉(用户只看到"按键没反应")。
    fn try_down(&mut self, idx: usize, others: usize) -> bool {
        if self.is_down(idx) || self.count >= MAX_CONCURRENT_KEYS {
            return false;
        }
        if self.count + others >= DEVICE_MAX_POINTERS {
            return false;
        }
        // 防御:调用方本应先 align,但引擎线程一旦 panic 就等于整个映射失效,
        // 所以这里宁可自己补一格,也不让越界索引把线程带走。
        if idx >= self.down.len() {
            self.down.resize(idx + 1, false);
        }
        self.down[idx] = true;
        self.count += 1;
        true
    }

    /// 登记一次抬起;仅当确有按下记录时返回 true(调用方据此注入 UP)
    fn release(&mut self, idx: usize) -> bool {
        if self.is_down(idx) {
            self.down[idx] = false;
            self.count = self.count.saturating_sub(1);
            return true;
        }
        false
    }

    fn free_all(&mut self) {
        for v in &mut self.down {
            *v = false;
        }
        self.count = 0;
    }
}

/// 物理按键状态镜像:当前**物理上真的按住**的键码集合。
///
/// 为什么必须有它:临时轮盘启用/停用时,同一个物理键的归属会在"轮盘方向"与
/// "普通绑定"之间来回切换,而键盘事件是边沿触发的 —— 切换的那一瞬间,
/// 引擎必须知道这个键此刻到底还按着没有。否则停用摇杆后,那个键的普通绑定要等
/// 用户"松开再按一次"才重新生效(用户反馈:"松开启用键后,那个键必须松开手一次
/// 才能触发。战场是瞬息万变的,所以请修复他")。
///
/// 它只认按键事件,不受配置改动、轮盘启用状态影响,因此是唯一可信的依据。
#[derive(Default)]
struct Held {
    codes: HashSet<u16>,
}

impl Held {
    fn set(&mut self, code: u16, pressed: bool) {
        if pressed {
            self.codes.insert(code);
        } else {
            self.codes.remove(&code);
        }
    }

    fn has(&self, code: u16) -> bool {
        self.codes.contains(&code)
    }
}

/// 引擎运行状态快照(界面诊断显示,回答"为什么按了没反应")
#[derive(Default, Clone, Copy)]
pub struct EngineLive {
    /// 当前占用的注入触点数(设备端最多同时 [`DEVICE_MAX_POINTERS`] 个)
    pub pointers: usize,
    /// 因触点池已满而被放弃的按下次数(累计)
    pub refused: u64,
    /// 最近一次被放弃的按键(便于定位是哪个键被挤掉)
    pub last_refused: u16,
}

/// 把"普通绑定"的实际按下状态对齐到物理按键状态。
///
/// 这是"归属切换"这条铁律的落地点:凡是会让某个物理键改换门庭的操作
/// (临时轮盘启用/停用、总开关切换、配置被编辑),都要在之后调用它。
///
/// 只对**状态型**动作对账:
///   * `Hold`      —— 按着就该有触点,松开就该抬起;
///   * `AndroidKey`—— 同上(系统键也要成对);
///   * `Tap`(duration=0)是"按一下翻一次"的开关、`Tap`(duration>0)与 `Swipe`
///     是一次性动作,都由事件自己收尾 —— 在这里对账会把它们反复翻转/重放。
///
/// 归属给某个生效中的轮盘方向键、或临时轮盘启用键的物理键不参与普通绑定
/// (与事件路径的优先级链完全一致:总开关键 > 启用键 > 轮盘方向键 > 普通绑定)。
fn reconcile_binds(
    ctl: &ControlClient,
    m: &Mapper,
    profile: &Profile,
    wheels: &[WheelState],
    aim_pointers: usize,
    held: &Held,
    fingers: &mut Fingers,
    active_android_keys: &mut HashSet<u16>,
    live: &mut EngineLive,
) {
    // 轮盘与瞄准占用的触点(普通绑定自己不占;对账期间它们不变)
    let reserved = wheel_pointers(wheels) + aim_pointers;
    for (idx, bind) in profile.binds.iter().enumerate() {
        let want = held.has(bind.key) && !key_owned_by_wheel(profile, wheels, bind.key);
        match &bind.action {
            Action::Hold { x, y, .. } => {
                let pid = bind_pid(idx);
                let (px, py) = m.point(*x, *y);
                if want {
                    if !fingers.is_down(idx) {
                        if fingers.try_down(idx, reserved) {
                            ctl.touch_down(pid, px, py);
                        } else {
                            refuse(live, bind.key);
                        }
                    }
                } else if fingers.release(idx) {
                    ctl.touch_up(pid, px, py);
                }
            }
            Action::AndroidKey { keycode } => {
                let kc = *keycode as u16;
                if want {
                    if active_android_keys.insert(kc) {
                        ctl.key(true, *keycode);
                    }
                } else if active_android_keys.remove(&kc) {
                    ctl.key(false, *keycode);
                }
            }
            _ => {}
        }
    }
}

/// 把轮盘方向状态对齐到物理按键状态(与 [`reconcile_binds`] 同理)。
///
/// 方向状态完全由"该轮盘此刻是否生效 × 对应物理键是否按着"决定,于是:
///   * 启用临时摇杆的瞬间,已按着的方向键立刻推动摇杆(而不是要重按一次);
///   * 停用/配置变化/切总开关时,不再生效的方向一定被抬起来;
///   * 不会出现"某个方向永远以为自己被按着"。
///
/// `extra_pointers` 是普通绑定 + 瞄准此刻占用的触点数(它们与轮盘抢同一个池子)。
fn reconcile_wheels(
    ctl: &ControlClient,
    m: &Mapper,
    profile: &Profile,
    wheels: &mut [WheelState],
    held: &Held,
    extra_pointers: usize,
    live: &mut EngineLive,
) {
    for (j, w) in profile.wheels.iter().enumerate() {
        if j >= wheels.len() {
            break;
        }
        let engaged = w.temp.is_none() || wheels[j].active;
        for (d, key) in [w.up, w.down, w.left, w.right].iter().enumerate() {
            wheels[j].pressed[d] = engaged && held.has(*key);
        }
        // 本轮盘此刻若已经按着,update_wheel 不会再去申请新触点,故不必减掉自己
        let reserved = extra_pointers + wheel_pointers(wheels);
        if update_wheel(ctl, m, j, w, &mut wheels[j], reserved) {
            refuse(live, w.up);
        }
    }
}

/// 该物理键此刻是否归摇杆(生效中的轮盘方向键,或任意临时轮盘的启用键)。
///
/// 临时轮盘的启用键**无论是否生效**都归它自己:事件路径上它总是被消费掉
/// (见 run() 里的启用键分支),所以普通绑定永远收不到它。
fn key_owned_by_wheel(profile: &Profile, wheels: &[WheelState], code: u16) -> bool {
    profile.wheels.iter().enumerate().any(|(j, w)| {
        if let Some(t) = &w.temp {
            if t.key == code {
                return true;
            }
        }
        let engaged = w.temp.is_none() || wheels.get(j).map(|s| s.active).unwrap_or(false);
        engaged && (w.up == code || w.down == code || w.left == code || w.right == code)
    })
}

/// 轮盘此刻占用的触点数(每个轮盘最多一个:按着才有)
fn wheel_pointers(wheels: &[WheelState]) -> usize {
    wheels.iter().filter(|w| w.down).count()
}

/// 记一次"因为触点池满而放弃按下"
fn refuse(live: &mut EngineLive, code: u16) {
    live.refused += 1;
    live.last_refused = code;
}

/// 抬起当前所有普通绑定触点与系统键(状态重建/结构变化前调用)。
/// 不动轮盘与瞄准 —— 它们各有自己的释放路径。
///
/// 遍历的是**按下表本身的长度**而不是当前配置的绑定数:用户删掉几个键位时,
/// 那几个被删掉的槽位里可能还记着"按着",漏掉它们就是永久卡在设备上的触点。
/// 越界槽位用 (0,0) 抬起 —— 坐标一定落在屏内,不会因为"落在屏外"被系统整条丢弃。
fn release_all_binds(
    ctl: &ControlClient,
    m: &Mapper,
    profile: &Profile,
    fingers: &mut Fingers,
    active_android_keys: &mut HashSet<u16>,
) {
    let n = fingers.down.len().max(profile.binds.len());
    for idx in 0..n {
        if fingers.release(idx) {
            let (x, y) = bind_point(profile, m, idx);
            ctl.touch_up(bind_pid(idx), x, y);
        }
    }
    for kc in active_android_keys.drain() {
        ctl.key(false, kc as u32);
    }
    fingers.free_all();
}

/// 让引擎的运行状态与当前配置对齐;返回是否发生了结构变化。
///
/// 用户随时可能在开打中增删键位/摇杆,索引会整体位移。这里的原则是
/// **宁可抬手重算,绝不带着旧状态跑**:
///   * 普通绑定表结构变了 -> 先抬起全部绑定触点与系统键,再重建按下表;
///   * 轮盘表结构变了 -> 先用旧状态把在按的轮盘触点抬起来(回中再抬),
///     再按新配置重建轮盘状态。
/// 重建后**按物理按键状态恢复临时轮盘的启用标记**(长按模式):用户正按着某个
/// 启用键时,重建不该让摇杆"悄悄失效"。调用方在返回 true 时应立刻对账一次,
/// 把仍按着的键/方向补回来 —— 于是对玩家来说,增删一个键位或摇杆是无感的。
fn sync_structures(
    ctl: &ControlClient,
    m: &Mapper,
    profile: &Profile,
    wheels: &mut Vec<WheelState>,
    wheel_sig: &mut u64,
    binds_sig: &mut u64,
    fingers: &mut Fingers,
    active_android_keys: &mut HashSet<u16>,
    held: &Held,
) -> bool {
    let mut changed = false;

    // ---- 普通绑定 ----
    let bs = binds_signature(&profile.binds);
    if bs != *binds_sig {
        *binds_sig = bs;
        release_all_binds(ctl, m, profile, fingers, active_android_keys);
        // 按下表按新长度重建(旧槽位里的残留已被 free_all 清掉)
        fingers.down.clear();
        fingers.align(profile.binds.len());
        changed = true;
    }
    fingers.align(profile.binds.len());

    // ---- 轮盘 ----
    let ws = wheel_signature(&profile.wheels);
    if ws != *wheel_sig {
        *wheel_sig = ws;
        for (j, st) in wheels.iter_mut().enumerate() {
            if !st.down {
                continue;
            }
            // 回中再抬:旧圆心若在新配置里已经不在了,就用最后推送的位置,
            // 兜底也要给一个屏内坐标 —— 落在屏外的 UP 会被系统整条丢弃,
            // 那个触点就永远留在设备上了。
            let (cx, cy) = profile
                .wheels
                .get(j)
                .map(|w| m.point(w.cx, w.cy))
                .unwrap_or(st.last);
            ctl.touch_move(wheel_pid(j), cx, cy);
            ctl.touch_up(wheel_pid(j), cx, cy);
            st.down = false;
        }
        *wheels = vec![WheelState::default(); profile.wheels.len()];
        // 长按模式的临时轮盘:启用状态直接由"启用键是否还按着"恢复;
        // 切换模式是锁存状态,没有可信的物理依据,一律回到未启用(按一下即可再开)
        for (j, w) in profile.wheels.iter().enumerate() {
            if let Some(t) = &w.temp {
                wheels[j].active = t.mode == TempMode::Hold && held.has(t.key);
            }
        }
        changed = true;
    }
    changed
}

/// 普通绑定的结构指纹:数量 + 每个键的键码与动作类型。
///
/// 索引是引擎跟踪"哪个键按着"的依据(pid = 1000+idx)。用户增删一个键位,
/// 后面所有索引都会整体位移:旧触点的按下记录会被错记到别的键上,
/// 那个键于是"按了没反应",而真正的旧触点则永久留在设备上。
/// 因此指纹一变就必须先抬起全部触点、再按新配置重建。
fn binds_signature(binds: &[KeyBind]) -> u64 {
    let mut h = binds.len() as u64;
    for b in binds {
        let kind = match &b.action {
            Action::Tap { duration_ms, .. } => {
                if *duration_ms == 0 { 1 } else { 2 }
            }
            Action::Hold { .. } => 3,
            Action::Swipe(_) => 4,
            Action::AndroidKey { .. } => 5,
        };
        h = h
            .wrapping_mul(0x100_0000_01b3)
            .wrapping_add(b.key as u64 + 1)
            .rotate_left(7)
            .wrapping_add(kind);
    }
    h
}

/// 轮盘的结构指纹:数量 + 方向键 + 启用键与模式。
///
/// 只关心会改变**键位归属与 pid 分配**的字段。坐标/半径/影响范围的变化不需要
/// 重建(下一帧推一下就是新的位置了),否则玩家一边调参一边打会被反复抬手。
///
/// 指纹变化必须"先释放旧触点、再重建状态",否则会同时踩中三种老毛病:
///   ① 旧触点永远留在设备上(触点池越用越少 → 新按键按不动);
///   ② 旧触点的 pid 被新轮盘接管(用户看到的"新摇杆与旧摇杆换位/乱动");
///   ③ 临时轮盘的启用状态错挂到别的轮盘上。
fn wheel_signature(wheels: &[Wheel]) -> u64 {
    let mut h = wheels.len() as u64;
    for w in wheels {
        for k in [w.up, w.down, w.left, w.right] {
            h = h.wrapping_mul(0x100_0000_01b3).wrapping_add(k as u64 + 1);
        }
        match &w.temp {
            None => h = h.wrapping_mul(31).wrapping_add(0x9E37_79B9),
            Some(t) => {
                h = h.wrapping_mul(31).wrapping_add(t.key as u64 + 1);
                h = h.wrapping_mul(31).wrapping_add(match t.mode {
                    TempMode::Hold => 1,
                    TempMode::Toggle => 2,
                });
            }
        }
    }
    h
}


// ============================ FPS 鼠标瞄准 ============================
//
// 手机 FPS 的视角靠"手指在屏幕上拖动"实现,而鼠标给的是相对位移,
// 因此这里维护一个独立的手指触点:
//   鼠标位移 -> 累积偏移 -> 落点 = 锚点 + 偏移 -> 注入 touch_move
// 偏移接近屏幕边界时无法继续转向,所以需要「抬指 + 在锚点重按」(=归中)。

/// 瞄准运行状态(供界面显示,用来确认"偏移到底有没有在累积、触点有没有落下")
#[derive(Default, Clone, Copy)]
pub struct AimLive {
    /// 收到的鼠标位移事件数
    pub motions: u64,
    /// 最近一次位移量
    pub last_dx: f32,
    pub last_dy: f32,
    /// 当前累积偏移
    pub ox: f32,
    pub oy: f32,
    /// 瞄准触点当前是否按在屏幕上
    pub down: bool,
    /// 当前是否满足瞄准生效条件
    pub active: bool,
    /// 已注入的瞄准触点消息数(down/move/up 合计)
    pub sent: u64,
}

/// 瞄准子系统状态
#[derive(Default)]
struct AimState {
    /// 手指当前是否按在屏幕上
    down: bool,
    /// 累积偏移(设备像素,相对锚点)
    ox: f32,
    oy: f32,
    /// 当前落点(抬指时需要给出正确坐标)
    cur: (i32, i32),
    /// 门控键是否按住(hold_key == 0 时忽略)
    gate_down: bool,
    /// 用户按 Ctrl+Alt 临时把鼠标交还给系统:此时不瞄准、也不捕获光标
    released: bool,
    /// 最近一次鼠标位移时间(静止归中的依据)
    last_motion: Option<Instant>,
    /// 当前拖动使用的锚点;锚点被改动时重置拖动
    anchor_used: (i32, i32),
    /// 已注入的触点消息数(仅诊断用)
    sent: u64,
}

/// 瞄准自身是否具备生效条件(是否映射开启由调用方判断)
fn aim_active(aim: &Aim, st: &AimState) -> bool {
    aim.enabled
        && aim.anchor_set()
        && !st.released
        && (aim.hold_key == 0 || st.gate_down)
}

/// 在当前位置抬起瞄准触点并清空偏移
fn aim_lift(ctl: &ControlClient, st: &mut AimState) {
    if st.down {
        let (x, y) = st.cur;
        ctl.touch_up(AIM_PID, x, y);
        st.sent += 1;
        st.down = false;
    }
    st.ox = 0.0;
    st.oy = 0.0;
    st.last_motion = None;
}

/// 没有控制通道时只清理本地状态
fn aim_release_local(st: &mut AimState) {
    st.down = false;
    st.ox = 0.0;
    st.oy = 0.0;
    st.last_motion = None;
}

/// 锚点换算成像素并钳进当前屏幕内。锚点可能是在别的屏幕方向下取的
/// (例如竖屏取的锚点,横屏时 y 会超出屏幕高度),而落在屏幕外的触摸
/// 会被系统整条丢弃,表现为"鼠标怎么动都没有反应",因此注入前必须钳制。
fn aim_anchor(m: &Mapper, aim: &Aim) -> (i32, i32) {
    let (x, y) = m.point(aim.anchor_x, aim.anchor_y);
    (
        x.clamp(0, (m.w as i32 - 1).max(0)),
        y.clamp(0, (m.h as i32 - 1).max(0)),
    )
}

/// 落点 = 锚点 + 累积偏移,并钳制在屏幕内;第三个返回值表示是否触到边界
fn aim_target(m: &Mapper, aim: &Aim, st: &AimState) -> (i32, i32, bool) {
    let (ax, ay) = aim_anchor(m, aim);
    let ax = ax as f32;
    let ay = ay as f32;
    let max_ox = (m.w - 1.0 - ax).max(0.0);
    let max_oy = (m.h - 1.0 - ay).max(0.0);
    let cx = st.ox.clamp(-ax.max(0.0), max_ox);
    let cy = st.oy.clamp(-ay.max(0.0), max_oy);
    let hit_edge = cx != st.ox || cy != st.oy;
    ((ax + cx).round() as i32, (ay + cy).round() as i32, hit_edge)
}

/// 归中:在当前位置抬指,回到锚点重新按下。
/// 这一轮拖动产生的转向已经生效,玩家侧只是看到视角继续转动。
fn aim_recenter(ctl: &ControlClient, m: &Mapper, aim: &Aim, st: &mut AimState) {
    if !st.down {
        st.ox = 0.0;
        st.oy = 0.0;
        return;
    }
    let (cx, cy) = st.cur;
    ctl.touch_up(AIM_PID, cx, cy);
    st.sent += 1;
    st.ox = 0.0;
    st.oy = 0.0;
    let (ax, ay) = aim_anchor(m, aim);
    ctl.touch_down(AIM_PID, ax, ay);
    st.sent += 1;
    st.cur = (ax, ay);
    // 刚归中,重置静止计时,避免立刻再次归中
    st.last_motion = Some(Instant::now());
}

/// 鼠标相对位移 -> 手机上的拖动。
/// 返回是否因为触点池已满而没能落下瞄准触点(供诊断计数;落不下就不落下,
/// 下一次位移会再试,不会留下半个状态)。
fn aim_on_motion(
    ctl: &ControlClient,
    m: &Mapper,
    aim: &Aim,
    st: &mut AimState,
    dx: f32,
    dy: f32,
    reserved: usize,
) -> bool {
    if !aim_active(aim, st) {
        return false;
    }
    if !st.down && reserved >= DEVICE_MAX_POINTERS {
        return true;
    }
    let anchor = aim_anchor(m, aim);
    // 锚点被改动过(或屏幕方向变了):结束旧拖动,从新锚点重新开始
    if st.anchor_used != anchor {
        aim_lift(ctl, st);
        st.anchor_used = anchor;
    }

    st.ox += dx * aim.sensitivity_x;
    let sy = if aim.invert_y {
        -aim.sensitivity_y
    } else {
        aim.sensitivity_y
    };
    st.oy += dy * sy;
    st.last_motion = Some(Instant::now());

    let (tx, ty, hit_edge) = aim_target(m, aim, st);
    if st.down {
        ctl.touch_move(AIM_PID, tx, ty);
    } else {
        ctl.touch_down(AIM_PID, tx, ty);
        st.down = true;
    }
    st.sent += 1;
    st.cur = (tx, ty);

    // 阈值归中:偏移超过设定值;或已顶到边界(否则无法继续同向转向)
    let th = aim.recenter_threshold.max(1) as f32;
    let threshold_hit =
        aim.recenter == RecenterMode::Threshold && (st.ox.abs() >= th || st.oy.abs() >= th);
    let edge_hit = hit_edge && aim.recenter != RecenterMode::Never;
    if threshold_hit || edge_hit {
        aim_recenter(ctl, m, aim, st);
    }
    false
}

/// 定期维护:映射关闭/门控松开 -> 收手;静止归中 -> 回锚点
fn aim_tick(ctl: &ControlClient, m: &Mapper, aim: &Aim, mapping_enabled: bool, st: &mut AimState) {
    if !mapping_enabled || !aim_active(aim, st) {
        aim_lift(ctl, st);
        return;
    }
    if !st.down || aim.recenter != RecenterMode::Idle {
        return;
    }
    let idle_ms = aim.recenter_idle_ms.max(16) as u128;
    let paused = st
        .last_motion
        .map(|t| t.elapsed().as_millis() >= idle_ms)
        .unwrap_or(false);
    if paused && (st.ox != 0.0 || st.oy != 0.0) {
        aim_recenter(ctl, m, aim, st);
    }
}

pub struct Shared {
    pub profile: Profile,
    pub enabled: bool,
    pub control: Option<ControlClient>,
    /// 瞄准运行状态(界面诊断显示)
    pub aim_live: AimLive,
    /// 引擎运行状态(触点占用 / 因触点池满被拒的按下次数),界面诊断显示。
    /// 用户抱怨过"普通按键按下无反应,原因不明" —— 有了它,界面就能直接
    /// 说出"此刻占了几个触点、刚才哪个键因为挤不进去被放弃了"。
    pub live: EngineLive,
    /// 顶栏[映射:开/关]按钮请求引擎执行一次"关闭映射"的收尾。
    ///
    /// 为什么需要它:关闭映射时必须抬起所有仍按着的触点(否则手机上会一直按着,
    /// 也就是俗称的"卡键"),而这些触点状态全部由引擎线程独占。早期版本只有总开关键
    /// 做这件事,顶栏按钮直接改 `enabled` —— 于是"用按钮关映射"会把长按触点
    /// 永久留在屏幕上。现在两条路径共用 [`release_all`] 这一份实现。
    pub toolbar_release: bool,
}

pub type SharedState = Arc<Mutex<Shared>>;

#[derive(Debug, Clone)]
enum SchedAct {
    Up { pid: u64, x: i32, y: i32 },
    Move { pid: u64, x: i32, y: i32 },
}

#[derive(Default, Clone)]
struct WheelState {
    pressed: [bool; 4], // up down left right
    down: bool,
    last: (i32, i32),
    /// 临时轮盘当前是否处于启用状态(永久轮盘恒为 true)
    active: bool,
}

/// 引擎线程独占的可变运行状态。
///
/// 抽成结构体是为了让"关闭映射收尾"能写成一份可复用实现([`release_all`]):
/// 总开关键(全局钩子路径)与顶栏按钮(界面路径)都要用它,而它必须同时访问
/// 触点表、轮盘状态、系统键集合与瞄准状态。
///
/// 注意:字段是**可变的借用**,借用范围只覆盖执行收尾的那一瞬间,
/// 因此不会与调用点上对 `Shared` 的借用冲突。
pub(crate) struct EngineState<'a> {
    fingers: &'a mut Fingers,
    wheels: &'a mut Vec<WheelState>,
    active_android_keys: &'a mut HashSet<u16>,
    aim: &'a mut AimState,
}

/// 关闭映射时的收尾:抬起所有按下中的触点、松开系统键、停用全部临时轮盘、
/// 抬掉瞄准触点。由总开关键与顶栏按钮两条路径共用。
///
/// 参数取 `screen` 而不是从 `shared.control` 里读,是为了让调用点能在**不额外持锁**
/// 的情况下先取好尺寸(否则 `control` 的不可变借用会与后面的 `&mut shared` 冲突)。
///
/// 无论控制通道在不在都要清本地状态,否则重新开打时状态会残留
/// (例如临时轮盘仍被当成"已启用",或按住记录还留着)。
pub(crate) fn release_all(
    ctl: Option<&ControlClient>,
    shared: &mut Shared,
    screen: (u32, u32),
    st: EngineState<'_>,
) {
    match ctl {
        Some(c) => {
            let m = shared.profile.mapper(screen);
            // 瞄准触点一并抬起(否则松手后仍按在屏幕上)
            aim_lift(c, st.aim);
            // 遍历按下表本身的长度而不是当前绑定数:被删掉的槽位里可能还记着"按着",
            // 漏掉它就是一个永久卡在设备上的触点(触点池会被越用越少)。
            let n = st.fingers.down.len().max(shared.profile.binds.len());
            for idx in 0..n {
                if st.fingers.release(idx) {
                    let (x, y) = bind_point(&shared.profile, &m, idx);
                    c.touch_up(bind_pid(idx), x, y);
                }
            }
            for kc in st.active_android_keys.drain() {
                c.key(false, kc as u32);
            }
            for (j, ws) in st.wheels.iter_mut().enumerate() {
                if !ws.down {
                    continue;
                }
                // 配置可能刚被编辑过:轮盘数量以取得到的那一个为准,
                // 取不到就用最后推送过的位置 —— 总之必须把这个触点抬起来
                let (cx, cy) = shared
                    .profile
                    .wheels
                    .get(j)
                    .map(|w| m.point(w.cx, w.cy))
                    .unwrap_or(ws.last);
                c.touch_move(wheel_pid(j), cx, cy);
                c.touch_up(wheel_pid(j), cx, cy);
                ws.down = false;
            }
        }
        None => {
            st.active_android_keys.clear();
            aim_release_local(st.aim);
        }
    }
    st.fingers.free_all();
    for ws in st.wheels.iter_mut() {
        ws.down = false;
        ws.pressed = [false; 4];
        ws.active = false;
    }
}

pub fn run(
    shared: SharedState,
    rx: Receiver<CaptureEvent>,
    gui_tx: Sender<CaptureEvent>,
    mouse_grab: Arc<AtomicBool>,
) {
    let mut scheduled: Vec<(Instant, SchedAct)> = Vec::new();
    let mut fingers = Fingers::default();
    let mut active_android_keys: HashSet<u16> = HashSet::new();
    let mut wheels: Vec<WheelState> = Vec::new();
    // 配置的"结构指纹":绑定表与轮盘表各一份。指纹变化 = 索引会整体位移,
    // 此时必须先把在按的触点全部抬起来再重建状态(见 sync_structures)。
    let mut binds_sig = 0u64;
    let mut wheel_sig = 0u64;
    // 物理按键镜像(归属切换对账的唯一依据)
    let mut held = Held::default();
    // 引擎运行状态(触点占用/被拒次数),供界面诊断显示
    let mut live = EngineLive::default();
    let mut aim = AimState::default();
    // Ctrl+Alt 组合:按下即把鼠标交还给系统,再按一次收回(需用全局钩子判定,
    // 因为开打时焦点通常在 scrcpy 窗口,主程序收不到按键)
    let mut ctrl_down = false;
    let mut alt_down = false;
    // 顶栏按钮请求的收尾是否已经执行过(用于识别请求的上升沿)
    let mut toolbar_release_prev = false;

    loop {
        // 处理到期的计划动作。
        // 优化:先把到期动作挑出来,再整批用一次加锁执行;旧写法对每个到期动作都要
        // 加两次锁(滑动一次会排入几十个 Move,等于几十轮加解锁)。
        // ControlClient 不是 Clone(内含两个 socket 线程的所有权),仍需借出引用,
        // 但持锁范围只覆盖真正要发命令的这几个动作。
        {
            let now = Instant::now();
            let mut due: Vec<SchedAct> = Vec::new();
            let mut i = 0;
            while i < scheduled.len() {
                if scheduled[i].0 <= now {
                    let (_, act) = scheduled.swap_remove(i);
                    due.push(act);
                } else {
                    i += 1;
                }
            }
            if !due.is_empty() {
                let g = shared.lock().unwrap();
                if let Some(c) = g.control.as_ref() {
                    for act in due {
                        match act {
                            SchedAct::Up { pid, x, y } => {
                                c.touch_up(pid, x, y);
                                // 定时抬起(点按 40ms / 滑动终点):该键本次按下结束,清除按下状态。
                                // 只处理绑定指针段(1000..2000),避免误动轮盘/瞄准的按下状态。
                                if (1000..2000).contains(&pid) {
                                    fingers.release((pid - 1000) as usize);
                                }
                            }
                            SchedAct::Move { pid, x, y } => c.touch_move(pid, x, y),
                        }
                    }
                }
            }
        }

        // 顶栏[映射:开/关]按钮请求的收尾(上升沿触发一次)。
        // 与总开关键走同一份 release_all,保证"用按钮关映射"也不会在手机上留下按住的触点。
        {
            let mut g = shared.lock().unwrap();
            if g.toolbar_release && !toolbar_release_prev {
                g.enabled = false;
                // 把控制通道临时取出,拿到 `Option<ControlClient>` 的所有权,
                // 这样既能读出屏幕尺寸、又能同时可变借用 `g`(借用检查器不会再抱怨)。
                // 取不出(未连接)就按"无通道"分支清本地状态。
                let ctl = g.control.take();
                let screen = ctl
                    .as_ref()
                    .map(|c| (c.screen_w, c.screen_h))
                    .unwrap_or((0, 0));
                release_all(
                    ctl.as_ref(),
                    &mut g,
                    screen,
                    EngineState {
                        fingers: &mut fingers,
                        wheels: &mut wheels,
                        active_android_keys: &mut active_android_keys,
                        aim: &mut aim,
                    },
                );
                g.control = ctl;
                // 重新开打时恢复鼠标捕获(丢掉上一局的"交还鼠标"状态)
                aim.released = false;
            }
            toolbar_release_prev = g.toolbar_release;
        }

        // 瞄准定期维护 + 同步鼠标捕获状态。
        // 这一步只做两件事:①维护 mouse_grab 标志(界面改不了原子量)
        // ②瞄准的"静止归中"。因此当通道没连上、映射没开、瞄准触点也没落下时,
        // 完全没必要按 4ms 跑 —— 这正是下面 idle 退避要利用的性质。
        let fast;
        {
            let mut g = shared.lock().unwrap();
            let s: &mut Shared = &mut g;
            let cfg = s.profile.aim.clone();
            let enabled = s.enabled;
            let connected = s
                .control
                .as_ref()
                .map(|c| c.is_connected())
                .unwrap_or(false);
            fast = (enabled && connected) || aim.down;
            // 仅当映射开启、控制通道在线、瞄准启用且未被 Ctrl+Alt 释放时才捕获光标,
            // 避免出现"光标被冻结但什么都做不了";
            // 绑了"按住才瞄准"时只在按住期间捕获,松手即把光标还给系统
            mouse_grab.store(
                enabled
                    && connected
                    && cfg.enabled
                    && cfg.capture_mouse
                    && cfg.anchor_set()
                    && (cfg.hold_key == 0 || aim.gate_down)
                    && !aim.released,
                Ordering::Relaxed,
            );
            match s.control.as_ref() {
                Some(ctl) if connected => {
                    let m = s.profile.mapper((ctl.screen_w, ctl.screen_h));
                    aim_tick(ctl, &m, &cfg, enabled, &mut aim);
                }
                _ => aim_release_local(&mut aim),
            }
            let l = &mut s.aim_live;
            l.ox = aim.ox;
            l.oy = aim.oy;
            l.down = aim.down;
            l.active = aim_active(&cfg, &aim);
            l.sent = aim.sent;
            // 触点占用是"此刻"的量:轮盘/瞄准的状态可能刚被上面的维护改动过
            live.pointers = fingers.count + wheel_pointers(&wheels) + usize::from(aim.down);
            s.live = live;
        }

        // 等待下一个事件。有未到期的计划动作时,精确等到最早那个到期为止,
        // 使点按的定时抬起/滑动的每一步都落在预定时刻(误差由 ~4ms 降到 ~1ms)。
        // 没有计划动作时按状态选档:
        //   快档(IDLE_FAST_MS):映射开着且通道在线,或瞄准触点还按在屏幕上
        //                        —— 归中与捕获状态都必须及时;
        //   慢档(IDLE_SLOW_MS):没连接/没开映射 —— 此时上面两步什么也不会做,
        //                        按快档空转等于白烧 250 次/秒的 CPU 与加锁。
        // 事件到达会立刻唤醒 recv_timeout,所以退避只影响"轮询精度",不影响响应延迟。
        let idle_ms = if fast { IDLE_FAST_MS } else { IDLE_SLOW_MS };
        let wait = scheduled
            .iter()
            .map(|(t, _)| t.saturating_duration_since(Instant::now()))
            .min()
            .map(|d| d.min(Duration::from_millis(idle_ms)))
            .unwrap_or(Duration::from_millis(idle_ms));
        let raw = match rx.recv_timeout(wait) {
            Ok(ev) => ev,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };

        // 鼠标相对位移:交给瞄准子系统,不参与按键绑定流程
        let ev = match raw {
            CaptureEvent::Button { code, pressed } => CaptureKey { code, pressed },
            CaptureEvent::Motion { dx, dy } => {
                let mut g = shared.lock().unwrap();
                let s: &mut Shared = &mut g;
                s.aim_live.motions += 1;
                s.aim_live.last_dx = dx;
                s.aim_live.last_dy = dy;
                if s.enabled {
                    // 只克隆瞄准配置(小结构),并临时借出(profile, control)两块互不相干的字段。
                    // 早期版本此处克隆整个 Profile(含全部键位/轮盘),而鼠标位移是高频事件,
                    // 每来一个位移就深拷贝一次配置纯属浪费。
                    let cfg = s.profile.aim.clone();
                    let (profile, control) = (&s.profile, &s.control);
                    if let Some(ctl) = control.as_ref() {
                        if ctl.is_connected() {
                            let m = profile.mapper((ctl.screen_w, ctl.screen_h));
                            // 瞄准触点也要占设备端指针池:挤不进去就别落下,
                            // 记为一次"被拒",免得用户以为瞄准坏了
                            let reserved = wheel_pointers(&wheels) + fingers.count;
                            if aim_on_motion(ctl, &m, &cfg, &mut aim, dx, dy, reserved) {
                                refuse(&mut live, 0);
                            }
                        }
                    }
                }
                let l = &mut s.aim_live;
                l.ox = aim.ox;
                l.oy = aim.oy;
                l.down = aim.down;
                l.sent = aim.sent;
                s.live = live;
                continue;
            }
        };

        // 转发给 GUI(按键捕获绑定用;鼠标位移不需要)
        if ev.pressed {
            let _ = gui_tx.send(CaptureEvent::Button {
                code: ev.code,
                pressed: true,
            });
        }

        let (enabled, toggle_key, aim_hold_key, aim_captured) = {
            let g = shared.lock().unwrap();
            let aim = &g.profile.aim;
            (
                g.enabled,
                g.profile.toggle_key,
                aim.hold_key,
                aim.enabled && aim.capture_mouse && aim.anchor_set(),
            )
        };

        // 瞄准门控键(如鼠标右键=开镜)的按下/松开
        if aim_hold_key != 0 && ev.code == aim_hold_key {
            aim.gate_down = ev.pressed;
        }

        // Ctrl+Alt:临时把鼠标交还给系统,再按一次收回。
        // 只在瞄准确实会捕获鼠标时才有意义,避免误触改状态。
        if is_ctrl(ev.code) {
            ctrl_down = ev.pressed;
        } else if is_alt(ev.code) {
            alt_down = ev.pressed;
        }
        if aim_captured && ctrl_alt_chord(ev.code, ev.pressed, ctrl_down, alt_down) {
            aim.released = !aim.released;
        }

        // 物理按键镜像:所有按键事件先落到这里。它是"某个物理键此刻是否真的按着"
        // 的唯一可信依据 —— 归属切换(临时摇杆启用/停用、配置改动)时的对账全靠它。
        held.set(ev.code, ev.pressed);

        // 总开关键:任何时候都生效
        if ev.code == toggle_key && ev.pressed {
            let mut g = shared.lock().unwrap();
            g.enabled = !g.enabled;
            let now_enabled = g.enabled;
            if !now_enabled {
                // 关闭时释放所有按下中的触点与系统键,并停用全部临时轮盘
                // (与顶栏按钮共用同一份实现,两条路径行为必须完全一致)
                let ctl = g.control.take();
                let screen = ctl
                    .as_ref()
                    .map(|c| (c.screen_w, c.screen_h))
                    .unwrap_or((0, 0));
                release_all(
                    ctl.as_ref(),
                    &mut g,
                    screen,
                    EngineState {
                        fingers: &mut fingers,
                        wheels: &mut wheels,
                        active_android_keys: &mut active_android_keys,
                        aim: &mut aim,
                    },
                );
                g.control = ctl;
            } else {
                // 每次重新开打都恢复捕获,避免上一局的"交还鼠标"状态带过来
                aim.released = false;
                // 重新开打时按**物理按键状态**对账一遍:用户此刻按住不放的键
                // 立刻生效,而不是要松手再按一次(关映射时全部触点都被抬起了)
                if let Some(ctl) = g.control.as_ref() {
                    if ctl.is_connected() {
                        let m = g.profile.mapper((ctl.screen_w, ctl.screen_h));
                        sync_structures(
                            ctl,
                            &m,
                            &g.profile,
                            &mut wheels,
                            &mut wheel_sig,
                            &mut binds_sig,
                            &mut fingers,
                            &mut active_android_keys,
                            &held,
                        );
                        let extra = usize::from(aim.down);
                        reconcile_binds(
                            ctl,
                            &m,
                            &g.profile,
                            &wheels,
                            extra,
                            &held,
                            &mut fingers,
                            &mut active_android_keys,
                            &mut live,
                        );
                        reconcile_wheels(
                            ctl,
                            &m,
                            &g.profile,
                            &mut wheels,
                            &held,
                            fingers.count + usize::from(aim.down),
                            &mut live,
                        );
                    }
                }
            }
            // 按钮若正好也请求了收尾,视作已一并处理,避免下一轮再收一次
            if g.toolbar_release {
                g.toolbar_release = false;
                toolbar_release_prev = false;
            }
            continue;
        }

        if !enabled {
            continue;
        }

        let g = shared.lock().unwrap();
        let Some(ctl) = g.control.as_ref() else {
            continue;
        };
        if !ctl.is_connected() {
            continue;
        }
        let profile = &g.profile;
        // 坐标换算器:配置里的相对坐标 -> 当前屏幕像素(唯一换算入口)
        let m = profile.mapper((ctl.screen_w, ctl.screen_h));

        // ---- 与配置对齐 ----
        // 用户随时可能在开打中增删键位/摇杆。结构一变,索引就整体位移 ——
        // 先把在按的触点全部抬起来,再按新配置重建,绝不会漏下一个触点;
        // 重建后立刻对账一次,于是对玩家来说"改配置"是无感的。
        let restructured = sync_structures(
            ctl,
            &m,
            profile,
            &mut wheels,
            &mut wheel_sig,
            &mut binds_sig,
            &mut fingers,
            &mut active_android_keys,
            &held,
        );
        if restructured {
            let extra = usize::from(aim.down);
            reconcile_binds(
                ctl,
                &m,
                profile,
                &wheels,
                extra,
                &held,
                &mut fingers,
                &mut active_android_keys,
                &mut live,
            );
            reconcile_wheels(
                ctl,
                &m,
                profile,
                &mut wheels,
                &held,
                fingers.count + usize::from(aim.down),
                &mut live,
            );
        }
        live.pointers = fingers.count + wheel_pointers(&wheels) + usize::from(aim.down);

        // ---- 临时轮盘启用键 ----
        let mut consumed = false;
        for (j, w) in profile.wheels.iter().enumerate() {
            let Some(t) = &w.temp else { continue };
            if ev.code != t.key {
                continue;
            }
            // 目标状态:长按模式 = 按住期间生效;切换模式 = 只在按下那一刻翻转
            let want_active = match t.mode {
                TempMode::Hold => ev.pressed,
                TempMode::Toggle => {
                    if ev.pressed {
                        !wheels[j].active
                    } else {
                        wheels[j].active
                    }
                }
            };
            if want_active != wheels[j].active {
                if want_active {
                    wheels[j].active = true;
                    // 启用瞬间,释放与方向键冲突的普通绑定(避免触点卡死),
                    // 再按物理状态对账:已经按着的方向键立刻推动摇杆(不必松手重按),
                    // 该让位的普通绑定也一定被抬起来
                    release_conflicting_binds(
                        ctl,
                        &m,
                        profile,
                        &mut fingers,
                        &mut active_android_keys,
                        w,
                    );
                } else {
                    deactivate_wheel(ctl, &m, j, w, &mut wheels[j]);
                }
                // 归属刚改变 -> 两个方向都立刻对账一遍。
                // 停用这一边正是"松开启用键后,原来长按着的键必须继续奏效"的关键:
                // 那个键还按在手上,普通绑定必须马上补上它的触点。
                let extra = usize::from(aim.down);
                reconcile_binds(
                    ctl,
                    &m,
                    profile,
                    &wheels,
                    extra,
                    &held,
                    &mut fingers,
                    &mut active_android_keys,
                    &mut live,
                );
                reconcile_wheels(
                    ctl,
                    &m,
                    profile,
                    &mut wheels,
                    &held,
                    fingers.count + usize::from(aim.down),
                    &mut live,
                );
            }
            consumed = true;
        }
        if consumed {
            continue;
        }

        // ---- 轮盘方向键(仅生效中的轮盘:永久 或 已启用的临时) ----
        // 方向状态 = "该轮盘此刻是否生效 × 对应物理键是否按着"(见 reconcile_wheels),
        // 所以这里只判断"这个事件是不是某个生效轮盘的方向键",剩下的交给对账。
        // 多个轮盘共用同一个方向键时,它们会一起响应(与旧行为一致)。
        let handled = profile.wheels.iter().enumerate().any(|(j, w)| {
            let engaged = w.temp.is_none() || wheels[j].active;
            engaged
                && (w.up == ev.code || w.down == ev.code || w.left == ev.code || w.right == ev.code)
        });
        if handled {
            reconcile_wheels(
                ctl,
                &m,
                profile,
                &mut wheels,
                &held,
                fingers.count + usize::from(aim.down),
                &mut live,
            );
            continue;
        }

        // ---- 普通绑定(每键独立按/抬状态,最多 MAX_CONCURRENT_KEYS 并发) ----
        // 与轮盘/瞄准共用同一个设备端触点池:先把它们的占用算出来,
        // 挤不进去的按下在这里被明确拒绝并计入诊断 —— 而不是发出去被服务端
        // 悄悄丢掉(那正是"按下没反应、原因不明"的来源)
        let reserved = wheel_pointers(&wheels) + usize::from(aim.down);
        for (idx, bind) in profile.binds.iter().enumerate() {
            if bind.key != ev.code {
                continue;
            }
            let pid = bind_pid(idx);
            match &bind.action {
                Action::Tap {
                    x,
                    y,
                    duration_ms,
                    ..
                } => {
                    let (px, py) = m.point(*x, *y);
                    if *duration_ms == 0 {
                        // 按住切换:按下不松手,直到再次按下同一键才抬起
                        if ev.pressed {
                            if fingers.is_down(idx) {
                                ctl.touch_up(pid, px, py);
                                fingers.release(idx);
                            } else if fingers.try_down(idx, reserved) {
                                ctl.touch_down(pid, px, py);
                            } else {
                                refuse(&mut live, bind.key);
                            }
                        }
                    } else if ev.pressed {
                        // 点按:按下注入 DOWN,持续 duration_ms(默认 40ms)后定时抬起。
                        // 若上一击尚未自动抬起又再次按下(极快连点/组合技排序),
                        // 先立即结束旧触点再开新一轮,保证每次点按都完整触发、不丢键。
                        if fingers.is_down(idx) {
                            cancel_up_for(pid, &mut scheduled);
                            ctl.touch_up(pid, px, py);
                            fingers.release(idx);
                        }
                        if fingers.try_down(idx, reserved) {
                            ctl.touch_down(pid, px, py);
                            scheduled.push((
                                Instant::now()
                                    + Duration::from_millis((*duration_ms as u64).max(5)),
                                SchedAct::Up { pid, x: px, y: py },
                            ));
                        } else {
                            refuse(&mut live, bind.key);
                        }
                    }
                    // 松开不在此处理,统一由定时抬起收尾
                }
                Action::Hold { x, y, .. } => {
                    let (px, py) = m.point(*x, *y);
                    if ev.pressed {
                        if fingers.try_down(idx, reserved) {
                            ctl.touch_down(pid, px, py);
                        } else {
                            // 已经按着(钩子自动重复)不算"被拒",只有挤不进触点池才算
                            if !fingers.is_down(idx) {
                                refuse(&mut live, bind.key);
                            }
                        }
                    } else if fingers.release(idx) {
                        ctl.touch_up(pid, px, py);
                    }
                }
                Action::Swipe(s) => {
                    // 按下触发滑动;中途松手不打断,滑到终点由定时抬起清理按下状态
                    if ev.pressed {
                        if fingers.try_down(idx, reserved) {
                            // 相对坐标 -> 像素后再生成轨迹采样点
                            let start_px = m.point(s.start.0, s.start.1);
                            let end_px = m.point(s.end.0, s.end.1);
                            let points = swipe_points(s.path, start_px, end_px, 64);
                            if points.len() >= 2 {
                                let (x0, y0) = points[0];
                                ctl.touch_down(pid, x0, y0);
                                let n = points.len();
                                let start = Instant::now();
                                let steps = (n - 1) as u64;
                                for k in 1..n {
                                    let t = k as f32 / steps as f32;
                                    // 曲线把时间映射为沿路径的进度,再做弧长插值
                                    let prog = easing_apply(s.easing, t);
                                    let (x, y) = point_at_progress(&points, prog);
                                    let time = start
                                        + Duration::from_millis(
                                            (s.duration_ms as u64) * k as u64 / steps,
                                        );
                                    scheduled.push((time, SchedAct::Move { pid, x, y }));
                                }
                                let (xe, ye) = points[n - 1];
                                scheduled.push((
                                    start
                                        + Duration::from_millis(s.duration_ms as u64 + 20),
                                    SchedAct::Up { pid, x: xe, y: ye },
                                ));
                            }
                        } else {
                            refuse(&mut live, bind.key);
                        }
                    }
                }
                Action::AndroidKey { keycode } => {
                    let kc = *keycode as u16;
                    if ev.pressed {
                        // 同键未抬起(钩子自动重复/抖动)时不重复按下
                        if active_android_keys.insert(kc) {
                            ctl.key(true, *keycode);
                        }
                    } else if active_android_keys.remove(&kc) {
                        ctl.key(false, *keycode);
                    }
                }
            }
        }
        live.pointers = fingers.count + reserved;
    }
}

fn bind_pid(idx: usize) -> u64 {
    1000 + idx as u64
}

/// 取消指定 pid 尚未执行的定时抬起(点按连发抢占旧点击用),
/// 避免旧排程把新一撃提前抬起。
fn cancel_up_for(pid: u64, scheduled: &mut Vec<(Instant, SchedAct)>) {
    let target = pid;
    scheduled.retain(|(_, act)| !matches!(act, SchedAct::Up { pid: p, .. } if *p == target));
}

fn wheel_pid(idx: usize) -> u64 {
    2000 + idx as u64
}

/// 绑定落点(相对坐标 -> 当前像素)
fn bind_point(profile: &Profile, m: &Mapper, idx: usize) -> (i32, i32) {
    match profile.binds.get(idx).map(|b| &b.action) {
        Some(Action::Hold { x, y, .. }) | Some(Action::Tap { x, y, .. }) => m.point(*x, *y),
        Some(Action::Swipe(s)) => m.point(s.start.0, s.start.1),
        _ => (0, 0),
    }
}

/// 在折线上按弧长进度(0..1)插值取点,使曲线缓动沿路径均匀分布。
///
/// 实现要点:先用一遍 O(n) 求和得到总弧长,再走一遍找到目标所在线段。
/// 早期版本每次调用都现场构建累积长度表(一次堆分配),而滑动一次会安排几十个
/// Move 动作、且会被 `swap_remove` 打乱顺序 —— 于是每步都要重建一张长度表,
/// 属于 O(n²) 的无谓开销。
fn point_at_progress(points: &[(i32, i32)], progress: f32) -> (i32, i32) {
    if points.is_empty() {
        return (0, 0);
    }
    if points.len() == 1 {
        return points[0];
    }
    let seg_len = |i: usize| -> f32 {
        let (x0, y0) = points[i];
        let (x1, y1) = points[i + 1];
        ((x1 - x0) as f32).hypot((y1 - y0) as f32)
    };
    let total: f32 = (0..points.len() - 1).map(seg_len).sum();
    if total <= 1e-6 {
        return points[0];
    }
    let target = progress.clamp(0.0, 1.0) * total;
    let mut acc = 0.0f32;
    for i in 0..points.len() - 1 {
        let seg = seg_len(i);
        if target <= acc + seg || i == points.len() - 2 {
            let frac = if seg <= 1e-6 {
                0.0
            } else {
                ((target - acc) / seg).clamp(0.0, 1.0)
            };
            let (x0, y0) = points[i];
            let (x1, y1) = points[i + 1];
            let x = x0 as f32 + (x1 - x0) as f32 * frac;
            let y = y0 as f32 + (y1 - y0) as f32 * frac;
            return (x.round() as i32, y.round() as i32);
        }
        acc += seg;
    }
    points[points.len() - 1]
}

/// 停用一个轮盘:释放触点、清方向状态(临时轮盘专用,但通用无害)
fn deactivate_wheel(ctl: &ControlClient, m: &Mapper, j: usize, w: &Wheel, st: &mut WheelState) {
    st.active = false;
    st.pressed = [false; 4];
    if st.down {
        let pid = wheel_pid(j);
        let (cx, cy) = m.point(w.cx, w.cy);
        ctl.touch_move(pid, cx, cy);
        ctl.touch_up(pid, cx, cy);
        st.down = false;
    }
}

/// 临时轮盘启用的瞬间,释放所有与该轮盘方向键冲突的普通绑定
/// (仍按住的点按/长按/滑动触点与系统键),避免方向输入被抢占或触点卡死。
fn release_conflicting_binds(
    ctl: &ControlClient,
    m: &Mapper,
    profile: &Profile,
    fingers: &mut Fingers,
    active_android_keys: &mut HashSet<u16>,
    w: &Wheel,
) {
    let dirs = [w.up, w.down, w.left, w.right];
    for (i, bind) in profile.binds.iter().enumerate() {
        if !dirs.contains(&bind.key) {
            continue;
        }
        match &bind.action {
            Action::AndroidKey { keycode } => {
                let kc = *keycode as u16;
                if active_android_keys.remove(&kc) {
                    ctl.key(false, *keycode);
                }
            }
            _ => {
                if fingers.release(i) {
                    let (x, y) = bind_point(profile, m, i);
                    ctl.touch_up(bind_pid(i), x, y);
                }
            }
        }
    }
}

/// 按当前方向状态更新轮盘触点。返回是否因为设备端触点池已满而**放弃**了这次按下
/// (供调用方计入诊断 —— 这种情况以前会直接把消息发出去、被服务端悄悄丢掉)。
///
/// `reserved` 是此刻被普通绑定/瞄准/其它轮盘占用的触点数:大家抢的是同一个
/// 设备端指针池(`PointersState.MAX_POINTERS`),所以按下之前必须先问一句挤不挤得下。
fn update_wheel(
    ctl: &ControlClient,
    m: &Mapper,
    j: usize,
    w: &Wheel,
    st: &mut WheelState,
    reserved: usize,
) -> bool {
    let pid = wheel_pid(j);
    let (wx, wy) = m.point(w.cx, w.cy);
    // 触点推出距离 = 半径 × 影响范围(scope)。
    // 半径仍是界面上那个圆环的大小,scope 单独放大/缩小"手指实际被推多远",
    // 于是可以让视觉圈与游戏里真实摇杆的判定圈解耦。默认 scope=1.0,与旧版一致。
    let wr = w.push_px(m);
    let dx = st.pressed[3] as i32 - st.pressed[2] as i32; // right - left
    let dy = st.pressed[1] as i32 - st.pressed[0] as i32; // down - up

    if dx == 0 && dy == 0 {
        if st.down {
            // 回中后抬起
            ctl.touch_move(pid, wx, wy);
            ctl.touch_up(pid, wx, wy);
            st.down = false;
        }
        return false;
    }

    // 斜向归一化
    let (fx, fy) = if dx != 0 && dy != 0 {
        let inv = std::f64::consts::FRAC_1_SQRT_2;
        (dx as f64 * inv, dy as f64 * inv)
    } else {
        (dx as f64, dy as f64)
    };
    let tx = wx + (fx * wr as f64).round() as i32;
    let ty = wy + (fy * wr as f64).round() as i32;

    if !st.down {
        if reserved >= DEVICE_MAX_POINTERS {
            // 触点池满了:不注入(注入了也会被服务端丢掉),下次方向变化时再试
            return true;
        }
        ctl.touch_down(pid, wx, wy);
        ctl.touch_move(pid, tx, ty);
        st.down = true;
    } else if st.last != (tx, ty) {
        ctl.touch_move(pid, tx, ty);
    }
    st.last = (tx, ty);
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 相对坐标的锚点(0.1 => 1000x1000 屏幕上的 (100,100))
    fn aim_at(x: f32, y: f32) -> Aim {
        Aim {
            enabled: true,
            anchor_x: x,
            anchor_y: y,
            ..Aim::default()
        }
    }

    /// 测试用换算器:相对坐标 + 1000x1000 屏幕
    fn mapper() -> Mapper {
        Mapper::new(crate::keymap::CoordUnit::Rel, (1000, 1000))
    }

    /// 落点必须始终落在屏幕内,并正确报告"触到边界"
    #[test]
    fn aim_target_clamps_to_screen() {
        let m = mapper();
        let aim = aim_at(0.1, 0.1);

        let st = AimState::default();
        assert_eq!(aim_target(&m, &aim, &st), (100, 100, false));

        let st = AimState {
            ox: -50.0,
            oy: 30.0,
            ..Default::default()
        };
        assert_eq!(aim_target(&m, &aim, &st), (50, 130, false));

        // 正向超界:钳到右下角
        let st = AimState {
            ox: 99999.0,
            oy: 99999.0,
            ..Default::default()
        };
        assert_eq!(aim_target(&m, &aim, &st), (999, 999, true));

        // 负向超界:钳到左上角
        let st = AimState {
            ox: -99999.0,
            oy: -99999.0,
            ..Default::default()
        };
        assert_eq!(aim_target(&m, &aim, &st), (0, 0, true));
    }

    /// 锚点超出当前坐标空间时(例如竖屏取点后转成横屏),
    /// 注入的触摸会落到屏幕外被系统丢弃 —— 必须先钳进屏幕内
    #[test]
    fn aim_target_clamps_anchor_into_screen() {
        // 竖屏(wm size 1280x2772)下自动放置的锚点,横屏实际坐标空间是 2772x1280
        let m = Mapper::new(crate::keymap::CoordUnit::Rel, (2772, 1280));
        let aim = aim_at(960.0 / 2772.0, 1386.0 / 2772.0);
        let st = AimState {
            down: true,
            ox: 40.0,
            oy: 40.0,
            ..Default::default()
        };
        let (x, y, _) = aim_target(&m, &aim, &st);
        assert!((0..2772).contains(&x));
        assert!((0..1280).contains(&y));
    }

    /// 未设锚点不生效;设了门控键则必须先按住
    #[test]
    fn aim_active_needs_anchor_and_gate() {
        let mut st = AimState::default();

        let mut aim = aim_at(0.1, 0.1);
        assert!(aim_active(&aim, &st));

        // 锚点未设置
        aim.anchor_x = 0.0;
        aim.anchor_y = 0.0;
        assert!(!aim_active(&aim, &st));

        // 门控键:按住才生效
        let mut aim = aim_at(0.1, 0.1);
        aim.hold_key = 273; // BTN_RIGHT
        assert!(!aim_active(&aim, &st));
        st.gate_down = true;
        assert!(aim_active(&aim, &st));
    }

    /// Ctrl+Alt:一次组合只切换一次,松开后再按才会再次触发
    #[test]
    fn ctrl_alt_chord_fires_once_per_press() {
        // 只按 Ctrl:不触发
        assert!(!ctrl_alt_chord(KEY_LEFTCTRL, true, true, false));
        // 在按住 Ctrl 的基础上按下 Alt:触发
        assert!(ctrl_alt_chord(KEY_LEFTALT, true, true, true));
        // Alt 保持按住时的重复按下事件:仍然算一次触发(由调用方取反,不会连发)
        assert!(ctrl_alt_chord(KEY_LEFTALT, true, true, true));
        // 松开 Alt:不触发
        assert!(!ctrl_alt_chord(KEY_LEFTALT, false, true, false));

        // 同时按住 Ctrl+Alt 时按其它键:不触发
        assert!(!ctrl_alt_chord(30, true, true, true)); // KEY_A
        // 右键的 Ctrl / Alt 同样识别
        assert!(ctrl_alt_chord(KEY_RIGHTCTRL, true, true, true));
        assert!(ctrl_alt_chord(KEY_RIGHTALT, true, true, true));
    }

    /// 交还鼠标后瞄准不生效
    #[test]
    fn released_blocks_aim() {
        let aim = aim_at(0.1, 0.1);
        let mut st = AimState::default();
        assert!(aim_active(&aim, &st));
        st.released = true;
        assert!(!aim_active(&aim, &st));
    }

    /// 折线取点(滑动轨迹与缓动的基础):
    /// 端点必须精确落在起/终点上,按弧长而非按段数插值,越界进度被夹住。
    /// 这条同时锁住"去掉累积长度表"这次重构的等价性。
    #[test]
    fn point_at_progress_is_arclength_based() {
        let pts = [(0, 0), (100, 0), (100, 300)];
        // 端点
        assert_eq!(point_at_progress(&pts, 0.0), (0, 0));
        assert_eq!(point_at_progress(&pts, 1.0), (100, 300));
        // 总长 400;进度 0.25 => 弧长 100 => 正好是折点
        assert_eq!(point_at_progress(&pts, 0.25), (100, 0));
        // 进度 0.5 => 弧长 200 => 第二段走了一半
        assert_eq!(point_at_progress(&pts, 0.5), (100, 100));
        // 越界与退化输入
        assert_eq!(point_at_progress(&pts, -1.0), (0, 0));
        assert_eq!(point_at_progress(&pts, 5.0), (100, 300));
        assert_eq!(point_at_progress(&[], 0.5), (0, 0));
        assert_eq!(point_at_progress(&[(7, 7)], 0.5), (7, 7));
        // 所有点重合:总长为 0,必须回退到起点而不是除零
        assert_eq!(point_at_progress(&[(5, 5), (5, 5)], 0.7), (5, 5));
    }

    /// 按键并发上限:同一绑定重复按下只算一次,总数不超过 MAX_CONCURRENT_KEYS,
    /// 且抬起严格与按下配对(绝不能误抬别的键)
    #[test]
    fn fingers_enforces_concurrency_and_pairing() {
        let mut f = Fingers::default();
        f.align(4);
        assert!(f.try_down(0, 0), "首次按下应成功");
        assert!(
            !f.try_down(0, 0),
            "未抬起的重复按下必须被忽略(防钩子自动重复)"
        );
        assert!(f.release(0), "抬起应有记录");
        assert!(!f.release(0), "无按下记录的抬起不得误抬其它键");

        // 占满并发额度后新按下被拒绝;释放一个后又能按下
        f.align(MAX_CONCURRENT_KEYS + 2);
        for i in 0..MAX_CONCURRENT_KEYS {
            assert!(f.try_down(i, 0), "第 {i} 个键应在额度内");
        }
        assert!(
            !f.try_down(MAX_CONCURRENT_KEYS, 0),
            "超出额度必须拒绝,否则设备端指针池会被撑爆"
        );
        assert!(f.release(3));
        assert!(f.try_down(MAX_CONCURRENT_KEYS, 0), "腾出额度后应可再按下");
    }

    /// 设备端触点池是**共享**的:普通绑定、轮盘、瞄准抢同一个池子(上限 10)。
    /// 轮盘/瞄准已经占住的名额必须从普通绑定的额度里扣掉 —— 否则我们发出的
    /// DOWN 会被服务端悄悄丢掉,表现就是"按下没反应、原因不明"。
    #[test]
    fn fingers_respects_shared_device_pointer_pool() {
        let mut f = Fingers::default();
        f.align(MAX_CONCURRENT_KEYS);
        // 摇杆+瞄准先占掉 4 个:普通绑定最多再占 6 个
        let others = 4;
        for i in 0..(DEVICE_MAX_POINTERS - others) {
            assert!(f.try_down(i, others), "第 {i} 个键仍在设备额度内");
        }
        assert_eq!(f.count, DEVICE_MAX_POINTERS - others);
        assert!(
            !f.try_down(DEVICE_MAX_POINTERS - others, others),
            "再按下去就会超过设备端的 10 个触点,必须在这里挡住"
        );
        // 松开一个轮盘触点(others 减少)后又能按下
        assert!(f.try_down(DEVICE_MAX_POINTERS - others, others - 1));
    }

    /// 变长绑定列表:align 只补长,补长后旧状态保留、新槽位为未按下
    #[test]
    fn fingers_align_grows_only() {
        let mut f = Fingers::default();
        f.align(2);
        assert!(f.try_down(1, 0));
        f.align(5);
        assert!(f.is_down(1), "补长不得丢失已按下状态");
        assert!(!f.is_down(4), "新槽位应为未按下");
        f.free_all();
        assert!(!f.is_down(1) && f.count == 0, "free_all 必须清空计数");
    }

    /// 结构指纹:键位表/轮盘表"数量或键码"一变就必须变,只有坐标之类
    /// 不影响归属的字段变化时保持不变(否则一边打一边调参会被反复抬手)。
    #[test]
    fn signatures_track_structure_not_geometry() {
        use crate::keymap::{KeyBind, TempWheel};

        let mut p = Profile::default();
        p.binds = vec![
            KeyBind {
                key: 37,
                action: Action::Hold {
                    x: 0.5,
                    y: 0.5,
                    radius: 0.03,
                },
            },
            KeyBind {
                key: 36,
                action: Action::Hold {
                    x: 0.5,
                    y: 0.5,
                    radius: 0.03,
                },
            },
        ];
        let b0 = binds_signature(&p.binds);
        // 改坐标/半径:不是结构变化,不该触发抬手重建
        p.binds[0].action = Action::Hold {
            x: 0.1,
            y: 0.9,
            radius: 0.09,
        };
        assert_eq!(binds_signature(&p.binds), b0, "只改坐标不算结构变化");
        // 改键码 / 增删键位:索引会整体位移,必须识别出来
        p.binds[0].key = 38;
        assert_ne!(binds_signature(&p.binds), b0, "换键码必须触发重建");
        let b1 = binds_signature(&p.binds);
        p.binds.pop();
        assert_ne!(binds_signature(&p.binds), b1, "增删键位必须触发重建");

        let w0 = wheel_signature(&p.wheels);
        p.wheels[0].cx = 0.9;
        p.wheels[0].radius = 0.02;
        assert_eq!(wheel_signature(&p.wheels), w0, "只改圆心/半径不算结构变化");
        p.wheels[0].up = 30;
        assert_ne!(wheel_signature(&p.wheels), w0, "换方向键必须触发重建");
        let w1 = wheel_signature(&p.wheels);
        p.wheels[0].temp = Some(TempWheel {
            key: 18,
            mode: TempMode::Hold,
        });
        assert_ne!(wheel_signature(&p.wheels), w1, "设置启用键必须触发重建");
        let w2 = wheel_signature(&p.wheels);
        p.wheels.push(crate::keymap::Wheel {
            up: 1,
            down: 2,
            left: 3,
            right: 4,
            cx: 0.5,
            cy: 0.5,
            radius: 0.05,
            scope: 1.0,
            temp: None,
        });
        assert_ne!(wheel_signature(&p.wheels), w2, "增删轮盘必须触发重建");
    }

    /// 归属判定:生效中的轮盘方向键归摇杆;临时轮盘的启用键**永远**归它自己;
    /// 未启用的临时轮盘方向键不归摇杆(要让位给普通绑定)。
    #[test]
    fn wheel_ownership_follows_engagement() {        use crate::keymap::TempWheel;
        let mut p = Profile::default();
        p.binds = Vec::new();
        p.wheels = vec![
            crate::keymap::Wheel {
                up: 17,
                down: 31,
                left: 30,
                right: 32,
                cx: 0.3,
                cy: 0.4,
                radius: 0.05,
                scope: 1.0,
                temp: None,
            },
            crate::keymap::Wheel {
                up: 23,
                down: 37,
                left: 36,
                right: 22,
                cx: 0.7,
                cy: 0.4,
                radius: 0.05,
                scope: 1.0,
                temp: Some(TempWheel {
                    key: 18,
                    mode: TempMode::Hold,
                }),
            },
        ];
        let mut st = vec![WheelState::default(); 2];

        // 永久轮盘:方向键一直归它
        assert!(key_owned_by_wheel(&p, &st, 17));
        // 临时轮盘未启用:方向键与启用键的归属不同
        assert!(!key_owned_by_wheel(&p, &st, 37), "未启用的临时摇杆不占方向键");
        assert!(key_owned_by_wheel(&p, &st, 18), "启用键永远归临时摇杆");
        // 启用后方向键归它
        st[1].active = true;
        assert!(key_owned_by_wheel(&p, &st, 37));
        assert!(key_owned_by_wheel(&p, &st, 23));
        // 与摇杆无关的键始终不归摇杆
        assert!(!key_owned_by_wheel(&p, &st, 24));
    }

    /// 用户实测场景(本轮修复的核心):"我按着 K 键正在连招,忽然按下临时摇杆的
    /// 启用键,又松开启用键 —— 这时 K 还按在手上,却必须等松开手再按一次才触发,
    /// 战场瞬息万变,这很被动。"
    ///
    /// 判定层必须表达出:松开启用键的**那一瞬间**,仍被按住的 K 的普通绑定
    /// 立刻重新生效(它由物理按键镜像 [`Held`] 推导,不依赖任何键盘事件)。
    #[test]
    fn held_key_resumes_its_normal_bind_the_moment_the_wheel_deactivates() {
        use crate::keymap::{KeyBind, TempWheel};

        // 配置:K(37) 是长按绑定;临时摇杆的方向键里也有 K,启用键是 E(18)
        let mut p = Profile::default();
        p.binds = vec![KeyBind {
            key: 37,
            action: Action::Hold {
                x: 0.9,
                y: 0.8,
                radius: 0.03,
            },
        }];
        p.wheels = vec![crate::keymap::Wheel {
            up: 23,
            down: 37,
            left: 36,
            right: 22,
            cx: 0.8,
            cy: 0.7,
            radius: 0.05,
            scope: 1.0,
            temp: Some(TempWheel {
                key: 18,
                mode: TempMode::Hold,
            }),
        }];
        let mut st = vec![WheelState::default(); 1];
        let held = {
            let mut h = Held::default();
            h.set(37, true); // 用户一直按着 K
            h
        };
        // "普通绑定此刻该不该按下"的判定(与 reconcile_binds 内完全同构)
        let want = |held: &Held, st: &[WheelState]| {
            held.has(37) && !key_owned_by_wheel(&p, st, 37)
        };

        // 摇杆未启用:K 归普通绑定,按着 -> 该有触点
        assert!(want(&held, &st));

        // 按下启用键:K 被摇杆接管 -> 普通绑定必须让位(同一时刻只能有一处生效)
        st[0].active = true;
        assert!(!want(&held, &st), "启用期间 K 归摇杆,普通绑定必须让位");

        // 松开启用键:摇杆停用,而 K 仍在手上 -> 普通绑定必须**立刻**重新生效
        st[0].active = false;
        assert!(
            want(&held, &st),
            "松开启用键的瞬间,仍按着的 K 必须马上回到普通绑定(不必松手重按)"
        );

        // 直到 K 真正松开,才该抬起
        let released = Held::default();
        assert!(!want(&released, &st));
    }
}
