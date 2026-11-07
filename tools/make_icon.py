# -*- coding: utf-8 -*-
"""生成 assets/ouuutput.ico —— 和 tray.rs 里画的是同一个 ouuut 标记。

**唯一真相源**：轮廓数据就在 `crates/ouuutput-core/src/tray.rs` 的 `MARK` 常量里
（本文件直接把它 parse 出来，不另存一份 —— 早先那种「Rust 画一遍、Python 再抄一遍」
的写法，改了一边忘了另一边就会静默不一致）。

形状 = ouuut 品牌字标里那个手绘 `o`（`ouuut-mark-square.svg`）。颜色 = 品牌纸白 #F5F3ED。
跑法：python tools/make_icon.py
"""
import os
import re
import struct

SIZES = [16, 32, 48]
# 品牌纸白（bg #F5F3ED）—— 与 tray.rs 里的 BGRA 值逐字节一致。
FILL_RGB = (0xF5, 0xF3, 0xED)
EXTRA_PAD = 2.0  # 与 tray.rs 的 mark_outline() 一致：32 空间里 2px 留白


def load_mark(root):
    """从 tray.rs 的 `const MARK: [(f32, f32); N] = [ ... ];` 里读出轮廓点。"""
    src = open(os.path.join(root, "crates", "ouuutput-core", "src", "tray.rs"), encoding="utf-8").read()
    m = re.search(r"const MARK:\s*\[\(f32, f32\);\s*(\d+)\]\s*=\s*\[(.*?)\];", src, re.S)
    if not m:
        raise SystemExit("tray.rs 里找不到 MARK 常量 —— 是不是改名/重构了？")
    n = int(m.group(1))
    pts = [(float(a), float(b)) for a, b in re.findall(r"\(([-\d.]+),\s*([-\d.]+)\)", m.group(2))]
    if len(pts) != n:
        raise SystemExit("MARK 声明 %d 个点，实际 parse 到 %d 个" % (n, len(pts)))
    return pts


def scaled(size, mark):
    _, _, x0, y0, x1, y1 = _bbox(mark)
    s = 32.0 / size
    pad = EXTRA_PAD / 32.0 * size  # 把 32 空间的留白换算到目标尺寸
    k = min((size - 2 * pad) / (x1 - x0), (size - 2 * pad) / (y1 - y0))
    ox = (size - (x1 - x0) * k) / 2.0
    oy = (size - (y1 - y0) * k) / 2.0
    return [((x - x0) * k + ox, (y - y0) * k + oy) for x, y in mark]


def _bbox(mark):
    xs = [p[0] for p in mark]
    ys = [p[1] for p in mark]
    return len(mark), 0, min(xs), min(ys), max(xs), max(ys)


def in_poly(poly, x, y):
    n = len(poly)
    inside = False
    j = n - 1
    for i in range(n):
        xi, yi = poly[i]
        xj, yj = poly[j]
        if (yi > y) != (yj > y):
            xint = (xj - xi) * (y - yi) / (yj - yi) + xi
            if x < xint:
                inside = not inside
        j = i
    return inside


def render(size, mark):
    """3x3 超采样，和 tray.rs 的 make_icon() 用同一套判据。"""
    poly = scaled(size, mark)
    ss = 3
    px = bytearray(size * size * 4)
    r, g, b = FILL_RGB
    for py in range(size):
        for pxx in range(size):
            hit = 0
            for sy in range(ss):
                for sx in range(ss):
                    if in_poly(poly, pxx + (sx + 0.5) / ss, py + (sy + 0.5) / ss):
                        hit += 1
            a = hit * 255 // (ss * ss)
            o = (py * size + pxx) * 4
            px[o], px[o + 1], px[o + 2], px[o + 3] = b, g, r, a  # BGRA
    return bytes(px)


def ico_image(size, mark):
    """ICO 里的单个图像：BITMAPINFOHEADER(高度写双倍) + BGRA 自下而上 + AND 掩码。"""
    header = struct.pack("<IiiHHIIiiII", 40, size, size * 2, 1, 32, 0, size * size * 4, 0, 0, 0, 0)
    bgra = render(size, mark)
    bottom_up = b"".join(bgra[y * size * 4:(y + 1) * size * 4] for y in range(size - 1, -1, -1))
    mask_row = ((size + 31) // 32) * 4
    return header + bottom_up + b"\x00" * (mask_row * size)


def build(path, mark):
    imgs = [ico_image(s, mark) for s in SIZES]
    out = struct.pack("<HHH", 0, 1, len(imgs))
    offset = 6 + 16 * len(imgs)
    for s, data in zip(SIZES, imgs):
        out += struct.pack("<BBBBHHII", s % 256, s % 256, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
    for data in imgs:
        out += data
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as f:
        f.write(out)
    print("写出 %s (%d bytes, sizes=%s, 轮廓 %d 点)" % (path, len(out), SIZES, len(mark)))


if __name__ == "__main__":
    root = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
    build(os.path.join(root, "assets", "ouuutput.ico"), load_mark(root))
