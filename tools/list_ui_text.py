"""把界面里所有面向用户的文字抽出来，方便统一校对和修改。

只挑**含中文的字符串字面量**，并按"出现在界面的哪个位置"分组。

用法：
    python tools/list_ui_text.py
"""

from __future__ import annotations

import glob
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# 文件 → 这份文件里的文字大致出现在界面哪里
FILES = [
    ("crates/sony-gui/src/main.rs", "窗口标题 / 命令行帮助"),
    ("crates/sony-gui/src/app.rs", "界面上的标签、按钮、提示条、日志"),
    ("crates/sony-install/src/lib.rs", "安装失败时显示在红条里的原因"),
    ("crates/sony-usb/src/usbdev.rs", "找不到相机时的提示"),
    ("crates/sony-core/src/market.rs", "安装进度文字"),
]

CJK = re.compile(r"[\u4e00-\u9fff]")
# 字符串字面量（含 \" 转义），非贪婪
LITERAL = re.compile(r'"((?:[^"\\]|\\.)*)"')


def is_rust_line_comment(line: str) -> bool:
    return line.lstrip().startswith("//")


def extract(path: str):
    """返回 [(行号, 文字)]，只保留含中文的。

    会跳过：
    - 行注释 / 块注释（注释不给用户看）
    - 测试模块（断言里的字不给用户看）
    """
    out = []
    full = os.path.join(ROOT, path)
    if not os.path.exists(full):
        return out
    lines = open(full, encoding="utf-8").read().split("\n")

    in_block_comment = False
    # 测试模块的嵌套深度：>0 表示正在跳过
    skip_depth = 0

    for i, line in enumerate(lines, 1):
        stripped = line.strip()

        if skip_depth > 0:
            skip_depth += line.count("{") - line.count("}")
            continue

        if in_block_comment:
            if "*/" in stripped:
                in_block_comment = False
            continue
        if stripped.startswith("/*"):
            if "*/" not in stripped:
                in_block_comment = True
            continue
        if is_rust_line_comment(line):
            continue

        # 进入测试模块：从 `mod tests {` 开始整段跳过
        if re.match(r"^(pub\s+)?mod\s+tests\b", stripped) and "{" in stripped:
            skip_depth = stripped.count("{") - stripped.count("}")
            continue

        for m in LITERAL.finditer(line):
            text = m.group(1)
            if CJK.search(text):
                out.append((i, text))
    return out


def main() -> int:
    try:
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    except Exception:
        pass

    n = 0
    for path, where in FILES:
        items = extract(path)
        if not items:
            continue
        print()
        print("=" * 72)
        print(f"【{where}】")
        print(f"  {path}")
        print("=" * 72)
        for line_no, text in items:
            n += 1
            # 太长的截断显示，但要提示
            shown = text if len(text) <= 70 else text[:70] + "…"
            print(f"  #{n:<3} (第 {line_no} 行) {shown}")
    print()
    print(f"共 {n} 条。")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
