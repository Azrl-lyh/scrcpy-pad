//! 中文字体发现(fontconfig 兜底)。
//!
//! `app::install_cjk_font` 里那份硬编码路径清单只覆盖 Debian/Ubuntu/Arch 的几种
//! 常见布局 —— Fedora(`/usr/share/fonts/google-noto-cjk/`)、openSUSE、NixOS、
//! 以及任何自定义安装都整条落空,界面汉字变方块而用户无从判断原因。
//!
//! 这里补上「问 fontconfig 要」这一步。两个判据上的坑(2026-10-19 WSL 实测):
//!
//! 1. **不能用 `fc-match`**。在没有中文字体的系统上(实测 Ubuntu-24.04 未装
//!    `fonts-noto-cjk`),`fc-match "sans-serif:lang=zh-cn"` 照样返回
//!    `/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf` —— fontconfig 的默认回退。
//!    该文件**不含汉字**,于是我们会"成功加载"一个假字体、还吞掉原本那条
//!    「未找到中文字体」告警,比不兜底更糟。
//! 2. **必须用 `fc-list :lang=zh-cn`**。它的语义是"确实声明支持中文的字体",
//!    没有就返回空(实测 24.04 返回 0 行,20.04 返回 4 行 Noto CJK)。
//!
//! 拿到的可能是多行(Bold/Serif/Sans 各一款),所以按"更像界面正文"打分挑一个。

/// 从 `fc-list :lang=zh-cn file` 的输出里挑一个最适合当界面正文的中文字体。
///
/// 打分意图:优先无衬线(sans)、优先常规字重(排除 Bold/Italic)、排除 Serif。
/// 同分时取字典序最小者 —— 只为了**确定性**(同一台机器每次启动命中同一个字体,
/// 不会因为 fontconfig 输出顺序抖动而换字体)。
// 非 Linux 的**非测试**构建里,fontconfig_cjk_font 走 cfg(not(linux)) 分支直接
// 返回 None,于是这个纯函数只在测试里被调用 —— 不标 allow 就会多一条 dead_code
// 告警,而 clippy 门禁是零增量基线(36 对)。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn pick_cjk_font(listing: &str) -> Option<String> {
    let mut best: Option<(i32, String)> = None;
    for line in listing.lines() {
        let path = line.trim();
        // **不能用 Path::is_absolute()** —— 在 Windows 上它对 "/usr/share/..." 返回
        // false(Windows 要求盘符前缀),于是这份解析在本机测试里会把每一行都当
        // 相对路径丢掉。fontconfig 的输出永远是 POSIX 绝对路径,直接判首字符即可,
        // 且这样解析器在哪个平台上跑行为都一致(本模块的测试在 Windows 上跑)。
        if !path.starts_with('/') {
            continue;
        }
        // fontconfig 偶尔会在路径后附加 ":" 之类的后缀(取决于 -f 格式),切掉
        let path = path
            .split_whitespace()
            .next()
            .unwrap_or(path)
            .trim_end_matches(':');
        let lower = path.to_ascii_lowercase();
        if !lower.ends_with(".ttf")
            && !lower.ends_with(".ttc")
            && !lower.ends_with(".otf")
            && !lower.ends_with(".otc")
        {
            continue;
        }
        // **必须带中文线索**,这是安全闸:实测 Ubuntu-24.04 未装中文字体时
        // `fc-match` 会回 DejaVuSans(不含汉字)。若这里放行任意文件,就会
        // "成功加载"一个假字体并吞掉告警。线索按发行版实际文件名列出。
        let cjk_clue = lower.contains("cjk")
            || lower.contains("wqy")
            || lower.contains("sourcehan")
            || lower.contains("source-han")
            || lower.contains("src-han")
            || lower.contains("han")
            || lower.contains("uming")
            || lower.contains("ukai")
            || lower.contains("arphic")
            || lower.contains("fallback"); // DroidSansFallbackFull.ttf 等
        if !cjk_clue {
            continue;
        }
        let mut score = 0i32;
        if lower.contains("sans") {
            score += 3;
        }
        if lower.contains("serif") && !lower.contains("sans") {
            score -= 3;
        }
        score += 2;
        if lower.contains("bold") {
            score -= 2;
        }
        if lower.contains("italic") || lower.contains("oblique") {
            score -= 1;
        }
        // 纯英文字体即便出现在列表里也不要(wqy-zenhei 之类名字不含 sans,但含 wqy 已加分)
        let better = match &best {
            None => true,
            Some((bs, bp)) => score > *bs || (score == *bs && path < bp.as_str()),
        };
        if better {
            best = Some((score, path.to_string()));
        }
    }
    best.map(|(_, p)| p)
}

