//! RGBA ピクセルバッファと PNG 書き出し。
//!
//! レンダリングの出力先となるキャンバス。1 ピクセル 4 バイト
//! （R, G, B, A の順）の行優先レイアウトで保持する。
//!
//! 通常の出力 Pixmap（[`Pixmap::new`]）は**不透明**（A=255 固定）で、
//! 描画はソースオーバー合成の RGB 混合になる。透明グループの中間バッファ
//! 用には [`Pixmap::new_transparent`] が **アルファ可変**の Pixmap を返し、
//! [`Pixmap::blend_pixel_with`] が PDF の透明モデルに沿った合成を行う。
//!
//! PNG 書き出しは既存の zlib 圧縮器（[`crate::filters::flate::compress`]、
//! stored ブロック方式）を流用するため、追加の圧縮実装を持たない。

use super::blend::{blend, BlendMode};
use super::raster::Mask;
use crate::error::Result;
use crate::filters::flate;
use std::path::Path;

/// RGBA8 のピクセルバッファ（左上原点・y 軸下向き）。
#[derive(Debug, Clone)]
pub struct Pixmap {
    width: u32,
    height: u32,
    /// 行優先 RGBA。長さは `width * height * 4`。
    data: Vec<u8>,
    /// アルファチャネルを保持するか（透明グループ用の中間バッファで `true`）。
    /// 通常出力は `false` で常に不透明（A=255）。
    has_alpha: bool,
}

impl Pixmap {
    /// 指定サイズの不透明な白いキャンバスを作る。
    ///
    /// 幅・高さが 0 の場合は 1 に切り上げる（空バッファによる
    /// 端ケースを避けるため）。
    pub fn new(width: u32, height: u32) -> Pixmap {
        let width = width.max(1);
        let height = height.max(1);
        let len = (width as usize) * (height as usize) * 4;
        Pixmap {
            width,
            height,
            data: vec![0xFF; len],
            has_alpha: false,
        }
    }

    /// 全面が完全に透明（R=G=B=A=0）な Pixmap を作る（透明グループの
    /// 中間バッファ用）。`has_alpha = true` となり、合成時にアルファチャネルが
    /// 維持される。
    pub fn new_transparent(width: u32, height: u32) -> Pixmap {
        let width = width.max(1);
        let height = height.max(1);
        let len = (width as usize) * (height as usize) * 4;
        Pixmap {
            width,
            height,
            data: vec![0u8; len],
            has_alpha: true,
        }
    }

    /// サイズを変更して全面を不透明な白に戻す（バッファは可能な限り再利用）。
    ///
    /// [`crate::Document::render_page_into`] の出力先再利用のための補助。
    /// 幅・高さが 0 の場合は [`new`](Self::new) と同様に 1 へ切り上げる。
    pub fn reset(&mut self, width: u32, height: u32) {
        let width = width.max(1);
        let height = height.max(1);
        let len = (width as usize) * (height as usize) * 4;
        self.width = width;
        self.height = height;
        self.data.clear();
        self.data.resize(len, 0xFF);
        self.has_alpha = false;
    }

    /// アルファチャネルが意味を持つか（透明グループ用バッファのとき `true`）。
    pub fn has_alpha(&self) -> bool {
        self.has_alpha
    }

    /// 幅（ピクセル）。
    pub fn width(&self) -> u32 {
        self.width
    }

    /// 高さ（ピクセル）。
    pub fn height(&self) -> u32 {
        self.height
    }

    /// 生バッファ（行優先 RGBA、長さ `width * height * 4`）。
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// 全面を単色（不透明）で塗りつぶす。
    pub fn fill(&mut self, rgb: [u8; 3]) {
        for px in self.data.chunks_exact_mut(4) {
            px[0] = rgb[0];
            px[1] = rgb[1];
            px[2] = rgb[2];
            px[3] = 0xFF;
        }
    }

    /// ピクセル位置のバッファ先頭インデックス。範囲外なら `None`。
    fn index(&self, x: u32, y: u32) -> Option<usize> {
        if x < self.width && y < self.height {
            Some(((y as usize) * (self.width as usize) + (x as usize)) * 4)
        } else {
            None
        }
    }

