#!/usr/bin/env bash
#
# scrcpy-pad 发布构建脚本(Linux/macOS 主机)
#
# 在本目录执行,即可编译出 Linux 与 Windows 两个平台的可执行文件,
# 并按"可直接对外发布"的形式整理成本目录下的两个子目录与压缩包。
#
#   ./build.sh            # 构建 Linux + Windows 并打包(推荐)
#   ./build.sh linux      # 只构建 Linux
#   ./build.sh windows    # 只构建 Windows(需要 mingw-w64 交叉编译工具链)
#   ./build.sh clean      # 删除所有构建产物
#
# 产物:
#   linux-x86_64/                        可直接发布的 Linux 包
#   windows-x86_64/                      可直接发布的 Windows 包
#   scrcpy-pad-<版本>-linux-x86_64.tar.gz
#   scrcpy-pad-<版本>-windows-x86_64.zip
#
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"

PKG="scrcpy-pad"
WIN_TARGET="x86_64-pc-windows-gnu"
LINUX_DIR="$HERE/linux-x86_64"
WIN_DIR="$HERE/windows-x86_64"

info() { printf '\033[32m[release]\033[0m %s\n' "$*"; }
warn() { printf '\033[33m[release]\033[0m %s\n' "$*" >&2; }
die() {
    printf '\033[31m[release]\033[0m %s\n' "$*" >&2
    exit 1
}

VERSION="$(awk -F'"' '/^\[package\]/{f=1} f&&/^version/{print $2; exit}' "$ROOT/Cargo.toml")"
[ -n "$VERSION" ] || die "无法从 Cargo.toml 解析版本号"

# ---------------------------------------------------------------- 通用资源

# 复制运行所需资源(图标 / 说明 / 许可)到指定包目录
copy_common() {
    local dest="$1"
    mkdir -p "$dest/icons"
    if [ -f "$ROOT/icons/scrcpy-pad.png" ]; then
        cp -f "$ROOT/icons/scrcpy-pad.png" "$dest/icons/"
    else
        warn "未找到图标 icons/scrcpy-pad.png(该文件是编译期依赖,缺失时无法编译)"
    fi
    [ -f "$ROOT/README.md" ] && cp -f "$ROOT/README.md" "$dest/README.md"
    [ -f "$ROOT/LICENSE" ] && cp -f "$ROOT/LICENSE" "$dest/LICENSE"
    return 0
}

# ---------------------------------------------------------------- Linux

# 生成随包发布的 Linux 启动器:自动处理 evdev 读取权限问题
write_linux_launcher() {
    cat >"$1/启动.sh" <<'LAUNCHER'
#!/usr/bin/env bash
# scrcpy-pad 启动器:自动处理 evdev 读取权限
set -e
cd "$(dirname "$0")"

if head -c 0 /dev/input/event0 2>/dev/null; then
    exec ./scrcpy-pad "$@"
elif id -nG "$USER" | grep -qw input; then
    # 已加入 input 组但当前会话未刷新(未重新登录)时走这里
    exec sg input -c "$PWD/scrcpy-pad"
else
    echo "=================================================="
    echo " 缺少输入设备读取权限,请先执行一次:"
    echo "   sudo usermod -aG input $USER"
    echo " 然后【注销重新登录】(或重启),再运行本脚本"
    echo "=================================================="
    exit 1
fi
LAUNCHER
    chmod +x "$1/启动.sh"
}

build_linux() {
    command -v cargo >/dev/null 2>&1 || die "未找到 cargo,请先安装 Rust: https://rustup.rs"
    info "构建 Linux (x86_64-unknown-linux-gnu) ..."
    (cd "$ROOT" && cargo build --release)
    [ -f "$ROOT/target/release/$PKG" ] || die "构建产物缺失: target/release/$PKG"

    rm -rf "$LINUX_DIR"
    mkdir -p "$LINUX_DIR"
    cp -f "$ROOT/target/release/$PKG" "$LINUX_DIR/$PKG"
    chmod +x "$LINUX_DIR/$PKG"
    copy_common "$LINUX_DIR"
    write_linux_launcher "$LINUX_DIR"
    info "Linux 包就绪: linux-x86_64/"
}

