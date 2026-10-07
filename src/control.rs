//! scrcpy 4.x 控制协议客户端。
//! 消息格式参照 scrcpy 源码 app/src/control_msg.c 的 sc_control_msg_serialize()。

use crate::{diag_info, diag_warn};
use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

pub const ACTION_DOWN: u8 = 0;
pub const ACTION_UP: u8 = 1;
pub const ACTION_MOVE: u8 = 2;

const TYPE_INJECT_KEYCODE: u8 = 0;
const TYPE_INJECT_TOUCH: u8 = 2;
const TYPE_UHID_CREATE: u8 = 12;
const TYPE_UHID_INPUT: u8 = 13;
const TYPE_UHID_DESTROY: u8 = 14;

/// scrcpy reserves HID ids 3..=10 for gamepads; slot 0 is id 3.
pub const GAMEPAD_HID_ID: u16 = 3;

/// scrcpy's virtual Xbox-360-compatible gamepad report descriptor.
/// Layout: four 16-bit stick axes, two 16-bit triggers, 16 buttons, hat.
pub const GAMEPAD_REPORT_DESC: &[u8] = &[
    0x05, 0x01, 0x09, 0x05, 0xA1, 0x01, 0xA1, 0x00, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x33,
    0x09, 0x34, 0x15, 0x00, 0x27, 0xFF, 0xFF, 0x00, 0x00, 0x75, 0x10, 0x95, 0x04, 0x81, 0x02, 0x05,
    0x01, 0x09, 0x32, 0x09, 0x35, 0x15, 0x00, 0x26, 0xFF, 0x7F, 0x75, 0x10, 0x95, 0x02, 0x81, 0x02,
    0x05, 0x09, 0x19, 0x01, 0x29, 0x10, 0x15, 0x00, 0x25, 0x01, 0x95, 0x10, 0x75, 0x01, 0x81, 0x02,
    0x05, 0x01, 0x09, 0x39, 0x15, 0x01, 0x25, 0x08, 0x75, 0x04, 0x95, 0x01, 0x81, 0x42, 0xC0, 0xC0,
];

#[derive(Debug, Clone)]
pub enum ControlCmd {
    Touch {
        action: u8,
        pointer_id: u64,
        x: u32,
        y: u32,
    },
    Key {
        action: u8,
        keycode: u32,
    },
    UhidCreate {
        id: u16,
        vendor_id: u16,
        product_id: u16,
        name: String,
        report_desc: Vec<u8>,
    },
    UhidInput {
        id: u16,
        data: Vec<u8>,
    },
    UhidDestroy {
        id: u16,
    },
}

/// Move 类命令允许的最大排队深度(W1-3)。达到它说明设备端/转发通道已经堵了:
/// 位移是绝对坐标语义,"丢旧保新"不改变指针最终位置;而**重放**一长串过期位移
/// 才是真的坏事 —— 指针会先沿陈旧路径划过一遍再追上当前位置。
const MOVE_BACKLOG_LIMIT: usize = 256;

/// 一次 `write_all` 最多拼多少条命令(见写线程里的攒批说明)。
/// 64 足够覆盖"一次按下引发的 down+move+滑动插值"这种突发,又不至于让
/// 单次写入的字节数失控(触摸 32B、按键 12B 量级 → 一次 write 最多 ~2KB)。
const WRITE_BATCH_LIMIT: usize = 64;

/// 控制队列的实时快照(诊断面板用)
#[derive(Debug, Clone, Copy, Default)]
pub struct QueueStats {
    pub depth: usize,
    pub peak: usize,
    pub dropped: u64,
}