    /// ピクセルの RGB を返す。範囲外なら `None`。
    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 3]> {
        self.index(x, y)
            .map(|i| [self.data[i], self.data[i + 1], self.data[i + 2]])
    }

    /// ピクセルを不透明な単色で上書きする。範囲外は無視。
    pub fn set_pixel(&mut self, x: u32, y: u32, rgb: [u8; 3]) {
        if let Some(i) = self.index(x, y) {
            self.data[i] = rgb[0];
            self.data[i + 1] = rgb[1];
            self.data[i + 2] = rgb[2];
            self.data[i + 3] = 0xFF;
        }
    }

    /// ピクセルへソースオーバー合成で色を混ぜる。範囲外は無視。
    ///
    /// `alpha` はカバレッジ（アンチエイリアスの被覆率）と塗りの不透明度を
    /// 掛け合わせた最終的な合成率（0 = 変化なし、255 = 完全上書き）。
    /// キャンバスは常に不透明なので RGB のみ混合する。
    pub fn blend_pixel(&mut self, x: u32, y: u32, rgb: [u8; 3], alpha: u8) {
        if alpha == 0 {
            return;
        }
        if let Some(i) = self.index(x, y) {
            if alpha == 0xFF {
                self.data[i] = rgb[0];
                self.data[i + 1] = rgb[1];
                self.data[i + 2] = rgb[2];
                return;
            }
            let a = alpha as u32;
            for (k, &src) in rgb.iter().enumerate() {
                let dst = self.data[i + k] as u32;
                // 四捨五入付きの src*a + dst*(1-a)
                self.data[i + k] = ((src as u32 * a + dst * (255 - a) + 127) / 255) as u8;
            }
        }
    }

    /// [`blend_pixel`](Self::blend_pixel) のブレンドモード指定版。
    ///
    /// 背景色との合成は PDF §11.3.4 の式で行う:
    ///
    /// - 不透明バッファ（[`Pixmap::new`] 由来）では α_b = 1 と仮定し、
    ///   `result = (1 − α_s)·背景 + α_s·B(背景, ソース)` の高速経路を使う。
    /// - 透明バッファ（[`Pixmap::new_transparent`] 由来）では α_b も
    ///   生かして `α_r = α_s + α_b·(1 − α_s)` と
    ///   `α_r·C_r = (1 − α_b)·α_s·C_s + α_b·α_s·B + α_b·(1 − α_s)·C_b` で
    ///   合成し、アルファチャネルも更新する。
    pub fn blend_pixel_with(&mut self, x: u32, y: u32, rgb: [u8; 3], alpha: u8, mode: BlendMode) {
        if alpha == 0 {
            return;
        }
        let i = match self.index(x, y) {
            Some(i) => i,
            None => return,
        };
        let cb = [self.data[i], self.data[i + 1], self.data[i + 2]];
        let ab = self.data[i + 3];
        let bb = blend(cb, rgb, mode);

        if !self.has_alpha {
            // 不透明背景: C_r = α_s·B + (1 − α_s)·C_b（Normal は B = C_s に縮退）。
            let a = alpha as u32;
            for k in 0..3 {
                let src = bb[k] as u32;
                let dst = cb[k] as u32;
                self.data[i + k] = ((src * a + dst * (255 - a) + 127) / 255) as u8;
            }
            return;
        }

        // 透明背景（透明グループ）。f32 で素直に計算する。
        let a_s = alpha as f32 / 255.0;
        let a_b = ab as f32 / 255.0;
        let a_r = a_s + a_b * (1.0 - a_s);
        if a_r < 1e-4 {
            // ほぼ完全透明: 全成分 0 にする。
            self.data[i] = 0;
            self.data[i + 1] = 0;
            self.data[i + 2] = 0;
            self.data[i + 3] = 0;
            return;
        }
        for k in 0..3 {
            let cb_k = cb[k] as f32 / 255.0;
            let cs_k = rgb[k] as f32 / 255.0;
            let bb_k = bb[k] as f32 / 255.0;
            let cr = ((1.0 - a_b) * a_s * cs_k + a_b * a_s * bb_k + a_b * (1.0 - a_s) * cb_k) / a_r;
            self.data[i + k] = (cr.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        }
        self.data[i + 3] = (a_r.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    }

    /// 別の Pixmap（透明グループの中間バッファ）を、不透明度 `alpha`・
    /// ブレンドモード `mode`・クリップ `clip` で自身に合成する。
    ///
    /// 左上原点で重ね合わせる。`other` のアルファを描画ピクセルごとの
    /// ソースアルファとして使い、`alpha` とクリップ被覆を乗算してから
    /// [`blend_pixel_with`](Self::blend_pixel_with) を呼ぶ。
    pub fn composite_from(
        &mut self,
        other: &Pixmap,
        alpha: u8,
        mode: BlendMode,
        clip: Option<&Mask>,
    ) {
        if alpha == 0 {
            return;
        }
        let base = alpha as u32;
        let w = self.width.min(other.width);
        let h = self.height.min(other.height);
        for y in 0..h {
            for x in 0..w {
                let oi = ((y as usize) * (other.width as usize) + (x as usize)) * 4;
                let sa = other.data[oi + 3];
                if sa == 0 {
                    continue;
                }
                let mut a = (sa as u32 * base + 127) / 255;
                if let Some(mask) = clip {
                    a = (a * mask.coverage(x, y) as u32 + 127) / 255;
                }
                if a == 0 {
                    continue;
                }
                let rgb = [other.data[oi], other.data[oi + 1], other.data[oi + 2]];
                self.blend_pixel_with(x, y, rgb, a.min(255) as u8, mode);
            }
        }
    }

    /// PNG 形式（8 ビット RGBA、非インターレース）のバイト列を返す。
    ///
    /// IDAT は stored ブロックの zlib なので圧縮率はないが、
    /// あらゆる PNG デコーダで読める正規のストリームになる。
    pub fn to_png(&self) -> Vec<u8> {
        let row_bytes = (self.width as usize) * 4;
        // 各行の先頭にフィルタ種別 0（None）を付ける
        let mut raw = Vec::with_capacity((row_bytes + 1) * self.height as usize);
        for row in self.data.chunks_exact(row_bytes) {
            raw.push(0);
            raw.extend_from_slice(row);
        }

        let mut png = Vec::with_capacity(raw.len() + 64);
        png.extend_from_slice(b"\x89PNG\r\n\x1a\n");

        let mut ihdr = Vec::with_capacity(13);
        ihdr.extend_from_slice(&self.width.to_be_bytes());
        ihdr.extend_from_slice(&self.height.to_be_bytes());
        // ビット深度 8、カラータイプ 6（RGBA）、圧縮 0、フィルタ 0、非インターレース
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        push_chunk(&mut png, b"IHDR", &ihdr);
        push_chunk(&mut png, b"IDAT", &flate::compress(&raw));
        push_chunk(&mut png, b"IEND", &[]);
        png
    }

    /// PNG ファイルとして保存する。
    pub fn save_png(&self, path: impl AsRef<Path>) -> Result<()> {
        std::fs::write(path, self.to_png())?;
        Ok(())
    }
}

/// PNG チャンク（長さ + 種別 + データ + CRC-32）を書き足す。
fn push_chunk(out: &mut Vec<u8>, tag: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(tag);
    out.extend_from_slice(data);
    let mut crc = update_crc32(0xFFFF_FFFF, tag);
    crc = update_crc32(crc, data);
    out.extend_from_slice(&(crc ^ 0xFFFF_FFFF).to_be_bytes());
}

/// CRC-32（PNG/zip 標準、多項式 0xEDB88320）のテーブル。
const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[n] = c;
        n += 1;
    }
    table
};

