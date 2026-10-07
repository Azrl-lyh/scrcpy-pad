//! 「手动 adb 命令」区的纯逻辑与执行器(界面接线在 `app.rs`)。
//!
//! 设计要点:
//! - 命令栏存的是 **token 列表**(不是一整行文本):这样"加入参数"能查冲突、
//!   单个参数能点掉、整段片段能撤去;落盘时再拼回字符串(settings.json 的
//!   `adb_command`),重启后重新拆分还原。
//! - 加入时的冲突检查只覆盖命令**开头的"- 参数区"**(adb 客户端自己解析的
//!   全局选项);`shell` 之后的设备侧参数(`grep -i`、`pm -3`、坐标数字)
//!   一律放行 —— 那些位置的重复是合法且常见的。
//! - 执行器自带输出上限与停止能力:adb 命令可能跑很久/输出很大
//!   (`--latency`、截图、日志),不能把界面线程拖住,也不能让读管道的内存无界。

use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// adb 的"设备定向"参数:同一时刻最多允许出现一个,且必须放在子命令之前。
pub const DEVICE_SELECTORS: [&str; 4] = ["-s", "-d", "-e", "-t"];

/// 单条命令 stdout / stderr 各自保留的上限(字节),超额部分丢弃但继续读,
/// 避免子进程因管道写满而卡死。
pub const OUTPUT_CAP: usize = 64 * 1024;

/// 用户自建预设的落盘文件名(与 settings.json 同目录)。
pub const PRESET_FILE: &str = "adb_presets.json";

// ============================ 命令栏解析与冲突检查 ============================

