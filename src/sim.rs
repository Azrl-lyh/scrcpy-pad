//! 无头端到端:真实引擎线程 → 真实 `ControlClient`(序列化 + TCP) → **假设备**。
//!
//! # 为什么是这一层(用户 2026-10-11)
//!
//! 上一批"只有真机能暴露"的 bug 之所以难查,是因为断言的对象是**源码里的
//! `ControlCmd` 队列**(`ControlClient::for_test`),而真机上的东西要经过
//! 序列化 → 写线程攒批 → TCP → 服务端语义。这中间的任何一环出问题,单测都
//! 看不见。这里把最后两环补上:
//!
//! * [`FakeDevice`]:一个真的 TCP 服务端,按 scrcpy 的握手发 dummy 字节,
//!   收字节流、**按协议解码回语义事件**;
//! * [`DeviceSim`]:按 scrcpy-server 的真实语义维护指针表与边界规则
//!   (出屏整条丢弃 / 十指池满拒绝 / 未知 pid 的 move·up 无效),
//!   于是断言的对象是"**设备端实际接受了什么**",而不是"我们以为发了什么"。
//!
//! # 三条不变量(每个场景都要断言)
//!
//! 1. **触点不泄漏**:结束时设备端没有残留手指([`assert_no_leak`]);
//! 2. **触点预算**:全程 ≤ 10 指([`assert_pointer_budget`]);
//! 3. **坐标合法 + 无被丢弃**:没有出屏被丢的触摸([`assert_all_in_bounds`])。
//!
//! 位移守恒按 pid 单独算([`path_stats`]):换手会让新指从锚点起步,所以统计
//! 规则是"DOWN 重置基准、MOVE 累加差值"——累加出来的正是**设备端真正收到的
//! 位移总量**,与鼠标位移 × 灵敏度对照(见 [`crate::sim::tests`] 的场景)。
//!
//! # 真机怎么用
//!
//! 真机不需要这一层就能跑:同一份轨迹([`crate::trace`])由
//! [`replay`] 灌回来,于是"真机复现一次"= 一份可回归的用例。

#![allow(dead_code)]
#![allow(
    clippy::field_reassign_with_default,
    clippy::too_many_arguments,
    clippy::type_complexity
)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::capture::CaptureEvent;
use crate::control::{ACTION_DOWN, ACTION_MOVE, ACTION_UP, ControlClient};
use crate::engine::{self, Shared, SharedState};
use crate::keymap::{KeySet, Profile};
use crate::trace;

/// 设备端同时能认的触点数(scrcpy-server 的 `PointersState` 上限,与
/// `engine::DEVICE_MAX_POINTERS` 同一个数 —— 这里独立写一份,是为了让假设备
/// **不依赖被测实现的常量**:常量被改错时它必须能报出来)。
pub const DEVICE_MAX_POINTERS: usize = 10;

// 协议消息号(与 `control.rs` 的私有常量一一对应;写在这里是刻意的 ——
// 假设备若引用被测实现的常量,常量写错就一起错,失去校验意义)。
const TYPE_INJECT_KEYCODE: u8 = 0;
const TYPE_INJECT_TOUCH: u8 = 2;
const TYPE_UHID_CREATE: u8 = 12;
const TYPE_UHID_INPUT: u8 = 13;
const TYPE_UHID_DESTROY: u8 = 14;

// ============================ 协议解码 ============================

/// 从控制通道字节流里解出来的**语义**事件。
#[derive(Debug, Clone, PartialEq)]
pub enum DevEv {
    Touch {
        action: u8,
        pid: u64,
        x: u32,
        y: u32,
        w: u16,
        h: u16,
        pressure: u16,
    },
    Key {
        action: u8,
        keycode: u32,
    },
    UhidCreate {
        id: u16,
    },
    UhidInput {
        id: u16,
        len: usize,
    },
    UhidDestroy {
        id: u16,
    },
}

/// 增量解码器:TCP 不保证消息边界,所以按"类型 → 长度"逐条切。
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
    /// 遇到无法识别的类型号:长度未知、无法再同步,记下来供断言。
    corrupt: Option<u8>,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn corrupt(&self) -> Option<u8> {
        self.corrupt
    }

    pub fn push(&mut self, bytes: &[u8]) -> Vec<DevEv> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        loop {
            if self.corrupt.is_some() {
                break;
            }
            let Some(&kind) = self.buf.first() else { break };
            // 先算这条消息的总长;不够长就等下一批字节。
            let Some(total) = self.len_of(kind) else {
                break;
            };
            if self.buf.len() < total {
                break;
            }
            let msg: Vec<u8> = self.buf.drain(..total).collect();
            out.push(Self::parse(kind, &msg));
        }
        out
    }

    /// 这条消息的**总长度**;类型未知或头都不够时返回 `None`。
    fn len_of(&mut self, kind: u8) -> Option<usize> {
        match kind {
            TYPE_INJECT_KEYCODE => Some(14),
            TYPE_INJECT_TOUCH => Some(32),
            TYPE_UHID_DESTROY => Some(3),
            TYPE_UHID_INPUT => {
                if self.buf.len() < 5 {
                    return None;
                }
                let size = u16::from_be_bytes([self.buf[3], self.buf[4]]) as usize;
                Some(5 + size)
            }
            TYPE_UHID_CREATE => {
                if self.buf.len() < 8 {
                    return None;
                }
                let name_len = self.buf[7] as usize;
                if self.buf.len() < 8 + name_len + 2 {
                    return None;
                }
                let desc_len =
                    u16::from_be_bytes([self.buf[8 + name_len], self.buf[8 + name_len + 1]])
                        as usize;
                Some(8 + name_len + 2 + desc_len)
            }
            other => {
                self.corrupt = Some(other);
                None
            }
        }
    }

    fn parse(kind: u8, msg: &[u8]) -> DevEv {
        match kind {
            TYPE_INJECT_TOUCH => DevEv::Touch {
                action: msg[1],
                pid: u64::from_be_bytes(msg[2..10].try_into().unwrap()),
                x: u32::from_be_bytes(msg[10..14].try_into().unwrap()),
                y: u32::from_be_bytes(msg[14..18].try_into().unwrap()),
                w: u16::from_be_bytes(msg[18..20].try_into().unwrap()),
                h: u16::from_be_bytes(msg[20..22].try_into().unwrap()),
                pressure: u16::from_be_bytes(msg[22..24].try_into().unwrap()),
            },
            TYPE_INJECT_KEYCODE => DevEv::Key {
                action: msg[1],
                keycode: u32::from_be_bytes(msg[2..6].try_into().unwrap()),
            },
            TYPE_UHID_CREATE => DevEv::UhidCreate {
                id: u16::from_be_bytes(msg[1..3].try_into().unwrap()),
            },
            TYPE_UHID_INPUT => DevEv::UhidInput {
                id: u16::from_be_bytes(msg[1..3].try_into().unwrap()),
                len: u16::from_be_bytes(msg[3..5].try_into().unwrap()) as usize,
            },
            _ => DevEv::UhidDestroy {
                id: u16::from_be_bytes(msg[1..3].try_into().unwrap()),
            },
        }
    }
}

// ============================ 设备端模型 ============================

/// 假设备的状态机:按 scrcpy-server 的真实语义处理每条触摸消息。
///
/// 三条规则都来自服务端实现,而不是我们的猜测:
/// * **出屏整条丢弃** —— 服务端对越界坐标直接报错、不做钳制;
/// * **十指池满拒绝** —— `PointersState` 最多 10 个指针,满了就丢掉这次 DOWN;
/// * **未知 pid 的 MOVE/UP 无效** —— 本地以为按着、设备端其实没有(这正是
///   "看起来死了"的一类故障,必须让它可见)。
#[derive(Debug, Clone, Default)]
pub struct DeviceSim {
    pub w: u32,
    pub h: u32,
    pub evs: Vec<DevEv>,
    pub open: Vec<u64>,
    pub peak_pointers: usize,
    pub dropped_oob: u32,
    pub rejected_pool: u32,
    pub unknown_pid: u32,
}

impl DeviceSim {
    pub fn new(w: u32, h: u32) -> Self {
        Self {
            w,
            h,
            ..Default::default()
        }
    }

    pub fn apply(&mut self, ev: DevEv) {
        self.evs.push(ev.clone());
        let DevEv::Touch {
            action, pid, x, y, ..
        } = ev
        else {
            return;
        };
        if x >= self.w || y >= self.h {
            self.dropped_oob += 1;
            return;
        }
        match action {
            ACTION_DOWN => {
                if self.open.len() >= DEVICE_MAX_POINTERS {
                    self.rejected_pool += 1;
                } else if !self.open.contains(&pid) {
                    self.open.push(pid);
                    self.peak_pointers = self.peak_pointers.max(self.open.len());
                }
            }
            ACTION_MOVE => {
                if !self.open.contains(&pid) {
                    self.unknown_pid += 1;
                }
            }
            ACTION_UP => {
                if let Some(i) = self.open.iter().position(|p| *p == pid) {
                    self.open.remove(i);
                } else {
                    self.unknown_pid += 1;
                }
            }
            _ => {}
        }
    }

    /// 结束时仍按在设备上的手指(非空 = 泄漏)。
    pub fn leaks(&self) -> &[u64] {
        &self.open
    }

    pub fn touch_count(&self) -> usize {
        self.evs
            .iter()
            .filter(|e| matches!(e, DevEv::Touch { .. }))
            .count()
    }
}

// ============================ 假设备(TCP) ============================

