"""生成 P3 的窗口图标。

按 16x16 逻辑网格画像素图，再整数倍放大到各个尺寸，保证标题栏的 16px
和任务栏的 32px 都不会被插值糊掉。

    python assets/make_icon.py

颜色取自 src/theme.rs 的琥珀色板，只是压暗了一档以便在小尺寸下仍有对比。
"""

from pathlib import Path

from PIL import Image

# 逻辑网格: 机壳高光 / 机壳 / 阴影 / 深琥珀凹槽 / 暗琥珀内缘 / 管面 / 琥珀 / 光标
PALETTE = {
    "H": (0x4A, 0x35, 0x20, 255),
    "K": (0x24, 0x1A, 0x10, 255),
    "D": (0x0A, 0x06, 0x03, 255),
    "d": (0x4D, 0x32, 0x08, 255),
    "a": (0xB8, 0x7C, 0x05, 255),
    "S": (0x05, 0x03, 0x00, 255),
    "A": (0xFF, 0xB0, 0x00, 255),
    "W": (0xFF, 0xDF, 0x93, 255),
}

# 机壳 -> 凹槽 -> 管面, 屏幕里是 ">" 提示符、两行文字和一块光标
GRID = [
    "HHHHHHHHHHHHHHHH",
    "HKKKKKKKKKKKKKKD",
    "KKddddddddddddKD",
    "KdaSSSSSSSSSSadK",
    "KdaSSSSSSSSSSadK",
    "KdaSSASSSSSSSadK",
    "KdaSSSASWWSSSadK",
    "KdaSSASSWWSSSadK",
    "KdaSSSSSSSSSSadK",
    "KdaSSAAAAAASSadK",
    "KdaSSSSSSSSSSadK",
    "KdaSSAAAASSSSadK",
    "KdaSSSSSSSSSSadK",
    "KdaSSSSSSSSSSadK",
    "KddddddddddddddK",
    "DDDDDDDDDDDDDDDD",
]


def render(size: int) -> Image.Image:
    base = Image.new("RGBA", (16, 16))
    base.putdata([PALETTE[ch] for row in GRID for ch in row])
    return base.resize((size, size), Image.Resampling.NEAREST)


def main() -> None:
    out = Path(__file__).resolve().parent
    render(32).save(out / "icon.png", optimize=True)
    render(64).save(out / "p3.png", optimize=True)
    render(64).save(out / "p3.ico", sizes=[(16, 16), (32, 32), (64, 64)])
    print(f"wrote {out / 'icon.png'}, {out / 'p3.png'}, {out / 'p3.ico'}")


if __name__ == "__main__":
    main()
