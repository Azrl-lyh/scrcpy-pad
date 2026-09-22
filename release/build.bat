@echo off
chcp 65001 > nul
setlocal
cd /d %~dp0

rem ============================================================
rem  scrcpy-pad 发布构建脚本(Windows 主机)
rem
rem  在本目录双击或命令行执行,即可编译 Windows 版可执行文件,
rem  并整理成本目录下可直接对外发布的 windows-x86_64\ 与 zip 包。
rem
rem  说明:Windows 主机无法交叉编译 Linux 版;
rem        Linux 版请在 Linux 上运行本目录的 build.sh。
rem ============================================================

set "PKG=scrcpy-pad"
set "ROOT=%~dp0.."
set "OUT=%~dp0windows-x86_64"

rem 读取版本号(取 Cargo.toml 中首个以 version 开头的行)
for /f "tokens=2 delims= " %%v in ('findstr /b "version" "%ROOT%\Cargo.toml"') do (
    if not defined VER set "VER=%%v"
)
set "VER=%VER:"=%"
if not defined VER set "VER=0.0.0"

echo [release] 构建 Windows 版 ...
pushd "%ROOT%"
cargo build --release
if errorlevel 1 (
    echo [release] 构建失败! 请检查是否已安装 Rust: https://rustup.rs
    popd
    pause
    exit /b 1
)
popd

if not exist "%ROOT%\target\release\%PKG%.exe" (
    echo [release] 找不到构建产物: target\release\%PKG%.exe
    pause
    exit /b 1
)

if exist "%OUT%" rmdir /s /q "%OUT%"
mkdir "%OUT%"
mkdir "%OUT%\icons"
copy /y "%ROOT%\target\release\%PKG%.exe" "%OUT%\%PKG%.exe" > nul
if exist "%ROOT%\icons\scrcpy-pad.png" copy /y "%ROOT%\icons\scrcpy-pad.png" "%OUT%\icons\" > nul
if exist "%ROOT%\README.md" copy /y "%ROOT%\README.md" "%OUT%\" > nul
if exist "%ROOT%\LICENSE" copy /y "%ROOT%\LICENSE" "%OUT%\" > nul

rem 生成随包发布的启动器
> "%OUT%\启动.bat" echo @echo off
>> "%OUT%\启动.bat" echo chcp 65001 ^> nul
>> "%OUT%\启动.bat" echo cd /d %%~dp0
>> "%OUT%\启动.bat" echo start "" "%PKG%.exe"

echo [release] Windows 包就绪: windows-x86_64\

rem 打包为 zip
where powershell > nul 2>&1
if not errorlevel 1 (
    if exist "%~dp0%PKG%-%VER%-windows-x86_64.zip" del /q "%~dp0%PKG%-%VER%-windows-x86_64.zip"
    powershell -NoProfile -Command "Compress-Archive -Path '%OUT%' -DestinationPath '%~dp0%PKG%-%VER%-windows-x86_64.zip' -Force" > nul
    if exist "%~dp0%PKG%-%VER%-windows-x86_64.zip" (
        echo [release] 已生成: %PKG%-%VER%-windows-x86_64.zip
    ) else (
        echo [release] zip 打包失败,目录 windows-x86_64\ 已可直接使用
    )
)

echo [release] 完成。版本 %VER%
pause