/// 控制命令队列(W1-3):替代原来的无界 `mpsc`。
///
/// 与 mpsc 的差别只有两点,都是"看得见 + 压得住":
/// 1. **深度可见**:发送方随时能读到当前/峰值深度与丢弃计数(诊断面板显示);
/// 2. **Move 类丢旧保新**:排队深度达到 `MOVE_BACKLOG_LIMIT` 时,新的
///    `ACTION_MOVE` 顶掉**同一触点最旧的那条** —— 每个来源(瞄准/轮盘/宏/
///    手柄)有自己的 `pointer_id`,互不干扰,顺序也不乱。
///
/// 其余命令(按下/抬起/按键/UHID)一条都不丢:它们是边沿语义,丢了就是
/// 卡键或丢操作 —— 宁可积压,不可失序。
struct CmdQueue {
    q: Mutex<VecDeque<ControlCmd>>,
    cv: Condvar,
    peak: AtomicUsize,
    dropped: AtomicU64,
    /// 已被写线程取走、但**还没写进 socket** 的命令条数(攒批的本地缓冲)。
    ///
    /// 攒批把最多 `WRITE_BATCH_LIMIT` 条命令从队列挪进了写线程自己的 `buf`,
    /// 那批命令还在途 —— 不算进来的话:①诊断面板的"深度"会在最该看的时候
    /// (socket 卡住、写线程堵在 `write_all` 上)少报一整个批次;②"丢旧保新"
    /// 的在途上限会从 256 放宽到 256+64,卡顿恢复后多写出几十条过期位移。
    /// 只由写线程改(取一条 +1、写完一批 -= n),`push` 在锁内读。
    inflight: AtomicUsize,
    /// 客户端销毁后置位:消费者"取空 + 已关闭"即退出(等价于 mpsc 的断开)
    closed: AtomicBool,
}

impl CmdQueue {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            q: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
            peak: AtomicUsize::new(0),
            dropped: AtomicU64::new(0),
            inflight: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
        })
    }

    fn push(&self, cmd: ControlCmd) {
        // 锁中毒不放弃:注入路径不能因为某个持锁线程 panic 就全线卡死
        let mut q = self.q.lock().unwrap_or_else(|e| e.into_inner());
        let mut dropped_now = None;
        let move_pid = match &cmd {
            ControlCmd::Touch {
                action: ACTION_MOVE,
                pointer_id,
                ..
            } => Some(*pointer_id),
            _ => None,
        };
        // 在途量 = 队列里 + 写线程手里还没落 socket 的那一批(见 `inflight`)
        let backlog = q.len() + self.inflight.load(Ordering::Relaxed);
        if let Some(pid) = move_pid.filter(|_| backlog >= MOVE_BACKLOG_LIMIT) {
            let oldest = q.iter().position(|c| {
                matches!(
                    c,
                    ControlCmd::Touch { action: ACTION_MOVE, pointer_id, .. } if *pointer_id == pid
                )
            });
            if let Some(i) = oldest {
                q.remove(i);
                dropped_now = Some(self.dropped.fetch_add(1, Ordering::Relaxed) + 1);
            }
        }
        q.push_back(cmd);
        // 峰值记"在途量"(队列 + 写线程手里的那批):面板上这两个数就是用户
        // 判断"堵在我们这一侧还是设备侧"的依据,不能少算一整个批次。
        self.peak.fetch_max(
            q.len() + self.inflight.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        drop(q);
        self.cv.notify_one();
        // 日志放在锁外:diag 的写盘不能拖住持锁的注入路径。
        // 首次与每 1000 条各记一次,免得持续积压时把日志刷爆。
        if let Some(n) = dropped_now.filter(|n| *n == 1 || n % 1000 == 0) {
            diag_warn!(
                "control",
                "控制队列积压(≥{MOVE_BACKLOG_LIMIT}):累计丢弃 {n} 条过期 Move(保新)——设备端或转发通道变慢"
            );
        }
    }

    /// 阻塞取一条;`None` = 队列已关闭且取空(客户端已销毁)。
    ///
    /// 取走即计入 `inflight`(它还没写进 socket),写完由 `written()` 冲销 ——
    /// 于是 `depth` 在"取走"这一步不下降(总数不变),只在真正写完后下降。
    fn pop(&self) -> Option<ControlCmd> {
        let mut q = self.q.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(c) = q.pop_front() {
                self.inflight.fetch_add(1, Ordering::Relaxed);
                return Some(c);
            }
            if self.closed.load(Ordering::Relaxed) {
                return None;
            }
            q = self.cv.wait(q).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// 非阻塞取一条(攒批用):拿不到就返回 None,绝不等。
    fn try_pop(&self) -> Option<ControlCmd> {
        let mut q = self.q.lock().unwrap_or_else(|e| e.into_inner());
        let c = q.pop_front();
        if c.is_some() {
            self.inflight.fetch_add(1, Ordering::Relaxed);
        }
        c
    }

    /// 写线程报账:这一批 `n` 条已经离开本地(写完,或写失败被丢弃),
    /// 从在途量里冲销。
    fn written(&self, n: usize) {
        self.inflight.fetch_sub(n, Ordering::Relaxed);
    }

    /// 客户端销毁:唤醒写线程,让它取空后自行退出
    fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        self.cv.notify_all();
    }

    /// 诊断快照。`depth` **现算**:队列里的 + 写线程手里还没落 socket 的那批
    /// (见 `inflight`)—— 攒批之后"队列长度"不再是"还没发出去的条数",而面板上
    /// 那个数要回答的正是后者。取一次锁的代价可以忽略(与 `push` 同一把锁,
    /// 界面每帧至多问一次)。
    fn stats(&self) -> QueueStats {
        let queued = self.q.lock().unwrap_or_else(|e| e.into_inner()).len();
        QueueStats {
            depth: queued + self.inflight.load(Ordering::Relaxed),
            peak: self.peak.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
        }
    }
}