/// 一个真的 control socket 服务端:发 dummy 字节、收字节、喂给 [`DeviceSim`]。
pub struct FakeDevice {
    port: u16,
    dev: Arc<Mutex<DeviceSim>>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl FakeDevice {
    pub fn start(w: u32, h: u32) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        listener.set_nonblocking(true)?;
        let dev = Arc::new(Mutex::new(DeviceSim::new(w, h)));
        let stop = Arc::new(AtomicBool::new(false));
        let join = {
            let dev = dev.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                // 1) 等客户端连上(轮询以便 stop 能被响应)
                let mut conn = None;
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((s, _)) => {
                            conn = Some(s);
                            break;
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(_) => return,
                    }
                }
                let Some(mut stream) = conn else { return };
                // **必须非阻塞**:客户端那条读线程会一直阻塞在 `read` 上(它的
                // 设计如此:靠服务端 EOF 收尾),于是这里的 `read` 在没有新消息时
                // 永远不会返回。阻塞读 + `Drop` 里的 `join` = 死锁(第一次跑就是
                // 这么挂住的):停止标志永远没机会被看见。改成轮询即可 ——
                // 最多 1ms 的延迟,对断言毫无影响。
                stream.set_nonblocking(true).ok();
                // 2) 握手:客户端 `connect()` 会阻塞等这一个字节
                if stream.write_all(&[0u8]).is_err() {
                    return;
                }
                let _ = stream.flush();
                // 3) 收 + 解码 + 应用
                let mut dec = Decoder::new();
                let mut buf = [0u8; 8192];
                while !stop.load(Ordering::Relaxed) {
                    match stream.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let evs = dec.push(&buf[..n]);
                            if !evs.is_empty() {
                                let mut d = dev.lock().unwrap_or_else(|e| e.into_inner());
                                for ev in evs {
                                    d.apply(ev);
                                }
                            }
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(1));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        Ok(Self {
            port,
            dev,
            stop,
            join: Some(join),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn snapshot(&self) -> DeviceSim {
        self.dev.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Drop for FakeDevice {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

// ============================ 无头端到端 ============================

/// 真实引擎 + 真实控制客户端 + 假设备。事件由测试线程按脚本喂进去。
pub struct Harness {
    pub shared: SharedState,
    pub screen: (u32, u32),
    tx: Option<Sender<CaptureEvent>>,
    /// 引擎会把"需要界面处理的按键"回传到这里(与真机同一条通路:
    /// `engine::run` 的 `gui_tx`)。
    ///
    /// 留着不收也不会 drop —— 发送端失败会让引擎多写一行日志,污染断言。
    /// 缝②接线测试([`tests::hotkey_action_is_wired_through_the_engine_loop`])
    /// 用它验证"引擎循环 → 热键动作"这一段真的接上了。
    gui_rx: Receiver<CaptureEvent>,
    engine: Option<JoinHandle<()>>,
    fake: Option<FakeDevice>,
}

impl Harness {
    /// 起一条完整链路。`profile` 就是"此刻生效的那套键位组合"。
    pub fn start(profile: Profile, screen: (u32, u32)) -> Self {
        // 场景也能进轨迹:设了 `SCRCPY_PAD_TRACE=<路径>` 就走与真机**同一份**
        // 落盘代码(`init_from_env` 幂等)—— 于是"调一个场景"和"打一局游戏"
        // 产出的是同一种文件,可以互相回放。
        trace::init_from_env();
        let (w, h) = screen;
        let fake = FakeDevice::start(w, h).expect("假设备起不来(端口占用?)");
        let client = ControlClient::connect(fake.port(), w, h)
            .expect("真实控制客户端连不上假设备(握手失败?)");
        let schemes = vec![profile.clone()];
        let shared: SharedState = Arc::new(Mutex::new(Shared {
            profile,
            enabled: true,
            control: Some(client),
            aim_live: Default::default(),
            live: Default::default(),
            notices: Vec::new(),
            space_recheck: false,
            space_changed_release: false,
            toolbar_release: false,
            recoil_gear_req: None,
            schemes,
            switch_keys: Vec::new(),
            fast_switch_enabled: false,
            active_scheme: 0,
        }));
        let (tx, rx) = channel::<CaptureEvent>();
        let (gui_tx, gui_rx) = channel::<CaptureEvent>();
        let engine = {
            let shared = shared.clone();
            let mouse_grab = Arc::new(AtomicBool::new(false));
            let cursor_hide = Arc::new(AtomicBool::new(false));
            thread::spawn(move || engine::run(shared, rx, gui_tx, mouse_grab, cursor_hide))
        };
        Self {
            shared,
            screen,
            tx: Some(tx),
            gui_rx,
            engine: Some(engine),
            fake: Some(fake),
        }
    }

    pub fn feed_button(&self, code: u16, pressed: bool) {
        #[cfg(test)]
        fed_probe::push(fed_probe::Fed::Button {
            code,
            down: pressed,
        });
        if let Some(tx) = &self.tx {
            let _ = tx.send(CaptureEvent::Button {
                code,
                pressed,
                at: Instant::now(),
            });
        }
    }

    pub fn feed_motion(&self, dx: f32, dy: f32) {
        #[cfg(test)]
        fed_probe::push(fed_probe::Fed::Motion { dx, dy });
        if let Some(tx) = &self.tx {
            let _ = tx.send(CaptureEvent::Motion {
                dx,
                dy,
                at: Instant::now(),
            });
        }
    }

    /// 按一下再松开(切换类功能键的完整周期)。
    pub fn tap(&self, code: u16) {
        self.feed_button(code, true);
        self.settle(20);
        self.feed_button(code, false);
        self.settle(20);
    }

    pub fn enable(&self, on: bool) {
        let mut g = engine::lock_shared(&self.shared);
        g.enabled = on;
        if !on {
            // **与界面同一条路**:顶栏[映射:开/关]按钮不只写 `enabled`,它同时置
            // `toolbar_release` 请求引擎做一次收尾(抬起所有仍按着的触点)——
            // 这是"关映射不能卡键"的实现方式(见 `Shared::toolbar_release` 的注释)。
            // 只写 `enabled` 的话触点会留在设备上:那不是产品 bug,而是测试用了
            // 一条产品里不存在的路径。第一版就是栽在这(假设备当场报出 pid 3000 卡键)。
            g.toolbar_release = true;
        }
    }

    pub fn update_profile(&self, f: impl FnOnce(&mut Profile)) {
        let mut g = engine::lock_shared(&self.shared);
        f(&mut g.profile);
    }

    /// 让引擎线程跑一会儿。**真实时间**(引擎内部用 `Instant::now()`,
    /// 不引入时钟抽象是为了不动被测代码);场景里各段加起来控制在几百毫秒。
    pub fn settle(&self, ms: u64) {
        thread::sleep(Duration::from_millis(ms));
    }

    pub fn device(&self) -> DeviceSim {
        self.fake
            .as_ref()
            .map(|f| f.snapshot())
            .unwrap_or_else(|| DeviceSim::new(self.screen.0, self.screen.1))
    }

    /// 停链路:先断开输入(引擎据此抬起所有触点,这正是"退出不卡键"的验收点),
    /// 再等引擎线程退出,最后读设备端终态。
    pub fn stop(mut self) -> DeviceSim {
        self.tx = None;
        if let Some(j) = self.engine.take() {
            let _ = j.join();
        }
        self.settle(120);
        let dev = self.device();
        self.fake = None;
        dev
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.tx = None;
        if let Some(j) = self.engine.take() {
            let _ = j.join();
        }
    }
}

// ============================ 不变量 ============================

/// 设备端收到的位移统计(按 pid 分别算,只看我们关心的那些 pid)。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PathStats {
    /// 各段增量之和(有符号)。换手时新指从锚点起步,所以这里累加的是
    /// "设备端真正收到的推进量"。
    pub net: (f32, f32),
    /// 路径长度(只累加长度)。
    pub path: f32,
    pub downs: usize,
    pub moves: usize,
    pub ups: usize,
}

pub fn path_stats(dev: &DeviceSim, want: impl Fn(u64) -> bool) -> PathStats {
    // **每个 pid 各记一个基准**。第一版只用了一个 `last`,结果把"足额送达"
    // 误判成"丢了 13%":重叠换手是 `DOWN(新指)` **先于** `UP(旧指)` 到达的
    // (`aim_overlap_handoff` 的重叠形态),那条 UP 会把新指刚建立的基准清掉,
    // 于是每次换手都丢掉段内第一个 MOVE —— 实测每段 32px × 五次换手 = 160px,
    // 1200 就这样变成了 1040。基准按 pid 分开之后这个误差不存在了。
    let mut last: std::collections::HashMap<u64, (f32, f32)> = std::collections::HashMap::new();
    let mut st = PathStats::default();
    for ev in &dev.evs {
        let DevEv::Touch {
            action, pid, x, y, ..
        } = *ev
        else {
            continue;
        };
        if !want(pid) {
            continue;
        }
        let p = (x as f32, y as f32);
        match action {
            ACTION_DOWN => {
                st.downs += 1;
                last.insert(pid, p);
            }
            ACTION_MOVE => {
                st.moves += 1;
                if let Some(prev) = last.get(&pid) {
                    let dx = p.0 - prev.0;
                    let dy = p.1 - prev.1;
                    st.net.0 += dx;
                    st.net.1 += dy;
                    st.path += dx.hypot(dy);
                }
                last.insert(pid, p);
            }
            ACTION_UP => {
                st.ups += 1;
                last.remove(&pid);
            }
            _ => {}
        }
    }
    st
}

pub fn assert_no_leak(dev: &DeviceSim, what: &str) {
    assert!(
        dev.leaks().is_empty(),
        "{what}:设备端仍按着 {:?}(卡键);注入序列末尾没有把它们抬起来",
        dev.leaks()
    );
}

pub fn assert_pointer_budget(dev: &DeviceSim, what: &str) {
    assert!(
        dev.peak_pointers <= DEVICE_MAX_POINTERS,
        "{what}:峰值触点 {} 超过设备上限 {DEVICE_MAX_POINTERS}",
        dev.peak_pointers
    );
}

pub fn assert_no_dropped_touch(dev: &DeviceSim, what: &str) {
    assert_eq!(
        dev.dropped_oob, 0,
        "{what}:有 {} 条触摸因为坐标出屏被设备端整条丢弃(注入的落点越界)",
        dev.dropped_oob
    );
    assert_eq!(
        dev.unknown_pid, 0,
        "{what}:有 {} 条 MOVE/UP 发给了设备端并不存在的手指(本地状态与设备脱节)",
        dev.unknown_pid
    );
}

/// 三条一起查 —— 每个场景收尾都调它。
pub fn assert_clean(dev: &DeviceSim, what: &str) {
    assert_no_leak(dev, what);
    assert_pointer_budget(dev, what);
    assert_no_dropped_touch(dev, what);
}

// ============================ 回放真机轨迹 ============================

/// **测试专用缝**:记录 [`Harness`] 实际喂进引擎的输入(逐条、原值、按序)。
///
/// 为什么需要它:`replay` 的返回值是"从文件读到的记录",不是"喂进去的东西" ——
/// 只比对前者,把 `feed_*` 那一行删掉/改值也照样绿(V2-4 假守护测试的形状)。
/// 这里用**线程局部**变量(每个测试一个线程),不碰生产路径、不加锁、不写文件。
/// 生产构建里整个模块不存在(`cfg(test)`),`feed_*` 里的调用也一并消失。
#[cfg(test)]
pub mod fed_probe {
    use std::cell::RefCell;

    /// 一条被喂进引擎的输入(只记 T5 断言要用的字段)。
    #[derive(Debug, Clone, PartialEq)]
    pub enum Fed {
        Button { code: u16, down: bool },
        Motion { dx: f32, dy: f32 },
    }

    thread_local! {
        static LOG: RefCell<Vec<Fed>> = const { RefCell::new(Vec::new()) };
    }

    pub fn push(e: Fed) {
        LOG.with(|l| l.borrow_mut().push(e));
    }

    pub fn take() -> Vec<Fed> {
        LOG.with(|l| std::mem::take(&mut *l.borrow_mut()))
    }
}

/// 把一份真机轨迹的**输入**灌回无头链路,返回终态设备 + 记录里的注入序列。
///
/// 用法:真机上复现一次(`SCRCPY_PAD_TRACE=...`),把文件路径交给它 ——
/// 于是"真机上偶尔才出"的问题变成一条可反复跑的断言。
///
/// 能力边界(协议「B4 会前登记」第 5 条):`trace::read` 丢弃 `st`/`aim` 行,
/// 这里只喂 `in/btn` + `in/motion`、忽略全部 `out`;`feed_motion` 直入引擎
/// `rx`、绕过合并器与 Windows 捕获层 ⇒ 回放验的是"引擎→设备"的守恒/泄漏/预算,
/// **不是** 1ms 合并率。
pub fn replay(path: &Path, screen: (u32, u32), profile: Profile) -> (DeviceSim, Vec<trace::Rec>) {
    let recs = trace::read(path).unwrap_or_else(|e| panic!("读不了轨迹 {path:?}: {e}"));
    let h = Harness::start(profile, screen);
    for r in &recs {
        match r {
            trace::Rec::Button { code, down, .. } => h.feed_button(*code, *down),
            trace::Rec::Motion { dx, dy, .. } => h.feed_motion(*dx, *dy),
            trace::Rec::Out(_) => {}
        }
    }
    h.settle(200);
    (h.stop(), recs)
}

/// FPS 场景的起手配置:无边界跑步机 + 静止归中 + 锚点居中。
///
/// 与界面上的「FPS 推荐参数」(`app.rs` 的 `apply_fps_preset`)口径一致 ——
/// 场景测的就是用户实际会用的那套参数。
pub fn fps_profile(screen: (u32, u32)) -> Profile {
    let (w, h) = screen;
    let mut p = Profile::default();
    p.aim.enabled = true;
    p.aim.toggle_key = KeySet::single(67); // F9
    p.aim.anchor_x = 0.5;
    p.aim.anchor_y = 0.5;
    p.aim.sensitivity_x = 2.0;
    p.aim.sensitivity_y = 2.0;
    p.aim.move_speed = 1.0;
    p.aim.drag_deadzone = 0.0;
    p.aim.boundary = false;
    p.aim.open_world = false;
    p.aim.recenter_idle_ms = 80;
    p.aim.recenter_threshold = 200;
    p.aim.hold_key = KeySet::new();
    // 锚点在屏幕正中,回转半径 200px 远小于到边的距离 —— 断言里的可用带是满的。
    p.aim.anchor_x = 0.5;
    p.aim.anchor_y = 0.5;
    let _ = (w, h);
    p
}

// ============================ 钩子/raw 交叉对账探针(T2) ============================

/// 轨迹心跳行 `st` 里的 `raw` 取值(与 `capture.rs` 的 `RAW_*` 同口径)。
///
/// 这里**独立写一份**而不是引捕获层的常量:探针若引用被测实现的常量,
/// 常量被改错就一起错、失去校验意义(同 [`DEVICE_MAX_POINTERS`] 的口径)。
pub const RAW_STATE_UNREGISTERED: u8 = 0;
/// 已注册、还没等到第一条 `WM_INPUT`。
pub const RAW_STATE_PENDING: u8 = 1;
/// 见过**相对**位移:原始输入接管位移。
pub const RAW_STATE_RELATIVE: u8 = 2;
/// 只见过**绝对**坐标(数位板/远程桌面/部分触控板):位移退回钩子光标差路径。
pub const RAW_STATE_ABSOLUTE_ONLY: u8 = 3;

/// 交叉对账的结论。
///
/// **仅作取证展示** —— 协议 §7 红线:设备账本不可用,账本/对账**禁止**作为
/// 主触发机制。本文件里没有任何调用方拿它改变行为;它只回答"这一份轨迹里,
/// 位移到底是哪条通道供的、差在哪"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrossVerdict {
    /// 位移与 `WM_INPUT` 都没有:没动过鼠标,没有可对账的差。
    Idle,
    /// 收到过 `WM_INPUT`:位移由原始输入供,钩子的光标差路径让位。
    RawServing,
    /// 状态是"只有绝对坐标设备":原始输入对它不适用,位移走钩子光标差(预期回退)。
    RawFallingBack,
    /// **有位移在流、`WM_INPUT` 一条没来**:位移走的是钩子回退路径 ——
    /// 这就是"降级"的可观测签名(与 `raw_probe` 的 `state` + `count` 同源)。
    RawSilentWhileMotionFlows,
}

impl CrossVerdict {
    /// 结论的一句话说明(落日志用)。
    pub fn label(self) -> &'static str {
        match self {
            CrossVerdict::Idle => "没有位移,无对账依据",
            CrossVerdict::RawServing => "原始输入在供位移(钩子光标差已让位)",
            CrossVerdict::RawFallingBack => "绝对坐标设备:原始输入不适用,位移走钩子光标差",
            CrossVerdict::RawSilentWhileMotionFlows => {
                "有位移却一条 WM_INPUT 都没有:位移走钩子回退路径"
            }
        }
    }
}

/// 一份对账报告:两条通道各自的计数 + 位移统计 + 结论。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CrossReport {
    /// 最后一条 `st` 行里的 `raw`(没有 `st` 行时 = 0 = 未注册)
    pub raw_state: u8,
    /// `st` 行数 —— 心跳还在不在的直接证据(停 = 消费线程卡死/退出)
    pub raw_ticks: u64,
    /// `wmin`:累计收到的 `WM_INPUT` 条数(取观测窗内**最大值**,累计量单调)
    pub raw_events: u64,
    /// `in/motion` 行数(引擎侧收到的位移条数)
    pub motion_events: u64,
    /// 位移净和(有符号)
    pub net: (f64, f64),
    /// 位移路径长(只累加长度)
    pub path: f64,
}

impl CrossReport {
    /// 对账结论(纯判定,只看计数)。
    pub fn verdict(&self) -> CrossVerdict {
        if self.raw_events > 0 {
            return CrossVerdict::RawServing;
        }
        if self.motion_events == 0 {
            return CrossVerdict::Idle;
        }
        if self.raw_state == RAW_STATE_ABSOLUTE_ONLY {
            return CrossVerdict::RawFallingBack;
        }
        CrossVerdict::RawSilentWhileMotionFlows
    }

    /// **日志样例格式**(仅诊断;单行,便于 grep 与落盘):
    ///
    /// ```text
    /// 原始输入对账: raw=2 wmin=4800 心跳=3 位移=2条 净=(25.0,-5.0) 路径=26.2px → 原始输入在供位移(钩子光标差已让位)
    /// ```
    pub fn line(&self) -> String {
        format!(
            "原始输入对账: raw={} wmin={} 心跳={} 位移={}条 净=({:.1},{:.1}) 路径={:.1}px → {}",
            self.raw_state,
            self.raw_events,
            self.raw_ticks,
            self.motion_events,
            self.net.0,
            self.net.1,
            self.path,
            self.verdict().label()
        )
    }
}

/// 交叉对账探针的累加器(纯数据结构 + 纯方法,不碰全局、不写文件)。
///
/// 两条输入流分别喂进来:
/// * [`Self::note_raw_tick`] —— 轨迹心跳行 `st`(`raw`/`wmin`,捕获层每 200ms 一条);
/// * [`Self::note_motion`] —— 引擎侧收到的位移(`in/motion` 行)。
///
/// 之所以要对账:位移有**两条**来源(原始输入的设备计数 / 钩子的光标差),
/// 真机上无法同时观测"哪条在供" —— 只有把 `wmin`(原始输入收到几条)与
/// 位移条数摆在一起,才能分辨"原始输入在跑"、"原始输入哑了而钩子接手"、
/// "绝对坐标设备预期回退"这三种完全不同的状态。
#[derive(Debug, Clone, Default)]
pub struct CrossLedger {
    report: CrossReport,
}

impl CrossLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记一条 `st` 心跳。`wmin` 是**累计**条数,故取最大值(乱序/重放都不倒扣)。
    pub fn note_raw_tick(&mut self, raw_state: u8, wmin: u64) {
        self.report.raw_ticks += 1;
        self.report.raw_state = raw_state;
        self.report.raw_events = self.report.raw_events.max(wmin);
    }

    /// 记一条位移(引擎侧收到的,设备计数)。
    pub fn note_motion(&mut self, dx: f32, dy: f32) {
        let (dx, dy) = (dx as f64, dy as f64);
        self.report.motion_events += 1;
        self.report.net.0 += dx;
        self.report.net.1 += dy;
        self.report.path += dx.hypot(dy);
    }

    pub fn report(&self) -> CrossReport {
        self.report
    }
}

/// 回放一份轨迹做**钩子/raw 交叉对账**(T2;验收观察点 = "回放 trace 对账")。
///
/// 与 [`crate::trace::read`] 分开:那个刻意丢弃 `st`/`aim` 心跳(回放器只认输入),
/// 而对账恰恰**只要** `st` 行 —— 于是这里按同一条 jsonl 格式自己逐行解析,
/// 容忍口径也照抄(认不出的行跳过:半截文件也该能读多少算多少)。
///
/// 只读文件,不写任何东西(§3.5)。
pub fn ledger_from_trace(path: &Path) -> std::io::Result<CrossReport> {
    let text = std::fs::read_to_string(path)?;
    let mut ledger = CrossLedger::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let k = v.get("k").and_then(|k| k.as_str()).unwrap_or("");
        let e = v.get("e").and_then(|e| e.as_str()).unwrap_or("");
        match (k, e) {
            ("st", _) => {
                let state = v.get("raw").and_then(|x| x.as_u64()).unwrap_or(0) as u8;
                let wmin = v.get("wmin").and_then(|x| x.as_u64()).unwrap_or(0);
                ledger.note_raw_tick(state, wmin);
            }
            ("in", "motion") => {
                let dx = v.get("dx").and_then(|x| x.as_f64()).unwrap_or(0.0) as f32;
                let dy = v.get("dy").and_then(|x| x.as_f64()).unwrap_or(0.0) as f32;
                ledger.note_motion(dx, dy);
            }
            _ => {}
        }
    }
    Ok(ledger.report())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 解码器:一次推半个消息也必须能拼回来(TCP 不保证边界)。
    #[test]
    fn decoder_handles_split_messages() {
        let mut dec = Decoder::new();
        // 一条 DOWN(32B)+ 一条 KEY(14B)
        let mut msg = vec![TYPE_INJECT_TOUCH];
        msg.push(ACTION_DOWN);
        msg.extend_from_slice(&3000u64.to_be_bytes());
        msg.extend_from_slice(&640u32.to_be_bytes());
        msg.extend_from_slice(&1386u32.to_be_bytes());
        msg.extend_from_slice(&1280u16.to_be_bytes());
        msg.extend_from_slice(&2772u16.to_be_bytes());
        msg.extend_from_slice(&0xFFFFu16.to_be_bytes());
        msg.extend_from_slice(&[0u8; 8]);
        assert_eq!(msg.len(), 32, "触控消息固定 32 字节");
        let mut key = vec![TYPE_INJECT_KEYCODE, 1];
        key.extend_from_slice(&4u32.to_be_bytes());
        key.extend_from_slice(&[0u8; 8]);
        assert_eq!(key.len(), 14);

        // 先给 20 字节(半个触摸消息):什么都不该解出来
        assert!(dec.push(&msg[..20]).is_empty());
        // 再给剩下的 + 完整按键:两条都要出来
        let mut rest = msg[20..].to_vec();
        rest.extend_from_slice(&key);
        let evs = dec.push(&rest);
        assert_eq!(evs.len(), 2, "{evs:?}");
        assert_eq!(
            evs[0],
            DevEv::Touch {
                action: ACTION_DOWN,
                pid: 3000,
                x: 640,
                y: 1386,
                w: 1280,
                h: 2772,
                pressure: 0xFFFF,
            }
        );
        assert_eq!(
            evs[1],
            DevEv::Key {
                action: 1,
                keycode: 4
            }
        );
        assert_eq!(dec.corrupt(), None);
    }

