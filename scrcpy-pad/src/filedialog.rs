//! 原生文件对话框(非阻塞封装)。
//! Linux 走 xdg-desktop-portal(纯 Rust zbus,无 GTK 构建依赖);
//! Windows 走 Win32 原生对话框。
//! 对话框在独立线程中打开,结果通过 channel 回收,UI 线程每帧轮询,期间界面保持响应。

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};

pub struct FileDialogHandle {
    rx: Receiver<Option<PathBuf>>,
}

impl FileDialogHandle {
    /// 非阻塞取结果:Some(None)=用户取消;None=对话框仍开着
    pub fn try_result(&self) -> Option<Option<PathBuf>> {
        self.rx.try_recv().ok()
    }
}

/// 对话框形态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// 打开一个已有文件
    Open,
    /// 另存为
    Save,
    /// 选择文件夹(选 scrcpy 所在目录用)
    Folder,
}

/// 打开"选择文件"对话框
pub fn pick_file() -> FileDialogHandle {
    spawn_dialog(Mode::Open, None)
}

/// 打开"选择文件夹"对话框
pub fn pick_folder() -> FileDialogHandle {
    spawn_dialog(Mode::Folder, None)
}

/// 打开"另存为"对话框,default_name 为预填文件名
pub fn save_file(default_name: &str) -> FileDialogHandle {
    spawn_dialog(Mode::Save, Some(default_name.to_string()))
}

fn spawn_dialog(mode: Mode, default_name: Option<String>) -> FileDialogHandle {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let r = run_dialog(mode, default_name);
        let _ = tx.send(r);
    });
    FileDialogHandle { rx }
}

#[cfg(windows)]
fn run_dialog(mode: Mode, default_name: Option<String>) -> Option<PathBuf> {
    let mut dlg = rfd::FileDialog::new();
    if let Some(n) = &default_name {
        dlg = dlg.set_file_name(n);
    }
    match mode {
        Mode::Save => dlg.save_file(),
        Mode::Folder => dlg.pick_folder(),
        Mode::Open => dlg.pick_file(),
    }
}

#[cfg(target_os = "linux")]
fn run_dialog(mode: Mode, default_name: Option<String>) -> Option<PathBuf> {
    pollster::block_on(async {
        let mut dlg = rfd::AsyncFileDialog::new();
        if let Some(n) = &default_name {
            dlg = dlg.set_file_name(n);
        }
        match mode {
            Mode::Save => dlg.save_file().await.map(|h| h.path().to_path_buf()),
            Mode::Folder => dlg.pick_folder().await.map(|h| h.path().to_path_buf()),
            Mode::Open => dlg.pick_file().await.map(|h| h.path().to_path_buf()),
        }
    })
}
