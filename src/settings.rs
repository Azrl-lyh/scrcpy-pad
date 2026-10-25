//! 程序级设置(scrcpy 三件套路径 / 启动参数 / 上次设备)的持久化。
//!
//! 为什么单独有一份:键位配置(profile.yaml)是**可传递、可另存**的用户资产,
//! 而"我这台电脑上 scrcpy 装在哪"是本机私有的环境信息 —— 两者混在一起,
//! 用户把配置发给别人时就会带上自己的绝对路径。外观(look.json)同理。
//!
//! 落盘位置与 profile.yaml / look.json 同目录(见 app::config_dir):
//!   Linux   : ~/.config/scrcpy-pad/settings.json
//!   Windows : %APPDATA%\scrcpy-pad\config\settings.json
//!   macOS   : ~/Library/Application Support/dev.scrcpy-pad/settings.json
//! 配置目录不可用时(极少数环境)则退化为"程序旁边的 settings.json"(便携模式)。
//!
//! 写入策略:改动后**延迟到帧末**统一落盘,且内容与上次写出的完全一致时跳过
//! (见 [`SettingsCache`]),因此每帧调用也不会造成反复写盘。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 程序级设置
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// 是否记住 scrcpy 三件套路径。关闭后启动时不再读取本文件里的路径。
    #[serde(default = "default_true")]
    pub remember_paths: bool,
    /// scrcpy 可执行文件路径(空 = 交给自动寻找 / PATH)
    #[serde(default)]
    pub scrcpy_path: String,
    /// scrcpy 所在目录(空 = 未指定)。
    ///
    /// 为什么除了可执行文件路径还要记目录:官方发行包换版本时目录名会变
    /// (scrcpy-win64-v3.1 → v3.3),用户也常常整个搬走。只记死的 exe 路径,
    /// 一旦搬动就"重启后依旧找不到";记着**目录**就能在那一带把它重新找回来。
    #[serde(default)]
    pub scrcpy_dir: String,
    /// scrcpy-server 路径(空 = 由 scrcpy 同目录推导)
    #[serde(default)]
    pub server_path: String,
    /// adb 可执行文件路径(空 = 由 scrcpy 同目录推导,再回退 PATH)
    #[serde(default)]
    pub adb_path: String,
    /// scrcpy 启动参数(为空则用程序内置默认值)
    #[serde(default)]
    pub scrcpy_args: String,
    /// 「其他功能 → 手动 adb 命令」命令栏的内容:token 之间以空格连接
    /// (引号片段保持原样,读回时按同样规则拆开)。
    #[serde(default)]
    pub adb_command: String,
    /// 上次使用的设备序列号(重连同一台手机时优先选中)
    #[serde(default)]
    pub selected_serial: String,
    /// 诊断日志级别(error / warn / info / debug / trace);空 = 用内置默认 info。
    ///
    /// 为什么放进 settings.json 而不是只认环境变量:Windows 用户设环境变量很麻烦,
    /// 而这个开关恰恰是让"问题只在别人机器上出现"时把现场记下来寄回来的唯一手段。
    /// 环境变量 `SCRCPY_PAD_LOG` 优先于本字段(临时排查用,不必改文件)。
    #[serde(default)]
    pub log_level: String,
    /// 当前在用的键位配置文件路径(W0-9);None = 还没记过(用默认的 profile.yaml)。
    ///
    /// 为什么需要:用户在 [配置] 里另选/新建的文件在此之前只活在内存里 ——
    /// "我选的配置重启后不见了"是必然现象,程序每次都回到默认 profile.yaml。
    ///
    /// 为什么**不受 [记住路径] 开关管辖**:那条开关的语义是"scrcpy 装在哪"
    /// (本机环境信息,不该跟着配置发给别人);而"我在编辑哪一份配置"换了却不记得,
    /// 重启后打开的是另一份文件,用户会以为改动丢了 —— 这是正确性问题,不是偏好。
    #[serde(default)]
    pub profile_path: Option<String>,
    /// 宏录制的「开始录制」热键(evdev 键码;0 = 未绑定)。
    ///
    /// 用户 2026-10-12 需求:给现有宏录制流程加**热键入口**(语法糖),
    /// 不改变录制核心逻辑。只支持**单键** —— 组合键无法绑定。
    #[serde(default)]
    pub macro_record_start_key: u16,
    /// 是否启用「开始录制」热键。**未勾选时即使绑了键也不触发任何行为**
    /// (键位值保留)。默认关 = 与从前逐位一致。
    #[serde(default)]
    pub macro_record_start_key_enabled: bool,
    /// 宏录制的「终止录制」热键(evdev 键码;0 = 未绑定)。同样只支持单键。
    #[serde(default)]
    pub macro_record_stop_key: u16,
    /// 是否启用「终止录制」热键。默认关。
    #[serde(default)]
    pub macro_record_stop_key_enabled: bool,
    /// 宏录制的「取消录制」热键(evdev 键码;0 = 未绑定)。
    ///
    /// 用户 2026-10-12 第 1 条:宏要能**取消** —— 与"终止录制"(保存已录内容)
    /// 不同,取消 = **丢弃**这次录制、不写入任何东西。用户明确要求它是
    /// "**全局公共的**":一个键,对所有宏录制生效,不必逐宏设置。
    #[serde(default)]
    pub macro_record_cancel_key: u16,
    /// 是否启用「取消录制」热键。默认关。
    #[serde(default)]
    pub macro_record_cancel_key_enabled: bool,
    /// **鼠标接管方式**:「隔膜」或「遮板」(用户 2026-10-13)。
    ///
    /// **为什么放在 settings.json 而不是 `Look`(外观)里** —— 这是实测出来的
    /// 架构错误:外观是**每套按键组合各存一份**的(`profile.yaml` 里每套 scheme
    /// 都有自己的 `look`),而程序启动时还会用全局的 `look.json` 覆盖它。
    /// 于是"我在外观页选了遮板"会写进**当时生效那套**的 look,一旦切换组合、
    /// 或被 look.json 覆盖,选择就丢了/串了(用户实测:选中 Mask 却跑成了 Shield)。
    ///
    /// 鼠标接管方式是**程序级**偏好(与"宏录制热键"同类),不该跟着按键组合走,
    /// 所以放这里。界面入口仍按用户要求在「外观」页。
    #[serde(default)]
    pub mouse_capture: crate::theme::MouseCaptureMode,
    /// 遮板要显示哪些项目(与「截图取点」的过滤项同一套语义,见 `OverlayFilter`)。
    ///
    /// 用位标志存(`u8`),加一项不会破坏老配置兼容性。
    #[serde(default = "default_mask_filter")]
    pub mask_filter: u8,
    /// **轨迹录制的总开关**(用户 2026-10-14):未勾选时快捷键**不触发**录制
    /// (卡片上的按钮仍可用)。默认关 = 与从前逐位一致。
    #[serde(default)]
    pub track_rec_enabled: bool,
    /// 轨迹录制的**快捷键**(evdev 键码;0 = 未绑定/空缺)。
    ///
    /// 按一下开始、再按一下结束(**同一个键切换**,用户 2026-10-14 拍板)。
    /// 只支持单键 —— 组合键无法绑定(捕获走 `waiting_key`,与宏触发键同路)。
    #[serde(default)]
    pub track_rec_key: u16,
    /// 轨迹录制的**结束快捷键**(用户 2026-10-23 第 2 条:与宏录制一样"共用一对",
    /// 开始/结束各一个键)。
    ///
    /// 0(缺省)= 未绑定 ⇒ 退回旧行为:开始键按一下开始、再按一下结束
    /// (2026-10-14 定下的单键切换口径,老配置不用迁移、逐位一致)。
    #[serde(default)]
    pub track_rec_stop_key: u16,
    /// **独占模式:已绑定键不再传给其它程序**(B21-2/G-b,2026-10-18)。默认关。
    ///
    /// 打开后,映射开启期间**当前配置里被绑定的键**(含 W/A/S/D 这类普通键)
    /// 在 Windows 低级钩子里被吞掉(按下与抬起都吞),其它程序收不到。
    /// 边界:仅在 legacy 消息层生效,拦不住通过 Raw Input 读输入的程序
    /// (原理性天花板,见 app.rs 的 WIN_GRAB_MODES_NOTE)。
    ///
    /// **程序级**(存 settings.json,不跟按键组合走):它是"这台机器上要不要
    /// 独占键盘"的环境偏好,与 mouse_capture 同类。老配置文件没有该字段 ⇒
    /// serde default = false = 与从前逐位一致(一个键都不多吞)。
    #[serde(default)]
    pub swallow_bound_keys: bool,
    /// **映射期间独占键盘设备**(Linux 的 `EVIOCGRAB`,仅非 Windows 使用)。默认关。
    ///
    /// 这是 Linux 侧**唯一的结构性优势**:整把设备被内核独占,同一把键鼠的
    /// 按键在系统里任何前台程序都收不到,不需要"已绑定键"白名单。
    ///
    /// 为什么必须落盘(2026-10-21 修):以前存在 `PadApp` 的字段里,每次启动
    /// 都重置为 false —— 用户勾了"映射时屏蔽原键",下次启动就悄悄失效,
    /// 而且界面上那个勾选框看起来还是"记得"的样子(实际是每次重新建)。
    /// 这是**承诺没兑现**,不是优化:`settings.json` 里没有它,等于这个开关
    /// 只在本进程生命周期内有效。
    ///
    /// **程序级**(存 settings.json):这是"这台机器上要不要独占键盘"的环境偏好,
    /// 与 `mouse_capture` / `swallow_bound_keys` 同类,不跟按键组合走。
    /// 老配置文件没有该字段 ⇒ `#[serde(default)]` = false = 与从前一致
    /// (Windows 侧本来就是死字段,见 `capture.rs` 的 `bound_key_words`)。
    #[serde(default)]
    pub grab_enabled: bool,
    /// **瞄准期间把光标钉住**(B21-3/G-c,2026-10-18)。**默认开**。
    ///
    /// 打开后,FPS 瞄准(`mouse_grab` 为真)期间用 `ClipCursor` 把系统光标关在
    /// 当前位置的 1x1 像素矩形里 —— 它不再满屏乱跑、也撞不到屏幕边。
    /// 与既有的"回中"路径**并存**(回中留作兜底:钉子被目标程序覆盖/忽略时仍能拉回)。
    ///
    /// 为什么默认**开**:钉住只发生在"真的在瞄准"那一刻(判据是既有的
    /// `mouse_grab`,不是新造的),退出瞄准/断连/退出程序都会释放;
    /// 而它解决的问题(甩枪时点错窗口、撞边丢位移)是 FPS 体验的直接改善。
    /// 关掉它 = 回到"只回中不钉住"的从前行为,一条路径都不变。
    ///
    /// **程序级**(存 settings.json):这是"这台机器上要不要钉光标"的环境偏好。
    /// 老配置文件没有该字段 ⇒ serde 走 `default_clip_cursor` = true(新功能默认享受),
    /// 而不是 `#[serde(default)]` 的 false。
    #[serde(default = "default_clip_cursor")]
    pub clip_cursor_in_aim: bool,
    /// 最后写入时间(epoch 秒,仅供用户查看,不参与逻辑)
    #[serde(default)]
    pub saved_at: u64,
}

