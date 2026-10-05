//! Windows 定时器精度提升。
//!
//! 背景(2026-10-05 实测,`scrcpy-pad-bench/tools/sleepbench`):
//! 进程未调用 `timeBeginPeriod` 时,Windows 会把 WaitOnAddress 系等待
//! (Rust 的 `mpsc::recv_timeout` / `thread::park_timeout`)的相对超时向上取整到
//! **~10.2ms 网格**——`recv_timeout(4ms)` 实测 10.245ms,`recv_timeout(16ms)`
//! 实测 20.479ms。引擎主循环靠 `recv_timeout` 等待"下一个事件或计划动作":
//! 事件到达会立刻唤醒不受影响,但 `scheduled` 计划动作(点按定时抬起、Hold 连发、
//! Swipe 步进、宏每一步、组合键 leader 判定)的派遣误差因此可达 ±6~10ms,
//! 空闲档节拍也从 4ms 掉到 ~10ms(heartbeat 实测 ~97 iters/s,与 10.245ms 吻合)。
//!
//! 调用 `timeBeginPeriod(1)` 后同一实测:4ms 档回到 4.253ms、16ms 档 16.457ms,
//! 误差约 ±0.25ms。该设置只在进程内生效(Windows 10 2004+ 不再影响其它进程),
//! 进程退出自动清除;这里仍提供 `restore()` 供正常退出路径显式配对调用。
//!
//! Linux 无需处理(单调时钟 + futex 超时本身足够精确),本模块空实现。

#[cfg(windows)]
mod imp {
    #[link(name = "winmm")]
    unsafe extern "system" {
        fn timeBeginPeriod(uperiod: u32) -> u32;
        fn timeEndPeriod(uperiod: u32) -> u32;
    }

    pub fn raise() {
        unsafe {
            let _ = timeBeginPeriod(1);
        }
    }

    pub fn restore() {
        unsafe {
            let _ = timeEndPeriod(1);
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn raise() {}
    pub fn restore() {}
}

pub use imp::{raise, restore};