/// 低延迟注入通道:专用写线程 + 命令队列(深度可见、Move 类丢旧保新,W1-3)
pub struct ControlClient {
    queue: Arc<CmdQueue>,
    connected: Arc<AtomicBool>,
    /// 设备屏幕宽(逻辑像素),用于瞄准偏移的边界钳制
    pub screen_w: u32,
    /// 设备屏幕高
    pub screen_h: u32,
    /// 写线程**真正用来序列化**的宽高。
    ///
    /// 为什么要单独一份:`screen_w/h` 是给引擎算坐标用的普通字段,
    /// 而写线程在 `move` 走了自己的副本之后再也看不到对它的修改 ——
    /// 于是会出现"引擎按新尺寸算坐标、消息里却声明旧尺寸"的不一致。
    /// 当前用 control-only 模式(服务端走原始坐标、不校验尺寸)所以无害,
    /// 但一旦有人启用视频或 `--new-display`,服务端 `PositionMapper.map()`
    /// 会因尺寸不符把**每一条**触摸事件丢弃,且不报任何错。
    /// 用原子量让两者始终一致,避免以后踩这个坑。
    proto_w: Arc<AtomicU32>,
    proto_h: Arc<AtomicU32>,
}

impl ControlClient {
    /// 连接到已转发到本地端口的 scrcpy control socket。
    /// 必须读取 server 的 dummy 字节以确认设备侧 socket 真的被接受
    /// (adb forward 在本地总是先完成 TCP 握手,设备侧未就绪时随即 EOF)。
    pub fn connect(port: u16, screen_w: u32, screen_h: u32) -> Result<Self> {
        let mut stream = TcpStream::connect(("127.0.0.1", port))
            .context("连接本地转发端口失败(scrcpy-server 是否已就绪?)")?;
        stream.set_nodelay(true).ok();
        stream
            .set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .ok();

        let mut dummy = [0u8; 1];
        match stream.read(&mut dummy) {
            Ok(1) => {} // 收到 dummy 字节,连接真实有效
            Ok(_) => anyhow::bail!("设备侧 socket 未就绪(EOF)"),
            Err(e) => anyhow::bail!("等待 dummy 字节失败: {e}"),
        }
        stream.set_read_timeout(None).ok();
        // 写入看门狗(2026-10-06):设备端 server 若卡住不读(系统繁忙/进程被冻),
        // TCP 发送缓冲填满后 `write_all` 会**无限**阻塞 —— 输入全堆在队列里,
        // 用户看到的是"延迟暴涨、按键无反应",而通道表面上还"连着",永远不会
        // 触发断线路径。给写操作设上限:超时即视为链路故障(与写失败同路),
        // 由界面层的自动重连在秒级内把通道拉起来。
        stream
            .set_write_timeout(Some(std::time::Duration::from_millis(2000)))
            .ok();

        let connected = Arc::new(AtomicBool::new(true));
        let proto_w = Arc::new(AtomicU32::new(screen_w));
        let proto_h = Arc::new(AtomicU32::new(screen_h));
        let queue = CmdQueue::new();

        // 写线程:把控制消息序列化后写入 socket
        {
            let connected = connected.clone();
            let proto_w = proto_w.clone();
            let proto_h = proto_h.clone();
            let queue = queue.clone();
            let mut stream = stream.try_clone()?;
            std::thread::spawn(move || {
                // 输入链最后一跳:只在有待发指令时运行(其余时间阻塞在队列上),
                // 提到最高优先级,保证写完 socket 不被满载的游戏线程挤后。
                crate::priority::boost(crate::priority::Class::Highest);
                while let Some(cmd) = queue.pop() {
                    // 攒批(2026-10-06 延迟修复):一次唤醒把队列里**已经排好**的
                    // 命令合成一次 `write_all`。一条一写在 Nagle 已关的连接上等于
                    // 一个 TCP 报文 + 一次系统调用;投屏开着时这些报文还要和视频流
                    // 一起挤 adbd 的转发,报文数就是排队延迟。攒批不改变顺序
                    // (FIFO 原样拼接),也不增加延迟:队列里没有第二条时行为与
                    // 旧实现逐字相同。
                    //
                    // 宽高**每条各读一次**(旧实现就是逐条读的):批里可能夹着
                    // `set_screen`(转屏/截图校正),用批次开头那一份会把之后的
                    // 命令按旧坐标空间换算 —— 服务端启用视频/`--new-display` 时
                    // `PositionMapper` 会因此把它们缩放错位。
                    let mut buf = serialize(
                        cmd,
                        proto_w.load(Ordering::Relaxed) as u16,
                        proto_h.load(Ordering::Relaxed) as u16,
                    );
                    let mut n = 1usize;
                    while n < WRITE_BATCH_LIMIT {
                        let Some(more) = queue.try_pop() else { break };
                        buf.extend_from_slice(&serialize(
                            more,
                            proto_w.load(Ordering::Relaxed) as u16,
                            proto_h.load(Ordering::Relaxed) as u16,
                        ));
                        n += 1;
                    }
                    let res = stream.write_all(&buf);
                    // 这一批无论结果如何都已离开本地(写成功或随失败丢弃),
                    // 先冲销在途量再处理错误 —— 否则失败路径会把在途量漏掉。
                    queue.written(n);
                    if let Err(e) = res {
                        connected.store(false, Ordering::Relaxed);
                        // 写入失败是最容易被忽略的故障:命令全部静默丢弃,
                        // 用户只看到"按键没反应"。把 errno 留下来。
                        diag_warn!(
                            "control",
                            "控制通道写入失败: {e} (errno {:?}) —— 后续注入命令将全部丢弃",
                            e.raw_os_error()
                        );
                        break;
                    }
                }
            });
        }

        // 读线程:丢弃服务端下行消息(剪贴板等),避免接收缓冲阻塞
        {
            let connected = connected.clone();
            let mut stream = stream;
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) => {
                            connected.store(false, Ordering::Relaxed);
                            diag_warn!("control", "控制通道对端关闭(EOF),连接标记为断开");
                            break;
                        }
                        Err(e) => {
                            connected.store(false, Ordering::Relaxed);
                            diag_warn!(
                                "control",
                                "控制通道读取失败: {e} (errno {:?}),连接标记为断开",
                                e.raw_os_error()
                            );
                            break;
                        }
                        Ok(_) => {}
                    }
                }
            });
        }

        diag_info!(
            "control",
            "控制通道已建立: 端口 {port},坐标空间 {screen_w}x{screen_h}"
        );
        Ok(Self {
            queue,
            connected,
            screen_w,
            screen_h,
            proto_w,
            proto_h,
        })
    }

    /// 队列深度快照(W1-3,诊断面板显示用)
    pub fn queue_stats(&self) -> QueueStats {
        self.queue.stats()
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// 更新触摸坐标空间。
    ///
    /// 必须走这个方法而不是直接写 `screen_w/h`:后者改不到写线程手里的值,
    /// 会造成"引擎按新尺寸算坐标、协议里声明旧尺寸"的不一致(见字段注释)。
    pub fn set_screen(&mut self, w: u32, h: u32) {
        self.screen_w = w;
        self.screen_h = h;
        self.proto_w.store(w, Ordering::Relaxed);
        self.proto_h.store(h, Ordering::Relaxed);
    }

    pub fn send(&self, cmd: ControlCmd) {
        self.queue.push(cmd);
    }

    pub fn touch_down(&self, pointer_id: u64, x: i32, y: i32) {
        self.send(ControlCmd::Touch {
            action: ACTION_DOWN,
            pointer_id,
            x: x.max(0) as u32,
            y: y.max(0) as u32,
        });
    }

    pub fn touch_move(&self, pointer_id: u64, x: i32, y: i32) {
        self.send(ControlCmd::Touch {
            action: ACTION_MOVE,
            pointer_id,
            x: x.max(0) as u32,
            y: y.max(0) as u32,
        });
    }

    pub fn touch_up(&self, pointer_id: u64, x: i32, y: i32) {
        self.send(ControlCmd::Touch {
            action: ACTION_UP,
            pointer_id,
            x: x.max(0) as u32,
            y: y.max(0) as u32,
        });
    }

    pub fn key(&self, down: bool, keycode: u32) {
        self.send(ControlCmd::Key {
            action: if down { ACTION_DOWN } else { ACTION_UP },
            keycode,
        });
    }

    pub fn uhid_create(
        &self,
        id: u16,
        vendor_id: u16,
        product_id: u16,
        name: impl Into<String>,
        report_desc: Vec<u8>,
    ) {
        self.send(ControlCmd::UhidCreate {
            id,
            vendor_id,
            product_id,
            name: name.into(),
            report_desc,
        });
    }

    pub fn uhid_input(&self, id: u16, data: Vec<u8>) {
        self.send(ControlCmd::UhidInput { id, data });
    }

    pub fn uhid_destroy(&self, id: u16) {
        self.send(ControlCmd::UhidDestroy { id });
    }
}

