//! 单实例守卫:同一个用户会话里只允许一个 scrcpy-pad 进程在跑。
//!
//! # 为什么必须有它(用户 2026-10-11:"我他妈不要残留")
//!
//! 用户习惯"只关 GUI、留着控制台",于是出现过**没有窗口却仍有进程**的僵尸实例:
//! 编 release 时 exe 被占用(`build.ps1` 报占用、cargo 报 `os error 5` 删不掉
//! `target/release/scrcpy-pad.exe`),而那个实例还攥着全局键盘/鼠标钩子与 FPS
//! 隔膜开关 —— 再双击一次就变成两个实例**互抢输入**,表现出来就是"怎么都不行"。
//!
//! 所以纪律只有一条:**已经在跑就明确拒绝,绝不静默再起一个**。
//!
//! # 凭据为什么不会变成新的残留
//!
//! 凭据只依赖内核对象,不依赖"退出时记得清理"这种约定 —— 后者恰恰是僵尸实例
//! 最不可靠的地方:
//!
//! * Windows:命名互斥体(`CreateMutexW` + `ERROR_ALREADY_EXISTS`)。对象在
//!   **最后一个句柄关闭**时被内核销毁:进程正常退出、崩溃、被任务管理器强杀
//!   都一样,不会留下"打不开的死锁"。
//! * 其它平台:`app::config_dir()/scrcpy-pad-single-instance-v1.lock` 上的文件锁
//!   (`std::fs::File::try_lock`,Linux 上是 flock),同样由内核在进程结束时释放;
//!   文件里写 pid 只是为了拒绝时能说清"是谁占着"。
//!
//! 两边都是"拿到守卫 = 本进程可以继续",守卫自身**没有任何需要手工执行的清理**。
//! 这一条由 [`tests::another_process_holding_the_guard_is_detected`] 盯着:真的起
//! 一个子进程拿着守卫,再确认它退出后守卫重新可用(顺带证明拒绝路径不漏句柄)。
//!
//! # 验证边界(别把"编过了"当成"验过了")
//!
//! 非 Windows 那条分支 2026-10-19 之前**只通过了类型检查**(本机 WSL 里没有
//! rustc、也链接不了 Linux 目标),`file_lock_rejects_a_second_holder` 从未被执行。
//! **2026-10-21 已在真 Linux(CachyOS)上跑过全量测试并通过**,这条边界已销账。
//! 跨进程"持有者退出后守卫重新可用"由真起子进程的那条测试验证,两个平台都跑。

/// 唯一实例的名字。
///
/// 带 `v1`:将来若改守卫的范围(例如从"按会话"改成"按整机"),改版本号就能与
/// 旧实例并存,不会互相误锁。Windows 侧会再加上 `Local\` 前缀。
const LOCK_NAME: &str = "scrcpy-pad-single-instance-v1";

/// "已经有一个实例在跑"。`detail` 是给用户看的人话,直接打 stdout / 诊断日志。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlreadyRunning {
    pub detail: String,
}

impl std::fmt::Display for AlreadyRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

/// 唯一实例凭据:活着 = 本进程有权继续跑。
///
/// `main` 把它绑在一个活到进程结束的变量上。**它自己就是凭证本体**:
/// `Drop` 里真的关句柄/放锁 —— 拒绝路径、测试、以及将来任何提前返回都因此
/// 不会漏释放(第一版把凭证放在一个内部类型里、只给外层写了 Drop,结果测试
/// 里直接拿到内层类型的那几处全在漏句柄 —— 现在只有一个类型,漏不掉)。
///
/// 正常退出走 `std::process::exit` 时不会跑到这里,但内核会随进程回收凭证,
/// 这正是"守卫自己不是残留"的兜底。
pub struct Guard {
    /// Windows:命名互斥体句柄;`null` = 创建失败的退化守卫(放行,见 [`acquire`])。
    #[cfg(windows)]
    handle: windows_sys::Win32::Foundation::HANDLE,
    /// 其它平台:锁文件句柄。**只持有不读**:`File` 一被 drop 文件锁就释放了,
    /// 所以它必须活到进程结束 —— "从没被读过"是刻意的,不是漏写。
    #[cfg(not(windows))]
    file: Option<std::fs::File>,
}

