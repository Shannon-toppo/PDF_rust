#!/usr/bin/env python3
"""progressive JPEG テストフィクスチャ生成スクリプト（progressive DCT デコーダ用）。

再生成手順:
    python tests/fixtures/gen_progressive_jpeg_fixtures.py

Pillow（PIL）でテスト画像を生成し、各画像を **progressive JPEG**（`progressive=True`）で
保存したのち、同じ JPEG を読み戻して全ピクセルの RGB を生バイト列（R,G,B 順、行優先）
として `.rgb` にダンプする。`.rgb` が期待値となる。

注意:
  - baseline 用フィクスチャ（`gen_jpeg_fixtures.ps1`）と対になる progressive 版。
    progressive と baseline で最終的な係数は同一なので、正しく復号できれば
    baseline と同等のピクセルが得られる。
  - 期待値 `.rgb` はエンコード後の JPEG を「Pillow 自身で」デコードした結果なので、
    IDCT 実装差で数値は完全一致しない。テスト側で誤差許容（チャネル絶対差 <= 12、
    平均絶対差 <= 2.0）で比較する。
  - `subsampling` を変えて 4:4:4（0）と 4:2:0（2）の両方を生成し、クロマ
    アップサンプリングと非インターリーブ AC スキャンの組み合わせを検証する。
  - 奇数サイズ（17x13）で右端・下端の半端 MCU を検証する。
"""

import os
from PIL import Image

DIR = os.path.dirname(os.path.abspath(__file__))


def gradient(w, h):
    img = Image.new("RGB", (w, h))
    px = img.load()
    for y in range(h):
        for x in range(w):
            r = int(255 * x / max(1, w - 1))
            g = int(255 * y / max(1, h - 1))
            b = 128
            px[x, y] = (r, g, b)
    return img


def blocks(w, h):
    img = Image.new("RGB", (w, h))
    px = img.load()
    for y in range(h):
        for x in range(w):
            left = x < w / 2
            top = y < h / 2
            if top and left:
                c = (220, 30, 30)
            elif top and not left:
                c = (30, 200, 40)
            elif not top and left:
                c = (40, 60, 230)
            else:
                c = (240, 230, 20)
            px[x, y] = c
    return img


def solid(w, h, color):
    return Image.new("RGB", (w, h), color)


def save_and_dump(img, name, quality, subsampling):
    jpg = os.path.join(DIR, name + ".jpg")
    rgb = os.path.join(DIR, name + ".rgb")
    img.save(
        jpg,
        format="JPEG",
        quality=quality,
        progressive=True,
        subsampling=subsampling,
    )
    # 読み戻して RGB を生ダンプ（これが期待値）。
    back = Image.open(jpg).convert("RGB")
    data = bytearray()
    px = back.load()
    for y in range(back.height):
        for x in range(back.width):
            r, g, b = px[x, y]
            data += bytes((r, g, b))
    with open(rgb, "wb") as f:
        f.write(data)
    print(f"生成: {name}.jpg ({img.width}x{img.height}, q={quality}, sub={subsampling})")


# name, generator, quality, subsampling(0=4:4:4, 2=4:2:0)
SPECS = [
    ("prog_solid_16_q90", lambda: solid(16, 16, (120, 60, 200)), 90, 0),
    ("prog_gradient_16_q90", lambda: gradient(16, 16), 90, 0),
    ("prog_blocks_16_q90", lambda: blocks(16, 16), 90, 0),
    ("prog_blocks_16_q50", lambda: blocks(16, 16), 50, 2),
    ("prog_gradient_17x13_q90", lambda: gradient(17, 13), 90, 0),
    ("prog_blocks_17x13_q50", lambda: blocks(17, 13), 50, 2),
    ("prog_gradient_32_q50", lambda: gradient(32, 32), 50, 2),
]

for name, gen, q, sub in SPECS:
    save_and_dump(gen(), name, q, sub)

print("完了")
