@echo off
chcp 65001 > nul
cd /d %~dp0

rem 程序位置按以下顺序决定:
rem   1) 已打包好的发布目录(release\windows-x86_64\)
rem   2) cargo 的构建产物(target\release\)
rem   3) 都没有就先编译一次,再用 target\release\ 里的新产物
rem 这样"双击就能用",不依赖是否事先跑过 release\build.bat。
set "EXE=.\release\windows-x86_64\scrcpy-pad.exe"
if exist "%EXE%" goto run

set "EXE=.\target\release\scrcpy-pad.exe"
if exist "%EXE%" goto run

echo   未找到可执行文件，正在编译（首次需要几分钟）...
cargo build --release
if errorlevel 1 (
	echo   构建失败！请检查是否已安装 Rust: https://rustup.rs
	pause
	exit /b 1
)
set "EXE=.\target\release\scrcpy-pad.exe"

:run
"%EXE%" %*
