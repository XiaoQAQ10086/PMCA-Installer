"""从一张源图生成 Windows 程序的图标。

用法：
    python tools/make_icon.py <源图.png>

做四件事：

1. **裁掉四周多余的空白** —— 源图通常留了很多边距，直接缩放会让图案显得很小
2. **重新排到正方形画布上**，让图案尽可能大（四周留一点边距，Windows 图标本身也有内边距）
3. **生成多尺寸 .ico** —— Windows 在不同地方会挑不同尺寸用，缺了哪档就会显示模糊：
       16×16   文件列表小图标、任务栏小图标
       24×24   资源管理器「详情」视图
       32×32   桌面快捷方式、Alt+Tab
       48×48   「中等图标」
       64×64   「大图标」
      128×128  「超大图标」
      256×256  「特大图标」（Win11 默认用这档）
4. **另外导出一张 PNG** 给程序运行时设窗口图标用
   （iced 的 `Icon::from_rgba` 要的是原始 RGBA，所以那边自己解码这张 PNG）

# 关于源图

- 必须是 **PNG**，**带透明背景**（不要白底，否则图标会是个白方块）
- 越大越好，建议 512×512 以上
- 图案尽量简洁：最终要缩到 16×16，细线和文字在那个尺寸下必然糊掉
"""

from __future__ import annotations

import os
import sys

from PIL import Image

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ASSETS = os.path.join(ROOT, "crates", "sony-gui", "assets")

# .ico 里要包含的尺寸。
# 少任何一档，Windows 在对应场景下就会拿相邻尺寸硬缩放，看起来发糊。
ICO_SIZES = [16, 24, 32, 48, 64, 128, 256]

# 图案占画布的比例。留一点边距，因为 Windows 图标周围本身还有内边距，
# 顶得太满会显得比别的图标大一圈。
CONTENT_RATIO = 0.92

# 生成各尺寸时用的画布基准（取最大档，保证缩放精度）
CANVAS = 256

# 窗口图标（标题栏 / 任务栏）用的尺寸。
# 高 DPI 下任务栏图标最多显示到 64 像素左右，128 留了余量。
WINDOW_ICON_SIZE = 128


def prepare(src_path: str) -> Image.Image:
    """把源图裁边、居中、缩放到正方形画布上。"""
    im = Image.open(src_path).convert("RGBA")

    # 用 alpha 通道算内容边界（透明的地方不算内容）
    alpha = im.getchannel("A")
    bbox = alpha.getbbox()
    if bbox is None:
        raise SystemExit("源图整张都是透明的，没有内容可裁。")
    print(f"  原图 {im.size[0]}×{im.size[1]}，内容区域 {bbox}")

    content = im.crop(bbox)
    cw, ch = content.size
    if cw == 0 or ch == 0:
        raise SystemExit("内容区域是空的。")

    # 按较长的一边算缩放比例，保证图案完整放得下
    target = CANVAS * CONTENT_RATIO
    scale = min(target / cw, target / ch)
    new_size = (max(1, round(cw * scale)), max(1, round(ch * scale)))
    content = content.resize(new_size, Image.LANCZOS)
    print(f"  裁边后 {cw}×{ch} → 缩放为 {new_size[0]}×{new_size[1]}")

    canvas = Image.new("RGBA", (CANVAS, CANVAS), (0, 0, 0, 0))
    canvas.paste(
        content,
        ((CANVAS - new_size[0]) // 2, (CANVAS - new_size[1]) // 2),
        content,
    )
    return canvas


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__)
        print("用法：python tools/make_icon.py <源图.png>")
        return 2

    src = sys.argv[1]
    if not os.path.exists(src):
        print(f"找不到文件：{src}")
        return 1

    print("正在处理：")
    canvas = prepare(src)
    os.makedirs(ASSETS, exist_ok=True)

    # 先切好各尺寸（显式用 LANCZOS，比让 ICO 插件自己缩要清晰）
    frames = {
        s: canvas.resize((s, s), Image.LANCZOS).convert("RGBA") for s in ICO_SIZES
    }

    # 窗口图标用的 PNG：取 256 这一档
    png_path = os.path.join(ASSETS, "icon.png")
    frames[256].save(png_path, format="PNG", optimize=True)

    # 窗口图标还要一份**原始 RGBA** 数据。
    #
    # 为什么不内嵌 PNG、让程序启动时解码：那样就得引入一个 PNG 解码库。
    # 而窗口图标在标题栏/任务栏最大也就显示到 48 像素左右，
    # 128×128 完全够用，原始数据 64 KB —— 比多一个依赖划算得多，
    # 而且启动时不用解码、更快。
    rgba_path = os.path.join(ASSETS, "icon-rgba.bin")
    frames[WINDOW_ICON_SIZE].tobytes()  # 行优先的 RGBA，正好是 iced 要的格式
    with open(rgba_path, "wb") as f:
        f.write(frames[WINDOW_ICON_SIZE].tobytes())

    # .ico：Pillow 只按 sizes 参数从**一张图**降采样，
    # 所以把最大档给进去，再让它生成其余尺寸；但那样小的尺寸会偏糊，
    # 于是这里用高质量源 + 指定 sizes，并在下面单独检查小尺寸效果。
    ico_path = os.path.join(ASSETS, "icon.ico")
    canvas.save(ico_path, format="ICO", sizes=[(s, s) for s in ICO_SIZES])

    print()
    print("已生成：")
    for path in (ico_path, png_path, rgba_path):
        size = os.path.getsize(path)
        print(f"  {os.path.relpath(path, ROOT)}  ({size / 1024:.1f} KB)")
    print()
    print(f"  .ico 内含尺寸：{'、'.join(str(s) for s in ICO_SIZES)}")
    print(f"  窗口图标原始数据：{WINDOW_ICON_SIZE}×{WINDOW_ICON_SIZE} RGBA")

    # 顺手导一张放大预览，方便肉眼检查小尺寸有没有糊掉
    cell = 128
    gap = 16
    total_w = len(ICO_SIZES) * (cell + gap)
    preview = Image.new("RGBA", (total_w, cell + 20), (255, 255, 255, 255))
    x = gap // 2
    for s in ICO_SIZES:
        # 每档都统一放大到 128，这样能直接比较"哪一档开始糊"
        shown = frames[s].resize((cell, cell), Image.NEAREST)
        preview.paste(shown, (x, 10), shown)
        x += cell + gap
    preview_path = os.path.join(ASSETS, "icon-preview.png")
    preview.save(preview_path)
    print(f"  预览图（各尺寸放大到同样大小对比）：{os.path.relpath(preview_path, ROOT)}")
    return 0


if __name__ == "__main__":
    try:
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    except Exception:
        pass
    raise SystemExit(main())
