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
//! 全部是"尽力而为":失败不影响功能。但**失败必须可见**(2026-10-22):
//! 以前两个平台都是 `let _ =` 静默吞掉,用户和排查者都以为"优先级已经提上去了",
//! 而满载时输入线程仍被游戏线程拖后几十毫秒 —— 这种"以为修好了"比没修更糟。
//! Linux 侧 2026-10-21 先改,WARN 里带补救命令;Windows 侧 2026-10-22 对齐。
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
    let (priority, label) = match class {
        Class::Highest => (THREAD_PRIORITY_HIGHEST, "Highest"),
        Class::AboveNormal => (THREAD_PRIORITY_ABOVE_NORMAL, "AboveNormal"),
    };
    // 失败必须**可见**,与 Linux 分支同一口径(2026-10-22)。
    // 常见原因:本进程被放进带 CPU 速率限制的作业对象(某些启动器/沙箱会这么做),
    // 或系统策略禁止改线程优先级。这类环境通常没有配置项可绕,只能如实告诉用户。
    let ok = unsafe { SetThreadPriority(GetCurrentThread(), priority) };
    if ok == 0 {
        let err = std::io::Error::last_os_error();
        crate::diag_warn!(
            "priority",
            "提高线程优先级失败({label}): {err}。\
             满载时输入线程仍可能被游戏线程拖后几十毫秒。\
             常见原因:本进程被放进带 CPU 速率限制的作业对象(如某些启动器/沙箱),\
             或系统策略禁止修改线程优先级;这类环境通常没有配置项可绕。"
        );
    }
}

#[cfg(target_os = "linux")]
pub fn boost(class: Class) {
    // Linux:降低 nice 值。`setpriority(PRIO_PROCESS, 0, …)` 作用于**调用线程**
    // (Linux 的 nice 是 per-thread 的,`PRIO_PROCESS` 在 Linux 上就是 `who=0`
    //  指当前线程 —— 这一点与 POSIX 的"进程"语义不同,别被名字骗了)。
    //
    // 权限:需要 CAP_SYS_NICE,或 `RLIMIT_NICE` 允许该深度。现代桌面会话管理器
    // 通常给一个宽松的 RLIMIT_NICE(实测本机 shell 的 nice 直接是 -8),所以
    // 常见情况下**这条是生效的** —— 2026-10-19 那份文档里"很可能完全无效"
    // 的说法是推断而不是观测,2026-10-21 在真机上用 `ps -Lo ni` 量到
    // 读线程 nice=-5、引擎线程 nice=-2,与代码一致,故已纠正。
    //
    // 失败必须**可见**(2026-10-21 第 6 条):以前是 `let _ =`,EPERM 被吞掉,
    // 用户和排查者都以为"优先级已经提上去了"。现在失败落一条 WARN,
    // 并给出可执行的补救办法。
    let nice = match class {
        Class::Highest => -5,
        Class::AboveNormal => -2,
    };
    let r = unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice) };
    if r != 0 {
        let err = std::io::Error::last_os_error();
        // 只告警**一次**(2026-10-26 第 5 条):每台输入设备一条读取线程,每条线程
        // 都会调一次 boost。nice 被系统策略拒绝(EPERM/EACCES,常见于收紧的
        // RLIMIT_NICE 的桌面会话)时,原先是每条线程各打一行 WARN ——
        // 用户那份日志里同一句话刷了几十遍,真正的一次性异常反而被淹没。
        // 副作用说明:级别提升是"尽力而为",失败**不影响功能**,只是在游戏吃满
        // CPU 时输入线程可能排队几十毫秒;所以一条 WARN 加上两条补救足够了。
        if warn_once(&NICE_WARNED) {
            // 把**本程序实际路径**写进补救命令里:用户复制粘贴就能用,
            // 不必自己去找二进制在哪(发行包/自编译/cargo 缓存路径各不相同)。
            let exe = std::env::current_exe()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "<本程序的二进制>".to_string());
            crate::diag_warn!(
                "priority",
                "提高线程优先级失败(nice={nice}): {err}。\
                 只影响满载时的排队延迟(游戏吃满 CPU 时输入可能晚几毫秒到几十毫秒),\
                 不影响功能。两条补救任选其一,做一次即可:\
                 ① sudo setcap cap_sys_nice+ep {exe}(替换或重新编译二进制之后要重做);\
                 ② 让程序跑在 systemd user 服务里,并在 unit 里加 LimitNICE=-5。\
                 本条只提示一次,之后同类失败不再重复刷屏。"
            );
        }
    }
}

/// "同一个异常只告警一次"的闸门。
///
/// 语义:swap(true) 是原子的"取旧值并置位" —— 第一次调用看到 false,返回
/// true(该告警);之后一律看到 true,返回 false(闭嘴)。用 swap 而不是
/// load + store:多台设备的读取线程会**并发**进来,只有拿到旧值 false
/// 的那个线程才该打日志,否则两个线程可能同时判定"我是第一个"。
///
/// 为什么不按 Class 分别记:同一个 nice 请求失败就是同一个系统策略问题,
/// 按类分开只会把同一句话打两遍。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn warn_once(flag: &std::sync::atomic::AtomicBool) -> bool {
    !flag.swap(true, std::sync::atomic::Ordering::Relaxed)
}

/// nice 提升失败的"已告警"标记(进程级,所有读取线程共享)。
///
/// 只放在 Linux 分支里:Windows 侧的告警频率与文案按任务要求**逐字不变**。
#[cfg(target_os = "linux")]
static NICE_WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(not(any(windows, target_os = "linux")))]
pub fn boost(_class: Class) {}

#[cfg(test)]
mod tests {
    use super::warn_once;
    use std::sync::atomic::AtomicBool;

    /// 守护 Linux 分支 boost() 里那句 `if warn_once(&NICE_WARNED)`:
    /// 第一次失败必须打 WARN,之后的(并发的)同类失败必须闭嘴。
    ///
    /// 阳性对照:全新标记的第一次调用返回 true;
    /// 阴性对照:同一标记的第二、三次调用必须返回 false —— 如果把实现改回
    /// "每条线程都告警"(即改前每台设备刷一行的行为),后两条断言立刻红。
    #[test]
    fn priority_warning_fires_once_then_stays_quiet() {
        let flag = AtomicBool::new(false);
        assert!(warn_once(&flag), "第一次失败必须告警");
        assert!(!warn_once(&flag), "第二次失败必须闭嘴");
        assert!(!warn_once(&flag), "第三次失败仍必须闭嘴");
        // 另一份标记不能被前一处置位影响(将来复用同一个闸门时不会互相吞掉)
        let other = AtomicBool::new(false);
        assert!(warn_once(&other), "不同标记之间不得互相污染");
    }
}
