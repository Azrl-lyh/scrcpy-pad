//! 输入链路线程的调度优先级。
//!
//! 为什么需要:真实使用场景里,这台机器同时跑着**游戏本体(吃满多核)**、
//! scrcpy 的视频编解码、可能还有 OBS/浏览器。默认(NORMAL)优先级下,
//! 钩子回调、消费、引擎、控制写这四段"每次只做 µs 级工作、但必须及时被调度"
//! 的线程会和游戏线程平权抢 CPU —— 空闲机器上测不出来的排队延迟,
//! 满载时就会变成几十毫秒的按键抖动。把它们适度提高:
//!
//! - 钩子线程 / 控制写线程:`Highest` —— 它们 99.99% 时间阻塞在
//!   `GetMessage` / `rx.recv()`,只在输入到达或有待发指令时短暂运行,
//!   提高优先级几乎不占用其它线程的 CPU,却能最快抢到核心。
//! - 消费 / 引擎线程:`AboveNormal` —— 它们计算量稍大(位移、对账、
//!   签名),不抢到 `Highest`,避免反过来饿死游戏画面。
//!
//! 全部是"尽力而为":失败(平台不支持/权限不足)静默忽略,不影响功能。
//! 进程退出时线程优先级随线程消失,无需恢复。

#[derive(Clone, Copy)]
pub enum Class {
    /// 只做 µs 级转发/写入、且绝大多数时间阻塞等待的线程。
    Highest,
    /// 需要持续计算、但必须优先于普通后台任务的线程。
    AboveNormal,
}

#[cfg(windows)]
pub fn boost(class: Class) {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL, THREAD_PRIORITY_HIGHEST,
    };
    let priority = match class {
        Class::Highest => THREAD_PRIORITY_HIGHEST,
        Class::AboveNormal => THREAD_PRIORITY_ABOVE_NORMAL,
    };
    unsafe {
        // 返回值仅表示成功与否;失败(如被作业对象限制)不致命,忽略即可。
        let _ = SetThreadPriority(GetCurrentThread(), priority);
    }
}

#[cfg(target_os = "linux")]
pub fn boost(class: Class) {
    // Linux:尽力而为地降低 nice 值。普通用户没有 CAP_SYS_NICE 时内核会拒绝
    // (EPERM),静默忽略;有权限(如打了 setcap)时则生效。
    let nice = match class {
        Class::Highest => -5,
        Class::AboveNormal => -2,
    };
    unsafe {
        let _ = libc::setpriority(libc::PRIO_PROCESS, 0, nice);
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn boost(_class: Class) {}
