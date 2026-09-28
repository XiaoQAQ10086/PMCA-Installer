"""生成"界面用中文字体子集"。

# 为什么要内嵌一个字库

界面文字是**固定的一小撮**。把这几个字单独做成一个很小的字体文件内嵌进 exe，
就能保证**不管用户机器上有没有中文字体，界面都能正常显示**。

而完整的中文字体太大了（微软雅黑 18.8 MB），内嵌它会让 exe 从 4 MB 涨到 20 MB 以上，
和"体积小、启动快"直接冲突。

# 为什么不把"用户可能输入的字"都内嵌

用户选的 APK 文件名可能是任意中文（甚至生僻字），子集化之后覆盖不全。
那部分**交给系统字体**：程序启动时同时加载系统字体，
字体引擎遇到子集里没有的字会自动回退过去。

所以是两层：
1. 内嵌子集 → 保证**界面文字**永远能显示（几十 KB）
2. 系统字体 → 覆盖**任意内容**（几 MB，但不占 exe 体积）

# 用法

    python tools/make_ui_font.py

输出：crates/sony-gui/assets/ui-font.ttf
"""

from __future__ import annotations

import glob
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
GUI_SRC = os.path.join(ROOT, "crates", "sony-gui", "src")
GUI_TESTS = os.path.join(ROOT, "crates", "sony-gui", "tests")
OUT_DIR = os.path.join(ROOT, "crates", "sony-gui", "assets")
OUT_FONT = os.path.join(OUT_DIR, "ui-font.ttf")
CHARS_FILE = os.path.join(OUT_DIR, "ui-chars.txt")

# 源字体：微软雅黑。它是 Win10/11 的默认界面字体，观感最好。
SOURCE_FONT = r"C:\Windows\Fonts\msyh.ttc"
SOURCE_FONT_NUMBER = 0  # TTC 里的第 0 个字体

# 内嵌字体的族名。
# ⚠️ 必须和系统字体（Microsoft YaHei）**区分开**，
#    否则两个同名字体会互相覆盖，回退就失效了。
FAMILY_NAME = "InstallerUI"

# 界面里一定可能出现、但字符串里不一定写到的字符
EXTRA_CHARS = (
    "0123456789"
    "abcdefghijklmnopqrstuvwxyz"
    "ABCDEFGHIJKLMNOPQRSTUVWXYZ"
    " \t"
    "　、。，．·；：？！…—～《》〈〉「」『』【】（）〔〕％＋－×÷＝"
    "①②③④⑤⑥⑦⑧⑨⑩"
    "✅⚠️❌●"
)


def collect_chars() -> set[str]:
    """收集界面字符串里用到的所有字符。"""
    chars: set[str] = set(EXTRA_CHARS)

    files = glob.glob(os.path.join(GUI_SRC, "**", "*.rs"), recursive=True)
    files += glob.glob(os.path.join(GUI_TESTS, "**", "*.rs"), recursive=True)

    for path in files:
        src = open(path, encoding="utf-8").read()
        # 去掉注释：注释里的字不会显示给用户，没必要占体积
        src = re.sub(r"//[^\n]*", "", src)
        # 取所有字符串字面量
        for m in re.finditer(r'"((?:[^"\\]|\\.)*)"', src):
            chars.update(m.group(1))

    # 换行、制表符不参与字形，去掉
    chars.discard("\n")
    chars.discard("\r")
    return chars


def main() -> int:
    if not os.path.exists(SOURCE_FONT):
        print(f"找不到源字体：{SOURCE_FONT}", file=sys.stderr)
        return 1

    os.makedirs(OUT_DIR, exist_ok=True)
    chars = collect_chars()
    text = "".join(sorted(chars))
    with open(CHARS_FILE, "w", encoding="utf-8") as f:
        f.write(text)

    print(f"收集到 {len(chars)} 个不同字符")
    cjk = [c for c in chars if "\u4e00" <= c <= "\u9fff"]
    print(f"  其中汉字 {len(cjk)} 个")

    cmd = [
        sys.executable,
        "-m",
        "fontTools.subset",
        SOURCE_FONT,
        f"--font-number={SOURCE_FONT_NUMBER}",
        f"--text-file={CHARS_FILE}",
        f"--output-file={OUT_FONT}",
        "--layout-features=",
        "--no-hinting",
        "--desubroutinize",
        "--drop-tables+=DSIG",
        "--name-IDs=*",
        "--recalc-bounds",
    ]
    print("正在裁剪字体…")
    result = subprocess.run(cmd, capture_output=True, text=True)
    if result.returncode != 0:
        print(result.stdout)
        print(result.stderr, file=sys.stderr)
        return result.returncode

    # 改族名，避免和系统里的微软雅黑撞名（撞名会让回退失效）
    from fontTools.ttLib import TTFont

    font = TTFont(OUT_FONT)
    name_table = font["name"]
    rename = {
        1: FAMILY_NAME,  # Family
        2: "Regular",  # Subfamily
        3: f"{FAMILY_NAME}:Regular",  # Unique ID
        4: FAMILY_NAME,  # Full name
        6: FAMILY_NAME,  # PostScript name
        16: FAMILY_NAME,  # Typographic family
        17: "Regular",  # Typographic subfamily
    }
    for record in name_table.names:
        if record.nameID in rename:
            record.string = rename[record.nameID]
    font.save(OUT_FONT)

    size = os.path.getsize(OUT_FONT)
    print(f"已生成 {OUT_FONT}")
    print(f"   大小 {size / 1024:.1f} KB（族名已改成 {FAMILY_NAME}）")
    print()
    print("   对比：完整微软雅黑 18.8 MB —— 内嵌完整字体显然不划算")
    return 0


if __name__ == "__main__":
    # 控制台可能是 GBK 编码，输出里别用 emoji，否则最后一行会抛异常
    try:
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    except Exception:
        pass
    raise SystemExit(main())
