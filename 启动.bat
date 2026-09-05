@echo off
chcp 936 >nul
cd /d %~dp0

if not exist .\target\release\scrcpy-pad.exe (
    echo 未找到编译产物，开始编译（仅首次需要）...
    cargo build --release
    if errorlevel 1 (
        echo 编译失败，请确保已安装 Rust: https://rustup.rs
        pause
        exit /b 1
    )
)

target\release\scrcpy-pad.exe