#[cfg(test)]
impl ControlClient {
    /// 测试用:不建 TCP、不建写线程,指令留在队列里,由 [`Self::take_cmds`] 取走。
    ///
    /// 引擎单测要断言的是"派发之后注入了什么"(`ControlCmd`,与写线程序列化的
    /// 是同一份数据),不必真的接一个 socket —— 同步、无线程、无时序抖动。
    pub(crate) fn for_test(screen_w: u32, screen_h: u32) -> Self {
        Self {
            queue: CmdQueue::new(),
            connected: Arc::new(AtomicBool::new(true)),
            screen_w,
            screen_h,
            proto_w: Arc::new(AtomicU32::new(screen_w)),
            proto_h: Arc::new(AtomicU32::new(screen_h)),
        }
    }

    /// 取走队列里当前积压的全部指令(测试用;`for_test` 没有消费者线程)。
    pub(crate) fn take_cmds(&self) -> Vec<ControlCmd> {
        let mut q = self.queue.q.lock().unwrap_or_else(|e| e.into_inner());
        let out: Vec<ControlCmd> = q.drain(..).collect();
        out
    }
}

impl Drop for ControlClient {
    fn drop(&mut self) {
        // 关队列 = 通知写线程退出。等价于旧实现里"所有 Sender 被丢弃"的断开,
        // 但它是显式的:断线重连会新建客户端,旧写线程必须能干净退出。
        self.queue.close();
    }
}