/// 遮板过滤项的缺省值:与「截图取点」的 `OverlayFilter::default()` 保持一致
/// (键位/组合键/永久轮盘/临时轮盘 = 开;宏/锚点 = 关)。
fn default_mask_filter() -> u8 {
    crate::app::MASK_FILTER_DEFAULT
}

/// serde 默认值函数:remember_paths 缺省为 true(老文件里没有该字段时按"记住"处理)
fn default_true() -> bool {
    true
}

/// serde 默认值函数:瞄准期间钉住光标缺省为 **true**(B21-3/G-c)。
///
/// 老配置文件里没有 `clip_cursor_in_aim` 这个字段 ⇒ 这里返回什么,升级后的行为
/// 就是什么。返回 true 是**有意的**:该功能只在瞄准那一刻生效、四条释放路径
/// 都已实现(见 `capture.rs` 的 G-c 段与消费线程的注释),默认享受它比"默认关、
/// 用户永远不知道有这功能"更合用户口径。
fn default_clip_cursor() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            remember_paths: true,
            scrcpy_path: String::new(),
            scrcpy_dir: String::new(),
            server_path: String::new(),
            adb_path: String::new(),
            scrcpy_args: String::new(),
            adb_command: String::new(),
            selected_serial: String::new(),
            log_level: String::new(),
            profile_path: None,
            macro_record_start_key: 0,
            macro_record_start_key_enabled: false,
            macro_record_stop_key: 0,
            macro_record_stop_key_enabled: false,
            macro_record_cancel_key: 0,
            macro_record_cancel_key_enabled: false,
            mouse_capture: crate::theme::MouseCaptureMode::default(),
            mask_filter: default_mask_filter(),
            track_rec_enabled: false,
            track_rec_key: 0,
            track_rec_stop_key: 0,
            swallow_bound_keys: false,
            grab_enabled: false,
            // B21-3/G-c:默认开(与 serde 的 `default_clip_cursor` 同一个值 ——
            // 两处必须一致,否则"新装"与"老配置文件升级"会走出两种行为)。
            clip_cursor_in_aim: default_clip_cursor(),
            saved_at: 0,
        }
    }
}