#[cfg(windows)]
impl Drop for Guard {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            // SAFETY: 句柄只来自本模块的 CreateMutexW,且只在这里关一次
            // (关完立刻置空,重复 drop 也安全)。
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.handle) };
            self.handle = std::ptr::null_mut();
        }
    }
}

#[cfg(not(windows))]
impl Drop for Guard {
    fn drop(&mut self) {
        // 把锁文件句柄 drop 掉 = 释放文件锁(内核在进程结束时还会兜底释放一次)。
        self.file = None;
    }
}

/// 尝试成为唯一实例。
///
/// * `Ok` = 拿到了(或退化成"无法判定",见下);调用方把返回值绑到进程结束即可。
/// * `Err` = 已经有一个实例在跑:**调用方必须提示并退出**,不能继续初始化捕获层。
///
/// 退化(互斥体/锁文件创建失败,例如只读介质上的配置目录)**放行**,同时留 warn:
/// 拦不住重复实例只是少一层保护,而让程序完全起不来是更糟的失败。
pub fn acquire() -> Result<Guard, AlreadyRunning> {
    let guard = imp::acquire_named(LOCK_NAME)?;
    crate::diag_info!("instance", "单实例守卫已取得(pid={})", std::process::id());
    Ok(guard)
}

// ==================== Windows:命名互斥体 ====================
//
// 用到的 CreateMutexW 在 windows-sys 0.59 里同时挂在 `Win32_System_Threading`
// 与 `Win32_Security` 两个 feature 后面(它的第一个参数类型 SECURITY_ATTRIBUTES
// 属于后者),Cargo.toml 里两个 feature 都已启用。
#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows_sys::Win32::System::Threading::CreateMutexW;

    use super::{AlreadyRunning, Guard};

    /// 内核对象名。`Local\` = 本登录会话:输入钩子与隔膜本来就是**按会话**的东西,
    /// 不同会话(另一个用户登录同一台机器)各跑一份是合理的。
    fn wide_name(name: &str) -> Vec<u16> {
        format!("Local\\{name}")
            .encode_utf16()
            .chain(std::iter::once(0)) // CreateMutexW 要 NUL 结尾
            .collect()
    }

    pub(super) fn acquire_named(name: &str) -> Result<Guard, AlreadyRunning> {
        let wide = wide_name(name);
        // SAFETY: `wide` 是以 NUL 结尾的 UTF-16 缓冲,活得比这次调用长。
        // 传空的 SECURITY_ATTRIBUTES = 默认安全描述符;不要求初始所有权 ——
        // 我们只用"对象是否已存在"这一位信息,不拿它做同步。
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
        // **必须紧接在这里读**:任何别的 Win32 调用都可能覆盖 last-error,
        // 读晚了就会把上一次的残留错误码当成"已存在"。
        let last = unsafe { GetLastError() };
        if handle.is_null() {
            // 连互斥体都建不出来(权限/内存):退化为"不拦",但要留下痕迹
            let e = std::io::Error::last_os_error();
            crate::diag_warn!(
                "instance",
                "创建单实例互斥体失败:{e} —— 本次不做重复实例拦截(拦不住比起不来轻)"
            );
            eprintln!("[instance] 创建单实例互斥体失败:{e} —— 本次不做重复实例拦截");
            return Ok(Guard { handle });
        }
        if last == ERROR_ALREADY_EXISTS {
            // 刚拿到的这个句柄不是"我们该留着的那个":真正的持有者是仍在跑的实例。
            // 当场关掉(`Guard::drop` 会 CloseHandle)—— 否则本进程退出后反而把对象
            // 留成孤儿,下一次启动就永远报"已有一个实例在跑"。
            drop(Guard { handle });
            return Err(AlreadyRunning {
                detail: format!(
                    "已有一个 scrcpy-pad 在运行(命名互斥体 {name} 已存在),拒绝再起第二个。\n\
                     提示:只关界面窗口不一定结束进程;请在任务管理器里结束 scrcpy-pad.exe 后重试。"
                ),
            });
        }
        Ok(Guard { handle })
    }
}