    /// 未知类型号必须被记下来(长度未知、无法再同步)—— 静默吞掉会让整份
    /// 回放看起来"什么都没发生",那是最坏的诊断结果。
    #[test]
    fn decoder_flags_unknown_type() {
        let mut dec = Decoder::new();
        assert!(dec.push(&[99, 1, 2, 3]).is_empty());
        assert_eq!(dec.corrupt(), Some(99));
    }

    /// 假设备三条语义:出屏丢弃 / 池满拒绝 / 未知 pid 计数。
    #[test]
    fn device_sim_follows_server_semantics() {
        let mut d = DeviceSim::new(100, 100);
        // 出屏:整条丢弃,连"按着"都不算
        d.apply(DevEv::Touch {
            action: ACTION_DOWN,
            pid: 1,
            x: 100,
            y: 50,
            w: 100,
            h: 100,
            pressure: 0xFFFF,
        });
        assert_eq!(d.dropped_oob, 1);
        assert!(d.open.is_empty());
        // 池满:第 11 根手指被拒
        for i in 0..DEVICE_MAX_POINTERS as u64 {
            d.apply(DevEv::Touch {
                action: ACTION_DOWN,
                pid: 100 + i,
                x: 10,
                y: 10,
                w: 100,
                h: 100,
                pressure: 0xFFFF,
            });
        }
        assert_eq!(d.open.len(), DEVICE_MAX_POINTERS);
        assert_eq!(d.peak_pointers, DEVICE_MAX_POINTERS);
        d.apply(DevEv::Touch {
            action: ACTION_DOWN,
            pid: 999,
            x: 10,
            y: 10,
            w: 100,
            h: 100,
            pressure: 0xFFFF,
        });
        assert_eq!(d.rejected_pool, 1);
        assert!(!d.open.contains(&999));
        // 未知 pid 的 move/up
        d.apply(DevEv::Touch {
            action: ACTION_MOVE,
            pid: 999,
            x: 11,
            y: 10,
            w: 100,
            h: 100,
            pressure: 0xFFFF,
        });
        d.apply(DevEv::Touch {
            action: ACTION_UP,
            pid: 999,
            x: 11,
            y: 10,
            w: 100,
            h: 100,
            pressure: 0,
        });
        assert_eq!(d.unknown_pid, 2);
    }