impl Settings {
    /// 是否有任何路径被记住(用于启动日志与界面上的"已记住"提示)
    pub fn has_any_path(&self) -> bool {
        !self.scrcpy_path.trim().is_empty()
            || !self.scrcpy_dir.trim().is_empty()
            || !self.server_path.trim().is_empty()
            || !self.adb_path.trim().is_empty()
    }

    /// 清空全部路径(保留 remember_paths 与启动参数)
    ///
    /// 注意**不清** `profile_path`:它记的是"我在编辑哪一份配置",
    /// 不是"scrcpy 装在哪",与 [记住路径] 开关无关(参见该字段的说明)。
    pub fn clear_paths(&mut self) {
        self.scrcpy_path.clear();
        self.scrcpy_dir.clear();
        self.server_path.clear();
        self.adb_path.clear();
        self.selected_serial.clear();
    }

    /// 内容是否一致(**不含** `saved_at`)。
    ///
    /// 必要性:`saved_at` 每次真正落盘都会被重新打上时间戳。若它参与"内容变没变"
    /// 的比较,那么"落盘后的内容"与"下一帧登记的内容"永远不相等 ——
    /// 结果是**每一帧都写一次盘**(界面每 120ms 一帧),既费磁盘又毫无意义。
    /// 判定只应看用户真正改了什么。
    pub fn content_eq(&self, other: &Self) -> bool {
        let mut a = self.clone();
        let mut b = other.clone();
        a.saved_at = 0;
        b.saved_at = 0;
        a == b
    }

    /// 值得"再去重新找一遍 scrcpy"的目录(按优先级)。
    ///
    /// 用于用户搬动/升级 scrcpy 目录后的自动恢复:`scrcpy_dir` 是明确指定的目录,
    /// 其次退到"上次那个 exe 的所在目录"。都取不到就返回空(走自动寻找)。
    pub fn search_dirs(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = Vec::new();
        let dir = self.scrcpy_dir.trim();
        if !dir.is_empty() {
            v.push(PathBuf::from(dir));
        }
        let exe = self.scrcpy_path.trim();
        if !exe.is_empty() {
            if let Some(parent) = Path::new(exe).parent() {
                let p = parent.to_path_buf();
                if !p.as_os_str().is_empty() && !v.contains(&p) {
                    v.push(p);
                }
            }
        }
        v
    }

