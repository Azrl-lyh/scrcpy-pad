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
# 选项(可放在子命令之前):
#   --strict-glibc        # 严格 glibc 基线:主机 glibc 与产物符号版本都必须 <= 2.31,
#                         # 否则直接拒绝。**发布/打包时必须带**(见下)。
#
# glibc 基线模式(用户 2026-10-26 定):
#   默认**宽松** —— 主机 glibc 比基线新只警告并继续构建,产物 readelf 复核也降为警告
#   (仍然打印实测最高 GLIBC_x.y),方便 Fedora/Arch 等新发行版直接编译自用。
#   带 --strict-glibc(等价 env SCRCPY_PAD_REQUIRE_GLIBC_BASELINE=1)恢复硬拒绝:
#   保证对外发布的二进制能被 glibc 2.31 及以上的发行版加载。
#   老开关 SCRCPY_PAD_ALLOW_NEW_GLIBC=1 仍然只豁免「主机版本」那一道,不豁免产物复核。
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
    echo " 缺少输入设备读取权限。两种解法,推荐第一种:"
    echo ""
    echo " ① 一次安装 udev 规则(不用注销重登,只对当前活动会话授权):"
    echo "      sudo install -m 0644 60-scrcpy-pad.rules /etc/udev/rules.d/"
    echo "      sudo udevadm control --reload-rules"
    echo "      sudo udevadm trigger --subsystem-match=input"
    echo "    然后重新运行本脚本。"
    echo ""
    echo " ② 传统做法(需要注销重新登录):"
    echo "      sudo usermod -aG input $USER"
    echo "=================================================="
    exit 1
fi
LAUNCHER
    chmod +x "$1/启动.sh"
    # 第 16 条:把 udev 规则一并放进发布包 —— 规则不在包里,用户就找不到,
    # 等于没写(启动器打印的安装命令指向包内这个文件)。
    if [ -f "$ROOT/release/60-scrcpy-pad.rules" ]; then
        cp -f "$ROOT/release/60-scrcpy-pad.rules" "$1/60-scrcpy-pad.rules"
    else
        warn "未找到 release/60-scrcpy-pad.rules(udev 规则):发布包不含一次性授权方案"
    fi
}

build_linux() {
    local baseline="2.31"
    command -v cargo >/dev/null 2>&1 || die "cargo not found; install Rust from https://rustup.rs"
    command -v ldd >/dev/null 2>&1 || die "ldd not found; install libc-bin"

    local libc_ver
    libc_ver="$(ldd --version 2>/dev/null | awk 'NR == 1 { print $NF }')"
    if [ -z "$libc_ver" ]; then
        die "cannot detect glibc version with ldd --version"
    fi
    if [ "$STRICT_GLIBC" = "1" ]; then
        info "glibc 基线:严格模式(主机与产物都必须 <= $baseline)"
    else
        info "glibc 基线:宽松模式(只提示;发布打包请加 --strict-glibc)"
    fi
    if [ "$(printf '%s\n%s\n' "$baseline" "$libc_ver" | sort -V | tail -n 1)" != "$baseline" ]; then
        if [ "$STRICT_GLIBC" = "1" ] && [ "${SCRCPY_PAD_ALLOW_NEW_GLIBC:-0}" != "1" ]; then
            die "glibc $libc_ver is newer than baseline $baseline; build Linux in Ubuntu 20.04 or set SCRCPY_PAD_ALLOW_NEW_GLIBC=1"
        fi
        warn "主机 glibc $libc_ver 高于基线 $baseline:继续构建,但产物可能无法在旧发行版运行"
        warn "  发布前请在 Ubuntu 20.04(glibc 2.31)重建,或加 --strict-glibc 让本脚本直接拒绝"
    fi

    local target_dir="${CARGO_TARGET_DIR:-$ROOT/target-linux-glibc231}"
    info "build Linux (x86_64-unknown-linux-gnu, glibc <= $baseline) ..."
    (cd "$ROOT" && CARGO_TARGET_DIR="$target_dir" cargo build --release --locked)
    local exe="$target_dir/release/$PKG"
    [ -f "$exe" ] || die "missing build artifact: $exe"

    local max_glibc
    if command -v readelf >/dev/null 2>&1; then
        max_glibc="$(readelf --version-info "$exe" | grep -o 'GLIBC_[0-9.]*' | sed 's/^GLIBC_//' | sort -V | tail -n 1)"
        if [ -z "$max_glibc" ]; then
            # 严格模式下"读不出符号版本"= 无法证明兼容性,fail-closed
            [ "$STRICT_GLIBC" = "1" ] && die "cannot read GLIBC symbol versions from $exe"
            warn "无法从 $exe 读出 GLIBC 符号版本,跳过基线校验"
        elif [ "$(printf '%s\n%s\n' "$baseline" "$max_glibc" | sort -V | tail -n 1)" != "$baseline" ]; then
            [ "$STRICT_GLIBC" = "1" ] && die "artifact requires GLIBC_$max_glibc, above baseline $baseline"
            warn "产物需要 GLIBC_$max_glibc > 基线 $baseline:该二进制无法在 glibc $baseline 的系统上运行!"
            warn "  发布前请在 Ubuntu 20.04 重建;--strict-glibc 会把这一条变成硬错误"
        else
            info "GLIBC baseline check passed: max GLIBC_$max_glibc"
        fi
    else
        [ "$STRICT_GLIBC" = "1" ] && die "readelf not found; cannot verify GLIBC baseline in --strict-glibc mode"
        warn "readelf not found; GLIBC symbol check skipped"
    fi

    rm -rf "$LINUX_DIR"
    mkdir -p "$LINUX_DIR"
    cp -f "$exe" "$LINUX_DIR/$PKG"
    chmod +x "$LINUX_DIR/$PKG"
    copy_common "$LINUX_DIR"
    write_linux_launcher "$LINUX_DIR"
    info "Linux package ready: linux-x86_64/"
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

# 第7条实现(用户 2026-10-26):glibc 基线默认宽松、显式 --strict-glibc 才严格。
# 两个来源取"或":命令行开关 或 环境变量,任一为 1 即严格(fail-closed)。
STRICT_GLIBC=0
if [ "${SCRCPY_PAD_REQUIRE_GLIBC_BASELINE:-0}" = "1" ]; then
    STRICT_GLIBC=1
fi

while [ $# -gt 0 ]; do
    case "$1" in
    --strict-glibc)
        STRICT_GLIBC=1
        shift
        ;;
    --)
        shift
        break
        ;;
    -*)
        die "未知选项: $1(可用: --strict-glibc;子命令: all|linux|windows|clean)"
        ;;
    *)
        break
        ;;
    esac
done

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
    echo "用法: $0 [--strict-glibc] [all|linux|windows|clean]"
    exit 1
    ;;
esac

info "完成。版本 $VERSION"
