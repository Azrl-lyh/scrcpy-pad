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
