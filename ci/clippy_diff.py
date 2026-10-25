#!/usr/bin/env python3
"""把 `cargo clippy --all-targets` 的告警与基线比对（W2-5 / 审计方案的验证纪律之一）。

用法::

    python ci/clippy_diff.py                 # 本机（Windows）目标，只报告，永远退出 0
    python ci/clippy_diff.py --strict        # 出现"新增/消失"即退出 1
    python ci/clippy_diff.py --target linux  # Linux 交叉目标，用自己的基线
    python ci/clippy_diff.py --target linux --strict

口径与《全面审计与提升方案-2026-10-05.md》里历次实施记录一致：

* 比对的是 **(文件, 消息文本) 去重集合**，不是行号 —— 行号每次重构都会动，
  消息文本才代表"同一类告警"；
* 基线文件每行 `src/xxx.rs: warning: <消息>`，由维护者在"确认每一对都是既有构造"
  之后手工更新（更新时要在方案文档里记账）；
* **两个目标各有一份基线**（2026-10-22 起）：本机基线 `ci/clippy-baseline.txt`，
  Linux 交叉目标基线 `ci/clippy-baseline-linux.txt`。原因：两个平台编译的
  `#[cfg]` 代码面不同，各有一对平台独有的告警（Linux 侧 manual implementation of
  .is_multiple_of()，Windows 侧 this function has too many arguments (8/7)），
  数量巧合都是 36 对。**不要试图用一份基线同时满足两边**。
* 本机（Windows）的基线是 36 对；GitHub runner 上的 rustc 版本可能更新，
  因此 CI 把它当**信息性**步骤，不拦提交。
"""

from __future__ import annotations

import io
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
BASELINE_NATIVE = os.path.join(HERE, "clippy-baseline.txt")
BASELINE_LINUX = os.path.join(HERE, "clippy-baseline-linux.txt")
LINUX_TARGET = "x86_64-unknown-linux-gnu"
BS = chr(92)  # 反斜杠

# Windows 控制台/重定向管道的默认编码是 GBK 或 cp1252，直接 print 中文会炸；
# 统一按 UTF-8 输出（本项目在 Windows + Linux 两边都跑这个脚本）。
sys.stdout.reconfigure(encoding="utf-8", errors="replace")


def load_baseline(path: str) -> set[tuple[str, str]]:
    pairs: set[tuple[str, str]] = set()
    with io.open(path, encoding="utf-8") as fh:
        for line in fh:
            line = line.rstrip("\n")
            m = re.match(r"^(src[^:]+): warning: (.*)$", line)
            if m:
                pairs.add((m.group(1).replace(BS, "/"), m.group(2).strip()))
    return pairs


def run_clippy(target: str | None) -> set[tuple[str, str]]:
    cmd = ["cargo", "clippy", "--all-targets"]
    if target:
        cmd += ["--target", target]
    proc = subprocess.run(
        cmd,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
    )
    pairs: set[tuple[str, str]] = set()
    current = None
    for line in proc.stderr.splitlines():
        m = re.match(r"^warning: (.*)", line)
        if m and not line.startswith("warning: `scrcpy-pad`"):
            current = m.group(1).strip()
        m2 = re.match(r"^\s+--> (src[^:]+):\d+:\d+", line)
        if m2 and current:
            pairs.add((m2.group(1).replace(BS, "/"), current))
            current = None
    return pairs


def parse_target(argv: list[str]) -> str | None:
    """返回 cargo 的 --target 值；本机目标返回 None。

    认 --target linux 与 --target=linux 两种写法；native/windows/本机 都当本机。
    """
    for i, arg in enumerate(argv):
        val = None
        if arg == "--target" and i + 1 < len(argv):
            val = argv[i + 1]
        elif arg.startswith("--target="):
            val = arg.split("=", 1)[1]
        if val is None:
            continue
        v = val.strip().lower()
        if v in ("linux", LINUX_TARGET):
            return LINUX_TARGET
        if v in ("native", "windows", "本机", "x86_64-pc-windows-msvc"):
            return None
        print("未知 --target 取值: " + val + "（认 linux / native）")
        raise SystemExit(2)
    return None


def main() -> int:
    strict = "--strict" in sys.argv
    target = parse_target(sys.argv)
    baseline_path = BASELINE_LINUX if target else BASELINE_NATIVE
    label = "linux(" + LINUX_TARGET + ")" if target else "本机(Windows)"
    baseline = load_baseline(baseline_path)
    current = run_clippy(target)
    new = sorted(current - baseline)
    gone = sorted(baseline - current)

    print("目标: " + label + "   基线: " + os.path.basename(baseline_path))
    print("当前告警对: " + str(len(current)) + "   基线对: " + str(len(baseline)))
    print("--- 新增（当前有、基线没有）---")
    for f, msg in new:
        print("  " + f + " | " + msg)
    print("--- 消失（基线有、当前没有）---")
    for f, msg in gone:
        print("  " + f + " | " + msg)

    if new or gone:
        print("提示：确认每一对都是既有构造/已消除后，更新 " + os.path.basename(baseline_path) + " 并在方案文档里记账。")
        return 1 if strict else 0
    print("与基线零差异。")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