/// 把输入行拆成 token:按空白拆分,但**双引号内的空白不拆**、引号原样保留在
/// token 里(adb/设备端 shell 自己会解释引号,程序不做去壳)。
pub fn split_tokens(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    for ch in input.chars() {
        if ch == '"' {
            in_quote = !in_quote;
            cur.push(ch);
        } else if ch.is_whitespace() && !in_quote {
            if !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(ch);
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}
/// 命令开头"- 参数区"的长度(tokens 下标)。
///
/// 走法:以 '-' 开头的 token 直接算参数;"紧跟在一个 '- 参数' 后面的非 '-'
/// token"视为该参数的值(如 `-s SERIAL` 的 SERIAL);其余位置的非 '-' token
/// 视为子命令开始(shell / reboot / tcpip …),参数区到此为止。
/// 无法精确知道每个开关带不带值,这个近似对"查设备定向参数与查重"足够:
/// 最坏情况也只是把子命令名多算进参数区,检查结果不受影响。
fn global_flag_region_len(tokens: &[String]) -> usize {
    let mut end = 0;
    for (i, t) in tokens.iter().enumerate() {
        if t.starts_with('-') {
            end = i + 1;
        } else if i > 0 && tokens[i - 1].starts_with('-') {
            end = i + 1; // 视为前一开关的值
        } else {
            break;
        }
    }
    end
}

/// 命令栏里是否已显式指定了设备定向参数(-s/-d/-e/-t,且位于参数区内)。
/// 执行时据此决定要不要自动前置 `-s <当前设备>`。
pub fn has_device_selector(bar: &[String]) -> bool {
    let end = global_flag_region_len(bar);
    bar[..end]
        .iter()
        .any(|t| DEVICE_SELECTORS.contains(&t.as_str()))
}

/// 检查一段新参数能否加入命令栏;Err 是给用户看的中文拒绝理由。
///
/// 规则(基础规则,刻意不做设备侧参数的语法解析):
/// 1. 设备定向参数 -s/-d/-e/-t 最多一个,且必须在子命令之前
///    (放在 `shell` 之后只会被当作传给设备侧的普通文字,起不到选设备作用);
/// 2. 命令开头"- 参数区"内不允许同名参数重复(如两个 -H);
/// 3. 待加入片段(≥2 个词)与命令栏里已有片段完全相同时拒绝;
/// 4. `shell` 等子命令之后的设备侧参数不受以上限制。
pub fn check_fragment(bar: &[String], frag: &[String]) -> Result<(), String> {
    if frag.is_empty() {
        return Err("没有可加入的参数(输入为空)".to_string());
    }
    // 规则 1 前半:必须放在子命令之前
    if DEVICE_SELECTORS.contains(&frag[0].as_str()) {
        let bar_end = global_flag_region_len(bar);
        if bar_end < bar.len() {
            return Err(format!(
                "命令栏已进入子命令({}),设备定向参数 {} 必须放在最前面才会生效",
                bar[bar_end], frag[0]
            ));
        }
    }
    let mut merged: Vec<String> = bar.to_vec();
    merged.extend_from_slice(frag);
    let region_end = global_flag_region_len(&merged);
    let region = &merged[..region_end];
    // 规则 1 后半:设备定向参数最多一个
    let selectors: Vec<&str> = region
        .iter()
        .filter(|t| DEVICE_SELECTORS.contains(&t.as_str()))
        .map(|s| s.as_str())
        .collect();
    if selectors.len() > 1 {
        return Err(format!(
            "设备定向参数最多一个({}),不能同时使用: {}",
            DEVICE_SELECTORS.join(" / "),
            selectors.join(" ")
        ));
    }
    // 规则 2:同名参数不得重复(仅限开头参数区)
    for (i, t) in region.iter().enumerate() {
        if region[..i].iter().any(|p| p == t) {
            return Err(format!("命令开头已有同名参数 {t},不能重复加入"));
        }
    }
    // 规则 3:整段片段不得重复
    if frag.len() >= 2
        && bar
            .windows(frag.len())
            .any(|w| w.iter().zip(frag.iter()).all(|(a, b)| a == b))
    {
        return Err(format!(
            "命令栏里已存在完全相同的参数片段: {}",
            frag.join(" ")
        ));
    }
    Ok(())
}

/// 展示单个 token:含空白且自身没带引号时补一对引号,让预览与实际传参一致。
fn display_token(t: &str) -> String {
    let quoted = t.len() >= 2 && t.starts_with('"') && t.ends_with('"');
    if !quoted && t.chars().any(char::is_whitespace) {
        format!("\"{t}\"")
    } else {
        t.to_string()
    }
}

/// 组装真正要执行的本机 adb 参数:命令栏里没有显式设备定向参数时自动前置
/// `-s <serial>`。返回 (adb 的 argv(不含 exe 本身), 给人看的完整命令行)。
pub fn build_command(serial: &str, bar: &[String]) -> (Vec<String>, String) {
    let mut args: Vec<String> = Vec::new();
    let serial = serial.trim();
    if !serial.is_empty() && !has_device_selector(bar) {
        args.push("-s".to_string());
        args.push(serial.to_string());
    }
    args.extend_from_slice(bar);
    let display = if args.is_empty() {
        "adb".to_string()
    } else {
        let body = args
            .iter()
            .map(|t| display_token(t))
            .collect::<Vec<_>>()
            .join(" ");
        format!("adb {body}")
    };
    (args, display)
}
// ================================ 执行器 ================================

/// 一次执行的结果(正常收尾时)。
#[derive(Debug, Clone)]
pub struct AdbDone {
    /// 退出码;被系统终止(如手动停止)时可能为 None。
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// 输出超过上限被截断。
    pub truncated: bool,
    /// 从启动到收尾的毫秒数。
    pub millis: u64,
    /// 是否由 [停止] 主动终止。
    pub killed: bool,
}

/// 一次执行的最终消息:display = 当时展示的命令行,便于对照日志。
#[derive(Debug, Clone)]
pub struct AdbRunOutcome {
    pub display: String,
    pub done: Result<AdbDone, String>,
}

/// 正在执行的一条 adb 命令的句柄。
///
/// 为什么不像截图那样"线程结束发消息"就完事:adb 命令可能长时间不返回
/// (`logcat`、`--latency`),界面要能中途 [停止],所以句柄保留着 Child 的
/// 弱控制权(stop 用于 kill);真正的收尾由内部等待线程完成后经 channel 送回。
pub struct AdbRun {
    rx: Receiver<AdbRunOutcome>,
    killer: Arc<Mutex<Option<Child>>>,
    killed: Arc<AtomicBool>,
}

impl AdbRun {
    /// **非阻塞**取走最终结果;没结束时返回 None。
    pub fn poll(&self) -> Option<AdbRunOutcome> {
        self.rx.try_recv().ok()
    }

    /// [停止]:终止本机 adb 进程(设备端已执行的子命令不回收)。
    pub fn stop(&self) {
        self.killed.store(true, Ordering::Relaxed);
        let mut guard = self.killer.lock().unwrap();
        if let Some(child) = guard.as_mut() {
            let _ = child.kill();
        }
    }
}

/// 读一个管道直到 EOF:最多保留 cap 字节,超出部分丢弃但**继续读**
/// (不读的话子进程会因管道写满而卡住)。
fn read_capped(mut reader: impl Read, cap: usize) -> (String, bool) {
    let mut kept: Vec<u8> = Vec::new();
    let mut truncated = false;
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if kept.len() < cap {
                    let take = n.min(cap - kept.len());
                    kept.extend_from_slice(&buf[..take]);
                    if take < n {
                        truncated = true;
                    }
                } else {
                    truncated = true;
                }
            }
            Err(_) => break,
        }
    }
    (String::from_utf8_lossy(&kept).into_owned(), truncated)
}

