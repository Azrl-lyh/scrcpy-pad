//! 程序级设置(scrcpy 三件套路径 / 启动参数 / 上次设备)的持久化。
//!
//! 为什么单独有一份:键位配置(profile.json)是**可传递、可另存**的用户资产,
//! 而"我这台电脑上 scrcpy 装在哪"是本机私有的环境信息 —— 两者混在一起,
//! 用户把配置发给别人时就会带上自己的绝对路径。外观(look.json)同理。
//!
//! 落盘位置与 profile.json / look.json 同目录(见 app::config_dir):
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
    /// 上次使用的设备序列号(重连同一台手机时优先选中)
    #[serde(default)]
    pub selected_serial: String,
    /// 最后写入时间(epoch 秒,仅供用户查看,不参与逻辑)
    #[serde(default)]
    pub saved_at: u64,
}

/// serde 默认值函数:remember_paths 缺省为 true(老文件里没有该字段时按"记住"处理)
fn default_true() -> bool {
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
            selected_serial: String::new(),
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
        let res = std::fs::write(&path, text);
        if res.is_ok() {
            self.saved = Some(want);
            self.warned = false;
        }
        Some(res.map(|_| path))
    }
}

/// 设置文件路径(与 profile.json / look.json 同目录;无配置目录时退化为程序旁边)
pub fn path() -> PathBuf {
    crate::app::config_dir().join("settings.json")
}

/// 读取设置;文件不存在、内容损坏或读取失败时返回 None(调用方用默认值)。
///
/// 这里走程序里唯一那份"容忍 BOM 的配置文件读取"(见
/// [`crate::app::read_config_text`]):Windows 编辑器加上的 BOM 曾经会让
/// 整份设置被当成损坏而退回默认值,进而在下一次落盘时被覆盖掉。
pub fn load() -> Option<Settings> {
    let path = path();
    let text = crate::app::read_config_text(&path)?;
    match serde_json::from_str::<Settings>(&text) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("[settings] {} 解析失败({e}),改用默认设置", path.display());
            crate::app::backup_broken_config(&path);
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
        assert_eq!(s.scrcpy_dir, "", "老文件没有目录字段,缺省为空");
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
            selected_serial: " ABC123 ".into(),
            saved_at: 0,
        };
        assert!(s.sanitize(), "存在死路径时 sanitize 应报告改动");
        assert_eq!(s.scrcpy_path, "", "不存在的文件必须清空");
        assert_eq!(s.selected_serial, "ABC123", "序列号应去掉首尾空白");
        assert_eq!(s.scrcpy_args, "--stay-awake", "启动参数不受影响");
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