fn serialize(cmd: ControlCmd, w: u16, h: u16) -> Vec<u8> {
    match cmd {
        ControlCmd::Touch {
            action,
            pointer_id,
            x,
            y,
        } => {
            let mut buf = [0u8; 32];
            buf[0] = TYPE_INJECT_TOUCH;
            buf[1] = action;
            buf[2..10].copy_from_slice(&pointer_id.to_be_bytes());
            // 坐标是协议里的 u32;负值(例如配置允许的超屏坐标)按 0 处理,
            // 否则 i32 -> u32 的负数转换会变成一个巨大的值,反而更容易触发服务端异常。
            buf[10..14].copy_from_slice(&(x.max(0) as u32).to_be_bytes());
            buf[14..18].copy_from_slice(&(y.max(0) as u32).to_be_bytes());
            buf[18..20].copy_from_slice(&w.to_be_bytes());
            buf[20..22].copy_from_slice(&h.to_be_bytes());
            let pressure: u16 = if action == ACTION_UP { 0 } else { 0xFFFF };
            buf[22..24].copy_from_slice(&pressure.to_be_bytes());
            // action_button(24..28) 与 buttons(28..32) 对触摸恒为 0
            buf.to_vec()
        }
        ControlCmd::Key { action, keycode } => {
            let mut buf = [0u8; 14];
            buf[0] = TYPE_INJECT_KEYCODE;
            buf[1] = action;
            buf[2..6].copy_from_slice(&keycode.to_be_bytes());
            // repeat(6..10) 与 metastate(10..14) 为 0
            buf.to_vec()
        }
        ControlCmd::UhidCreate {
            id,
            vendor_id,
            product_id,
            name,
            report_desc,
        } => {
            let name_bytes = name.as_bytes();
            let name_len = name_bytes.len().min(127);
            let desc_len = report_desc.len().min(u16::MAX as usize);
            let mut buf = Vec::with_capacity(7 + 1 + name_len + 2 + desc_len);
            buf.push(TYPE_UHID_CREATE);
            buf.extend_from_slice(&id.to_be_bytes());
            buf.extend_from_slice(&vendor_id.to_be_bytes());
            buf.extend_from_slice(&product_id.to_be_bytes());
            buf.push(name_len as u8);
            buf.extend_from_slice(&name_bytes[..name_len]);
            buf.extend_from_slice(&(desc_len as u16).to_be_bytes());
            buf.extend_from_slice(&report_desc[..desc_len]);
            buf
        }
        ControlCmd::UhidInput { id, data } => {
            let size = data.len().min(u16::MAX as usize);
            let mut buf = Vec::with_capacity(5 + size);
            buf.push(TYPE_UHID_INPUT);
            buf.extend_from_slice(&id.to_be_bytes());
            buf.extend_from_slice(&(size as u16).to_be_bytes());
            buf.extend_from_slice(&data[..size]);
            buf
        }
        ControlCmd::UhidDestroy { id } => {
            let mut buf = Vec::with_capacity(3);
            buf.push(TYPE_UHID_DESTROY);
            buf.extend_from_slice(&id.to_be_bytes());
            buf
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 触控消息必须是 32 字节、字段偏移与字节序完全符合 scrcpy 的
    /// `sc_control_msg_serialize()` —— 这是注入能否被设备端接受的根本,
    /// 任何"看起来能跑但手机没反应"的问题最后都会回到这里。
    #[test]
    fn touch_message_layout_matches_scrcpy() {
        let buf = serialize(
            ControlCmd::Touch {
                action: ACTION_DOWN,
                pointer_id: 0x0102_0304_0506_0708,
                x: 0x1122_3344,
                y: 0x5566_7788,
            },
            0x0A0B,
            0x0C0D,
        );
        assert_eq!(buf.len(), 32, "触控消息固定 32 字节");
        assert_eq!(buf[0], TYPE_INJECT_TOUCH);
        assert_eq!(buf[1], ACTION_DOWN);
        assert_eq!(&buf[2..10], &0x0102_0304_0506_0708u64.to_be_bytes());
        assert_eq!(&buf[10..14], &0x1122_3344u32.to_be_bytes());
        assert_eq!(&buf[14..18], &0x5566_7788u32.to_be_bytes());
        assert_eq!(&buf[18..20], &0x0A0Bu16.to_be_bytes());
        assert_eq!(&buf[20..22], &0x0C0Du16.to_be_bytes());
        assert_eq!(&buf[22..24], &0xFFFFu16.to_be_bytes(), "按下时压力满值");
        assert_eq!(&buf[24..32], &[0u8; 8], "action_button 与 buttons 恒为 0");
    }

    /// 抬起时压力必须为 0;坐标本身是 u32,负数在 touch_* 包装里就已被夹到 0
    #[test]
    fn touch_message_clamps_negative_coords_and_release_pressure() {
        let buf = serialize(
            ControlCmd::Touch {
                action: ACTION_UP,
                pointer_id: 1,
                x: (-5i32).max(0) as u32,
                y: (-1i32).max(0) as u32,
            },
            100,
            200,
        );
        assert_eq!(&buf[22..24], &0u16.to_be_bytes(), "抬起时压力为 0");
        assert_eq!(&buf[10..14], &0u32.to_be_bytes());
        assert_eq!(&buf[14..18], &0u32.to_be_bytes());
    }

    /// 按键消息是 14 字节,keycode 为大端 u32,repeat/metastate 为 0
    #[test]
    fn key_message_layout_matches_scrcpy() {
        let buf = serialize(
            ControlCmd::Key {
                action: ACTION_DOWN,
                keycode: 4,
            },
            0,
            0,
        );
        assert_eq!(buf.len(), 14);
        assert_eq!(buf[0], TYPE_INJECT_KEYCODE);
        assert_eq!(buf[1], ACTION_DOWN);
        assert_eq!(&buf[2..6], &4u32.to_be_bytes());
        assert_eq!(&buf[6..14], &[0u8; 8]);
    }

    #[test]
    fn uhid_create_and_input_layout_matches_scrcpy() {
        let desc = vec![0xAA, 0xBB, 0xCC];
        let create = serialize(
            ControlCmd::UhidCreate {
                id: GAMEPAD_HID_ID,
                vendor_id: 0x045E,
                product_id: 0x028E,
                name: "Pad".into(),
                report_desc: desc.clone(),
            },
            0,
            0,
        );
        assert_eq!(create[0], TYPE_UHID_CREATE);
        assert_eq!(&create[1..3], &GAMEPAD_HID_ID.to_be_bytes());
        assert_eq!(&create[3..5], &0x045Eu16.to_be_bytes());
        assert_eq!(&create[5..7], &0x028Eu16.to_be_bytes());
        assert_eq!(create[7], 3);
        assert_eq!(&create[8..11], b"Pad");
        assert_eq!(&create[11..13], &(desc.len() as u16).to_be_bytes());
        assert_eq!(&create[13..], &desc);

        let input = serialize(
            ControlCmd::UhidInput {
                id: GAMEPAD_HID_ID,
                data: vec![0x01, 0x02, 0x03],
            },
            0,
            0,
        );
        assert_eq!(
            input,
            vec![TYPE_UHID_INPUT, 0x00, 0x03, 0x00, 0x03, 0x01, 0x02, 0x03]
        );
    }

    // ---- W1-3:控制队列的深度可见与 Move 类丢旧保新 ----

    fn mv(pid: u64, x: u32) -> ControlCmd {
        ControlCmd::Touch {
            action: ACTION_MOVE,
            pointer_id: pid,
            x,
            y: 0,
        }
    }

    fn key(k: u32) -> ControlCmd {
        ControlCmd::Key {
            action: ACTION_DOWN,
            keycode: k,
        }
    }

    /// 未达上限时一条不丢、顺序不变:队列的基础行为必须与旧的 mpsc 完全一致
    #[test]
    fn queue_preserves_fifo_order_below_the_limit() {
        let q = CmdQueue::new();
        q.push(mv(7, 1));
        q.push(key(4));
        match q.pop() {
            Some(ControlCmd::Touch { action, x, .. }) => {
                assert_eq!((action, x), (ACTION_MOVE, 1));
            }
            other => panic!("应为第一条 Move,实得 {other:?}"),
        }
        assert!(matches!(q.pop(), Some(ControlCmd::Key { .. })));
        assert_eq!(q.stats().dropped, 0);
    }

    /// 达到上限后:同触点最旧的一条被顶掉,深度净增 0;
    /// 另一个触点没有可顶掉的条目,不受影响
    #[test]
    fn move_backlog_drops_the_oldest_of_the_same_pointer_only() {
        let q = CmdQueue::new();
        for i in 0..MOVE_BACKLOG_LIMIT as u32 {
            q.push(mv(7, i));
        }
        q.push(mv(9, 1000));
        assert_eq!(q.stats().depth, MOVE_BACKLOG_LIMIT + 1);
        assert_eq!(q.stats().dropped, 0, "别的触点不该被牵连");

        q.push(mv(7, 999));
        let st = q.stats();
        assert_eq!(st.depth, MOVE_BACKLOG_LIMIT + 1, "顶掉一条又入队一条");
        assert_eq!(st.dropped, 1);
        assert!(st.peak > MOVE_BACKLOG_LIMIT, "峰值不能被后续丢弃盖掉");

        q.close();
        let mut entries: Vec<(u64, u32)> = Vec::new();
        while let Some(c) = q.pop() {
            if let ControlCmd::Touch { pointer_id, x, .. } = c {
                entries.push((pointer_id, x));
            }
        }
        assert!(!entries.contains(&(7, 0)), "被顶掉的必须是同触点最旧的一条");
        assert!(entries.contains(&(7, 1)), "其余同触点的条目必须保留");
        assert!(entries.contains(&(9, 1000)), "别的触点不受影响");
        assert!(entries.contains(&(7, 999)), "新来的那条必须在");
        assert_eq!(entries.len(), MOVE_BACKLOG_LIMIT + 1);
    }

    /// 攒批把命令先挪进写线程手里:`depth` 必须报"还没写进 socket 的条数"
    /// (队列 + 在途),否则 socket 卡住、写线程堵在 `write_all` 上时,面板会
    /// 少报一整个批次 —— 而那两个数正是用户判断"堵在我们这侧还是设备侧"的依据。
    #[test]
    fn queue_depth_counts_commands_already_handed_to_the_writer() {
        let q = CmdQueue::new();
        q.push(mv(7, 1));
        q.push(mv(7, 2));
        assert_eq!(q.stats().depth, 2);
        let first = q.pop().expect("队列非空");
        assert!(matches!(first, ControlCmd::Touch { .. }));
        assert_eq!(q.stats().depth, 2, "被写线程取走 ≠ 已经写出去");
        q.written(1);
        assert_eq!(q.stats().depth, 1, "写完一条才冲销一条");
        let second = q.pop().expect("队列还有一条");
        assert!(matches!(second, ControlCmd::Touch { .. }));
        assert_eq!(q.stats().depth, 1, "取走第二条:总数仍不变");
        q.written(1);
        assert_eq!(q.stats().depth, 0);

        // 在途量也要计入"丢旧保新"的上限:否则队列自己没到 256 就不顶掉,
        // 卡顿恢复后会多写出几十条过期位移。
        let q2 = CmdQueue::new();
        for i in 0..MOVE_BACKLOG_LIMIT as u32 {
            q2.push(mv(7, i));
        }
        let inflight_one = q2.pop().expect("队列非空"); // x=0 交给写线程,还没写
        assert_eq!(q2.stats().depth, MOVE_BACKLOG_LIMIT, "在途的那条照算");
        q2.push(mv(7, 7777));
        assert_eq!(
            q2.stats().dropped,
            1,
            "队列 255 + 在途 1 = 到上限,应当顶掉最旧一条"
        );
        q2.close();
        let mut xs: Vec<u32> = Vec::new();
        if let ControlCmd::Touch { x, .. } = inflight_one {
            xs.push(x);
        }
        while let Some(c) = q2.pop() {
            if let ControlCmd::Touch { x, .. } = c {
                xs.push(x);
            }
        }
        assert!(!xs.contains(&1), "被顶掉的应是队列里最旧的一条(x=1)");
        assert!(
            xs.contains(&0) && xs.contains(&7777),
            "在途与新来的都必须保留"
        );
    }

    /// 按下/抬起/按键是边沿语义:任何时候都不许丢,顶掉只发生在 Move 类身上
    #[test]
    fn edge_commands_are_never_dropped() {
        let q = CmdQueue::new();
        for i in 0..MOVE_BACKLOG_LIMIT as u32 {
            q.push(mv(7, i));
        }
        q.push(ControlCmd::Touch {
            action: ACTION_DOWN,
            pointer_id: 7,
            x: 42,
            y: 43,
        });
        q.push(key(4));
        q.push(mv(7, 5000)); // 顶掉最旧的一条 Move
        assert_eq!(q.stats().dropped, 1);

        q.close();
        let (mut has_down, mut has_key) = (false, false);
        while let Some(c) = q.pop() {
            match c {
                ControlCmd::Touch {
                    action: ACTION_DOWN,
                    x: 42,
                    ..
                } => has_down = true,
                ControlCmd::Key { keycode: 4, .. } => has_key = true,
                _ => {}
            }
        }
        assert!(has_down, "按下必须留在队列里");
        assert!(has_key, "按键必须留在队列里");
    }

    /// 关闭后:已入队的命令仍能取完,然后 pop 返回 None(写线程据此退出)
    #[test]
    fn close_drains_then_returns_none() {
        let q = CmdQueue::new();
        q.push(key(9));
        q.close();
        assert!(matches!(q.pop(), Some(ControlCmd::Key { keycode: 9, .. })));
        assert!(q.pop().is_none(), "取空 + 已关闭 = 写线程退出信号");
    }
}