    /// 位移统计:换手时新指从锚点起步,DOWN 重置基准 —— 累加出来的正是
    /// 设备端真正收到的推进量。
    #[test]
    fn path_stats_resets_baseline_on_down() {
        let mut d = DeviceSim::new(1000, 1000);
        let mk = |action, pid, x, y| DevEv::Touch {
            action,
            pid,
            x,
            y,
            w: 1000,
            h: 1000,
            pressure: 0,
        };
        d.apply(mk(ACTION_DOWN, 3000, 500, 500));
        d.apply(mk(ACTION_MOVE, 3000, 600, 500)); // +100
        d.apply(mk(ACTION_UP, 3000, 600, 500));
        // 换手:新 pid 从锚点重新落下,老位置到锚点之间的差**不算位移**
        d.apply(mk(ACTION_DOWN, 3001, 500, 500));
        d.apply(mk(ACTION_MOVE, 3001, 700, 500)); // +200
        let st = path_stats(&d, |pid| pid == 3000 || pid == 3001);
        assert_eq!(st.net.0, 300.0);
        assert_eq!(st.path, 300.0);
        assert_eq!((st.downs, st.ups), (2, 1));
    }

    /// 重叠换手的基准必须**按 pid** 分开:换手的实际到达顺序是
    /// `DOWN(新指)` → `UP(旧指)` → `MOVE(新指)`。用单一基准时那条 UP 会把
    /// 新指刚建立的基准清掉,段内第一个 MOVE 就被静默漏算 —— 这正是第一版把
    /// "位移足额送达"误判成"丢了 13%"的原因,单独钉一条防它回来。
    #[test]
    fn path_stats_keeps_baseline_per_pid_across_overlap_handoff() {
        let mut d = DeviceSim::new(1000, 1000);
        let mk = |action, pid, x, y| DevEv::Touch {
            action,
            pid,
            x,
            y,
            w: 1000,
            h: 1000,
            pressure: 0,
        };
        d.apply(mk(ACTION_DOWN, 3000, 500, 500));
        d.apply(mk(ACTION_MOVE, 3000, 600, 500)); // +100
        d.apply(mk(ACTION_DOWN, 3001, 500, 500)); // 重叠:新指先落
        d.apply(mk(ACTION_UP, 3000, 600, 500)); // 再抬旧指(不许动新指的基准)
        d.apply(mk(ACTION_MOVE, 3001, 700, 500)); // +200,必须被算上
        let st = path_stats(&d, |_| true);
        assert_eq!(st.net.0, 300.0, "重叠换手后段内首个 MOVE 丢了:{st:?}");
        assert_eq!(st.downs, 2);
        assert_eq!(st.ups, 1);
    }

    /// 场景①:静止时**一条命令都不发**。空闲乱发是"手感漂移/莫名位移"的常见来源,
    /// 这条把它钉死。
    #[test]
    fn scenario_idle_is_silent() {
        let h = Harness::start(fps_profile((1280, 2772)), (1280, 2772));
        h.settle(200);
        let dev = h.device();
        assert!(
            dev.evs.is_empty(),
            "没有任何输入时不该注入任何命令,却发了 {} 条:{:?}",
            dev.evs.len(),
            &dev.evs[..dev.evs.len().min(4)]
        );
        h.stop();
    }

    /// 场景②:无边界跑步机把鼠标位移**足额**送到设备端,且不泄漏触点。
    /// 这是"手感"的底线:累计注入位移 == 鼠标位移 × 灵敏度。
    #[test]
    fn scenario_flick_is_delivered_and_conserved() {
        let screen = (1280u32, 2772u32);
        let h = Harness::start(fps_profile(screen), screen);
        h.tap(67); // F9:进入 FPS 模式
        h.settle(60);
        // 30 次 × 20 计数 × 灵敏度 2.0 = 1200 设备像素,横向一直往右推
        let events = 30;
        let per = 20.0f32;
        for _ in 0..events {
            h.feed_motion(per, 0.0);
            h.settle(8);
        }
        h.settle(400); // 等 AIM_MAX_STEP_PX 把余量摊完
        let dev = h.device();
        let st = path_stats(&dev, |_| true);
        let want = events as f32 * per * 2.0;
        println!(
            "注入 down {} move {} up {};净位移 {:?} 路径 {:.0}px(期望 {:.0}px)",
            st.downs, st.moves, st.ups, st.net, st.path, want
        );
        assert!(
            (st.path - want).abs() <= want * 0.1,
            "横向位移没有足额送达:路径 {:.0}px,期望 {:.0}px(±10%)",
            st.path,
            want
        );
        assert!(st.downs >= 1, "FPS 模式下一次都没落指");
        let final_dev = h.stop();
        assert_clean(&final_dev, "甩枪");
        assert!(
            final_dev.evs.len() >= 2,
            "设备端至少该看到落指与推进:{:?}",
            final_dev.evs
        );
    }

    /// 场景③:关映射必须把按键上的手指全部抬起(卡键是用户最痛的老问题)。
    #[test]
    fn scenario_disable_releases_every_finger() {
        let screen = (1280u32, 2772u32);
        let h = Harness::start(fps_profile(screen), screen);
        h.tap(67);
        h.settle(60);
        for _ in 0..5 {
            h.feed_motion(30.0, 0.0);
            h.settle(8);
        }
        h.settle(80);
        let during = h.device();
        assert!(
            !during.leaks().is_empty(),
            "瞄准期间设备端应当按着一根手指:{:?}",
            during.evs
        );
        h.enable(false);
        h.settle(150);
        let after = h.device();
        assert_no_leak(&after, "关映射");
        h.stop();
    }

    /// 场景④:压枪按住不松、鼠标不动时**不许**出现"抬指/重按"空转。
    ///
    /// 真机日志(2026-10-11,debug 级)里 **125 秒 277 次换手**,而多数换手的累计
    /// 偏移只有 **2~3px**:静止归中的判据原本是"偏移 ≠ 0",清零之后压枪/余量立刻
    /// 又把它填回同一量级,于是归中变成每秒 2~5 次的"抬指 + 重按"。它既直接伤手感,
    /// 也是用户报的"手指碰一下手机屏,FPS 就锁死"的合理成因(设备端指针集合被
    /// 高频 down/up 搅乱)。修法是三道闸:位移门槛、最小间隔、压枪期间不归中。
    #[test]
    fn scenario_idle_recoil_does_not_churn_handoffs() {
        let screen = (1280u32, 2772u32);
        let mut p = fps_profile(screen);
        p.aim.recoil.enabled = true;
        p.aim.recoil.trigger_key = KeySet::single(272); // 左键:按住开火
        p.aim.recoil.rate_hz = 60.0;
        // 0.4px/拍 = 24px/s:小到不会顶到回转带(200px),但**每一轮维护都让
        // "待发偏移"非零** —— 旧判据 `ox != 0` 正是这么被反复点着的。
        // (用 9px/拍会真的顶到 200px 回转带,那时换手是跑步机的**正确**行为,
        //  不该拿来断言"不许换手"。)
        p.aim.recoil.strengths = vec![0.4];
        let h = Harness::start(p, screen);
        h.tap(67); // F9 进 FPS
        h.settle(60);
        h.feed_button(272, true); // 按住触发键:压枪持续加偏移
        h.settle(900);
        let dev = h.device();
        let st = path_stats(&dev, |_| true);
        println!(
            "压枪静止 900ms:down {} up {} move {}",
            st.downs, st.ups, st.moves
        );
        assert!(
            st.downs <= 2,
            "压枪静止期间不该反复抬指重按:down {} (期望 ≤2)",
            st.downs
        );
        assert!(st.ups <= 1, "压枪静止期间不该反复抬指:up {}", st.ups);
        assert!(st.moves >= 1, "压枪应当真的在注入位移,却一条 MOVE 都没有");
        h.feed_button(272, false);
        assert_clean(&h.stop(), "压枪静止");
    }

    /// 场景⑤:停顿之后的第一次瞄准位移,**当"分离式续接"被打开时**必须先只抬指。
    ///
    /// 真机定性(用户 2026-10-11):锁住时**按 F8(关映射 → `release_all` 把触点全抬起来)
    /// 立刻恢复**,F9 退出/重进 FPS 也恢复 —— 共同点都是"先把按着的东西抬干净"。
    /// 所以续接**不能**用换手那种重叠(先按新的再抬旧的):重叠全程 pointerCount ≥ 2,
    /// 游戏永远看不到手势结束,状态解不开(上一版就是这么无效的)。
    ///
    /// ⚠ 该行为**默认关闭**(见 `engine::AIM_RELIFT_ENABLED` 的注释:用户可能用双指
    /// 按屏滑视角,自动抬指有打断他手势的风险,等他确认用法)。本测试显式打开它,
    /// 于是这条不变量在被启用时仍然成立;关闭时"什么都不做"由下面那半条对照锁住。
    #[test]
    fn scenario_aim_resume_lifts_first_then_presses_again() {
        use std::sync::atomic::Ordering;
        let screen = (1280u32, 2772u32);

        // (对照)默认关闭时:停顿后的第一下位移**不许**抬指 —— 行为与用户现况一致。
        {
            let h = Harness::start(fps_profile(screen), screen);
            h.tap(67);
            h.settle(60);
            h.feed_motion(3.0, 0.0);
            h.settle(80);
            h.settle(600);
            h.feed_motion(3.0, 0.0);
            h.settle(120);
            let st = path_stats(&h.device(), |_| true);
            assert_eq!(
                st.ups, 0,
                "分离式续接默认关闭时不许抬指(会打断用户的双指手势):up {}",
                st.ups
            );
            h.stop();
        }

        // (启用)打开之后:必须先抬指(设备端零瞄准触点),下一次位移再重新落指。
        crate::engine::AIM_RELIFT_ENABLED.store(true, Ordering::Relaxed);
        let h = Harness::start(fps_profile(screen), screen);
        h.tap(67); // F9 进 FPS
        h.settle(60);
        // 极小的位移:留下几像素偏移,但**不足以触发静止归中**(门槛 14px)。
        h.feed_motion(3.0, 0.0);
        h.settle(80);
        assert_eq!(
            path_stats(&h.device(), |_| true).downs,
            1,
            "首次拖动应当只落一次指"
        );
        h.settle(600); // 停 600ms > AIM_RESUME_GAP(400ms)
        h.feed_motion(3.0, 0.0); // 停顿后的第一下:只抬,不按
        h.settle(120);
        let dev = h.device();
        let st = path_stats(&dev, |_| true);
        assert!(
            st.ups >= 1,
            "启用后:停顿后的第一下位移必须**抬指**(否则游戏看不到手势结束):up {}",
            st.ups
        );
        assert!(
            dev.leaks().is_empty(),
            "续接必须经过'设备端零瞄准触点'的瞬间,此刻却还按着 {:?}",
            dev.leaks()
        );
        h.feed_motion(3.0, 0.0);
        h.settle(120);
        assert!(
            path_stats(&h.device(), |_| true).downs >= 2,
            "抬指之后的下一次位移必须重新落指"
        );
        assert_clean(&h.stop(), "停顿分离式续接");
        crate::engine::AIM_RELIFT_ENABLED.store(false, Ordering::Relaxed);
    }