/// 问 fontconfig 要一个确实声明支持中文的字体文件。没有可用 fontconfig / 没装
/// 中文字体 / 命令执行失败一律返回 `None`(调用方据此打印原有告警)。
///
/// 非 Linux 平台直接 `None`:macOS 走系统字体栈,Windows 在硬编码清单里已有 msyh。
pub fn fontconfig_cjk_font() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let out = std::process::Command::new("fc-list")
            .args([":lang=zh-cn", "file"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let picked = pick_cjk_font(&text)?;
        // 二次确认文件真的可读,免得把 fontconfig 的失效缓存当成功
        std::fs::metadata(&picked).ok()?;
        Some(picked)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真机原文(2026-10-19 Ubuntu-20.04 `fc-list :lang=zh-cn file` 的实际输出)。
    /// 冻结成 golden:四款里必须挑中**常规无衬线**那款,不能挑 Bold/Serif。
    #[test]
    fn picks_the_regular_sans_font_from_real_fontconfig_output() {
        let listing = "\
/usr/share/fonts/opentype/noto/NotoSansCJK-Bold.ttc
/usr/share/fonts/opentype/noto/NotoSerifCJK-Regular.ttc
/usr/share/fonts/opentype/noto/NotoSerifCJK-Bold.ttc
/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc
";
        assert_eq!(
            pick_cjk_font(listing).as_deref(),
            Some("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc")
        );
    }

    /// 阴性对照(真实踩坑):Ubuntu-24.04 没装中文字体时 `fc-match` 会回 DeviVuSans,
    /// 而 `fc-list :lang=zh-cn` 返回**空**。空输入必须得到 `None`,
    /// 绝不许"回退到某个英文字体"——那会让界面显示方块却没有告警。
    #[test]
    fn empty_listing_yields_none_so_the_warning_still_fires() {
        assert_eq!(pick_cjk_font(""), None);
        assert_eq!(pick_cjk_font("\n\n  \n"), None);
        // DejaVu 不含汉字。它混进列表时**必须被拒**,否则界面会显示方块却没有
        // 任何告警(这正是 fc-match 兜底的坑,见本文件头)。
        assert_eq!(
            pick_cjk_font("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf\n"),
            None
        );
        // 阳性对照:同样混在一起时,真正的中文字体仍要被选中
        assert_eq!(
            pick_cjk_font(
                "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf\n\
/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf\n"
            ),
            Some("/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf".to_string())
        );
    }

    /// 有多个候选时同分要**确定**:排序依赖 fontconfig 输出顺序的话,同一台机器
    /// 每次启动可能换字体(字体度量不同 ⇒ 界面轻微跳动)。
    #[test]
    fn ties_are_broken_deterministically() {
        let a = pick_cjk_font(
            "/usr/share/fonts/a/wqy-zenhei.ttc\n/usr/share/fonts/z/wqy-microhei.ttc\n",
        );
        let b = pick_cjk_font(
            "/usr/share/fonts/z/wqy-microhei.ttc\n/usr/share/fonts/a/wqy-zenhei.ttc\n",
        );
        assert_eq!(a, b, "输出顺序不同不许改变选择结果");
        assert_eq!(a.as_deref(), Some("/usr/share/fonts/a/wqy-zenhei.ttc"));
    }

    /// 非字体路径/相对路径不许被当成字体(防止把以 ':' 分隔的附属字段读进来)
    #[test]
    fn ignores_non_font_entries() {
        assert_eq!(pick_cjk_font("relative/NotoSansCJK-Regular.ttc"), None);
        assert_eq!(pick_cjk_font("/usr/share/fonts/noto-cjk/README.txt"), None);
        assert_eq!(
            pick_cjk_font(
                "/usr/share/fonts/noto-cjk/NotoSansCJK-Bold.ttc: Noto Sans CJK:style=Bold"
            ),
            Some("/usr/share/fonts/noto-cjk/NotoSansCJK-Bold.ttc".to_string())
        );
    }
}
