@echo off
chcp 65001 > nul
cd /d %~dp0
if not exist target\release\scrcpy-pad.exe (
	echo  '找不到目标文件，执行首次生成任务…'
	cargo build --release
	if errorlevel 1 (
		echo '构建失败！ 请检查是否已安装rust： https://rustup.rs'
		pause
		exit /b 1
	)
)

target\release\scrcpy-pad.exe