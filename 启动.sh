#!/usr/bin/env bash
# scrcpy-pad 启动器:自动处理编译与 evdev 权限
set -e
cd "$(dirname "$0")"

BIN="target/release/scrcpy-pad"

# 1. 没有可执行文件就先编译
if [ ! -x "$BIN" ]; then
    echo "未找到编译产物,开始编译(仅首次需要)..."
    export PATH="$HOME/.cargo/bin:$PATH"
    cargo build --release
fi

# 2. evdev 权限自检:能读就直接启动;在 input 组就借 sg 启动;都不行则提示
if head -c 0 /dev/input/event0 2>/dev/null; then
    exec "$BIN"
elif id -nG "$USER" | grep -qw input; then
    # 已加入 input 组但当前会话未刷新(未重新登录)时走这里
    exec sg input -c "$PWD/$BIN"
else
    echo "=================================================="
    echo " 缺少输入设备读取权限,请先执行一次:"
    echo "   sudo usermod -aG input \$USER"
    echo " 然后【注销重新登录】(或重启),再运行本脚本"
    echo "=================================================="
    exit 1
fi