    /// **缝②接线层回归**(2026-10-12,协议 §6.3 必测缝②):
    /// 引擎 `run` 循环 → 热键动作,**真的接上了**。
    ///
    /// 为什么必须有这一条:仓库里此前只有**判定函数级**测试
    /// (`wake_key_action` / `self_heal_lift` / `macro_hotkey_action`),
    /// 而"`run()` 里那一行到底有没有把按键接到动作上"**没有任何测试** ——
    /// 这正是 F5 类错误的温床:判定对了、接线断了,门禁照样全绿而功能是死的。
    ///
    /// 这里走**真引擎线程 + 真控制通道 + 假设备**(与真机同一条 `engine::run`),
    /// 只做一件事:按一下「唤醒视角」热键,然后看**设备端**有没有真的收到抬起。
    /// 断言的落点是设备,不是引擎内部状态 —— 只有"接线真的通了"才可能通过。
    #[test]
    fn hotkey_action_is_wired_through_the_engine_loop() {
        let screen = (1280u32, 2772u32);
        let mut p = fps_profile(screen);
        // 绑「唤醒视角」热键(默认不绑),其余保持默认
        p.aim.wake_key = KeySet::single(88); // F12
        let h = Harness::start(p, screen);
        h.tap(67); // F9 进 FPS
        h.settle(60);
        // 动一下鼠标让瞄准指落到设备上
        h.feed_motion(30.0, 0.0);
        h.settle(120);
        let before = h.device();
        assert!(
            !before.leaks().is_empty(),
            "前置条件:瞄准指应当已经按在设备上 {:?}",
            before.evs
        );
        let downs_before = path_stats(&before, |_| true).downs;
        let ups_before = path_stats(&before, |_| true).ups;

        // ★ 按下热键:走的是**引擎循环里的那条接线**(不是直接调函数)
        h.feed_button(88, true);
        h.settle(150);
        let after = h.device();
        let st = path_stats(&after, |_| true);
        assert!(
            st.ups > ups_before,
            "按下「唤醒视角」后,设备端必须收到抬起(说明引擎循环把键接到了 aim_lift):\
             ups {} → {}",
            ups_before,
            st.ups
        );
        assert!(
            after.leaks().is_empty(),
            "热键动作之后设备端不该还按着任何指 {:?}",
            after.leaks()
        );
        // 映射必须**还开着**(热键只抬指,不关映射)
        assert!(
            crate::engine::lock_shared(&h.shared).enabled,
            "「唤醒视角」只抬指,不许关映射"
        );
        // 松开热键,再动一下:应当重新落指(下一次位移自动重建)
        h.feed_button(88, false);
        h.settle(30);
        h.feed_motion(20.0, 0.0);
        h.settle(150);
        let rebuilt = h.device();
        assert!(
            path_stats(&rebuilt, |_| true).downs > downs_before,
            "抬指之后的位移必须重新落指(否则视角再也回不来)"
        );
        assert_clean(&h.stop(), "热键接线");
    }

    /// **缝②接线层回归(第二半)**:热键**未绑定**时,同一条循环里不许有任何动作。
    ///
    /// 这是"默认不改行为"的接线级对照:判定函数说"空集合不触发",
    /// 这条验证**引擎循环**真的没有把它接上(而不是"判定对了但接线绕过了判定")。
    #[test]
    fn unbound_hotkey_is_not_wired_to_any_action() {
        let screen = (1280u32, 2772u32);
        let mut p = fps_profile(screen);
        p.aim.wake_key = KeySet::new(); // 未绑定(默认)
        let h = Harness::start(p, screen);
        h.tap(67);
        h.settle(60);
        h.feed_motion(30.0, 0.0);
        h.settle(120);
        let before = h.device();
        let ups_before = path_stats(&before, |_| true).ups;

        // 按同一个键码:未绑定 ⇒ 不该有任何抬起
        h.feed_button(88, true);
        h.settle(150);
        let after = h.device();
        assert_eq!(
            path_stats(&after, |_| true).ups,
            ups_before,
            "未绑定的键不许触发任何动作(默认行为必须逐位不变)"
        );
        assert!(
            !after.leaks().is_empty(),
            "瞄准指应当还按着(没人抬它) {:?}",
            after.leaks()
        );
        h.feed_button(88, false);
        assert_clean(&h.stop(), "未绑定热键");
    }

    /// **V2-2 连点:引擎级行为验证**(用户 2026-10-13)。
    ///
    /// 走**真引擎线程 + 真控制通道 + 假设备**,验证三件事:
    /// ① `repeat_interval_ms > 0` 时,按住期间设备端收到**多次**点击;
    /// ② 每一点击都有**配对的抬起**(不许"只按不抬" —— 那是会卡键的);
    /// ③ `repeat_interval_ms = 0` 时与从前一致:**只点一次**(默认行为不变)。
    #[test]
    fn scenario_tap_repeat_clicks_while_held() {
        let screen = (1280u32, 2772u32);
        // 绑定一个"连点"键:按住期间每 100ms 点一次,单次按住 40ms。
        //
        // **测试键必须避开 `Profile::default()` 已占用的码**:默认轮盘把
        // 17/31/30/32(W/S/A/D)绑成了四个方向键,而方向键事件会被轮盘**消费掉**
        // (`dispatch_input` 里 `return true`),根本走不到普通绑定 ——
        // 第一版用 30 就是这么失败在 `down 1 up 0` 的(不是产品 bug,是测试选键冲突)。
        // 57 = KEY_SPACE,默认未被占用。
        const TEST_KEY: u16 = 57;
        let mut p = Profile::default();
        p.binds.push(crate::keymap::KeyBind {
            key: TEST_KEY,
            action: crate::keymap::Action::Tap {
                x: 0.5,
                y: 0.5,
                duration_ms: 40,
                radius: 0.02,
                repeat_interval_ms: 100,
            },
            fps_only: false,
            tail_delay_ms: 0,
        });
        let h = Harness::start(p, screen);
        h.enable(true);
        h.settle(60);
        // 按住 700ms ⇒ 约 7 次点击(首次 + 6 次连点)
        h.feed_button(TEST_KEY, true);
        h.settle(700);
        let dev = h.device();
        let st = path_stats(&dev, |_| true);
        println!(
            "连点 700ms(间隔100ms):down {} up {} move {}",
            st.downs, st.ups, st.moves
        );
        assert!(
            st.downs >= 3,
            "按住期间必须多次点击(至少 3 次),实测 down={}",
            st.downs
        );
        assert_eq!(
            st.downs, st.ups,
            "每一次按下必须有配对的抬起(不许只按不抬):down={} up={}",
            st.downs, st.ups
        );
        h.feed_button(TEST_KEY, false);
        h.settle(200);
        assert_clean(&h.stop(), "连点");

        // ② 对照:repeat_interval_ms = 0 ⇒ 与从前一致,只点一次
        let mut p0 = Profile::default();
        p0.binds.push(crate::keymap::KeyBind {
            key: TEST_KEY,
            action: crate::keymap::Action::Tap {
                x: 0.5,
                y: 0.5,
                duration_ms: 40,
                radius: 0.02,
                repeat_interval_ms: 0,
            },
            fps_only: false,
            tail_delay_ms: 0,
        });
        let h0 = Harness::start(p0, screen);
        h0.enable(true);
        h0.settle(60);
        h0.feed_button(TEST_KEY, true);
        h0.settle(700);
        let st0 = path_stats(&h0.device(), |_| true);
        println!("对照(连点关闭)700ms:down {} up {}", st0.downs, st0.ups);
        assert_eq!(st0.downs, 1, "连点关闭时按住只点一次(默认行为不变)");
        assert_eq!(st0.ups, 1, "那一次也要有配对抬起");
        h0.feed_button(TEST_KEY, false);
        h0.settle(200);
        assert_clean(&h0.stop(), "连点关闭对照");
    }

    /// **V2-2 轨迹:引擎级行为验证**(用户 2026-10-13)。
    ///
    /// 验证:给了 `recorded_points` 的滑动,设备端收到的触点路径**经过那些点**
    /// (而不是走"起点→终点"的直线),且总量守恒、无泄漏。
    #[test]
    fn scenario_swipe_track_follows_recorded_points() {
        let screen = (1280u32, 2772u32);
        // 一条"折返"轨迹:右 → 左 → 右。若走直线(起点→终点)就只会在右半边。
        let track = vec![(0.2, 0.5), (0.8, 0.5), (0.2, 0.5), (0.8, 0.5)];
        const TEST_KEY: u16 = 57; // KEY_SPACE:避开默认轮盘的 W/S/A/D
        let mut p = Profile::default();
        p.binds.push(crate::keymap::KeyBind {
            key: TEST_KEY,
            action: crate::keymap::Action::Swipe(crate::keymap::Swipe {
                start: (0.2, 0.5),
                end: (0.8, 0.5),
                duration_ms: 400,
                easing: crate::keymap::Easing::Linear,
                path: crate::keymap::SwipePath::Line,
                recorded_points: track.clone(),
            }),
            fps_only: false,
            tail_delay_ms: 0,
        });
        let h = Harness::start(p, screen);
        h.enable(true);
        h.settle(60);
        h.feed_button(TEST_KEY, true);
        h.settle(700);
        let dev = h.device();
        let st = path_stats(&dev, |_| true);
        println!("轨迹滑动:down {} up {} move {}", st.downs, st.ups, st.moves);
        assert!(st.downs >= 1, "轨迹滑动必须落指");
        assert_eq!(st.downs, st.ups, "轨迹滑动收尾必须抬指(无泄漏)");
        assert!(st.moves >= 2, "轨迹必须产生多次移动(实测 {})", st.moves);
        // 关键:轨迹**折返**过 ⇒ 设备端 x 坐标应同时出现"左半"和"右半"。
        // 若走了直线(起点→终点),x 只会单调变化、不会回到起点附近。
        let xs: Vec<u32> = dev
            .evs
            .iter()
            .filter_map(|e| match e {
                DevEv::Touch { x, .. } => Some(*x),
                _ => None,
            })
            .collect();
        let min_x = xs.iter().copied().min().unwrap_or(0);
        let max_x = xs.iter().copied().max().unwrap_or(0);
        // 设备空间宽 1280:轨迹从 0.2×1280=256 到 0.8×1280=1024
        assert!(
            min_x <= 320,
            "轨迹应回到左半(实测 min_x={min_x}),若只走直线不会回来"
        );
        assert!(max_x >= 960, "轨迹应到过右半(实测 max_x={max_x})");
        assert_clean(&h.stop(), "轨迹滑动");
    }

    // ==================== T2 / T4 / T7(W3/B5,2026-10-16) ====================

    /// 临时轨迹文件:离开作用域(含测试 panic)即删。
    ///
    /// §6.6 临时物零残留:变异验证时测试**故意**失败,靠收尾代码删文件会漏
    /// (第一次变异验证就漏了一个 `t2-serving.jsonl`),所以用 `Drop` 兜底。
    struct TempTrace(std::path::PathBuf);