    /// 校正路径:去掉首尾空白,并**把死路径里的位置信息保留下来**。
    ///
    /// 必要性:用户升级/移动 scrcpy 目录后,记住的旧路径会变成死路径。
    /// 死路径比空路径更糟 —— 空路径还能走"同目录推导 + PATH"的自动逻辑,
    /// 死路径却会让 [启动 scrcpy] 直接失败。所以这里仍会清掉死路径,
    /// 但**先把它的所在目录记进 `scrcpy_dir`**,供启动时在那一带重新找回
    /// (旧版是直接抹掉,于是"重启后依旧找不到 scrcpy"),返回是否发生了改动。
    pub fn sanitize(&mut self) -> bool {
        let mut changed = false;

        // ---- 1) scrcpy 目录字段 ----
        let trimmed = self.scrcpy_dir.trim().to_string();
        if trimmed != self.scrcpy_dir {
            self.scrcpy_dir = trimmed;
            changed = true;
        }
        let mut dir = self.scrcpy_dir.clone();
        if !dir.is_empty() && !Path::new(dir.as_str()).is_dir() {
            // 目录栏里被填成了文件:还给"路径"一栏处理(下面按可执行文件校验)
            if Path::new(dir.as_str()).is_file() && self.scrcpy_path.trim().is_empty() {
                self.scrcpy_path = dir.clone();
            }
            dir.clear();
            changed = true;
        }

        // ---- 2) 三个文件路径 ----
        // 一起取出引用(三个字段互不相交,借用合法),目录提示写在本地的 dir 里
        for field in [
            &mut self.scrcpy_path,
            &mut self.server_path,
            &mut self.adb_path,
        ] {
            let t = field.trim().to_string();
            if t != *field {
                *field = t;
                changed = true;
            }
            if field.is_empty() {
                continue;
            }
            let p = Path::new(field.as_str());
            if p.is_file() {
                continue;
            }
            if p.is_dir() {
                // 用户把**目录**填/选进了"路径"栏:这不算错,记成 scrcpy 目录,
                // 具体可执行文件由启动逻辑在目录里找。
                // (旧版把这个目录当死路径直接抹掉 —— 这正是
                //  "我设置好 scrcpy 目录后,再次启动依旧找不到" 的成因之一。)
                if dir.is_empty() {
                    dir = field.clone();
                }
            } else if dir.is_empty() {
                // 死路径:把"它原来在哪"留下来,启动时可在那一带重新找到
                if let Some(up) = nearest_existing_dir(p, 2) {
                    dir = up.display().to_string();
                }
            }
            field.clear();
            changed = true;
        }
        self.scrcpy_dir = dir;

        // ---- 3) 上次设备 ----
        let serial = self.selected_serial.trim().to_string();
        if serial != self.selected_serial {
            self.selected_serial = serial;
            changed = true;
        }
        changed
    }
}

/// 从一条(已经不存在的)路径出发,向上找最近的**确实存在**的目录。
///
/// `max_up` 是允许跳过的、同样不存在的层数。为什么要往上跳:用户升级 scrcpy 时
/// 常常是"把新版本解压到旁边、删掉旧版本目录" —— 旧 exe 的**父目录也没了**,
/// 只有再上一级还在。记着那一级,启动时就能在附近把新版本重新找回来
/// (搜索是[`crate::adb::find_scrcpy_under`],只往名字像 scrcpy 的子目录里下探,
/// 所以即便记到桌面/下载目录这种大目录也不会拖慢启动)。
///
/// 上溯有限(默认两层):一路退到盘符根目录对用户没有任何意义。
fn nearest_existing_dir(path: &Path, max_up: usize) -> Option<PathBuf> {
    let mut cur = path.parent();
    let mut up = 0;
    while let Some(p) = cur {
        if p.is_dir() {
            return Some(p.to_path_buf());
        }
        if up >= max_up {
            return None;
        }
        up += 1;
        cur = p.parent();
    }
    None
}

/// 延迟写盘的缓存:持有"待写入的内容"与"上次已写出的内容",
/// 两者相同就什么都不做 —— 于是每帧调用 [`Self::save_if_dirty`] 也是安全的。
pub struct SettingsCache {
    pending: Option<Settings>,
    saved: Option<Settings>,
    /// 写入失败只提示一次,避免刷屏
    warned: bool,
}

impl Default for SettingsCache {
    fn default() -> Self {
        Self {
            pending: None,
            saved: None,
            warned: false,
        }
    }
}

impl SettingsCache {
    /// 用刚读出来的设置初始化(视为"已落盘内容",避免启动时立刻回写一遍)
    pub fn new(loaded: Settings) -> Self {
        Self {
            pending: None,
            saved: Some(loaded),
            warned: false,
        }
    }