// ==================== 其它平台:文件锁 + pid ====================
#[cfg(not(windows))]
mod imp {
    use std::fs::{File, OpenOptions, TryLockError};
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::path::{Path, PathBuf};

    use super::{AlreadyRunning, Guard};

    /// 锁文件路径:与配置同目录,出问题时能直接看到"是谁占着"。
    fn lock_path(name: &str) -> PathBuf {
        crate::app::config_dir().join(format!("{name}.lock"))
    }

    pub(super) fn acquire_named(name: &str) -> Result<Guard, AlreadyRunning> {
        acquire_path(&lock_path(name))
    }

    /// 真正的文件锁逻辑(路径可注入:单测用临时目录,不污染真配置目录)。
    pub(super) fn acquire_path(path: &Path) -> Result<Guard, AlreadyRunning> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
        {
            Ok(f) => f,
            Err(e) => {
                crate::diag_warn!(
                    "instance",
                    "打不开单实例锁文件 {}: {e} —— 本次不做重复实例拦截",
                    path.display()
                );
                eprintln!(
                    "[instance] 打不开单实例锁文件 {}: {e} —— 本次不做重复实例拦截",
                    path.display()
                );
                return Ok(Guard { file: None });
            }
        };
        match file.try_lock() {
            Ok(()) => {
                // pid 只为了拒绝时能说清"是谁占着";写失败不影响锁本身
                let _ = file.set_len(0);
                let _ = file.seek(SeekFrom::Start(0));
                let _ = write!(file, "{}", std::process::id());
                let _ = file.flush();
                Ok(Guard { file: Some(file) })
            }
            Err(TryLockError::WouldBlock) => {
                let holder = match holder_pid(path) {
                    Some(pid) => format!("pid {pid}"),
                    None => "pid 未知".to_string(),
                };
                Err(AlreadyRunning {
                    detail: format!(
                        "已有一个 scrcpy-pad 在运行({holder},锁文件 {}),拒绝再起第二个。\n\
                         提示:只关界面窗口不一定结束进程;请结束那个进程后重试。",
                        path.display()
                    ),
                })
            }
            Err(TryLockError::Error(e)) => {
                crate::diag_warn!(
                    "instance",
                    "锁定单实例文件 {} 失败: {e} —— 本次不做重复实例拦截",
                    path.display()
                );
                eprintln!(
                    "[instance] 锁定单实例文件 {} 失败: {e} —— 本次不做重复实例拦截",
                    path.display()
                );
                Ok(Guard { file: None })
            }
        }
    }

    /// 读出锁文件里记着的 pid。半截/空文件都必须容错(另一端可能刚被强杀,
    /// 正写到一半)—— 解析不出来就只是"pid 未知",不影响"已有一个在跑"这个结论。
    fn holder_pid(path: &Path) -> Option<u32> {
        let mut text = String::new();
        File::open(path).ok()?.read_to_string(&mut text).ok()?;
        text.trim().parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    /// 父测试叫起子进程时用的标记与"测试专用 token"。平时(直接 `cargo test`)
    /// 子测试什么都不做。
    const CHILD_ENV: &str = "SCRCPY_PAD_INSTANCE_CHILD_TEST";
    const CHILD_TOKEN_ENV: &str = "SCRCPY_PAD_INSTANCE_CHILD_TOKEN";

    /// **测试串行锁**(2026-10-21):本模块的测试会 `fork+exec` 子进程,而 POSIX 的
    /// `fork` 把进程内**所有线程**的 fd 一并复制给子进程 —— 此刻别的测试线程若正
    /// 持有锁文件,那个 fd 就到了子进程手里,于是父进程 drop 自己的守卫也释放不了
    /// 锁(锁还被子进程持有一份),`drop` 之后那条"必须能重新拿到"的断言偶发变红。
    ///
    /// 量化(2026-10-21,工作区外 8 秒压测:4 个 fork/exec 线程 + 1 个反复取锁线程):
    /// 456 万轮 / 约 632 万次取锁失败;修复前 25 次全量跑红 4 次,修复后 30 次全量
    /// 跑 **30 次全绿**。
    ///
    /// 生产代码零改动:生产只有一个进程、一个固定锁名,不存在这个窗口。
    /// 中毒也照拿(`unwrap_or_else`),与 `diag::DIAG_TEST_LOCK` 同一套写法。
    static INSTANCE_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn serialize() -> std::sync::MutexGuard<'static, ()> {
        INSTANCE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 每次调用都不同的 token(进程 pid + 进程内自增):同一台机器上反复跑、
    /// 并行跑,以及**用户正开着程序**的时候,都不会与生产锁名相撞。
    ///
    /// 为什么必须这样(Lead 2026-10-11 抓出来的门禁缺陷):第一版跨进程测试直接
    /// 用生产锁名,而用户明说常"只关 GUI、留着控制台" —— 那种时候生产互斥体是
    /// **真被占着**的,测试就必然变红。那是测试设计缺陷而不是产品缺陷:
    /// 门禁不能建立在"跑测试前先把程序关掉"这种约定上。
    fn test_token() -> String {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        format!(
            "{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// 测试专用守卫入口:锁的身份可注入,**生产锁名永远不参与测试**。
    ///
    /// Windows 注入的是命名互斥体的名字;非 Windows 注入的是临时目录里的锁文件
    /// (不往真配置目录里塞测试垃圾)。父子进程靠同一个 token 抢同一把锁。
    #[cfg(windows)]
    fn acquire_test_guard(token: &str) -> Result<Guard, AlreadyRunning> {
        imp::acquire_named(&format!("{LOCK_NAME}-test-{token}"))
    }

    #[cfg(not(windows))]
    fn acquire_test_guard(token: &str) -> Result<Guard, AlreadyRunning> {
        imp::acquire_path(&test_lock_path(token))
    }

    /// 非 Windows 的测试锁文件路径(测试结束会删掉,不留垃圾)。
    #[cfg(not(windows))]
    fn test_lock_path(token: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("scrcpy-pad-instance-test-{token}.lock"))
    }

    /// 同名第二次必须被拒绝,而且守卫消失后必须能重新拿到 —— 后者才是"不残留"。
    #[test]
    fn second_guard_with_same_name_is_rejected() {
        let _g = serialize();
        let token = test_token();
        let first = acquire_test_guard(&token).expect("第一次必须拿到守卫");
        match acquire_test_guard(&token) {
            Ok(_) => panic!("同名第二次必须报\"已有一个实例在跑\""),
            Err(already) => assert!(
                already.detail.contains("已有一个"),
                "拒绝理由要说人话:{}",
                already.detail
            ),
        }
        // 守卫一 drop 就必须真的释放(Windows 关句柄 / POSIX 放文件锁)
        drop(first);
        assert!(
            acquire_test_guard(&token).is_ok(),
            "前一个守卫 drop 后必须能重新拿到:Drop 没真释放?"
        );
    }

    /// 跨进程验证:这**才是**守卫的真实用法(同进程撞名只证明了名字);
    /// 顺带盯住两件事 —— 子进程退出后守卫重新可用、拒绝/放行路径都没漏句柄。
    #[test]
    fn another_process_holding_the_guard_is_detected() {
        let _g = serialize();
        let token = test_token();
        // 控制组:token 唯一,这把锁必然空闲。**先把话说清楚**,别再让人去追一个
        // 不存在的进程(第一版这里是"请先关掉正在运行的 scrcpy-pad",结果 lead
        // 跑了三次 tasklist 也没找到任何 scrcpy-pad.exe —— 真正的原因是同一个
        // 测试二进制/多个测试进程在并发抢同一个生产锁名,现在测试只用自己的 token,
        // 这条分支一旦触发就是真 bug)。
        match acquire_test_guard(&token) {
            Ok(guard) => drop(guard),
            Err(_) => panic!(
                "测试专用锁 {token} 竟已被占用 —— 这个名字只属于本次测试(pid+序号),\
                 并行测试与真实实例都用别的名字,占用它只可能是守卫自己漏了释放"
            ),
        }

        let exe = std::env::current_exe().expect("取不到测试程序自身路径");
        let mut child = std::process::Command::new(exe)
            .args(["--exact", "instance::tests::child_holds_guard"])
            .env(CHILD_ENV, "1")
            .env(CHILD_TOKEN_ENV, &token)
            // 走 null 而不是管道:子进程的输出与我们无关,也不该被管道阻塞
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("子进程起不来");

        // 等子进程真正拿到守卫(最多 10s;机器被别的编译/测试压满时启动会慢)。
        // 这期间本进程每次尝试都该失败。
        let mut detected = false;
        for _ in 0..400 {
            match acquire_test_guard(&token) {
                // 拿到了 = 子进程还没起好。这个守卫必须**当场放掉** ——
                // 留着不但让子进程拿不到,还会把后续断言变成假阳性。
                Ok(guard) => drop(guard),
                Err(_) => {
                    detected = true;
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let _ = child.wait();
        assert!(
            detected,
            "另一个进程持有守卫时,本进程必须被拒绝(子进程没起来?过滤名写错了?)"
        );
        assert!(
            acquire_test_guard(&token).is_ok(),
            "持有者退出后守卫必须重新可用:拒绝路径漏关句柄了?内核没回收?"
        );
        // 非 Windows 的测试锁文件是我们自己造的:最后一句断言里的守卫已经 drop,
        // 锁已释放,收尾删掉别留垃圾。
        #[cfg(not(windows))]
        {
            let _ = std::fs::remove_file(test_lock_path(&token));
        }
    }

    /// 跨进程测试的被叫方:真的拿着守卫睡一会儿,让父进程去撞它。
    /// 平时直接 `cargo test` 时立刻返回 —— 不白占 1.5s,也不占锁。
    #[test]
    fn child_holds_guard() {
        // 锁必须**第一句**(`every_test_in_this_module_takes_the_serial_lock` 按结构扫)。
        // 子进程有自己的地址空间,这把锁是**进程内**的串行锁,拿它不会与父进程互等。
        let _g = serialize();
        if std::env::var_os(CHILD_ENV).is_none() {
            return;
        }
        let token = std::env::var(CHILD_TOKEN_ENV).expect("父测试必须把测试 token 传进来");
        let guard = acquire_test_guard(&token).expect("子进程应当能取得守卫");
        std::thread::sleep(std::time::Duration::from_millis(1500));
        drop(guard);
    }

    /// 非 Windows 侧的真身(文件锁):两个句柄抢同一个锁文件,第二个必须被拒。
    ///
    /// 注意 Windows 上 `File::try_lock` **不保证**同进程内跨句柄互斥(std 自己的
    /// 用例在 Windows 上直接标了 `should_panic`),所以这条只在非 Windows 有意义 ——
    /// Windows 的真身是上面那条互斥体测试。这里用临时目录,不碰真配置目录。
    #[cfg(not(windows))]
    #[test]
    fn file_lock_rejects_a_second_holder() {
        let _g = serialize();
        let path =
            std::env::temp_dir().join(format!("scrcpy-pad-instance-lock-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let first = imp::acquire_path(&path).expect("第一次必须拿到锁");
        match imp::acquire_path(&path) {
            Ok(_) => panic!("同一锁文件第二次必须被拒绝"),
            Err(already) => assert!(
                already.detail.contains("已有一个"),
                "拒绝理由要说人话:{}",
                already.detail
            ),
        }
        drop(first);
        assert!(
            imp::acquire_path(&path).is_ok(),
            "前一个守卫 drop 后必须能重新拿到:文件锁没真释放?"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// **守护测试**:本模块每条 `#[test]` 都必须先拿测试串行锁。
    ///
    /// 为什么需要它(2026-10-22):串行锁曾经**只写在文档里、没落到代码里** ——
    /// `文档/Linux真机欠账偿还-2026-10-21.md` 与协议都写"已用 `INSTANCE_TEST_LOCK`
    /// 串行化",而 `grep` 在源码里零命中,flaky 的根因其实一直在。这类"文档声称修好"
    /// 靠人工复核抓不住,必须有会变红的断言。
    ///
    /// 判据是**结构**而不是行为:行为层面的 flaky 是概率的(实测 25 次红 4 次),
    /// 断言它"不偶发"永远测不稳;而"每条测试第一句是不是拿锁"是确定的。
    /// 扫描按行进行(§3.2:禁止用会被 `cargo fmt` 重排的转义串模式)。
    ///
    /// 阳性/阴性对照都在下面(`unserialized_tests` 是纯函数,可以直接喂样本)。
    #[test]
    fn every_test_in_this_module_takes_the_serial_lock() {
        let _g = serialize();
        // 阴性对照:样本里**有**锁 ⇒ 不许报违规(否则本测试是"永远红")。
        let with_lock = format!(
            "{attr}\nfn ok_one() {{\n    let _g = serialize();\n    let x = 1;\n}}\n",
            attr = attr_marker()
        );
        assert!(
            unserialized_tests(&with_lock).is_empty(),
            "阴性对照失败:样本里明明拿了锁却被判违规 ⇒ 判据写错了"
        );
        // 阳性对照:样本里**没**有锁 ⇒ 必须被点名(否则本测试是"永远绿")。
        let without_lock = format!(
            "{attr}\nfn bad_one() {{\n    // 注释不算第一句\n    let x = 1;\n}}\n",
            attr = attr_marker()
        );
        let bad = unserialized_tests(&without_lock);
        assert_eq!(
            bad,
            vec!["bad_one".to_string()],
            "阳性对照失败:没拿锁的测试必须被点名,实际 {bad:?}"
        );
        // 真身:扫描本文件,一条都不许漏。
        let src = include_str!("instance.rs");
        assert!(
            src.lines().count() > 300,
            "扫到的源码只有 {} 行,像是扫错了文件",
            src.lines().count()
        );
        let missing = unserialized_tests(src);
        assert!(
            missing.is_empty(),
            "这些测试没拿 INSTANCE_TEST_LOCK 就跑了(会与 fork+exec 的 fd 复制打架,见模块内注释):{missing:?}"
        );
    }

    /// `#[test]` 属性行的判据。**必须用拼接构造**:如果这里写字面量,
    /// 上一行扫描`include_str!` 时会把本测试自己判成一条"没有锁的测试"。
    fn attr_marker() -> String {
        ["#", "[", "test", "]"].concat()
    }

    /// 纯函数:返回源码里**每条 `#[test]` 函数中,第一句有效代码不是 `serialize()`
    /// 的那些函数名**。跳过空行与 `//` 注释行(注释不能顶替拿锁)。
    fn unserialized_tests(src: &str) -> Vec<String> {
        let attr = attr_marker();
        let lines: Vec<&str> = src.lines().collect();
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < lines.len() {
            if lines[i].trim() != attr {
                i += 1;
                continue;
            }
            // 属性下面可能是其它属性(如 `#[cfg(...)]`),找到 `fn ` 那一行为止
            let mut j = i + 1;
            let mut name = String::from("<找不到函数名>");
            while j < lines.len() {
                let t = lines[j].trim();
                if let Some(rest) = t.strip_prefix("fn ") {
                    name = rest.split('(').next().unwrap_or("").to_string();
                    break;
                }
                if t == attr {
                    break;
                }
                j += 1;
            }
            // 函数体第一句有效代码
            let mut k = j + 1;
            let mut takes_lock = false;
            while k < lines.len() {
                let t = lines[k].trim();
                if t.is_empty() || t.starts_with("//") {
                    k += 1;
                    continue;
                }
                takes_lock = t.contains("serialize()");
                break;
            }
            if !takes_lock {
                out.push(name);
            }
            i = j.max(i + 1);
        }
        out
    }
}
