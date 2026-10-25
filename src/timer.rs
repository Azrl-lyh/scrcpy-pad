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
//! 失败必须**可见**(2026-10-22):以前 `timeBeginPeriod` 的返回值被 `let _ =` 吞掉,
//! 于是"精度没提上去"这件事在日志里完全看不出来,用户只会觉得"点了没反应/慢半拍",
//! 排查者也以为定时器已经调好了。与 `src/priority.rs` 同一口径
//! (那边 Linux 侧 2026-10-21 先改、Windows 侧 2026-10-22 对齐):失败落 WARN,
//! 并写明后果与常见原因。
//!
//! Linux 无需处理(单调时钟 + futex 超时本身足够精确),本模块空实现。

#[cfg(windows)]
mod imp {
    #[link(name = "winmm")]
    unsafe extern "system" {
        fn timeBeginPeriod(uperiod: u32) -> u32;
        fn timeEndPeriod(uperiod: u32) -> u32;
    }

    /// `MMRESULT` 的"成功"值(`TIMERR_NOERROR`)。
    const TIMERR_NOERROR: u32 = 0;

    pub fn raise() {
        // 失败必须**可见**,与 `src/priority.rs` 同一口径(2026-10-22)。
        // 后果不是"略慢一点":未生效时 `recv_timeout(4ms)` 实测 10.245ms、
        // 16ms 档 20.479ms,计划动作派遣误差可达 ±6~10ms,空闲档 4ms 档掉到 ~10ms。
        let r = unsafe { timeBeginPeriod(1) };
        if r != TIMERR_NOERROR {
            crate::diag_warn!(
                "timer",
                "提升定时器精度失败(timeBeginPeriod(1) 返回 {r}): \
                 计划动作(点按抬起/Hold 连发/Swipe 步进/宏每一步)的派遣误差可能达 ±6~10ms,\
                 空闲档节拍从 4ms 掉到 ~10ms。\
                 常见原因:系统音频/计时器服务异常、被组策略或安全软件限制、winmm 不可用;\
                 这类环境通常没有配置项可绕,只能如实记录。"
            );
        }
    }

    pub fn restore() {
        // 正常退出路径上的配对调用:失败无关紧要(进程退出本身就会清除该设置),
        // 所以只落 DEBUG,不用 WARN 去惊动用户。
        let r = unsafe { timeEndPeriod(1) };
        if r != TIMERR_NOERROR {
            crate::diag_debug!(
                "timer",
                "恢复定时器精度失败(timeEndPeriod(1) 返回 {r}),进程退出后系统会自行清除"
            );
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn raise() {}
    pub fn restore() {}
}

pub use imp::{raise, restore};