    /// 只读当前待写内容(供单元测试验证"内容比较"的语义,不参与运行逻辑)
    #[cfg(test)]
    fn pending_for_test(&self) -> Option<Settings> {
        self.pending.clone()
    }

    /// 登记一份"待落盘"的设置(通常来自帧中的界面改动)。
    ///
    /// 这里会把 `saved_at`(写入时间戳)归零后再比较/保存 ——
    /// 时间戳每次都不一样,若参与内容比较,就会变成"每帧都判定为有变化"从而反复写盘。
    /// 真正落盘的那一刻才重新打上时间戳(见 [`Self::save_if_dirty`])。
    pub fn mark_dirty(&mut self, mut s: Settings) {
        s.saved_at = 0;
        self.pending = Some(s);
    }

    /// 把标记过的最新设置写入磁盘;无改动或内容未变时直接返回 None。
    /// 返回 Some(...) 表示"确实尝试了写入"(供调用方决定是否提示用户)。
    pub fn save_if_dirty(&mut self) -> Option<std::io::Result<PathBuf>> {
        let want = self.pending.take()?;
        if let Some(saved) = &self.saved {
            if saved.content_eq(&want) {
                return None;
            }
        }
        let want = stamped(want);
        let path = path();
        if let Some(dir) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                if !self.warned {
                    self.warned = true;
                    return Some(Err(e));
                }
                return None;
            }
        }
        let text = match serde_json::to_string_pretty(&want) {
            Ok(t) => t,
            Err(e) => return Some(Err(std::io::Error::other(e.to_string()))),
        };
        // 原子写盘(W0-9):写到一半被杀进程也不会留下半截 settings.json
        let res = crate::app::write_atomic(&path, &text);
        if res.is_ok() {
            self.saved = Some(want);
            self.warned = false;
        }
        Some(res.map(|_| path))
    }
}

/// 设置文件路径(与 profile.yaml / look.json 同目录;无配置目录时退化为程序旁边)
pub fn path() -> PathBuf {
    crate::app::config_dir().join("settings.json")
}

/// 读取设置;文件不存在、内容损坏或读取失败时返回 None(调用方用默认值)。
///
/// 这里走程序里唯一那份"容忍 BOM 的配置文件读取"(见
/// [`crate::app::read_config_text_graded`]):Windows 编辑器加上的 BOM 曾经会让
/// 整份设置被当成损坏而退回默认值,进而在下一次落盘时被覆盖掉。
///
/// 失败分级(W0-9):文件不存在 = 首次运行,静默;读不动(权限/被占用)= 打日志;
/// 解析失败 = 打日志 + 留带时间戳的 `.broken` 备份(不再每次覆盖同一份)。
/// 本函数在启动早期(界面尚未建好)运行,所以提示走 `eprintln!` + diag,
/// 不经过 PadApp 的日志区。
pub fn load() -> Option<Settings> {
    let path = path();
    let text = match crate::app::read_config_text_graded(&path) {
        Ok(Some(t)) => t,
        Ok(None) => return None,
        Err(e) => {
            crate::diag_warn!("settings", "{e},改用默认设置");
            return None;
        }
    };
    match serde_json::from_str::<Settings>(&text) {
        Ok(s) => Some(s),
        Err(e) => {
            let mut msg = format!("{} 解析失败({e}),改用默认设置", path.display());
            match crate::app::backup_broken_config(&path) {
                Ok(b) => msg.push_str(&format!(";原文件已备份为 {}", b.display())),
                Err(be) => msg.push_str(&format!(";备份也失败({be})")),
            }
            crate::diag_error!("settings", "{msg}");
            None
        }
    }
}

