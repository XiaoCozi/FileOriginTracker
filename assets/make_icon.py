"""生成 FileOriginTracker 的程序图标（多尺寸 .ico）。

设计思路：
- 深蓝圆角方块作为底，稳重且和资源管理器深色/浅色主题都不冲突
- 白色文档 + 折角，代表“文件”
- 琥珀色放大镜压在右下角，代表“追踪来源”
- 三种元素都是大色块，缩小到 16x16 仍能分辨

用法：python make_icon.py 输出路径
"""

import sys
from PIL import Image, ImageDraw

SCALE = 4  # 先按 4 倍尺寸绘制再缩小，边缘更平滑
SIZE = 256 * SCALE


def lerp(a, b, t):
    return tuple(round(a[i] + (b[i] - a[i]) * t) for i in range(3))


def draw_icon() -> Image.Image:
    canvas = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))

    # 1. 圆角方块 + 斜向渐变背景
    background = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    gradient = Image.new("RGB", (SIZE, SIZE))
    pixels = gradient.load()
    top_left = (37, 99, 235)      # #2563EB
    bottom_right = (23, 37, 84)   # #172554
    for y in range(SIZE):
        for x in range(SIZE):
            t = (x + y) / (2 * SIZE - 2)
            pixels[x, y] = lerp(top_left, bottom_right, t)

    mask = Image.new("L", (SIZE, SIZE), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        [0, 0, SIZE - 1, SIZE - 1], radius=int(56 * SCALE), fill=255
    )
    background.paste(gradient, (0, 0), mask)
    canvas.alpha_composite(background)

    draw = ImageDraw.Draw(canvas)

    # 2. 白色文档（带右上折角）
    page = [int(58 * SCALE), int(40 * SCALE), int(160 * SCALE), int(178 * SCALE)]
    fold = int(40 * SCALE)
    draw.rounded_rectangle(page, radius=int(10 * SCALE), fill=(248, 250, 252, 255))

    # 折角：先画一个浅蓝三角，再用背景色盖掉右上角，形成“翻起”的效果
    draw.polygon(
        [
            (page[2] - fold, page[1]),
            (page[2], page[1] + fold),
            (page[2] - fold, page[1] + fold),
        ],
        fill=(191, 219, 254, 255),
    )

    # 文档内容线条
    line_color = (147, 197, 253, 255)
    for index, width_ratio in enumerate((0.60, 0.72, 0.48)):
        y = int((78 + index * 26) * SCALE)
        draw.rounded_rectangle(
            [int(76 * SCALE), y, int((76 + 64 * width_ratio) * SCALE), y + int(9 * SCALE)],
            radius=int(4 * SCALE),
            fill=line_color,
        )

    # 3. 琥珀色放大镜
    ring_color = (245, 158, 11, 255)
    lens_center = (int(172 * SCALE), int(160 * SCALE))
    lens_radius = int(44 * SCALE)

    # 镜片内部：半透明白，压在文档上能看到文字
    draw.ellipse(
        [
            lens_center[0] - lens_radius,
            lens_center[1] - lens_radius,
            lens_center[0] + lens_radius,
            lens_center[1] + lens_radius,
        ],
        fill=(239, 246, 255, 235),
    )
    # 镜框
    draw.ellipse(
        [
            lens_center[0] - lens_radius,
            lens_center[1] - lens_radius,
            lens_center[0] + lens_radius,
            lens_center[1] + lens_radius,
        ],
        outline=ring_color,
        width=int(14 * SCALE),
    )
    # 手柄
    draw.line(
        [
            (lens_center[0] + int(30 * SCALE), lens_center[1] + int(30 * SCALE)),
            (lens_center[0] + int(58 * SCALE), lens_center[1] + int(58 * SCALE)),
        ],
        fill=ring_color,
        width=int(18 * SCALE),
    )
    # 手柄末端圆头
    draw.ellipse(
        [
            lens_center[0] + int(50 * SCALE),
            lens_center[1] + int(50 * SCALE),
            lens_center[0] + int(66 * SCALE),
            lens_center[1] + int(66 * SCALE),
        ],
        fill=ring_color,
    )

    return canvas


def main() -> None:
    output = sys.argv[1] if len(sys.argv) > 1 else "app.ico"
    icon = draw_icon().resize((256, 256), Image.LANCZOS)
    icon.save(output, format="ICO", sizes=[(256, 256), (128, 128), (64, 64), (48, 48), (32, 32), (24, 24), (16, 16)])
    preview = output.rsplit(".", 1)[0] + "_preview.png"
    icon.save(preview)
    print("saved", output)
    print("preview", preview)


if __name__ == "__main__":
    main()
