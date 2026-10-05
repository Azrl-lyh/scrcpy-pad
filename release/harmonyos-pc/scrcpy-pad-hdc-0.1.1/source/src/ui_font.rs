use std::path::{Path, PathBuf};
use std::sync::Arc;

use eframe::egui;

const LINUX_CANDIDATES: &[&str] = &[
    "/usr/share/fonts/google-noto-sans-cjk-vf-fonts/NotoSansCJK-VF.ttc",
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/wqy-zenhei/wqy-zenhei.ttc",
    "/usr/share/fonts/wqy-microhei/wqy-microhei.ttc",
];

/// Installs a CJK-capable system font as the fallback for both proportional
/// and monospace text. egui's built-in font collection does not contain Chinese
/// glyphs, so a missing fallback shows every Han character as a square.
pub fn install_cjk_font(ctx: &egui::Context) {
    let mut candidates: Vec<PathBuf> = LINUX_CANDIDATES.iter().map(PathBuf::from).collect();

    if cfg!(target_os = "windows") {
        if let Some(windir) = std::env::var_os("WINDIR") {
            let fonts = PathBuf::from(windir).join("Fonts");
            for name in [
                "msyh.ttc",
                "msyh.ttf",
                "NotoSansSC-VF.ttf",
                "Deng.ttf",
                "simhei.ttf",
                "simsun.ttc",
            ] {
                candidates.push(fonts.join(name));
            }
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            let fonts = PathBuf::from(local)
                .join("Microsoft")
                .join("Windows")
                .join("Fonts");
            for name in ["msyh.ttc", "msyh.ttf", "NotoSansSC-VF.ttf", "Deng.ttf"] {
                candidates.push(fonts.join(name));
            }
        }
    }

    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(
            PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("fonts")
                .join("SourceHanSans.ttc"),
        );
    }

    let Some((path, bytes)) = candidates
        .iter()
        .find_map(|path| std::fs::read(path).ok().map(|bytes| (path.clone(), bytes)))
    else {
        eprintln!("[font] No CJK font found; Chinese text may render as squares");
        return;
    };

    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "cjk".to_owned(),
        Arc::new(egui::FontData::from_owned(bytes)),
    );
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .push("cjk".to_owned());
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .push("cjk".to_owned());
    ctx.set_fonts(fonts);
    let _ = path;
}

#[allow(dead_code)]
pub fn load_candidate(path: &Path) -> std::io::Result<Vec<u8>> {
    std::fs::read(path)
}