    impl Drop for TempTrace {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// 把一份轨迹内容写进**系统临时目录**并返回路径。
    ///
    /// §3.5 / §6.6:测试绝不写用户配置目录;临时物只放系统临时目录,用完即删。
    fn write_temp_trace(tag: &str, body: &str) -> TempTrace {
        let dir = std::env::temp_dir().join(format!("scrcpy-pad-sim-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建临时目录失败");
        let path = dir.join(format!("{tag}.jsonl"));
        std::fs::write(&path, body).expect("写临时轨迹失败");
        TempTrace(path)
    }

    /// 设备端注入序列(按到达顺序):`(action, pid, x, y)`。
    ///
    /// 黑盒断言用的就是这个 —— 断言的落点是**设备真正收到了什么**,
    /// 不是"我们以为发了什么"。
    fn touch_seq(dev: &DeviceSim) -> Vec<(u8, u64, u32, u32)> {
        dev.evs
            .iter()
            .filter_map(|e| match *e {
                DevEv::Touch {
                    action, pid, x, y, ..
                } => Some((action, pid, x, y)),
                _ => None,
            })
            .collect()
    }

    /// 某个 pid 在设备端的 `(down 条数, up 条数)`。
    fn edges_of(seq: &[(u8, u64, u32, u32)], pid: u64) -> (usize, usize) {
        let n = |a: u8| seq.iter().filter(|t| t.1 == pid && t.0 == a).count();
        (n(ACTION_DOWN), n(ACTION_UP))
    }

    /// **T2 接线测试 + 阳性/阴性对照(§3.3)**:钩子/raw 交叉对账探针。
    ///
    /// 为什么要这一条:位移有两条来源(原始输入的设备计数 / 钩子的光标差),
    /// 真机上无法同时观测"哪条在供" —— 只能靠轨迹里 `st`(`raw`/`wmin`)与
    /// `in/motion` 两条流**事后对账**。这里用**人造轨迹**走同一个探针:
    /// "原始输入在供位移"与"原始输入一条没来而位移在流"必须给出**不同**结论 ——
    /// 只产一种结果的探针不得作证据(§3.3)。
    ///
    /// 它同时是**接线测试**:`ledger_from_trace` 的"解析 → 探针"这一段被删掉时,
    /// 计数与结论会一起归零 ⇒ 立刻变红(见交付登记的变异验证)。
    #[test]
    fn t2_cross_ledger_reconciles_replayed_traces_with_both_polarities() {
        // 阳性:原始输入在供位移(状态 1 → 2,wmin 0 → 4800),两条位移在流
        let serving = write_temp_trace(
            "t2-serving",
            &[
                r#"{"t":10,"k":"st","raw":1,"wmin":0,"streak":0,"now":10}"#,
                r#"{"t":20,"k":"st","raw":2,"wmin":1600,"streak":0,"now":20}"#,
                r#"{"t":30,"k":"st","raw":2,"wmin":4800,"streak":0,"now":30}"#,
                r#"{"t":21,"k":"in","e":"motion","dx":15.0,"dy":0.0}"#,
                r#"{"t":22,"k":"in","e":"motion","dx":10.0,"dy":-5.0}"#,
                r#"{"t":23,"k":"out","e":"touch","a":0,"pid":3000,"x":640,"y":1386}"#,
                r#"{"t":24,"k":"aim","en":1,"mode":1,"susp":0,"gate":0,"anchor":1,"down":1,"others":0}"#,
            ]
            .join("\n"),
        );
        let r = ledger_from_trace(&serving.0).expect("读人造轨迹");
        println!("[T2 阳性] {}", r.line());
        assert_eq!(r.raw_ticks, 3, "三条 st 心跳都要被记到");
        assert_eq!(r.raw_events, 4800, "wmin 是累计量,取观测窗内最大值");
        assert_eq!(r.raw_state, RAW_STATE_RELATIVE);
        assert_eq!(r.motion_events, 2, "两条 in/motion;out/aim 行不参与对账");
        assert_eq!(r.net, (25.0, -5.0));
        assert!((r.path - (15.0f64.hypot(0.0) + 10.0f64.hypot(5.0))).abs() < 1e-9);
        assert_eq!(r.verdict(), CrossVerdict::RawServing);

        // 阴性:raw 一条没来(状态停在"未注册"、wmin 恒 0),而位移照流 ⇒ 降级签名
        let silent = write_temp_trace(
            "t2-silent",
            &[
                r#"{"t":10,"k":"st","raw":0,"wmin":0,"streak":0,"now":10}"#,
                r#"{"t":30,"k":"st","raw":0,"wmin":0,"streak":120,"now":30}"#,
                r#"{"t":21,"k":"in","e":"motion","dx":15.0,"dy":0.0}"#,
                r#"{"t":22,"k":"in","e":"motion","dx":10.0,"dy":-5.0}"#,
            ]
            .join("\n"),
        );
        let r2 = ledger_from_trace(&silent.0).expect("读人造轨迹");
        println!("[T2 阴性] {}", r2.line());
        assert_eq!(r2.raw_events, 0);
        assert_eq!(r2.motion_events, 2);
        assert_eq!(
            r2.verdict(),
            CrossVerdict::RawSilentWhileMotionFlows,
            "有位移却一条 WM_INPUT 都没有 ⇒ 位移走钩子回退路径"
        );
        assert_ne!(
            r.verdict(),
            r2.verdict(),
            "同一个探针必须能区分两种通道状态(否则它是恒真探针)"
        );

        // 第三极:绝对坐标设备(状态 3)⇒ 预期回退,不是"降级告警"
        let abs_only = write_temp_trace(
            "t2-abs",
            &[
                r#"{"t":10,"k":"st","raw":3,"wmin":0,"streak":0,"now":10}"#,
                r#"{"t":21,"k":"in","e":"motion","dx":15.0,"dy":0.0}"#,
            ]
            .join("\n"),
        );
        let r3 = ledger_from_trace(&abs_only.0).expect("读人造轨迹");
        println!("[T2 绝对坐标设备] {}", r3.line());
        assert_eq!(r3.verdict(), CrossVerdict::RawFallingBack);

        // 第四极:没有位移、没有心跳 ⇒ 无对账依据
        let empty = write_temp_trace("t2-empty", "");
        let r4 = ledger_from_trace(&empty.0).expect("读人造轨迹");
        assert_eq!(r4, CrossReport::default());
        assert_eq!(r4.verdict(), CrossVerdict::Idle);
    }

    /// **T4-① 甩枪**:Raw 计数序列 → 引擎 → 注入序列,黑盒断言。
    ///
    /// 本场景断言的是**注入序列的形状**(第一条落指、最后一条抬指、一路不回头、
    /// 总量守恒)。为此把推程压在**回转带(200px)以内**、并关掉静止归中 ——
    /// 换手与归中各自有独立测试(`scenario_flick_is_delivered_and_conserved` /
    /// `scenario_idle_recoil_does_not_churn_handoffs`),混进来只会让形状不可断言。
    #[test]
    fn t4_flick_raw_count_stream_lands_as_one_monotone_touch_path() {
        let screen = (1280u32, 2772u32);
        let (ticks, per) = (15usize, 5.0f32);

        // 阴性对照:同一串计数,**不进 FPS** ⇒ 设备端一条命令都不该有
        {
            let h = Harness::start(fps_profile(screen), screen);
            h.settle(60);
            for _ in 0..ticks {
                h.feed_motion(per, 0.0);
                h.settle(6);
            }
            h.settle(200);
            assert!(
                h.stop().evs.is_empty(),
                "瞄准未启用时位移序列不该产生任何注入"
            );
        }

        // 阳性:进 FPS,喂 15 拍 × 5 设备计数,灵敏度 2.0 ⇒ 75×2.0 = 150 设备像素
        let mut p = fps_profile(screen);
        p.aim.recenter = crate::keymap::RecenterMode::Never;
        let h = Harness::start(p, screen);
        h.tap(67); // F9
        h.settle(60);
        for _ in 0..ticks {
            h.feed_motion(per, 0.0);
            h.settle(6);
        }
        h.settle(300);
        let dev = h.stop();
        let seq = touch_seq(&dev);
        println!(
            "[T4 甩枪] 注入序列 {} 条,前 5 条 {:?}",
            seq.len(),
            &seq[..seq.len().min(5)]
        );
        let (downs, ups) = edges_of(&seq, seq.first().map(|t| t.1).unwrap_or(0));
        assert_eq!(downs, 1, "150px 不撞回转带 ⇒ 恰好落一次指:{seq:?}");
        assert_eq!(ups, 1, "收尾必须抬指:{seq:?}");
        assert_eq!(seq[0].0, ACTION_DOWN, "序列第一条必须是落指");
        assert_eq!(
            seq[seq.len() - 1].0,
            ACTION_UP,
            "序列最后一条必须是抬指(注入序列要有始有终)"
        );
        let xs: Vec<u32> = seq.iter().map(|t| t.2).collect();
        assert!(
            xs.windows(2).all(|w| w[0] <= w[1]),
            "一路往右甩必须单调不回头:{xs:?}"
        );
        let st = path_stats(&dev, |_| true);
        let want = ticks as f32 * per * 2.0;
        assert!(
            (st.path - want).abs() <= want * 0.12,
            "位移没有足额送达:路径 {:.0}px,期望 {want:.0}px(±12%)",
            st.path
        );
        assert_clean(&dev, "T4 甩枪");
    }

    /// **T4-② 点按**:Raw 计数序列在流,按键边沿必须精确成对、精确落点。
    #[test]
    fn t4_tap_edge_is_exact_while_a_raw_count_stream_flows() {
        const TEST_KEY: u16 = 57; // KEY_SPACE:避开默认轮盘的 W/S/A/D
        let screen = (1280u32, 2772u32);
        let mut p = Profile::default();
        p.binds.push(crate::keymap::KeyBind {
            key: TEST_KEY,
            action: crate::keymap::Action::Tap {
                x: 0.25,
                y: 0.75,
                duration_ms: 40,
                radius: 0.02,
                repeat_interval_ms: 0,
            },
            fps_only: false,
            tail_delay_ms: 0,
        });
        let h = Harness::start(p, screen);
        h.enable(true);
        h.settle(60);

        // 阴性对照:只喂 Raw 计数序列、不按键 ⇒ 设备端零命令(位移不产生注入)
        for _ in 0..10 {
            h.feed_motion(12.0, 0.0);
            h.settle(8);
        }
        h.settle(150);
        assert!(
            h.device().evs.is_empty(),
            "未进 FPS 时 Raw 计数序列不该产生注入:{:?}",
            h.device().evs
        );

        // 阳性:Raw 计数继续流的同时按下/松开 ⇒ 恰好一条落指 + 一条抬指
        for _ in 0..6 {
            h.feed_motion(12.0, 0.0);
            h.settle(6);
        }
        h.feed_button(TEST_KEY, true);
        h.settle(120);
        h.feed_button(TEST_KEY, false);
        h.settle(150);
        let dev = h.device();
        let seq = touch_seq(&dev);
        println!("[T4 点按] 注入序列 {seq:?}");
        assert_eq!(seq.len(), 2, "点按必须恰好一条落指 + 一条抬指:{seq:?}");
        assert_eq!((seq[0].0, seq[1].0), (ACTION_DOWN, ACTION_UP));
        assert_eq!(
            (seq[0].2, seq[0].3),
            (320, 2079),
            "落点必须是配置点 (0.25,0.75) 在 1280×2772 上的像素位置"
        );
        assert_eq!((seq[1].2, seq[1].3), (320, 2079), "抬起落在同一处");
        assert_eq!(seq[0].1, seq[1].1, "同一根手指落下与抬起");
        assert_clean(&h.stop(), "T4 点按");
    }

    /// **T4-③ 抬起**:按住型键位的**抬起边沿**必须在 Raw 计数流中逐条送达。
    ///
    /// 含阴性对照:只按不抬时设备端**必须**还按着 —— 否则下面那条"抬起之后
    /// 不许还按着"的断言就是空跑(§3.3)。
    #[test]
    fn t4_release_edge_ends_the_touch_path_exactly() {
        const TEST_KEY: u16 = 57;
        let screen = (1280u32, 2772u32);
        let mut p = Profile::default();
        p.binds.push(crate::keymap::KeyBind {
            key: TEST_KEY,
            action: crate::keymap::Action::Hold {
                x: 0.5,
                y: 0.5,
                radius: 0.02,
            },
            fps_only: false,
            tail_delay_ms: 0,
        });

        // 阴性对照:按住不松手,Raw 计数在流 ⇒ 设备端只有落指、仍按着
        {
            let h = Harness::start(p.clone(), screen);
            h.enable(true);
            h.settle(60);
            h.feed_button(TEST_KEY, true);
            h.settle(80);
            for _ in 0..10 {
                h.feed_motion(12.0, 0.0);
                h.settle(6);
            }
            let dev = h.device();
            assert_eq!(touch_seq(&dev).len(), 1, "只按不抬:设备端只有一条落指");
            assert!(
                !dev.leaks().is_empty(),
                "只按不抬:设备端必须仍按着 {:?}",
                dev.leaks()
            );
            h.feed_button(TEST_KEY, false);
            h.settle(150);
            assert_clean(&h.stop(), "T4 抬起(对照)");
        }

        // 阳性:松手 ⇒ 抬起边沿送到设备,序列 = [DOWN, UP],无残留
        let h = Harness::start(p, screen);
        h.enable(true);
        h.settle(60);
        h.feed_button(TEST_KEY, true);
        h.settle(80);
        for _ in 0..10 {
            h.feed_motion(12.0, 0.0);
            h.settle(6);
        }
        h.feed_button(TEST_KEY, false);
        h.settle(150);
        let dev = h.device();
        let seq = touch_seq(&dev);
        println!("[T4 抬起] 注入序列 {seq:?}");
        assert_eq!(seq.len(), 2, "按下 + 抬起恰好两条:{seq:?}");
        assert_eq!((seq[0].0, seq[1].0), (ACTION_DOWN, ACTION_UP));
        assert_eq!(
            (seq[1].2, seq[1].3),
            (640, 1386),
            "抬起必须落在原落点上(0.5,0.5) ⇒ 640,1386"
        );
        assert!(dev.leaks().is_empty(), "抬起之后设备端不许还按着");
        assert_clean(&h.stop(), "T4 抬起");
    }

    /// **T7 降级演练**:Raw 通道不可用(注册失败/被拦截)⇒ 位移走回退路径,
    /// 按键与滚轮的**边沿一条不丢**。
    ///
    /// 演练形状:进 FPS(位移这条由"回退路径"送进来,即 `CaptureEvent::Motion`),
    /// 位移持续流动的同时按下/松开一个普通键 4 次、再各转一齿滚轮上/下。
    /// 断言落在**设备端注入序列**:位移足额送达、每个边沿都成对且落在配置点上、
    /// 收尾无残留。
    ///
    /// "提示"这一半断言在**探针层**(降级签名 = `wmin` 不涨而位移在流,与界面
    /// 三态行 `raw_status_line` 同源);界面上的三态文案本身属 B2(R11/T1,
    /// 已变异验证),本测试不重复它。
    #[test]
    fn t7_raw_unavailable_drill_keeps_key_and_wheel_edges() {
        const KEY: u16 = 57; // KEY_SPACE
        const WHEEL_UP: u16 = 277; // BTN_WHEEL_UP
        const WHEEL_DOWN: u16 = 278; // BTN_WHEEL_DOWN
        let screen = (1280u32, 2772u32);
        let mut p = fps_profile(screen);
        // 关掉 FPS 滚轮缩放:本演练要的是**滚轮键码的边沿本身**能不能送到设备,
        // 而不是缩放手势(那条另有一套)。
        p.aim.wheel_zoom = false;
        p.aim.recenter = crate::keymap::RecenterMode::Never;
        let tap = |x: f32, y: f32| crate::keymap::Action::Tap {
            x,
            y,
            duration_ms: 40,
            radius: 0.02,
            repeat_interval_ms: 0,
        };
        for (key, action) in [
            (KEY, tap(0.25, 0.5)),
            (WHEEL_UP, tap(0.5, 0.25)),
            (WHEEL_DOWN, tap(0.5, 0.75)),
        ] {
            p.binds.push(crate::keymap::KeyBind {
                key,
                action,
                fps_only: false,
                tail_delay_ms: 0,
            });
        }

        let h = Harness::start(p, screen);
        h.tap(67); // F9 进 FPS:位移这条走"回退路径"
        h.settle(60);
        let presses = 4;
        for i in 0..40 {
            h.feed_motion(4.0, 0.0);
            h.settle(6);
            if i % 10 == 3 {
                h.feed_button(KEY, true);
                h.settle(60);
                h.feed_button(KEY, false);
            }
        }
        // 滚轮:上、下各一齿
        for wheel in [WHEEL_UP, WHEEL_DOWN] {
            h.feed_button(wheel, true);
            h.settle(40);
            h.feed_button(wheel, false);
            h.settle(140);
        }
        h.settle(200);
        let dev = h.stop();
        let seq = touch_seq(&dev);
        println!(
            "[T7 降级演练] 注入序列 {} 条:{:?}",
            seq.len(),
            &seq[..seq.len().min(8)]
        );

        // ① 回退路径的位移照样送达(不是"降级就没了")
        let st = path_stats(&dev, |pid| pid == 3000 || pid == 3001);
        assert!(
            st.path > 0.0 && st.moves >= 2,
            "回退路径的位移必须照常送达设备:path={:.0} moves={}",
            st.path,
            st.moves
        );
        // ② 按键边沿:4 次按下 ⇒ 恰好 4 对,且落在配置点 (0.25,0.5) = (320,1386)
        let key_pid = crate::engine::BIND_PID_BASE; // binds[0] = KEY
        assert_eq!(
            edges_of(&seq, key_pid),
            (presses, presses),
            "按键边沿不许丢、不许重复(pid={key_pid}):{seq:?}"
        );
        let key_pts: Vec<(u32, u32)> = seq
            .iter()
            .filter(|t| t.1 == key_pid)
            .map(|t| (t.2, t.3))
            .collect();
        assert!(
            key_pts.iter().all(|pt| *pt == (320, 1386)),
            "每一次落点都必须是配置点:实际 {key_pts:?}"
        );
        // ③ 滚轮边沿:上/下各一齿 ⇒ 各恰好一对,落点各自不同
        for (idx, want) in [(1usize, (640u32, 693u32)), (2, (640, 2079))] {
            let pid = crate::engine::BIND_PID_BASE + idx as u64;
            assert_eq!(
                edges_of(&seq, pid),
                (1, 1),
                "滚轮齿的边沿不许丢(pid={pid},期望落点 {want:?}):{seq:?}"
            );
            let pts: Vec<(u32, u32)> = seq
                .iter()
                .filter(|t| t.1 == pid)
                .map(|t| (t.2, t.3))
                .collect();
            assert!(
                pts.iter().all(|pt| *pt == want),
                "滚轮齿落点必须是配置点 {want:?}:实际 {pts:?}"
            );
        }
        assert_clean(&dev, "T7 降级演练");

        // ④ 提示:降级在探针层可观测(阳性),raw 在供位移时不许报降级(阴性)
        let mut degraded = CrossLedger::new();
        degraded.note_raw_tick(RAW_STATE_UNREGISTERED, 0); // 注册失败 ⇒ 状态 0
        degraded.note_raw_tick(RAW_STATE_UNREGISTERED, 0);
        for _ in 0..40 {
            degraded.note_motion(4.0, 0.0);
        }
        let rep = degraded.report();
        println!("[T7 降级提示] {}", rep.line());
        assert_eq!(
            rep.verdict(),
            CrossVerdict::RawSilentWhileMotionFlows,
            "有位移却一条 WM_INPUT 都没有 = 降级签名"
        );
        let mut healthy = CrossLedger::new();
        healthy.note_raw_tick(RAW_STATE_PENDING, 0);
        healthy.note_raw_tick(RAW_STATE_RELATIVE, 40);
        for _ in 0..40 {
            healthy.note_motion(4.0, 0.0);
        }
        println!("[T7 正常通道对照] {}", healthy.report().line());
        assert_ne!(
            healthy.report().verdict(),
            CrossVerdict::RawSilentWhileMotionFlows,
            "raw 在供位移时不许报降级(否则探针是恒真告警)"
        );
    }

    // ==================== T3 / T5(W3/B6,2026-10-17) ====================

    /// fixture 的绝对路径(`CARGO_MANIFEST_DIR` ⇒ 与测试进程 cwd 无关)。
    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/sim_fixtures")
            .join(name)
    }

