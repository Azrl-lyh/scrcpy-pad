//! scrcpy 4.x 控制协议客户端。
//! 消息格式参照 scrcpy 源码 app/src/control_msg.c 的 sc_control_msg_serialize()。

use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};

pub const ACTION_DOWN: u8 = 0;
pub const ACTION_UP: u8 = 1;
pub const ACTION_MOVE: u8 = 2;

const TYPE_INJECT_KEYCODE: u8 = 0;
const TYPE_INJECT_TOUCH: u8 = 2;

#[derive(Debug, Clone, Copy)]
pub enum ControlCmd {
    Touch { action: u8, pointer_id: u64, x: u32, y: u32 },
    Key { action: u8, keycode: u32 },
}

/// 低延迟注入通道:专用写线程 + 无锁命令队列
pub struct ControlClient {
    tx: Sender<ControlCmd>,
    connected: Arc<AtomicBool>,
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

        let connected = Arc::new(AtomicBool::new(true));
        let (tx, rx) = channel::<ControlCmd>();

        // 写线程:把控制消息序列化后写入 socket
        {
            let connected = connected.clone();
            let mut stream = stream.try_clone()?;
            std::thread::spawn(move || {
                let w = screen_w as u16;
                let h = screen_h as u16;
                while let Ok(cmd) = rx.recv() {
                    let buf = serialize(cmd, w, h);
                    if stream.write_all(&buf).is_err() {
                        connected.store(false, Ordering::Relaxed);
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
                        Ok(0) | Err(_) => {
                            connected.store(false, Ordering::Relaxed);
                            break;
                        }
                        Ok(_) => {}
                    }
                }
            });
        }

        Ok(Self { tx, connected })
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    pub fn send(&self, cmd: ControlCmd) {
        let _ = self.tx.send(cmd);
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
            buf[10..14].copy_from_slice(&x.to_be_bytes());
            buf[14..18].copy_from_slice(&y.to_be_bytes());
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
    }
}
