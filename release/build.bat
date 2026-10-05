@echo off
chcp 65001 > nul
setlocal

rem 默认构建 Windows + Linux；可由第一个参数限制为 windows 或 linux。
set "TARGET=%~1"
if "%TARGET%"=="" set "TARGET=all"
if /i not "%TARGET%"=="all" if /i not "%TARGET%"=="windows" if /i not "%TARGET%"=="linux" (
    echo [release] 参数无效: %TARGET%
    echo [release] 用法: build.bat [all^|windows^|linux]
    pause
    exit /b 2
)

powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0build.ps1" -Target "%TARGET%"
set "RC=%ERRORLEVEL%"
if not "%RC%"=="0" (
    echo [release] 构建失败，退出码 %RC%
    pause
    exit /b %RC%
)

echo [release] 完成。
if /i "%TARGET%"=="all" pause
exit /b 0