/// CRC-32 の途中状態 `crc` にデータを流し込む（初期値 0xFFFFFFFF、
/// 最終値は呼び出し側でビット反転する）。
fn update_crc32(crc: u32, data: &[u8]) -> u32 {
    let mut c = crc;
    for &b in data {
        c = CRC_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CRC-32 の標準チェック値（"123456789" → 0xCBF43926）。
    #[test]
    fn crc32_check_value() {
        assert_eq!(
            update_crc32(0xFFFF_FFFF, b"123456789") ^ 0xFFFF_FFFF,
            0xCBF4_3926
        );
    }

    #[test]
    fn new_is_opaque_white() {
        let pm = Pixmap::new(3, 2);
        assert_eq!(pm.width(), 3);
        assert_eq!(pm.height(), 2);
        assert_eq!(pm.data().len(), 3 * 2 * 4);
        assert!(pm.data().iter().all(|&b| b == 0xFF));
    }

    #[test]
    fn zero_size_is_clamped() {
        let pm = Pixmap::new(0, 0);
        assert_eq!((pm.width(), pm.height()), (1, 1));
    }

    #[test]
    fn set_and_get_pixel() {
        let mut pm = Pixmap::new(2, 2);
        pm.set_pixel(1, 0, [10, 20, 30]);
        assert_eq!(pm.pixel(1, 0), Some([10, 20, 30]));
        assert_eq!(pm.pixel(0, 0), Some([255, 255, 255]));
        assert_eq!(pm.pixel(2, 0), None); // 範囲外
        pm.set_pixel(5, 5, [1, 2, 3]); // 範囲外書き込みは無視
    }

    /// `new_transparent` で作った Pixmap は全成分 0、`has_alpha` が true。
    #[test]
    fn new_transparent_is_zero_alpha() {
        let pm = Pixmap::new_transparent(2, 2);
        assert!(pm.has_alpha());
        assert!(pm.data().iter().all(|&b| b == 0));
    }

    /// 不透明バッファ上の Multiply ブレンド: 灰背景 × 赤 = 暗い赤。
    #[test]
    fn blend_pixel_with_multiply_on_opaque() {
        let mut pm = Pixmap::new(1, 1);
        // 背景を灰 (128) で塗ってから赤を Multiply で被せる。
        pm.set_pixel(0, 0, [128, 128, 128]);
        pm.blend_pixel_with(0, 0, [255, 0, 0], 255, BlendMode::Multiply);
        let px = pm.pixel(0, 0).unwrap();
        // Multiply(128, 255) ≈ 128、Multiply(128, 0) = 0。
        assert!(
            (px[0] as i32 - 128).abs() <= 2 && px[1] <= 2 && px[2] <= 2,
            "{:?}",
            px
        );
    }

    /// 透明バッファに 50% の赤を塗ると αr ≈ 128、色は赤、A は 128 になる。
    #[test]
    fn blend_pixel_with_on_transparent_accumulates_alpha() {
        let mut pm = Pixmap::new_transparent(1, 1);
        pm.blend_pixel_with(0, 0, [255, 0, 0], 128, BlendMode::Normal);
        let raw = &pm.data()[..4];
        // αs=128 αb=0 → αr=128。Cr = Cs = 255,0,0。
        assert_eq!(raw[0], 255);
        assert_eq!(raw[1], 0);
        assert_eq!(raw[2], 0);
        assert!((raw[3] as i32 - 128).abs() <= 1, "alpha {:?}", raw[3]);
    }

    /// 透明グループのオフスクリーンを composite_from でメインへ合成する。
    /// 黒の半透明オフスクリーンを白いメインに足すと中間灰になる。
    #[test]
    fn composite_from_blends_transparent_into_opaque() {
        let mut off = Pixmap::new_transparent(4, 4);
        // 全面に黒 (0,0,0) α=128 を蓄積。
        for y in 0..4 {
            for x in 0..4 {
                off.blend_pixel_with(x, y, [0, 0, 0], 128, BlendMode::Normal);
            }
        }
        let mut main = Pixmap::new(4, 4);
        main.composite_from(&off, 255, BlendMode::Normal, None);
        let px = main.pixel(2, 2).unwrap();
        // 白×黒 を 50% で合成 → 約 127。
        assert!((px[0] as i32 - 127).abs() <= 2, "合成結果 {:?}", px);
    }

    #[test]
    fn blend_pixel_source_over() {
        let mut pm = Pixmap::new(1, 1);
        // 白地に黒を 50% 弱（128/255）混合 → (0*128 + 255*127 + 127)/255 = 127
        pm.blend_pixel(0, 0, [0, 0, 0], 128);
        assert_eq!(pm.pixel(0, 0), Some([127, 127, 127]));
        // alpha=255 は完全上書き
        pm.blend_pixel(0, 0, [10, 20, 30], 255);
        assert_eq!(pm.pixel(0, 0), Some([10, 20, 30]));
        // alpha=0 は変化なし
        pm.blend_pixel(0, 0, [200, 200, 200], 0);
        assert_eq!(pm.pixel(0, 0), Some([10, 20, 30]));
    }

    /// PNG のチャンク構造を検査し、IDAT を自前 inflate で伸長して
    /// 生スキャンラインと一致することを確かめる（往復検証）。
    #[test]
    fn png_structure_roundtrip() {
        let mut pm = Pixmap::new(2, 1);
        pm.set_pixel(0, 0, [255, 0, 0]);
        pm.set_pixel(1, 0, [0, 0, 255]);
        let png = pm.to_png();

        // シグネチャ
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        // IHDR: 長さ 13、幅 2、高さ 1、深度 8、カラータイプ 6
        assert_eq!(&png[8..12], &13u32.to_be_bytes());
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(&png[16..20], &2u32.to_be_bytes());
        assert_eq!(&png[20..24], &1u32.to_be_bytes());
        assert_eq!(&png[24..29], &[8, 6, 0, 0, 0]);
        // IEND で終わる（最後の 12 バイト = 長さ 0 + "IEND" + CRC）
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");

        // IDAT を伸長してスキャンライン（フィルタ 0 + RGBA×2）を検証
        let idat_len = u32::from_be_bytes([png[33], png[34], png[35], png[36]]) as usize;
        assert_eq!(&png[37..41], b"IDAT");
        let idat = &png[41..41 + idat_len];
        let raw = flate::decompress(idat).expect("IDAT は正規の zlib のはず");
        assert_eq!(raw, vec![0, 255, 0, 0, 255, 0, 0, 255, 255],);
    }
}
