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
            || !self.server_path.trim().is_empty()
            || !self.adb_path.trim().is_empty()
    }

    /// 清空全部路径(保留 remember_paths 与启动参数)
    pub fn clear_paths(&mut self) {
        self.scrcpy_path.clear();
        self.server_path.clear();
        self.adb_path.clear();
        self.selected_serial.clear();
    }

    /// 校正路径:只保留**当前确实存在**的文件路径,顺带去掉首尾空白。
    /// 返回是否发生了改动。
    ///
    /// 必要性:用户升级/移动 scrcpy 目录后,记住的旧路径会变成死路径。
    /// 死路径比空路径更糟 —— 空路径还能走"同目录推导 + PATH"的自动逻辑,
    /// 死路径却会让 [启动 scrcpy] 直接失败。因此读到之后立即校验并丢弃。
    pub fn sanitize(&mut self) -> bool {
        let mut changed = false;
        let mut fix = |field: &mut String| {
            let t = field.trim().to_string();
            if t != *field {
                *field = t;
                changed = true;
            }
            if !field.is_empty() && !Path::new(field.as_str()).is_file() {
                field.clear();
                changed = true;
            }
        };
        fix(&mut self.scrcpy_path);
        fix(&mut self.server_path);
        fix(&mut self.adb_path);
        let serial = self.selected_serial.trim().to_string();
        if serial != self.selected_serial {
            self.selected_serial = serial;
            changed = true;
        }
        changed
    }
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
        let mut want = self.pending.take()?;
        if self.saved.as_ref() == Some(&want) {
            return None;
        }
        want = stamped(want);
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

/// 读取设置;文件不存在、内容损坏或读取失败时返回 None(调用方用默认值)
pub fn load() -> Option<Settings> {
    let text = std::fs::read_to_string(path()).ok()?;
    serde_json::from_str::<Settings>(&text).ok()
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
    }

    /// 死路径必须被清空 —— 留着它会让 [启动 scrcpy] 直接失败,
    /// 清空后才能走"同目录推导 + PATH"的自动逻辑
    #[test]
    fn sanitize_drops_nonexistent_paths() {
        let mut s = Settings {
            scrcpy_path: "  /definitely/not/here/scrcpy  ".into(),
            server_path: "/also/not/here/scrcpy-server".into(),
            adb_path: String::new(),
            remember_paths: true,
            scrcpy_args: "--stay-awake".into(),
            selected_serial: " ABC123 ".into(),
            saved_at: 0,
        };
        assert!(s.sanitize(), "存在死路径时 sanitize 应报告改动");
        assert_eq!(s.scrcpy_path, "", "不存在的文件必须清空");
        assert_eq!(s.server_path, "", "不存在的文件必须清空");
        assert_eq!(s.selected_serial, "ABC123", "序列号应去掉首尾空白");
        assert_eq!(s.scrcpy_args, "--stay-awake", "启动参数不受影响");

        // 已干净的内容再调用不应报告改动(否则会每帧写盘)
        assert!(!s.sanitize());
    }

    /// 缓存只在内容真的变化时才报告"需要落盘"。
    ///
    /// 注意:这里**不真的调用 save_if_dirty**(它会写用户真实的 settings.json),
    /// 而是直接验证判定语义 —— 写盘与否完全由这一步的相等判断决定。
    #[test]
    fn cache_skips_identical_content() {
        let base = Settings::default();
        let mut c = SettingsCache::new(base.clone());

        // 登记与"已落盘内容"完全一致的一份:应当不产生待写内容
        c.mark_dirty(base.clone());
        assert!(
            c.pending_for_test().as_ref() == Some(&base),
            "登记的内容应原样保存(时间戳归零)"
        );

        // 关键行为:时间戳不参与"内容是否变化"的判断。
        // 若参与,由于每次登记的时间戳都不同,会变成每帧都写盘。
        let mut stamped = base.clone();
        stamped.saved_at = 12345;
        c.mark_dirty(stamped);
        assert_eq!(
            c.pending_for_test().unwrap().saved_at,
            0,
            "登记时必须把时间戳归零,否则内容永不相等"
        );
        assert_eq!(
            c.pending_for_test().as_ref(),
            Some(&base),
            "归零后应与已保存内容逐字段相等 -> 不触发写盘"
        );
    }

    /// has_any_path / clear_paths 的语义
    #[test]
    fn path_helpers() {
        let mut s = Settings::default();
        assert!(!s.has_any_path());
        s.scrcpy_path = "/x".into();
        assert!(s.has_any_path());
        s.clear_paths();
        assert!(!s.has_any_path());
    }
}