/// 启动一条 adb 命令。立即返回句柄;结果通过 `poll` 取。
pub fn run(exe: &str, args: Vec<String>, display: String) -> AdbRun {
    let (tx, rx) = mpsc::channel();
    let killed = Arc::new(AtomicBool::new(false));
    let killer: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
    let started = Instant::now();
    let spawned = Command::new(exe)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    match spawned {
        Err(e) => {
            let _ = tx.send(AdbRunOutcome {
                display,
                done: Err(format!("无法启动 adb({exe}): {e}")),
            });
        }
        Ok(mut child) => {
            let out_pipe = child.stdout.take();
            let err_pipe = child.stderr.take();
            *killer.lock().unwrap() = Some(child);
            let t_out = out_pipe.map(|r| std::thread::spawn(move || read_capped(r, OUTPUT_CAP)));
            let t_err = err_pipe.map(|r| std::thread::spawn(move || read_capped(r, OUTPUT_CAP)));
            let killer_wait = Arc::clone(&killer);
            let killed_wait = Arc::clone(&killed);
            std::thread::spawn(move || {
                // 每 25ms 探一次子进程状态;sleep 而不是阻塞 wait,便于将来扩展超时。
                let status = loop {
                    {
                        let mut guard = killer_wait.lock().unwrap();
                        match guard.as_mut() {
                            Some(c) => match c.try_wait() {
                                Ok(Some(st)) => break Some(st),
                                Ok(None) => {}
                                Err(_) => break None,
                            },
                            None => break None,
                        }
                    }
                    std::thread::sleep(Duration::from_millis(25));
                };
                let (stdout, out_cut) = t_out
                    .map(|t| t.join().unwrap_or_default())
                    .unwrap_or_default();
                let (stderr, err_cut) = t_err
                    .map(|t| t.join().unwrap_or_default())
                    .unwrap_or_default();
                *killer_wait.lock().unwrap() = None; // 回收 Child(已 wait 过)
                let outcome = AdbRunOutcome {
                    display,
                    done: Ok(AdbDone {
                        exit_code: status.and_then(|s| s.code()),
                        stdout,
                        stderr,
                        truncated: out_cut || err_cut,
                        millis: started.elapsed().as_millis() as u64,
                        killed: killed_wait.load(Ordering::Relaxed),
                    }),
                };
                let _ = tx.send(outcome);
            });
        }
    }
    AdbRun { rx, killer, killed }
}
// ============================ 用户自建预设的存取 ============================