# ---------------------------------------------------------------- Windows

# 交叉编译 Windows 需要 mingw-w64 的链接器与资源编译器
check_win_toolchain() {
    local c
    for c in x86_64-w64-mingw32-gcc x86_64-w64-mingw32-dlltool; do
        command -v "$c" >/dev/null 2>&1 || return 1
    done
    return 0
}

write_win_launcher() {
    cat >"$1/启动.bat" <<'LAUNCHER'
@echo off
chcp 65001 > nul
cd /d %~dp0
scrcpy-pad.exe
LAUNCHER
    # 批处理文件统一使用 CRLF 行尾,避免部分环境解析异常
    sed -i 's/$/\r/' "$1/启动.bat" 2>/dev/null || true
}

build_windows() {
    if ! check_win_toolchain; then
        warn "未检测到 mingw-w64 交叉编译工具链,跳过 Windows 构建。"
        warn "  Fedora : sudo dnf install mingw64-gcc mingw64-binutils"
        warn "  Debian : sudo apt install gcc-mingw-w64-x86-64"
        warn "  Arch   : sudo pacman -S mingw-w64-gcc"
        return 1
    fi
    command -v cargo >/dev/null 2>&1 || die "未找到 cargo,请先安装 Rust: https://rustup.rs"

    if ! rustup target list --installed 2>/dev/null | grep -qx "$WIN_TARGET"; then
        info "安装 Rust 编译目标: $WIN_TARGET"
        rustup target add "$WIN_TARGET"
    fi

    info "构建 Windows ($WIN_TARGET) ..."
    (cd "$ROOT" && cargo build --release --target "$WIN_TARGET")
    [ -f "$ROOT/target/$WIN_TARGET/release/$PKG.exe" ] \
        || die "构建产物缺失: target/$WIN_TARGET/release/$PKG.exe"

    rm -rf "$WIN_DIR"
    mkdir -p "$WIN_DIR"
    cp -f "$ROOT/target/$WIN_TARGET/release/$PKG.exe" "$WIN_DIR/$PKG.exe"
    copy_common "$WIN_DIR"
    write_win_launcher "$WIN_DIR"
    info "Windows 包就绪: windows-x86_64/"
}

# ---------------------------------------------------------------- 打包

make_archives() {
    local made=0
    if [ -d "$LINUX_DIR" ]; then
        command -v tar >/dev/null 2>&1 || die "未找到 tar"
        rm -f "$HERE/$PKG-$VERSION-linux-x86_64.tar.gz"
        (cd "$HERE" && tar -czf "$PKG-$VERSION-linux-x86_64.tar.gz" "linux-x86_64")
        info "已生成: $PKG-$VERSION-linux-x86_64.tar.gz"
        made=1
    fi
    if [ -d "$WIN_DIR" ]; then
        if command -v zip >/dev/null 2>&1; then
            rm -f "$HERE/$PKG-$VERSION-windows-x86_64.zip"
            (cd "$HERE" && zip -qr "$PKG-$VERSION-windows-x86_64.zip" "windows-x86_64")
            info "已生成: $PKG-$VERSION-windows-x86_64.zip"
        else
            warn "未找到 zip,跳过 Windows 压缩包(目录 windows-x86_64/ 已可直接使用)"
        fi
        made=1
    fi
    [ "$made" = 1 ] || warn "没有任何可打包的产物"
}

clean() {
    rm -rf "$LINUX_DIR" "$WIN_DIR"
    rm -f "$HERE"/*.tar.gz "$HERE"/*.zip
    info "已清理构建产物"
}

# ---------------------------------------------------------------- 入口

case "${1:-all}" in
all)
    build_linux
    build_windows || warn "Windows 构建被跳过(见上方提示)"
    make_archives
    ;;
linux)
    build_linux
    make_archives
    ;;
windows)
    build_windows
    make_archives
    ;;
clean)
    clean
    ;;
*)
    echo "用法: $0 [all|linux|windows|clean]"
    exit 1
    ;;
esac

info "完成。版本 $VERSION"