    /// 逐行读 fixture 的 jsonl —— **独立于 `trace::read` 的第二读者**。
    fn fixture_lines(name: &str) -> Vec<serde_json::Value> {
        std::fs::read_to_string(fixture(name))
            .expect("读 fixture 失败")
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("fixture 行不是合法 json"))
            .collect()
    }

    /// 折叠**相邻同点**(轮盘保活每 100ms 重发同一落点)。
    fn collapse_keepalive(v: &[(u8, u64, u32, u32)]) -> Vec<(u8, u64, u32, u32)> {
        let mut out: Vec<(u8, u64, u32, u32)> = Vec::new();
        for &e in v {
            if out.last() != Some(&e) {
                out.push(e);
            }
        }
        out
    }

    /// 从录制里**复原**的轮盘参数(推导过程见 T5 输出侧测试的文档注释)。
    fn recorded_wheel_profile() -> Profile {
        let (w, h) = (2772.0f32, 1280.0f32);
        let mut p = Profile::default();
        p.wheels[0].cx = 386.0 / w;
        p.wheels[0].cy = 990.0 / h;
        // 半径按屏宽换算(见 `Wheel::push_px` / `DEFAULT_WHEEL_RADIUS`)
        p.wheels[0].radius = 153.0 / w;
        p.wheels[0].mode = crate::keymap::WheelMode::Sensitive;
        p
    }

    /// **T3 真机 Raw 原文 golden test**(§3.8;验收 = 原文回放解析逐位一致)。
    ///
    /// 出处(协议「B4 会前登记」第 4 条 ①;bench 块 `r6-a-rawon-accon`,R6 ④a):
    /// 源切片 `D:\GitHub\scrcpy-pad-bench\results\12-motion-injection\blocks\r6-a-rawon-accon\trace-slice.jsonl`
    /// = md5 `257c1fa4724abd138f859bf760e00180` / 506,284 B。本 fixture 是其中
    /// **全部 162 条 `k="st"` 行原文**(逐行可在源切片原文命中,162/162),观测窗
    /// t=684879→717409(32,530 ms),md5 `b38b0e0794b3d0b1254f2d15239a7cbb` / 162 行。
    /// 采样:200ms 一条心跳,`raw` 恒 2(RELATIVE),`wmin` 22910→26750(Δ3840,150 个不同值)。
    ///
    /// 为什么是 golden:这 162 行是"真机 Raw 通道活着"的唯一原文证据(第 ④ 件逐条
    /// `WM_INPUT` 的 `usFlags/lLastX/lLastY/usButtonFlags` 生产零落盘,另开 block)。
    /// 断言双口径:① 每行解析→重建**逐字节一致**(字段集/整数精度/顺序都不许变);
    /// ② `ledger_from_trace` 的计数与最大值必须与原文一致。
    #[test]
    fn t3_real_raw_state_lines_are_frozen_and_parse_bit_exactly() {
        let path = fixture("t3_raw_state_lines.jsonl");
        let text = std::fs::read_to_string(&path).expect("读不了 T3 fixture");
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 162, "T3 fixture 必须恰好 162 行 st 原文");