/// 用户自建的命名预设(存 adb_presets.json,与 settings.json 同目录)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserPreset {
    pub name: String,
    pub tokens: Vec<String>,
}

/// 读用户预设文件;文件不存在按空列表处理(首次使用不算错误)。
pub fn load_user_presets(path: &Path) -> Result<Vec<UserPreset>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("读取失败: {e}")),
    };
    serde_json::from_str(&text).map_err(|e| format!("解析失败: {e}"))
}

/// 写用户预设文件(原子写:临时文件 + 改名,与设置/配置的落盘方式一致)。
pub fn save_user_presets(path: &Path, list: &[UserPreset]) -> Result<(), String> {
    let text = serde_json::to_string_pretty(list).map_err(|e| format!("序列化失败: {e}"))?;
    crate::app::write_atomic(path, &text).map_err(|e| format!("写入失败: {e}"))
}

// ============================ 内置离线命令库 ============================

/// 内置命令库条目(全部离线写死在程序里,不联网)。
pub struct CmdEntry {
    pub name: &'static str,
    /// 一句话说明这条命令做什么。
    pub desc: &'static str,
    /// 使用/注意:预期输出怎么看、参数怎么改。
    pub usage: &'static str,
    /// 命令 token(不含开头的 adb 本身;设备定向参数会按需自动补 -s)。
    pub tokens: &'static [&'static str],
    /// 是否同时出现在"预设"下拉里(本机排查最常用的那批)。
    pub preset: bool,
}

impl CmdEntry {
    /// 助手窗口的搜索匹配:名称/说明/使用/参数里任一命中即可。
    pub fn matches(&self, query_lower: &str) -> bool {
        self.name.to_lowercase().contains(query_lower)
            || self.desc.to_lowercase().contains(query_lower)
            || self.usage.to_lowercase().contains(query_lower)
            || self
                .tokens
                .iter()
                .any(|t| t.to_lowercase().contains(query_lower))
    }
}