/// 生成一份带当前时间戳的设置副本(落盘前调用)
pub fn stamped(mut s: Settings) -> Settings {
    s.saved_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 老版本写出的、缺字段的 json 必须仍能读入,且 remember_paths 缺省为 true
    #[test]
    fn legacy_settings_json_still_loads() {
        let s: Settings = serde_json::from_str(r#"{ "scrcpy_path": "/usr/bin/scrcpy" }"#).unwrap();
        assert!(s.remember_paths, "缺省应视为记住");
        assert_eq!(s.scrcpy_path, "/usr/bin/scrcpy");
        assert_eq!(s.scrcpy_args, "");
        assert_eq!(s.adb_command, "", "老文件没有命令栏字段,缺省为空");
        assert_eq!(s.scrcpy_dir, "", "老文件没有目录字段,缺省为空");
        assert!(
            s.profile_path.is_none(),
            "老文件没有配置路径字段 -> 视为没记过(用默认 profile.yaml)"
        );
    }

    /// **轨迹录制快捷键配置的往返 + 老配置兼容**(用户 2026-10-14)。
    ///
    /// 两件事都必须成立:
    /// ① 用户在界面上设的三项(启用 / 开始键 / 结束键)**重启后还在** ——
    ///    否则"我设好了快捷键,重启又变空缺"就是必然故障;
    /// ② 老 settings.json(没有这两个字段)**仍能读入**,且缺省 = 与从前一致
    ///    (未启用 + 空缺 ⇒ 快捷键不触发,不会因为升级了一下程序就突然开始录轨迹)。
    #[test]
    fn track_recording_trigger_roundtrips_and_legacy_json_stays_inert() {
        // ① 往返:两项都改成"非默认"值,存取一圈必须原样回来
        let s = Settings {
            track_rec_enabled: true,
            track_rec_key: 88,
            track_rec_stop_key: 89,
            ..Settings::default()
        };
        let text = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&text).unwrap();
        assert!(back.track_rec_enabled, "启用状态必须能存回来");
        assert_eq!(back.track_rec_key, 88, "开始键必须能存回来");
        assert_eq!(back.track_rec_stop_key, 89, "结束键必须能存回来");
        assert_eq!(back, s, "除上述两项外不该有字段被改动");
        // ② 老配置:完全没有这两个字段 ⇒ 缺省必须是"惰性"的
        let old: Settings =
            serde_json::from_str(r#"{ "scrcpy_path": "/usr/bin/scrcpy" }"#).unwrap();
        assert!(
            !old.track_rec_enabled,
            "老配置必须缺省为**未启用**(升级不该让录制突然生效)"
        );
        assert_eq!(old.track_rec_key, 0, "缺省快捷键应为空缺 ⇒ 不触发");
        assert_eq!(
            old.track_rec_stop_key, 0,
            "缺省结束键应为空缺 ⇒ 退回单键切换(升级不改变手感)"
        );
        // ③ 曾经存在的字段(鼠标键触发方式)从配置里**读不出来也不再需要** ——
        //    用户 2026-10-14 明确要去掉鼠标控制录制;老文件里若残留该字段,
        //    必须被**静默忽略**而不是解析失败(否则用户重启后整份设置回默认)。
        let stale: Settings = serde_json::from_str(
            r#"{ "track_rec_enabled": true, "track_rec_trigger": "mouse_right", "track_rec_key": 88 }"#,
        )
        .expect("残留的旧字段必须被忽略而不是报错");
        assert!(
            stale.track_rec_enabled && stale.track_rec_key == 88,
            "旧文件里其余字段仍要正常读入"
        );
    }

    /// **T3(B21-2/G-b):`swallow_bound_keys` 的往返 + 缺省 + 老配置兼容**。
    ///
    /// 三件事都必须成立(§5 配置字段新增证据矩阵):
    /// ① 缺省 = false(独占模式默认关,与从前逐位一致);
    /// ② 写 true 存盘再读回 = true(用户勾了重启还在);
    /// ③ 老 JSON(**没有**该字段)解析不报错且 = false —— 升级程序不许让
    ///    吞键突然生效,也不许整份设置回默认。
    #[test]
    fn swallow_bound_keys_roundtrips_and_legacy_json_defaults_off() {
        // ① 缺省
        assert!(
            !Settings::default().swallow_bound_keys,
            "独占模式必须默认关"
        );
        // ② 往返
        let s = Settings {
            swallow_bound_keys: true,
            ..Settings::default()
        };
        let text = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&text).unwrap();
        assert!(back.swallow_bound_keys, "勾选状态必须能存回来");
        assert_eq!(back, s, "除该字段外不该有字段被改动");
        // ③ 老 JSON(无该字段):不报错、= false
        let old: Settings =
            serde_json::from_str(r#"{ "scrcpy_path": "/usr/bin/scrcpy" }"#).unwrap();
        assert!(
            !old.swallow_bound_keys,
            "老配置必须缺省为**关**(升级不该让吞键突然生效)"
        );
        assert_eq!(old.scrcpy_path, "/usr/bin/scrcpy", "其余字段照常读入");
        // ③' 上一代真实字段集(track_rec 时代)也必须过
        let recent: Settings = serde_json::from_str(
            r#"{ "remember_paths": true, "track_rec_enabled": true, "track_rec_key": 88, "saved_at": 1760000000 }"#,
        )
        .expect("上一代配置必须能读入");
        assert!(!recent.swallow_bound_keys, "上一代配置缺省为关");
        assert!(recent.track_rec_enabled && recent.track_rec_key == 88);
    }

    /// **`grab_enabled` 的往返 + 缺省 + 老配置兼容**(2026-10-21 第 6 类欠账:
    /// Linux 唯一的结构性优势开关曾经**根本不落盘**,每���启动重置为关)。
    ///
    /// 三件事(§5 配置字段新增证据矩阵):
    /// ① 缺省 = false(与从前逐位一致:不勾就不独占);
    /// ② 写 true 存盘再读回 = true —— **这一条是本次修复的核心**:
    ///    以前这个状态只活在 `PadApp` 结构体里,`set()` 里没有它,
    ///    于是"勾了 → 重启 → 悄悄变回关";
    /// ③ 老 JSON(没有该字段)解析不报错且 = false。
    #[test]
    fn grab_enabled_roundtrips_and_legacy_json_defaults_off() {
        // ① 缺省
        assert!(
            !Settings::default().grab_enabled,
            "独占键盘必须默认关(不勾就不独占)"
        );
        // ② 往返:勾上之后必须能存回来(这正是以前丢掉的那一段)
        let s = Settings {
            grab_enabled: true,
            ..Settings::default()
        };
        let text = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&text).unwrap();
        assert!(
            back.grab_enabled,
            "grab 勾选状态必须能存回来 —— 丢了它等于用户每次启动都要重勾一次"
        );
        assert_eq!(back, s, "除该字段外不该有字段被改动");
        // ③ 老 JSON(无该字段):不报错、= false
        let old: Settings =
            serde_json::from_str(r#"{ "scrcpy_path": "/usr/bin/scrcpy" }"#).unwrap();
        assert!(
            !old.grab_enabled,
            "老配置必须缺省为**关**(升级不该让键盘突然被独占)"
        );
        assert_eq!(old.scrcpy_path, "/usr/bin/scrcpy", "其余字段照常读入");
    }

    /// **B21-3/G-c:`clip_cursor_in_aim` 的往返 + 缺省 + 老配置兼容**。
    ///
    /// 三件事(§5 配置字段新增证据矩阵):
    /// ① 缺省 = **true**(新功能默认享受;注意与 `swallow_bound_keys` 默认 false 不同,
    ///    这一条是用户口径下"FPS 体验的直接改善",不是有风险的独占行为);
    /// ② 写 false 存盘再读回 = false(用户关掉后重启必须还是关);
    /// ③ 老 JSON(**没有**该字段)解析不报错且 = true —— 升级不该让用户失去这个功能,
    ///    也不许整份设置回默认。
    #[test]
    fn clip_cursor_in_aim_roundtrips_and_legacy_json_defaults_on() {
        // ① 缺省:开(而且是与 serde 默认值**同一个**值)
        assert!(
            Settings::default().clip_cursor_in_aim,
            "钉住光标必须默认开(与 default_clip_cursor 一致)"
        );
        assert!(
            default_clip_cursor(),
            "serde 默认值必须与 Default 实现一致,否则新装与老配置升级走两种行为"
        );
        // ② 往返:关掉后必须能存回来
        let s = Settings {
            clip_cursor_in_aim: false,
            ..Settings::default()
        };
        let text = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&text).unwrap();
        assert!(
            !back.clip_cursor_in_aim,
            "关掉的状态必须能存回来(否则用户每次重启都要重新关一遍)"
        );
        assert_eq!(back, s, "除该字段外不该有字段被改动");
        // ③ 老 JSON(无该字段):不报错、= true(升级默认享受)
        let old: Settings =
            serde_json::from_str(r#"{ "scrcpy_path": "/usr/bin/scrcpy" }"#).unwrap();
        assert!(
            old.clip_cursor_in_aim,
            "老配置必须缺省为**开**(升级后默认就有这个功能)"
        );
        assert_eq!(old.scrcpy_path, "/usr/bin/scrcpy", "其余字段照常读入");
        // ③' 显式写了 false 的配置**不许**被默认值盖掉
        let opted_out: Settings =
            serde_json::from_str(r#"{ "clip_cursor_in_aim": false }"#).unwrap();
        assert!(
            !opted_out.clip_cursor_in_aim,
            "显式的 false 被默认值盖掉了 —— 用户关不掉这个功能"
        );
    }

    /// W0-9:记住的配置文件路径必须能原样存取(重启后 app 靠它把配置读回来)
    #[test]
    fn profile_path_roundtrips_through_json() {
        let s = Settings {
            profile_path: Some(r"D:\配置\我的键位.yaml".into()),
            ..Settings::default()
        };
        let text = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&text).unwrap();
        assert_eq!(back.profile_path, s.profile_path);
        assert_eq!(back, s, "除 profile_path 外也不该有字段被改动");
    }

    /// 死路径必须被清空 —— 留着它会让 [启动 scrcpy] 直接失败,
    /// 清空后才能走"同目录推导 + PATH"的自动逻辑。
    ///
    /// 同时**必须留下位置线索**:用户搬动/升级 scrcpy 目录后,
    /// 记着它原来在哪个目录,启动时才能在那附近重新找回;
    /// 旧版把位置信息一并抹掉,于是"重启后依旧找不到 scrcpy"。
    #[test]
    fn sanitize_drops_nonexistent_paths_but_keeps_location_hint() {
        let dir = std::env::temp_dir();
        let gone = dir.join("scrcpy-pad-这个文件不存在.exe");
        let mut s = Settings {
            scrcpy_path: format!("  {}  ", gone.display()),
            server_path: String::new(),
            adb_path: String::new(),
            scrcpy_dir: String::new(),
            remember_paths: true,
            scrcpy_args: "--stay-awake".into(),
            adb_command: "shell settings get global low_power".into(),
            selected_serial: " ABC123 ".into(),
            grab_enabled: false,
            log_level: String::new(),
            profile_path: Some("/tmp/我的配置.yaml".into()),
            macro_record_start_key: 0,
            macro_record_start_key_enabled: false,
            macro_record_stop_key: 0,
            macro_record_stop_key_enabled: false,
            macro_record_cancel_key: 0,
            macro_record_cancel_key_enabled: false,
            mouse_capture: crate::theme::MouseCaptureMode::default(),
            mask_filter: default_mask_filter(),
            track_rec_enabled: false,
            track_rec_key: 0,
            track_rec_stop_key: 0,
            swallow_bound_keys: false,
            clip_cursor_in_aim: default_clip_cursor(),
            saved_at: 0,
        };
        assert!(s.sanitize(), "存在死路径时 sanitize 应报告改动");
        assert_eq!(s.scrcpy_path, "", "不存在的文件必须清空");
        assert_eq!(s.selected_serial, "ABC123", "序列号应去掉首尾空白");
        assert_eq!(s.scrcpy_args, "--stay-awake", "启动参数不受影响");
        assert_eq!(
            s.adb_command, "shell settings get global low_power",
            "命令栏内容不属于路径清理范围,必须原样保留"
        );
        assert_eq!(
            s.profile_path.as_deref(),
            Some("/tmp/我的配置.yaml"),
            "sanitize 不该动『在编辑哪一份配置』这条记忆"
        );
        // 死路径的所在目录存在 -> 必须被记下来供启动时重新寻找
        assert_eq!(
            PathBuf::from(&s.scrcpy_dir),
            dir,
            "死路径的位置线索必须保留,否则无法在附近重新找到 scrcpy"
        );
        assert_eq!(s.search_dirs(), vec![dir]);

        // 已干净的内容再调用不应报告改动(否则会每帧写盘)
        assert!(!s.sanitize());
    }

    /// 用户把**目录**填进"scrcpy 路径"栏时必须被当作目录接受(而不是当死路径丢掉)
    /// —— 用户原话就是"我设置好 scrcpy 目录后…"。
    #[test]
    fn sanitize_accepts_a_directory_as_scrcpy_location() {
        let dir = std::env::temp_dir();
        let mut s = Settings {
            scrcpy_path: dir.display().to_string(),
            scrcpy_dir: String::new(),
            ..Settings::default()
        };
        assert!(s.sanitize());
        assert_eq!(s.scrcpy_path, "", "目录不该留在可执行文件栏里");
        assert_eq!(
            s.scrcpy_dir,
            dir.display().to_string(),
            "目录必须被记为 scrcpy 目录,启动时才好在里面找 scrcpy.exe"
        );
        // 目录本身就是"位置",必须算作已记住的路径
        assert!(s.has_any_path());
        assert_eq!(s.search_dirs(), vec![dir.clone()]);
    }

    /// 缓存只在内容真的变化时才报告"需要落盘"。
    ///
    /// 注意:这里**不真的调用 save_if_dirty**(它会写用户真实的 settings.json),
    /// 而是直接验证判定语义 —— 写盘与否完全由 content_eq 这一步决定。
    #[test]
    fn cache_skips_identical_content() {
        let mut base = Settings::default();
        // 模拟"刚从磁盘读出来"的设置:带上一轮落盘的时间戳
        base.saved_at = 1_789_821_325;
        let mut zeroed = base.clone();
        zeroed.saved_at = 0; // mark_dirty 会把时间戳归零
        let mut c = SettingsCache::new(base.clone());

        c.mark_dirty(base.clone());
        assert!(
            c.pending_for_test().as_ref() == Some(&zeroed),
            "登记的内容应原样保存(时间戳归零)"
        );

        // 关键行为:时间戳不参与"内容是否变化"的判断。
        // 若参与,那么"已落盘内容(带时间戳)"与"下一帧登记的内容(时间戳归零)"
        // 永远不相等 —— 结果是每帧都写一次盘(界面 120ms 一帧)。
        assert!(
            base.content_eq(&zeroed),
            "只有时间戳不同时,必须判定为『内容没变』-> 不写盘"
        );
        // 真正的字段变化必须被识别出来
        let mut changed = zeroed.clone();
        changed.scrcpy_path = "/opt/scrcpy/scrcpy".into();
        assert!(!base.content_eq(&changed), "路径变了就必须写盘");
        let mut flag = zeroed.clone();
        flag.remember_paths = !base.remember_paths;
        assert!(
            !base.content_eq(&flag),
            "『记住路径』开关本身也必须能落盘(否则用户关了它下次又变回开着)"
        );
    }

    /// has_any_path / clear_paths 的语义
    #[test]
    fn path_helpers() {
        let mut s = Settings::default();
        assert!(!s.has_any_path());
        s.scrcpy_path = "/x".into();
        assert!(s.has_any_path());
        s.scrcpy_dir = "/y".into();
        s.clear_paths();
        assert!(!s.has_any_path());
        assert!(s.scrcpy_dir.is_empty(), "clear_paths 也必须清掉目录记忆");
    }

    /// search_dirs 的优先级:明确的 scrcpy 目录 > 上次 exe 的所在目录;去重
    #[test]
    fn search_dirs_prefers_explicit_dir() {
        let s = Settings {
            scrcpy_path: "/opt/scrcpy/scrcpy".into(),
            scrcpy_dir: "/data/scrcpy".into(),
            ..Settings::default()
        };
        assert_eq!(
            s.search_dirs(),
            vec![PathBuf::from("/data/scrcpy"), PathBuf::from("/opt/scrcpy")]
        );
        // 只记住 exe 时,退回它的所在目录
        let s2 = Settings {
            scrcpy_path: "/opt/scrcpy/scrcpy".into(),
            ..Settings::default()
        };
        assert_eq!(s2.search_dirs(), vec![PathBuf::from("/opt/scrcpy")]);
        // 什么都没有 -> 空(走自动寻找)
        assert!(Settings::default().search_dirs().is_empty());
    }
}