        let mut t_prev = 0u64;
        let mut wmins = std::collections::BTreeSet::new();
        for (i, line) in lines.iter().enumerate() {
            let v: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("第 {} 行不是合法 json: {e}", i + 1));
            let obj = v.as_object().expect("每行必须是对象");
            let keys: std::collections::BTreeSet<&str> = obj.keys().map(|k| k.as_str()).collect();
            assert_eq!(
                keys,
                ["k", "now", "raw", "streak", "t", "wmin"]
                    .into_iter()
                    .collect(),
                "第 {} 行的字段集与真机原文不符(多了/少了字段)",
                i + 1
            );
            assert_eq!(v["k"], "st", "第 {} 行不是 st 心跳", i + 1);
            let t = v["t"].as_u64().expect("t");
            let raw = v["raw"].as_u64().expect("raw");
            let wmin = v["wmin"].as_u64().expect("wmin");
            let streak = v["streak"].as_u64().expect("streak");
            let now = v["now"].as_u64().expect("now");
            assert_eq!(raw, 2, "第 {} 行 raw 必须是 2(RELATIVE)", i + 1);
            assert!(t > t_prev, "第 {} 行 t 必须严格递增:{t_prev} → {t}", i + 1);
            assert!(now <= t, "第 {} 行 now 不许晚于 t", i + 1);
            t_prev = t;
            wmins.insert(wmin);
            // 逐字节重建:解析没有窄化/四舍五入/改字段序(逐位一致的落点)
            let rebuilt = format!(
                r#"{{"t":{t},"k":"st","raw":{raw},"wmin":{wmin},"streak":{streak},"now":{now}}}"#
            );
            assert_eq!(
                rebuilt,
                *line,
                "第 {} 行原文解析→重建不是逐字节一致(字段被改写/窄化)",
                i + 1
            );
        }
        assert_eq!(wmins.len(), 150, "观测窗内不同 wmin 值的个数");
        assert_eq!(*wmins.iter().next().unwrap(), 22910, "wmin 起点");
        assert_eq!(*wmins.iter().next_back().unwrap(), 26750, "wmin 终点");

        let r = ledger_from_trace(&path).expect("读 T3 fixture");
        println!(
            "[T3] 心跳 {} 条 raw={} wmin={} 位移 {} 条 t 684879→{}",
            r.raw_ticks, r.raw_state, r.raw_events, r.motion_events, t_prev
        );
        assert_eq!(r.raw_ticks, 162, "st 心跳一条都不许丢");
        assert_eq!(r.raw_state, RAW_STATE_RELATIVE, "末态 raw 必须是 2");
        assert_eq!(r.raw_events, 26750, "wmin 取观测窗内最大值");
        assert_eq!(r.motion_events, 0, "切片全是 st 行,没有 in/motion");
        assert_eq!(
            r.raw_events - 22910,
            3840,
            "Δwmin 必须与 B4 登记的 3840 一致"
        );
        assert_eq!(t_prev - 684879, 32530, "观测窗长度(ms)");

        // 阴性对照(§3.3):把**所有取到 26750 的行**改小 1 ⇒ 探针必须报出不同的
        // 最大值,证明 26750 这条断言对 1 个计数单位敏感,不是恒真。
        let mut mutated: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
        let mut touched = 0usize;
        for line in mutated.iter_mut() {
            let v: serde_json::Value = serde_json::from_str(line).expect("行");
            if v["wmin"].as_u64() == Some(26750) {
                *line = format!(
                    r#"{{"t":{},"k":"st","raw":{},"wmin":{},"streak":{},"now":{}}}"#,
                    v["t"], v["raw"], 26749, v["streak"], v["now"]
                );
                touched += 1;
            }
        }
        assert!(touched > 0, "对照必须真的改到行");
        let t = write_temp_trace("t3-wmin-minus-one", &mutated.join("\n"));
        let r2 = ledger_from_trace(&t.0).expect("读阴性对照");
        assert_eq!(
            r2.raw_events, 26749,
            "改小 1 后探针必须跟着变(否则断言恒真)"
        );
        assert_ne!(r2.raw_events, r.raw_events);
    }

    /// **T5 输入侧**:回放把录制里的每一条输入**逐条、原值**喂进引擎。
    ///
    /// 出处:`D:\GitHub\scrcpy-pad-bench\results\12-motion-injection\blocks\realplay-06-nofps\trace-slice.jsonl`
    /// = md5 `89df6061ec5be41456c35fe89110a745` / 818,057 B。本 fixture 是其中
    /// t=3311810→3317634(5,824 ms)的 **281 行原文**(逐行命中 281/281),屏幕
    /// 2772×1280、活动方案「🔥正常」,md5 `941fff129033de3a92d723090acd5c4b`。
    /// 构成:`in/motion` 99 + `in/btn` 37 + `out/touch` 60 + `aim` 57 + `st` 28。
    ///
    /// 两条断言:
    /// ① **能力边界**(协议「B4 会前登记」第 5 条):`trace::read` 只交出 136 条输入 +
    ///    60 条 `out`,28 条 `st` 与 57 条 `aim` 被丢弃 ⇒ 回放器看不到它们,本文件
    ///    **不能**用来测 1ms 合并率;
    /// ② **逐条喂入**:`fed_probe`(仅 `cfg(test)`)记录到 136 条,与独立 JSON 解析
    ///    逐字段相等、顺序不变 —— 删掉/改值 `feed_*` 那一行必红。
    #[test]
    fn t5_replay_feeds_every_recorded_input_verbatim() {
        let path = fixture("t5_realplay06_nofps_sample.jsonl");
        let lines = fixture_lines("t5_realplay06_nofps_sample.jsonl");
        assert_eq!(lines.len(), 281, "T5 fixture 行数");
        let want: Vec<fed_probe::Fed> = lines
            .iter()
            .filter_map(|v| match (v["k"].as_str(), v["e"].as_str()) {
                (Some("in"), Some("btn")) => Some(fed_probe::Fed::Button {
                    code: v["code"].as_u64().expect("code") as u16,
                    down: v["down"].as_bool().expect("down"),
                }),
                (Some("in"), Some("motion")) => Some(fed_probe::Fed::Motion {
                    dx: v["dx"].as_f64().expect("dx") as f32,
                    dy: v["dy"].as_f64().expect("dy") as f32,
                }),
                _ => None,
            })
            .collect();
        assert_eq!(want.len(), 136, "录制输入条数");
        assert_eq!(
            want.iter()
                .filter(|e| matches!(e, fed_probe::Fed::Button { .. }))
                .count(),
            37
        );
        assert_eq!(
            want.iter()
                .filter(|e| matches!(e, fed_probe::Fed::Motion { .. }))
                .count(),
            99
        );

        let recs = trace::read(&path).expect("读 T5 fixture");
        assert_eq!(
            recs.len(),
            196,
            "trace::read 只留 136 输入 + 60 out;28 条 st / 57 条 aim 被丢弃"
        );
        let mut idx = 0usize;
        for r in &recs {
            match r {
                trace::Rec::Out(_) => continue,
                trace::Rec::Button { code, down, .. } => {
                    assert_eq!(
                        want[idx],
                        fed_probe::Fed::Button {
                            code: *code,
                            down: *down
                        },
                        "第 {idx} 条输入在 trace::read 里被改写"
                    );
                }
                trace::Rec::Motion { dx, dy, .. } => {
                    assert_eq!(
                        want[idx],
                        fed_probe::Fed::Motion { dx: *dx, dy: *dy },
                        "第 {idx} 条输入在 trace::read 里被改写"
                    );
                }
            }
            idx += 1;
        }
        assert_eq!(idx, 136, "trace::read 交出的输入条数");

        // 真回放:探针必须看到同样的 136 条(证明 replay 没有跳喂/改值)
        let _ = fed_probe::take(); // 清掉本线程之前的残留
        let (dev, _) = replay(&path, (2772, 1280), recorded_wheel_profile());
        let fed = fed_probe::take();
        assert_eq!(fed.len(), want.len(), "replay 喂入条数不对(有输入被跳过)");
        for (i, (a, b)) in fed.iter().zip(want.iter()).enumerate() {
            assert_eq!(a, b, "replay 第 {i} 条喂入值与录制不一致");
        }
        assert_clean(&dev, "T5 回放输入侧");
        assert_eq!(dev.peak_pointers, 1, "单轮盘只该占一根手指");
    }

    /// **T5 输出侧:逐点断言**。回放的设备触点序列与录制 `out/touch` 逐点一致
    /// —— **在折叠"保活重复点"之后**(严格度见下)。
    ///
    /// 录制 60 条 = 44 个状态迁移 + 16 条保活重复(`WHEEL_KEEPALIVE` 100ms 节拍
    /// 重发同一落点)。`sim::replay` 不重放真实时间间隔 ⇒ 保活节拍不会复现;
    /// 折叠相邻同点后 **44/44 逐点相等**,含 9 条 DOWN、9 条 UP 与全部方向迁移。
    ///
    /// 轮盘参数是从录制里**复原**的(不是猜的):
    /// * 圆心 = 所有 DOWN/UP 的落点 (386,990);
    /// * 半径 = 圆心到四向落点的距离 153px(对角线 108px = round(153/√2) 亦吻合);
    /// * 模式 = Sensitive:第 5 段 D→S→A 得 (278,1098)=S+A(后按覆盖),
    ///   第 7 段 A→S→D 得 (494,1098)=S+D ⇒ 是"同轴后按覆盖",不是 Classic 的抵消。
    ///
    /// 阴性对照:把模式改回 Classic ⇒ 同一比较必须判"不一致"(否则比较恒真)。
    #[test]
    fn t5_replayed_touch_points_match_the_recording_after_keepalive_collapse() {
        let path = fixture("t5_realplay06_nofps_sample.jsonl");
        let screen = (2772u32, 1280u32);
        let (dev, recs) = replay(&path, screen, recorded_wheel_profile());
        let recorded: Vec<(u8, u64, u32, u32)> = recs
            .iter()
            .filter_map(|r| match r {
                trace::Rec::Out(trace::OutRec::Touch {
                    action, pid, x, y, ..
                }) => Some((*action, *pid, *x, *y)),
                _ => None,
            })
            .collect();
        let replayed = touch_seq(&dev);
        let rd = collapse_keepalive(&recorded);
        let sd = collapse_keepalive(&replayed);
        println!(
            "[T5] 录制 {} 条→折叠 {};回放 {} 条→折叠 {}",
            recorded.len(),
            rd.len(),
            replayed.len(),
            sd.len()
        );

        assert_eq!(recorded.len(), 60, "录制触点条数");
        assert_eq!(rd.len(), 44, "录制折叠保活后 = 44 个状态迁移");
        assert_eq!(recorded.len() - rd.len(), 16, "16 条保活重复(100ms 节拍)");
        assert_eq!(sd.len(), rd.len(), "回放的状态迁移条数与录制不同");
        for (i, (a, b)) in rd.iter().zip(sd.iter()).enumerate() {
            assert_eq!(a, b, "第 {i} 个状态迁移与录制不一致(去重后)");
        }
        // 边沿单独再钉一遍(消息更精确,也是"有始有终"的验收)
        assert_eq!(edges_of(&rd, 2000), (9, 9), "录制:9 次落指/抬指");
        assert_eq!(edges_of(&sd, 2000), (9, 9), "回放:9 次落指/抬指");
        assert_eq!(sd.first(), Some(&(ACTION_DOWN, 2000, 386, 990)));
        assert_eq!(sd.last(), Some(&(ACTION_UP, 2000, 386, 990)));
        assert_clean(&dev, "T5 逐点");

        // 阴性对照(§3.3):Classic 在第 5 段会多出一个瞬时纯 S 态 (386,1143) ⇒ 必不相等
        let mut wrong = recorded_wheel_profile();
        wrong.wheels[0].mode = crate::keymap::WheelMode::Classic;
        let (dev_w, _) = replay(&path, screen, wrong);
        let wd = collapse_keepalive(&touch_seq(&dev_w));
        assert_ne!(wd, rd, "错误的轮盘模式也必须逐点一致 ⇒ 这个比较是恒真的");
        println!("[T5 阴性对照] Classic 模式折叠 {} 条,与录制不同", wd.len());
    }
}