/// 内置命令库:前 9 条 preset=true(预设下拉),其余只在助手窗口里可见。
pub fn builtin_entries() -> &'static [CmdEntry] {
    const E: &[CmdEntry] = &[
        CmdEntry {
            name: "刷新率相关设置总览",
            desc: "列出系统设置里所有和刷新率有关的键值(含 min_refresh_rate / peak_refresh_rate)。",
            usage: "示例输出:peak_refresh_rate=120、min_refresh_rate=null(null = 系统自适应)。排查游戏帧率时先跑这条。",
            tokens: &[
                "shell", "settings", "list", "system", "|", "grep", "-i", "refresh",
            ],
            preset: true,
        },
        CmdEntry {
            name: "钉住最低刷新率 120Hz",
            desc: "强制面板最低刷新率为 120Hz(即\"钉屏\"),用于验证游戏帧率是否受屏幕刷新率影响。",
            usage: "改完立即生效、重启后仍保留;恢复自适应请用『恢复刷新率自动』。个别机型/系统会忽略该键。",
            tokens: &[
                "shell",
                "settings",
                "put",
                "system",
                "min_refresh_rate",
                "120",
            ],
            preset: true,
        },
        CmdEntry {
            name: "恢复刷新率自动",
            desc: "删除钉住的 min_refresh_rate,恢复系统自适应刷新率。",
            usage: "与『钉住最低刷新率 120Hz』配套使用。",
            tokens: &["shell", "settings", "delete", "system", "min_refresh_rate"],
            preset: true,
        },
        CmdEntry {
            name: "面板 vsync 周期",
            desc: "读 SurfaceFlinger 的 --latency:不带层名时只回一行 vsync 周期。",
            usage: "144Hz≈6944444ns、120Hz≈8333333ns、60Hz≈16666666ns。要测某个游戏的真实出帧节奏,把图层名加在 --latency 后面(层名用『列出所有图层』找)。",
            tokens: &["shell", "dumpsys", "SurfaceFlinger", "--latency"],
            preset: true,
        },
        CmdEntry {
            name: "列出所有图层",
            desc: "列出 SurfaceFlinger 能跟踪的所有图层名。",
            usage: "找游戏图层:名字通常含包名;跑 --latency 时要取 RequestedLayerState{...} 大括号里的名字。",
            tokens: &["shell", "dumpsys", "SurfaceFlinger", "--list"],
            preset: true,
        },
        CmdEntry {
            name: "游戏帧率覆盖表",
            desc: "查看系统对哪些应用做了帧率覆盖(每应用一组 uid/覆盖值)。",
            usage: "示例:GameFrameRateOverrides={10335, 0 120} 表示 uid 10335 被覆盖为 120fps。列表里没有的游戏 = 未覆盖。",
            tokens: &[
                "shell",
                "dumpsys",
                "SurfaceFlinger",
                "|",
                "grep",
                "-A3",
                "GameFrameRateOverrides",
            ],
            preset: true,
        },
        CmdEntry {
            name: "电池/温度",
            desc: "电量、充电状态与温度(排查帧率时的条件变量)。",
            usage: "关注 status(2 = 充电中)与 temperature(单位 0.1°C,404 = 40.4°C)。",
            tokens: &["shell", "dumpsys", "battery"],
            preset: true,
        },
        CmdEntry {
            name: "热状态",
            desc: "读 Thermal Status:系统温控级别。",
            usage: "0 = 正常,数值越大越烫;过热会强制降频降帧。",
            tokens: &["shell", "dumpsys", "thermalservice"],
            preset: true,
        },
        CmdEntry {
            name: "当前前台应用",
            desc: "看当前前台 Activity(哪只应用在最前)。",
            usage: "管道 | 会被原样传给设备端 shell 执行。",
            tokens: &[
                "shell",
                "dumpsys",
                "activity",
                "activities",
                "|",
                "grep",
                "ResumedActivity",
            ],
            preset: true,
        },
        CmdEntry {
            name: "屏幕分辨率",
            desc: "读逻辑分辨率与实际分辨率。",
            usage: "override 一行表示被手动改过;physical 是物理分辨率。",
            tokens: &["shell", "wm", "size"],
            preset: false,
        },
        CmdEntry {
            name: "屏幕密度",
            desc: "读屏幕密度(dpi)。",
            usage: "排查 UI 缩放异常时用;恢复默认:wm density reset。",
            tokens: &["shell", "wm", "density"],
            preset: false,
        },
        CmdEntry {
            name: "设备型号",
            desc: "读设备型号(ro.product.model)。",
            usage: "多设备时用来确认 -s 选的到底是哪一台。",
            tokens: &["shell", "getprop", "ro.product.model"],
            preset: false,
        },
        CmdEntry {
            name: "系统版本",
            desc: "读 Android 版本号(ro.build.version.release)。",
            usage: "排查系统相关差异时用。",
            tokens: &["shell", "getprop", "ro.build.version.release"],
            preset: false,
        },
        CmdEntry {
            name: "省电模式开关状态",
            desc: "读省电模式:1 = 开启,空/0 = 关闭。",
            usage: "省电模式会压 CPU/GPU 频率,影响游戏帧率。",
            tokens: &["shell", "settings", "get", "global", "low_power"],
            preset: false,
        },
        CmdEntry {
            name: "列出第三方应用",
            desc: "列出已安装的第三方应用包名。",
            usage: "设备侧参数 -3 只是过滤条件(仅第三方),不受加入冲突检查限制。",
            tokens: &["shell", "pm", "list", "packages", "-3"],
            preset: false,
        },
        CmdEntry {
            name: "强制停止应用",
            desc: "强制停止指定包名的应用(示例:KiHan = 王者荣耀)。",
            usage: "把包名换成目标应用,如三角洲行动 com.tencent.tmgp.dfm。",
            tokens: &["shell", "am", "force-stop", "com.tencent.KiHan"],
            preset: false,
        },
        CmdEntry {
            name: "查应用 uid",
            desc: "查应用的 uid(排查 SurfaceFlinger 图层归属时用)。",
            usage: "示例输出 userId=10358;与『游戏帧率覆盖表』里的 uid 对照。",
            tokens: &[
                "shell",
                "dumpsys",
                "package",
                "com.tencent.KiHan",
                "|",
                "grep",
                "userId",
            ],
            preset: false,
        },
        CmdEntry {
            name: "按电源键",
            desc: "模拟按一下电源键(26 = KEYCODE_POWER)。",
            usage: "可把 26 换成其它键码:3 = Home、4 = 返回、187 = 最近任务。",
            tokens: &["shell", "input", "keyevent", "26"],
            preset: false,
        },
        CmdEntry {
            name: "输入文本",
            desc: "向当前焦点输入一段文字。",
            usage: "把 hello world 换成要输入的内容;含空格时整个引号片段算一个参数。",
            tokens: &["shell", "input", "text", "\"hello world\""],
            preset: false,
        },
        CmdEntry {
            name: "屏幕截图到手机",
            desc: "让手机截屏并保存到 /sdcard/screenshot.png。",
            usage: "配『取回截图』把文件拉到电脑。",
            tokens: &["shell", "screencap", "-p", "/sdcard/screenshot.png"],
            preset: false,
        },
        CmdEntry {
            name: "取回截图",
            desc: "把手机上的文件拉到本机(示例:上一档截图)。",
            usage: "末尾是保存到电脑的相对路径(相对本程序的工作目录)。",
            tokens: &["pull", "/sdcard/screenshot.png", "screenshot.png"],
            preset: false,
        },
        CmdEntry {
            name: "无线调试(TCP 模式)",
            desc: "把 adb 切到 TCP 模式,监听 5555 端口。",
            usage: "之后用 adb connect 手机IP:5555 连接;出问题重新插 USB 线即可恢复。",
            tokens: &["tcpip", "5555"],
            preset: false,
        },
        CmdEntry {
            name: "重启设备",
            desc: "重启手机。",
            usage: "立即执行、无确认 —— 慎点;重启后投屏/控制需重新连接。",
            tokens: &["reboot"],
            preset: false,
        },
    ];
    E
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn split_tokens_splits_on_whitespace() {
        assert_eq!(
            split_tokens("  shell   wm  size "),
            v(&["shell", "wm", "size"])
        );
        assert_eq!(split_tokens("a\tb\nc"), v(&["a", "b", "c"]));
        assert!(split_tokens("   \t ").is_empty());
        assert!(split_tokens("").is_empty());
    }

    #[test]
    fn split_tokens_keeps_quoted_token_whole() {
        let t = split_tokens("input text \"hello world\"");
        assert_eq!(t, v(&["input", "text", "\"hello world\""]));
        // 引号原样保留 —— 设备端 shell 自己解释,程序不去壳
        assert!(t[2].starts_with('"') && t[2].ends_with('"'));
    }

    #[test]
    fn check_rejects_empty_fragment() {
        assert!(check_fragment(&[], &[]).is_err());
    }

    #[test]
    fn device_selectors_are_mutually_exclusive() {
        // 空命令栏先加 -d 没问题
        assert!(check_fragment(&[], &v(&["-d"])).is_ok());
        // 已有 -d 再加 -s X:拒绝
        let err = check_fragment(&v(&["-d"]), &v(&["-s", "ABC"])).unwrap_err();
        assert!(err.contains("最多一个"), "{err}");
        // -s X 之后再加普通子命令:放行(值算进参数区,不误伤)
        assert!(check_fragment(&v(&["-s", "ABC"]), &v(&["shell", "wm", "size"])).is_ok());
    }

    #[test]
    fn selector_after_subcommand_is_rejected() {
        let err = check_fragment(&v(&["shell", "wm", "size"]), &v(&["-d"])).unwrap_err();
        assert!(err.contains("子命令"), "{err}");
        // 空命令栏 + shell 之后加 -d 也一样拒(bar 已进入子命令)
        let err = check_fragment(&v(&["reboot"]), &v(&["-e"])).unwrap_err();
        assert!(err.contains("子命令"), "{err}");
        // 但 shell 之后的设备侧 -3/-A 不受影响
        assert!(check_fragment(&v(&["shell", "pm"]), &v(&["-3"])).is_ok());
    }

    #[test]
    fn duplicate_flag_in_header_region_is_rejected() {
        let bar = v(&["-H", "127.0.0.1:5037"]);
        let err = check_fragment(&bar, &v(&["-H", "other:5037"])).unwrap_err();
        assert!(err.contains("重复"), "{err}");
        // 同名片段只出现一次时正常
        assert!(check_fragment(&bar, &v(&["shell", "echo", "hi"])).is_ok());
    }

    #[test]
    fn duplicate_fragment_is_rejected() {
        let bar = v(&[
            "shell",
            "settings",
            "put",
            "system",
            "min_refresh_rate",
            "120",
        ]);
        let err = check_fragment(&bar, &bar.clone()).unwrap_err();
        assert!(err.contains("片段"), "{err}");
        // 差一个词的片段不算重复
        assert!(
            check_fragment(
                &bar,
                &v(&[
                    "shell",
                    "settings",
                    "put",
                    "system",
                    "min_refresh_rate",
                    "60"
                ])
            )
            .is_ok()
        );
    }

    #[test]
    fn device_side_repeats_are_unrestricted() {
        // grep -i、pm -3 这类设备侧参数:重复加入必须放行
        let bar = v(&["shell", "ps", "-A"]);
        assert!(check_fragment(&bar, &v(&["-A"])).is_ok());
        let bar = v(&["shell", "pm", "list", "packages", "-3"]);
        assert!(check_fragment(&bar, &v(&["-3"])).is_ok());
    }

    #[test]
    fn build_command_prepends_serial_when_missing() {
        let (args, display) = build_command("ABC123", &v(&["shell", "wm", "size"]));
        assert_eq!(args[0], "-s");
        assert_eq!(args[1], "ABC123");
        assert_eq!(display, "adb -s ABC123 shell wm size");
    }

    #[test]
    fn build_command_keeps_explicit_selector() {
        let (args, display) = build_command("ABC123", &v(&["-d", "shell", "wm", "size"]));
        assert_eq!(args[0], "-d", "已有设备定向参数时不得再补 -s");
        assert!(!display.contains(" -s "), "{display}");
    }

    #[test]
    fn build_command_without_device_or_selector() {
        let bar = v(&["shell", "wm", "size"]);
        let (args, _) = build_command("   ", &bar);
        assert_eq!(args, bar);
    }

    #[test]
    fn has_device_selector_ignores_device_side_flags() {
        assert!(has_device_selector(&v(&["-s", "X", "shell"])));
        assert!(has_device_selector(&v(&["-d"])));
        assert!(!has_device_selector(&v(&["shell", "-s"])));
        assert!(!has_device_selector(&[]));
    }

    #[test]
    fn display_quotes_tokens_with_spaces() {
        let (_, display) = build_command("X", &v(&["shell", "input", "text", "a b"]));
        assert!(display.contains("\"a b\""), "{display}");
        // 自带引号的 token 原样展示,不再套一层
        let (_, display) = build_command("X", &v(&["shell", "input", "text", "\"a b\""]));
        assert!(
            display.contains("\"a b\"") && !display.contains("\"\"a b\"\""),
            "{display}"
        );
    }

    #[test]
    fn read_capped_truncates_but_keeps_prefix() {
        let small = std::io::Cursor::new(b"hello".to_vec());
        let (s, cut) = read_capped(small, 32);
        assert_eq!(s, "hello");
        assert!(!cut);

        let big = std::io::Cursor::new(vec![b'a'; OUTPUT_CAP + 4096]);
        let (s, cut) = read_capped(big, OUTPUT_CAP);
        assert_eq!(s.len(), OUTPUT_CAP);
        assert!(cut);
    }

    #[test]
    fn user_presets_roundtrip_and_missing_file_is_empty() {
        let dir = std::env::temp_dir().join(format!("scrcpy-pad-presets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join(PRESET_FILE);

        // 不存在 = 空列表,不是错误
        assert_eq!(load_user_presets(&path).unwrap(), Vec::new());

        let list = vec![
            UserPreset {
                name: "钉刷新率".into(),
                tokens: v(&[
                    "shell",
                    "settings",
                    "put",
                    "system",
                    "min_refresh_rate",
                    "120",
                ]),
            },
            UserPreset {
                name: "查前后台".into(),
                tokens: v(&["shell", "dumpsys", "activity", "activities"]),
            },
        ];
        save_user_presets(&path, &list).expect("保存必须成功");
        assert_eq!(load_user_presets(&path).unwrap(), list);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn builtin_entries_are_sane_and_presets_pass_conflict_check() {
        let entries = builtin_entries();
        assert!(
            entries.len() >= 20,
            "内置库至少 20 条,实得 {}",
            entries.len()
        );
        let mut names = std::collections::HashSet::new();
        let mut preset_count = 0;
        for e in entries {
            assert!(!e.name.is_empty() && !e.desc.is_empty() && !e.usage.is_empty());
            assert!(!e.tokens.is_empty());
            assert!(
                e.tokens.iter().all(|t| !t.is_empty()),
                "{} 有空 token",
                e.name
            );
            assert!(names.insert(e.name), "重名: {}", e.name);
            if e.preset {
                preset_count += 1;
                // 预设必须本身合法:能加进空命令栏
                let frag: Vec<String> = e.tokens.iter().map(|s| s.to_string()).collect();
                check_fragment(&[], &frag)
                    .unwrap_or_else(|why| panic!("预设 {} 不合法: {why}", e.name));
            }
        }
        assert!(preset_count >= 5, "预设至少 5 条,实得 {preset_count}");
    }

    /// 真跑一条子进程:验证 run/poll 的收尾路径(echo 在两平台都存在)
    #[test]
    fn run_captures_stdout_and_exit_code() {
        #[cfg(windows)]
        let (exe, args) = ("cmd", v(&["/C", "echo", "adbcmd-test"]));
        #[cfg(not(windows))]
        let (exe, args) = ("sh", v(&["-c", "echo adbcmd-test"]));
        let run = run(exe, args, "test".into());
        let outcome = wait_outcome(&run, 5000).expect("echo 应在 5 秒内结束");
        let done = outcome.done.expect("echo 不应启动失败");
        assert_eq!(done.exit_code, Some(0), "stderr: {}", done.stderr);
        assert!(
            done.stdout.contains("adbcmd-test"),
            "stdout: {}",
            done.stdout
        );
        assert!(!done.killed && !done.truncated);
    }

    /// [停止] 必须能终止长命令并拿到结局(killed=true)
    #[test]
    fn run_stop_kills_long_process() {
        #[cfg(windows)]
        let (exe, args) = ("cmd", v(&["/C", "ping", "-n", "30", "127.0.0.1"]));
        #[cfg(not(windows))]
        let (exe, args) = ("sleep", v(&["30"]));
        let run = run(exe, args, "test".into());
        run.stop();
        let outcome = wait_outcome(&run, 5000).expect("停止后应在 5 秒内收到结局");
        let done = outcome.done.expect("不应启动失败");
        assert!(done.killed, "killed 标记应置位");
    }

    fn wait_outcome(run: &AdbRun, millis: u32) -> Option<AdbRunOutcome> {
        let deadline = Instant::now() + Duration::from_millis(u64::from(millis));
        while Instant::now() < deadline {
            if let Some(o) = run.poll() {
                return Some(o);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        None
    }
}
