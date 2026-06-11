//! 画像 XObject / インライン画像のデコードと描画。
//!
//! ストリーム辞書（または `BI` のインライン画像辞書）+ 生データを解釈し、
//! 描画可能な RGBA ピクセル列（[`DecodedImage`]）へ変換する。描画は
//! [`draw_image`] が CTM の逆行列でデバイスピクセルを画像 UV へ逆写像し、
//! サンプリングして [`Pixmap`] へアルファ合成する。
//!
//! ## 対応・非対応
//!
//! | 項目 | 対応 |
//! |---|---|
//! | /BitsPerComponent | 1 / 2 / 4 / 8 / 16（16 は上位 8bit 使用） |
//! | 色空間 | [`ColorSpace`] が解決できるもの全て（Indexed・CMYK・Separation 等） |
//! | /Decode | 任意の成分再写像（ImageMask の `[1 0]` 反転含む） |
//! | /ImageMask | 1bpc ステンシル（塗り色で塗る） |
//! | /SMask | 別画像（DeviceGray）をアルファチャネルにする。サイズ違いは最近傍リサンプル |
//! | /Filter | Flate/LZW/ASCII85/ASCIIHex/RunLength + 末尾 DCTDecode（JPEG） |
//! | /Mask（ステンシル・カラーキー） | **非対応**（無視して不透明に描く） |
//! | JPXDecode / CCITTFax / JBIG2 / progressive JPEG | **非対応**（描画せず読み飛ばす） |
//!
//! ## サンプリングと境界
//!
//! - 拡大・回転は**双線形補間**。ただし `/ImageMask` と `/Interpolate false` の
//!   1bpc 画像は**最近傍**にする（くっきりした 2 値の見た目を保つため）。
//! - 画像境界は簡略化のため、デバイスピクセル中心の UV が `[0,1)` を出たら
//!   そのピクセルを描かない（**境界 1 ピクセルのアンチエイリアスは行わない**）。
//! - CTM の行列式が ~0（特異）の場合は逆写像できないため描画しない。
//!
//! ## 安全性
//!
//! 画像データは信頼できない入力として扱う。全アクセスは `get(..)`、算術は
//! checked / saturating を用い、壊れた・切り詰められた入力でも panic しない。
//! ピクセル総数には上限ガード（[`MAX_PIXELS`]）を設ける。

use std::sync::atomic::{AtomicBool, Ordering};

use super::path::Matrix;
use super::pixmap::Pixmap;
use super::raster::Mask;
use crate::document::Document;
use crate::object::{Dictionary, Object};

use super::colorspace::ColorSpace;

/// 協調キャンセルフラグが立っているか（`None` は常に false）。
fn is_cancelled(cancel: Option<&AtomicBool>) -> bool {
    cancel.map(|c| c.load(Ordering::Relaxed)).unwrap_or(false)
}

/// デコード後ピクセル総数（幅 × 高さ）の上限。過大な割り当てを防ぐ。
const MAX_PIXELS: u64 = 1 << 26; // 約 6700 万ピクセル

/// 描画用にデコード済みの画像。
///
/// `pixels` は行優先 RGBA8（左上原点）。`alpha` は SMask 由来の不透明度を
/// 含む。`stencil` が `Some` の場合は [`ImageMask`] で、`(width*height)` の
/// ブール列（true = 塗る、false = 透明）を持ち、`pixels` は使わない。
pub(crate) struct DecodedImage {
    /// 画像幅（ピクセル）。
    pub width: u32,
    /// 画像高さ（ピクセル）。
    pub height: u32,
    /// RGBA8 ピクセル列（`width*height*4` バイト）。ステンシル時は空。
    pub pixels: Vec<u8>,
    /// ステンシルマスク（ImageMask）。`Some` なら塗り色でステンシル描画する。
    /// 各要素 true = 現在の塗り色で塗る、false = 透明。
    pub stencil: Option<Vec<bool>>,
    /// 最近傍サンプリングを強制するか。
    ///
    /// ImageMask、または 1bpc かつ `/Interpolate false` の画像で true。
    /// それ以外（拡大・回転を含む通常画像）は双線形でサンプリングする。
    pub force_nearest: bool,
}

impl DecodedImage {
    /// `(u, v)` ∈ `[0,1)` を画像座標へ写してサンプリングする。
    ///
    /// `bilinear` が真なら双線形補間、偽なら最近傍。戻り値は `(rgb, alpha)`。
    /// ステンシル画像では `alpha` が 0 か 255、`rgb` は呼び出し側が無視する。
    fn sample(&self, u: f64, v: f64, fill: [u8; 3], bilinear: bool) -> ([u8; 3], u8) {
        if self.width == 0 || self.height == 0 {
            return ([0, 0, 0], 0);
        }
        if let Some(stencil) = &self.stencil {
            // ステンシルは常に最近傍（2 値）。
            let ix =
                ((u * self.width as f64).floor() as i64).clamp(0, self.width as i64 - 1) as usize;
            let iy =
                ((v * self.height as f64).floor() as i64).clamp(0, self.height as i64 - 1) as usize;
            let idx = iy * self.width as usize + ix;
            let on = stencil.get(idx).copied().unwrap_or(false);
            return (fill, if on { 255 } else { 0 });
        }

        if bilinear {
            self.sample_bilinear(u, v)
        } else {
            self.sample_nearest(u, v)
        }
    }

    /// 最近傍サンプリング。
    fn sample_nearest(&self, u: f64, v: f64) -> ([u8; 3], u8) {
        let ix = ((u * self.width as f64).floor() as i64).clamp(0, self.width as i64 - 1) as usize;
        let iy =
            ((v * self.height as f64).floor() as i64).clamp(0, self.height as i64 - 1) as usize;
        self.texel(ix, iy)
    }

    /// 双線形サンプリング（テクセル中心を基準）。
    fn sample_bilinear(&self, u: f64, v: f64) -> ([u8; 3], u8) {
        // テクセル中心が (i+0.5)/N に来るよう -0.5 する。
        let fx = u * self.width as f64 - 0.5;
        let fy = v * self.height as f64 - 0.5;
        let x0 = fx.floor();
        let y0 = fy.floor();
        let tx = fx - x0;
        let ty = fy - y0;
        let clampx = |x: f64| -> usize { (x as i64).clamp(0, self.width as i64 - 1) as usize };
        let clampy = |y: f64| -> usize { (y as i64).clamp(0, self.height as i64 - 1) as usize };
        let x0i = clampx(x0);
        let x1i = clampx(x0 + 1.0);
        let y0i = clampy(y0);
        let y1i = clampy(y0 + 1.0);

        let (c00, a00) = self.texel(x0i, y0i);
        let (c10, a10) = self.texel(x1i, y0i);
        let (c01, a01) = self.texel(x0i, y1i);
        let (c11, a11) = self.texel(x1i, y1i);

        let lerp = |a: f64, b: f64, t: f64| a + (b - a) * t;
        let mut rgb = [0u8; 3];
        for k in 0..3 {
            let top = lerp(c00[k] as f64, c10[k] as f64, tx);
            let bot = lerp(c01[k] as f64, c11[k] as f64, tx);
            rgb[k] = lerp(top, bot, ty).round().clamp(0.0, 255.0) as u8;
        }
        let top_a = lerp(a00 as f64, a10 as f64, tx);
        let bot_a = lerp(a01 as f64, a11 as f64, tx);
        let alpha = lerp(top_a, bot_a, ty).round().clamp(0.0, 255.0) as u8;
        (rgb, alpha)
    }

    /// テクセル `(ix, iy)` の RGB とアルファを返す（範囲外は透明）。
    fn texel(&self, ix: usize, iy: usize) -> ([u8; 3], u8) {
        let base = (iy * self.width as usize + ix) * 4;
        match self.pixels.get(base..base + 4) {
            Some(px) => ([px[0], px[1], px[2]], px[3]),
            None => ([0, 0, 0], 0),
        }
    }
}

/// 画像辞書とパラメータをまとめた、デコードへの入力。
struct ImageParams {
    width: u32,
    height: u32,
    bpc: u32,
    interpolate: bool,
    /// 各成分の Decode 範囲（`[lo, hi]`）。
    decode: Vec<(f64, f64)>,
    color_space: ColorSpace,
}

/// ストリーム辞書（または BI 辞書）+ 生データを RGBA 画像へデコードする。
///
/// `dict` は画像辞書、`raw` は **エンコードされたままの**データ、`resources`
/// は色空間名の解決に使う実効リソース辞書。デコードできない（未対応形式・
/// 壊れている等）場合は `None` を返す（panic しない）。`cancel` は協調
/// キャンセルフラグで、立っていたら途中で打ち切って `None` を返す
/// （キャンセル全体の扱いは呼び出し側の Renderer が判定する）。
pub(crate) fn decode_image(
    doc: &Document,
    dict: &Dictionary,
    raw: &[u8],
    resources: &Dictionary,
    cancel: Option<&AtomicBool>,
) -> Option<DecodedImage> {
    let width = dict_int(dict, &["Width", "W"])?;
    let height = dict_int(dict, &["Height", "H"])?;
    if width <= 0 || height <= 0 {
        return None;
    }
    let width = width as u32;
    let height = height as u32;
    let pixels = (width as u64).checked_mul(height as u64)?;
    if pixels == 0 || pixels > MAX_PIXELS {
        return None;
    }

    let image_mask = dict_bool(dict, &["ImageMask", "IM"]).unwrap_or(false);
    let interpolate = dict_bool(dict, &["Interpolate", "I"]).unwrap_or(false);

    // /Filter チェーンを見て、末尾が DCTDecode なら JPEG として処理する。
    let filters = filter_names(doc, dict);
    let last_is_dct = filters
        .last()
        .map(|f| matches!(f.as_str(), "DCTDecode" | "DCT"))
        .unwrap_or(false);
    // 非対応の画像コーデックは描画しない。
    if filters.iter().any(|f| {
        matches!(
            f.as_str(),
            "JPXDecode" | "CCITTFaxDecode" | "CCF" | "JBIG2Decode"
        )
    }) {
        return None;
    }

    // ImageMask は 1bpc 固定。
    let bpc = if image_mask {
        1
    } else {
        dict_int(dict, &["BitsPerComponent", "BPC"]).unwrap_or(8) as u32
    };

    // --- ImageMask（ステンシル）の経路 ---
    if image_mask {
        return decode_image_mask(doc, dict, raw, width, height, &filters, cancel);
    }

    // --- 通常の画像 ---
    // 色空間を解決する。
    let cs_obj = dict.get("ColorSpace").or_else(|| dict.get("CS")).cloned();
    let color_space = match cs_obj {
        Some(o) => ColorSpace::parse(doc, &o, resources),
        None => {
            // DCT で 3 成分なら RGB、それ以外は DeviceGray を仮定。
            ColorSpace::DeviceGray
        }
    };

    // /Decode 配列（無ければ既定）。
    let decode = parse_decode(dict, &color_space, bpc);

    let params = ImageParams {
        width,
        height,
        bpc,
        interpolate,
        decode,
        color_space,
    };

    let mut img = if last_is_dct {
        decode_dct_image(doc, dict, raw, &filters, &params, cancel)?
    } else {
        decode_raster_image(doc, dict, raw, &params, cancel)?
    };

    // SMask（ソフトマスク）をアルファチャネルへ反映する。
    apply_smask(doc, dict, resources, &mut img, cancel);

    if is_cancelled(cancel) {
        return None;
    }
    Some(img)
}

/// ImageMask（1bpc ステンシル）をデコードする。
fn decode_image_mask(
    doc: &Document,
    dict: &Dictionary,
    raw: &[u8],
    width: u32,
    height: u32,
    filters: &[String],
    cancel: Option<&AtomicBool>,
) -> Option<DecodedImage> {
    // ステンシルにフィルタが付くのは通常 Flate/Run 等。DCT 等はサポートしない。
    if filters
        .iter()
        .any(|f| matches!(f.as_str(), "DCTDecode" | "DCT"))
    {
        return None;
    }
    let data = strip_filters(doc, dict, raw, filters)?;

    // /Decode のデフォルトは [0 1]。0 のサンプルを塗り、1 を透明とする。
    // /Decode [1 0] が指定されると反転（1 を塗り、0 を透明）。
    let decode = parse_decode(dict, &ColorSpace::DeviceGray, 1);
    // decode[0] = (lo, hi)。lo > hi（= [1 0]）なら反転。
    let invert = decode.first().map(|&(lo, hi)| lo > hi).unwrap_or(false);

    let row_bytes = (width as usize).div_ceil(8);
    let mut stencil = vec![false; (width as usize) * (height as usize)];
    for y in 0..height as usize {
        if is_cancelled(cancel) {
            return None;
        }
        let row_off = y.checked_mul(row_bytes)?;
        for x in 0..width as usize {
            let byte = data.get(row_off + x / 8).copied().unwrap_or(0);
            let bit = (byte >> (7 - (x % 8))) & 1;
            // bit==0 が「塗る」。invert で反転。
            let paint = if invert { bit == 1 } else { bit == 0 };
            if let Some(slot) = stencil.get_mut(y * width as usize + x) {
                *slot = paint;
            }
        }
    }

    Some(DecodedImage {
        width,
        height,
        pixels: Vec::new(),
        stencil: Some(stencil),
        force_nearest: true,
    })
}

/// DCTDecode（JPEG）画像をデコードする。手前のフィルタを剥がしてから
/// [`crate::filters::dct::decode`] を呼ぶ。
fn decode_dct_image(
    doc: &Document,
    dict: &Dictionary,
    raw: &[u8],
    filters: &[String],
    params: &ImageParams,
    cancel: Option<&AtomicBool>,
) -> Option<DecodedImage> {
    // 末尾 DCT の手前までのフィルタを剥がす。
    let dct_pos = filters.len().checked_sub(1)?;
    let pre = &filters[..dct_pos];
    let jpeg = if pre.is_empty() {
        raw.to_vec()
    } else {
        strip_filters_subset(doc, dict, raw, pre)?
    };

    if is_cancelled(cancel) {
        return None;
    }
    let decoded = crate::filters::dct::decode(&jpeg).ok()?;
    if decoded.width != params.width || decoded.height != params.height {
        // サイズが食い違う場合でも JPEG 側のサイズで描く（辞書が嘘のことがある）。
    }
    let w = decoded.width;
    let h = decoded.height;
    let nc = decoded.components as usize;
    let mut pixels = vec![0u8; (w as usize).checked_mul(h as usize)?.checked_mul(4)?];

    for i in 0..(w as usize * h as usize) {
        // 4096 ピクセルごとにキャンセルを確認（内周のオーバーヘッドを抑える）。
        if i % 4096 == 0 && is_cancelled(cancel) {
            return None;
        }
        let base = i * nc;
        let rgb = match nc {
            // DCT が 3 成分なら ColorSpace 指定より優先して RGB とみなす。
            3 => {
                let r = decoded.data.get(base).copied().unwrap_or(0);
                let g = decoded.data.get(base + 1).copied().unwrap_or(0);
                let b = decoded.data.get(base + 2).copied().unwrap_or(0);
                [r, g, b]
            }
            4 => {
                // CMYK（0=インクなし〜255=最大インク）→ RGB。colorspace の変換を流用。
                let c = decoded.data.get(base).copied().unwrap_or(0) as f64 / 255.0;
                let m = decoded.data.get(base + 1).copied().unwrap_or(0) as f64 / 255.0;
                let y = decoded.data.get(base + 2).copied().unwrap_or(0) as f64 / 255.0;
                let k = decoded.data.get(base + 3).copied().unwrap_or(0) as f64 / 255.0;
                ColorSpace::DeviceCMYK.to_rgb(&[c, m, y, k])
            }
            _ => {
                // 1 成分グレースケール。
                let g = decoded.data.get(base).copied().unwrap_or(0);
                [g, g, g]
            }
        };
        let o = i * 4;
        if let Some(px) = pixels.get_mut(o..o + 4) {
            px[0] = rgb[0];
            px[1] = rgb[1];
            px[2] = rgb[2];
            px[3] = 255;
        }
    }

    Some(DecodedImage {
        width: w,
        height: h,
        pixels,
        stencil: None,
        // JPEG（連続調）は常に双線形が自然。
        force_nearest: false,
    })
}

/// 生のラスタ画像（DCT 以外）をデコードする。ビット深度をアンパックし
/// `/Decode` で再写像してから色空間で RGB へ変換する。
fn decode_raster_image(
    doc: &Document,
    dict: &Dictionary,
    raw: &[u8],
    params: &ImageParams,
    cancel: Option<&AtomicBool>,
) -> Option<DecodedImage> {
    let filters = filter_names(doc, dict);
    let data = strip_filters(doc, dict, raw, &filters)?;

    let n = params.color_space.n_components();
    if n == 0 {
        return None;
    }
    let bpc = params.bpc;
    if !matches!(bpc, 1 | 2 | 4 | 8 | 16) {
        return None;
    }
    let width = params.width as usize;
    let height = params.height as usize;

    // 1 行のビット数（成分 × bpc）を 8 の倍数へパディング。
    let row_bits = width.checked_mul(n)?.checked_mul(bpc as usize)?;
    let row_bytes = row_bits.div_ceil(8);

    let max_val = ((1u64 << bpc) - 1) as f64;
    let mut pixels = vec![0u8; width.checked_mul(height)?.checked_mul(4)?];

    // Indexed 色空間は to_rgb にインデックス整数値をそのまま渡す。
    let is_indexed = matches!(params.color_space, ColorSpace::Indexed { .. });

    let mut comps = vec![0f64; n];
    for y in 0..height {
        if is_cancelled(cancel) {
            return None;
        }
        let row_off = match y.checked_mul(row_bytes) {
            Some(v) => v,
            None => break,
        };
        let mut reader = BitReader::new(&data, row_off, bpc);
        for x in 0..width {
            for (c, slot) in comps.iter_mut().enumerate() {
                let raw_val = reader.next() as f64;
                if is_indexed {
                    // Decode をインデックス範囲で再写像（既定は [0, 2^bpc-1]）。
                    let (lo, hi) = params.decode.first().copied().unwrap_or((0.0, max_val));
                    *slot = map_decode(raw_val, max_val, lo, hi);
                } else {
                    let (lo, hi) = params.decode.get(c).copied().unwrap_or((0.0, 1.0));
                    *slot = map_decode(raw_val, max_val, lo, hi);
                }
            }
            let rgb = params.color_space.to_rgb(&comps);
            let o = (y * width + x) * 4;
            if let Some(px) = pixels.get_mut(o..o + 4) {
                px[0] = rgb[0];
                px[1] = rgb[1];
                px[2] = rgb[2];
                px[3] = 255;
            }
        }
    }

    Some(DecodedImage {
        width: params.width,
        height: params.height,
        pixels,
        stencil: None,
        // 1bpc かつ補間オフは最近傍（くっきりした 2 値の見た目を保つ）。
        force_nearest: params.bpc == 1 && !params.interpolate,
    })
}

/// 生整数サンプルを Decode 範囲へ線形写像する。
///
/// `raw / max_val` を `[lo, hi]` へ写す。Indexed では `lo`/`hi` がインデックス
/// 範囲（整数）なので結果はインデックス値そのものになる。
fn map_decode(raw: f64, max_val: f64, lo: f64, hi: f64) -> f64 {
    if max_val <= 0.0 {
        return lo;
    }
    lo + (raw / max_val) * (hi - lo)
}

/// MSB ファーストのビット列から成分値を 1 つずつ取り出すリーダ。
struct BitReader<'a> {
    data: &'a [u8],
    byte_pos: usize,
    bit_pos: u32,
    bpc: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8], byte_off: usize, bpc: u32) -> BitReader<'a> {
        BitReader {
            data,
            byte_pos: byte_off,
            bit_pos: 0,
            bpc,
        }
    }

    /// 次の bpc ビット分の整数値を返す（ビッグエンディアン・MSB ファースト）。
    /// 16bit は 16bit 値をそのまま返し、`map_decode` で `max_val=65535` により
    /// 0.0–1.0 へ正規化される（上位 8bit を使うのと等価）。
    /// データ末尾を超えたビットは 0 とみなす（耐故障）。
    fn next(&mut self) -> u32 {
        let mut val: u32 = 0;
        for _ in 0..self.bpc {
            let byte = self.data.get(self.byte_pos).copied().unwrap_or(0);
            let bit = (byte >> (7 - self.bit_pos)) & 1;
            val = (val << 1) | bit as u32;
            self.bit_pos += 1;
            if self.bit_pos == 8 {
                self.bit_pos = 0;
                self.byte_pos += 1;
            }
        }
        val
    }
}

/// SMask ストリームをデコードして本体画像のアルファチャネルへ反映する。
///
/// SMask は DeviceGray 画像で、各サンプルがアルファ（0=透明〜255=不透明）。
/// 本体とサイズが異なる場合は最近傍でリサンプルする。/Mask（ステンシル・
/// カラーキー）は今回スコープ外のため無視する。
fn apply_smask(
    doc: &Document,
    dict: &Dictionary,
    resources: &Dictionary,
    img: &mut DecodedImage,
    cancel: Option<&AtomicBool>,
) {
    let smask_obj = match dict.get("SMask") {
        Some(o) => doc.resolve(o).clone(),
        None => return,
    };
    let smask_stream = match &smask_obj {
        Object::Stream(s) => s.clone(),
        _ => return,
    };
    // SMask 画像をグレースケールとしてデコードする。
    let mask = match decode_image(
        doc,
        &smask_stream.dict,
        &smask_stream.data,
        resources,
        cancel,
    ) {
        Some(m) => m,
        None => return,
    };
    if mask.stencil.is_some() || mask.width == 0 || mask.height == 0 {
        return;
    }

    let iw = img.width as usize;
    let ih = img.height as usize;
    for y in 0..ih {
        if is_cancelled(cancel) {
            return;
        }
        for x in 0..iw {
            // 本体ピクセル (x,y) に対応する SMask テクセル（最近傍）。
            let u = (x as f64 + 0.5) / iw as f64;
            let v = (y as f64 + 0.5) / ih as f64;
            let mx =
                ((u * mask.width as f64).floor() as i64).clamp(0, mask.width as i64 - 1) as usize;
            let my =
                ((v * mask.height as f64).floor() as i64).clamp(0, mask.height as i64 - 1) as usize;
            let (gray, _) = mask.texel(mx, my);
            // DeviceGray なので R=G=B。アルファとして使う。
            let a = gray[0];
            let o = (y * iw + x) * 4;
            if let Some(px) = img.pixels.get_mut(o..o + 4) {
                // 既存アルファ（通常 255）と乗算。
                px[3] = ((px[3] as u32 * a as u32 + 127) / 255) as u8;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 描画
// ---------------------------------------------------------------------------

/// デコード済み画像を CTM に従って Pixmap へ描画する。
///
/// 画像は単位正方形 `[0,1]×[0,1]` に定義され、`ctm` でデバイス空間へ写る。
/// `ctm` の逆行列でデバイスピクセル中心 → 画像 UV を逆写像し、サンプリング
/// して合成する（回転・せん断も自然に扱える）。`clip` があれば被覆値を乗算、
/// `alpha` は描画全体の不透明度（ExtGState `ca` 等）。`fill` はステンシル色。
/// `bilinear_allowed` が偽なら常に最近傍サンプリング（高速品質モード用）、
/// `cancel` は協調キャンセルフラグ（行単位で確認して打ち切る）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_image(
    pm: &mut Pixmap,
    img: &DecodedImage,
    ctm: &Matrix,
    clip: Option<&Mask>,
    alpha: u8,
    fill: [u8; 3],
    bilinear_allowed: bool,
    cancel: Option<&AtomicBool>,
) {
    if alpha == 0 || img.width == 0 || img.height == 0 {
        return;
    }
    // 逆行列を計算（特異なら描画しない）。
    let inv = match invert_matrix(ctm) {
        Some(m) => m,
        None => return,
    };

    // 画像四隅をデバイス空間へ写し、AABB を求めて走査範囲をクリップ。
    let corners = [
        ctm.apply(super::path::Point::new(0.0, 0.0)),
        ctm.apply(super::path::Point::new(1.0, 0.0)),
        ctm.apply(super::path::Point::new(0.0, 1.0)),
        ctm.apply(super::path::Point::new(1.0, 1.0)),
    ];
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for p in &corners {
        if !p.x.is_finite() || !p.y.is_finite() {
            return;
        }
        min_x = min_x.min(p.x);
        min_y = min_y.min(p.y);
        max_x = max_x.max(p.x);
        max_y = max_y.max(p.y);
    }
    let x_start = (min_x.floor().max(0.0)) as i64;
    let y_start = (min_y.floor().max(0.0)) as i64;
    let x_end = (max_x.ceil().min(pm.width() as f64)) as i64;
    let y_end = (max_y.ceil().min(pm.height() as f64)) as i64;
    if x_start >= x_end || y_start >= y_end {
        return;
    }

    // サンプリング方式: force_nearest（ImageMask・1bpc 補間オフ）と
    // 高速品質モード（bilinear_allowed = false）は最近傍、それ以外
    // （拡大・回転を含む通常画像）は双線形。ステンシルは sample 内で
    // bilinear フラグを無視して常に最近傍になる。
    let bilinear = bilinear_allowed && !img.force_nearest;

    let base_alpha = alpha as u32;
    for py in y_start..y_end {
        if is_cancelled(cancel) {
            return;
        }
        for px in x_start..x_end {
            // デバイスピクセル中心を画像 UV へ逆写像。
            let dx = px as f64 + 0.5;
            let dy = py as f64 + 0.5;
            let p = inv.apply(super::path::Point::new(dx, dy));
            let u = p.x;
            let v = p.y;
            // UV が単位正方形外なら描かない（境界の AA は省略）。
            if !(0.0..1.0).contains(&u) || !(0.0..1.0).contains(&v) {
                continue;
            }
            // PDF 画像の行 0 は単位正方形の上端（v=1）に対応する。サンプル空間は
            // 左上原点なので、v を反転して画像の行インデックスへ写す。
            let (rgb, src_a) = img.sample(u, 1.0 - v, fill, bilinear);
            if src_a == 0 {
                continue;
            }
            // 合成率 = src_a × alpha × clip 被覆。
            let mut a = (src_a as u32 * base_alpha + 127) / 255;
            if let Some(mask) = clip {
                let cov = mask.coverage(px as u32, py as u32) as u32;
                a = (a * cov + 127) / 255;
            }
            if a > 0 {
                pm.blend_pixel(px as u32, py as u32, rgb, a.min(255) as u8);
            }
        }
    }
}

/// アフィン行列の逆行列を返す（特異・非有限なら `None`）。
fn invert_matrix(m: &Matrix) -> Option<Matrix> {
    let det = m.a * m.d - m.b * m.c;
    if !det.is_finite() || det.abs() < 1e-12 {
        return None;
    }
    let inv_det = 1.0 / det;
    let a = m.d * inv_det;
    let b = -m.b * inv_det;
    let c = -m.c * inv_det;
    let d = m.a * inv_det;
    let e = -(m.e * a + m.f * c);
    let f = -(m.e * b + m.f * d);
    let r = Matrix { a, b, c, d, e, f };
    if [r.a, r.b, r.c, r.d, r.e, r.f].iter().all(|v| v.is_finite()) {
        Some(r)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// フィルタ剥がし
// ---------------------------------------------------------------------------

/// 辞書の `/Filter`（または省略名 `/F`）を名前の列として返す。
fn filter_names(doc: &Document, dict: &Dictionary) -> Vec<String> {
    let obj = match dict.get("Filter").or_else(|| dict.get("F")) {
        Some(o) => doc.resolve(o).clone(),
        None => return Vec::new(),
    };
    match obj {
        Object::Name(n) => vec![expand_filter_name(&n)],
        Object::Array(a) => a
            .iter()
            .filter_map(|o| doc.resolve(o).as_name().ok().map(expand_filter_name))
            .collect(),
        _ => Vec::new(),
    }
}

/// インライン画像の省略フィルタ名を正規名へ展開する。
fn expand_filter_name(n: &str) -> String {
    match n {
        "Fl" => "FlateDecode",
        "AHx" => "ASCIIHexDecode",
        "A85" => "ASCII85Decode",
        "RL" => "RunLengthDecode",
        "LZW" => "LZWDecode",
        "DCT" => "DCTDecode",
        "CCF" => "CCITTFaxDecode",
        other => other,
    }
    .to_string()
}

/// 全フィルタを剥がしてデコード済みデータを返す。
///
/// DCT 等の画像コーデックが混ざっている場合は剥がせないため `None`。
fn strip_filters(
    doc: &Document,
    dict: &Dictionary,
    raw: &[u8],
    filters: &[String],
) -> Option<Vec<u8>> {
    if filters.is_empty() {
        return Some(raw.to_vec());
    }
    strip_filters_subset(doc, dict, raw, filters)
}

/// 指定したフィルタ名の列だけを順に剥がす。
///
/// `decode_stream` を成分名を差し替えた一時辞書で呼ぶことで、DecodeParms の
/// 解決を流用する。画像コーデックが含まれるとエラーになり `None` を返す。
fn strip_filters_subset(
    doc: &Document,
    dict: &Dictionary,
    raw: &[u8],
    filters: &[String],
) -> Option<Vec<u8>> {
    if filters.is_empty() {
        return Some(raw.to_vec());
    }
    // 一時辞書に Filter 配列と元の DecodeParms を載せて decode_stream を呼ぶ。
    let mut tmp = Dictionary::new();
    let filter_arr: Vec<Object> = filters.iter().map(Object::name).collect();
    tmp.set("Filter", Object::Array(filter_arr));
    if let Some(dp) = dict.get("DecodeParms").or_else(|| dict.get("DP")) {
        tmp.set("DecodeParms", dp.clone());
    }
    let resolve = |o: &Object| doc.resolve(o).clone();
    crate::filters::decode_stream(&tmp, raw, Some(&resolve)).ok()
}

// ---------------------------------------------------------------------------
// /Decode の解釈
// ---------------------------------------------------------------------------

/// `/Decode`（または省略名 `/D`）配列を `(lo, hi)` のペア列として返す。
///
/// 無い場合は色空間の `default_decode(bpc)` を使う。
fn parse_decode(dict: &Dictionary, cs: &ColorSpace, bpc: u32) -> Vec<(f64, f64)> {
    let arr = match dict.get("Decode").or_else(|| dict.get("D")) {
        Some(Object::Array(a)) => a,
        _ => return cs.default_decode(bpc),
    };
    let nums: Vec<f64> = arr.iter().filter_map(|o| o.as_number().ok()).collect();
    if nums.len() < 2 {
        return cs.default_decode(bpc);
    }
    let mut pairs = Vec::new();
    let mut i = 0;
    while i + 1 < nums.len() {
        pairs.push((nums[i], nums[i + 1]));
        i += 2;
    }
    pairs
}

// ---------------------------------------------------------------------------
// 辞書ヘルパ（正規名・省略名の両対応）
// ---------------------------------------------------------------------------

/// 与えたキー候補のいずれかから整数を取り出す。
fn dict_int(dict: &Dictionary, keys: &[&str]) -> Option<i64> {
    for k in keys {
        if let Some(o) = dict.get(k) {
            if let Ok(v) = o.as_int() {
                return Some(v);
            }
        }
    }
    None
}

/// 与えたキー候補のいずれかから真偽値を取り出す。
fn dict_bool(dict: &Dictionary, keys: &[&str]) -> Option<bool> {
    for k in keys {
        if let Some(Object::Boolean(b)) = dict.get(k) {
            return Some(*b);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::StringFormat;

    fn doc() -> Document {
        Document::new()
    }

    /// 1bpc アンパック: 各ビットが MSB ファーストで読まれる。
    #[test]
    fn unpack_1bpc() {
        // 1 ピクセル 1 成分 1bpc、幅 8 → 1 バイトで 8 ピクセル。
        let data = [0b1010_0000u8];
        let mut r = BitReader::new(&data, 0, 1);
        assert_eq!(r.next(), 1);
        assert_eq!(r.next(), 0);
        assert_eq!(r.next(), 1);
        assert_eq!(r.next(), 0);
        assert_eq!(r.next(), 0);
    }

    /// 2bpc / 4bpc アンパック。
    #[test]
    fn unpack_2_and_4_bpc() {
        let data = [0b11_01_00_10u8];
        let mut r = BitReader::new(&data, 0, 2);
        assert_eq!(r.next(), 0b11);
        assert_eq!(r.next(), 0b01);
        assert_eq!(r.next(), 0b00);
        assert_eq!(r.next(), 0b10);

        let data4 = [0xAB, 0xCD];
        let mut r4 = BitReader::new(&data4, 0, 4);
        assert_eq!(r4.next(), 0xA);
        assert_eq!(r4.next(), 0xB);
        assert_eq!(r4.next(), 0xC);
        assert_eq!(r4.next(), 0xD);
    }

    /// 8bpc / 16bpc アンパック（16 は全 16bit 値を返す）。
    #[test]
    fn unpack_8_and_16_bpc() {
        let data8 = [0x12, 0x34];
        let mut r8 = BitReader::new(&data8, 0, 8);
        assert_eq!(r8.next(), 0x12);
        assert_eq!(r8.next(), 0x34);

        let data16 = [0x12, 0x34, 0xAB, 0xCD];
        let mut r16 = BitReader::new(&data16, 0, 16);
        assert_eq!(r16.next(), 0x1234);
        assert_eq!(r16.next(), 0xABCD);
    }

    /// 末尾を超えたら 0 を返す（耐故障）。
    #[test]
    fn bitreader_past_end_is_zero() {
        let data = [0xFFu8];
        let mut r = BitReader::new(&data, 0, 8);
        assert_eq!(r.next(), 0xFF);
        assert_eq!(r.next(), 0); // 範囲外
    }

    /// map_decode の線形写像。
    #[test]
    fn decode_linear_map() {
        // 8bit, [0,1]: raw 255 → 1.0, raw 0 → 0.0。
        assert!((map_decode(255.0, 255.0, 0.0, 1.0) - 1.0).abs() < 1e-9);
        assert!((map_decode(0.0, 255.0, 0.0, 1.0)).abs() < 1e-9);
        // 反転 [1,0]: raw 0 → 1.0。
        assert!((map_decode(0.0, 255.0, 1.0, 0.0) - 1.0).abs() < 1e-9);
    }

    /// 2x2 RGB 8bpc 画像をデコードして各ピクセルの色を検証。
    #[test]
    fn decode_rgb_2x2() {
        let doc = doc();
        let mut dict = Dictionary::new();
        dict.set("Width", Object::Integer(2));
        dict.set("Height", Object::Integer(2));
        dict.set("BitsPerComponent", Object::Integer(8));
        dict.set("ColorSpace", Object::name("DeviceRGB"));
        // 行ごとに 2px × 3ch。赤・緑 / 青・白。
        let raw = vec![
            255, 0, 0, 0, 255, 0, // row 0: 赤, 緑
            0, 0, 255, 255, 255, 255, // row 1: 青, 白
        ];
        let img = decode_image(&doc, &dict, &raw, &Dictionary::new(), None).unwrap();
        assert_eq!(img.width, 2);
        assert_eq!(img.height, 2);
        assert_eq!(img.texel(0, 0).0, [255, 0, 0]);
        assert_eq!(img.texel(1, 0).0, [0, 255, 0]);
        assert_eq!(img.texel(0, 1).0, [0, 0, 255]);
        assert_eq!(img.texel(1, 1).0, [255, 255, 255]);
    }

    /// Decode 反転（DeviceGray [1 0]）で白黒が入れ替わる。
    #[test]
    fn decode_gray_inverted() {
        let doc = doc();
        let mut dict = Dictionary::new();
        dict.set("Width", Object::Integer(2));
        dict.set("Height", Object::Integer(1));
        dict.set("BitsPerComponent", Object::Integer(8));
        dict.set("ColorSpace", Object::name("DeviceGray"));
        dict.set(
            "Decode",
            Object::Array(vec![Object::Real(1.0), Object::Real(0.0)]),
        );
        // raw 0（通常は黒）→ 反転で白、raw 255 → 黒。
        let raw = vec![0u8, 255];
        let img = decode_image(&doc, &dict, &raw, &Dictionary::new(), None).unwrap();
        assert_eq!(img.texel(0, 0).0, [255, 255, 255]);
        assert_eq!(img.texel(1, 0).0, [0, 0, 0]);
    }

    /// Indexed 画像: 1bpc でパレット 2 色を引く。
    #[test]
    fn decode_indexed_1bpc() {
        let doc = doc();
        let mut dict = Dictionary::new();
        dict.set("Width", Object::Integer(2));
        dict.set("Height", Object::Integer(1));
        dict.set("BitsPerComponent", Object::Integer(1));
        // [/Indexed /DeviceRGB 1 <赤,青>]
        let lookup = Object::String(vec![255, 0, 0, 0, 0, 255], StringFormat::Literal);
        dict.set(
            "ColorSpace",
            Object::Array(vec![
                Object::name("Indexed"),
                Object::name("DeviceRGB"),
                Object::Integer(1),
                lookup,
            ]),
        );
        // 1bpc, 幅 2 → 1 バイト（上位 2 ビットが index 0,1）。
        // index 0 → 赤, index 1 → 青。ビット列 0b01...
        let raw = vec![0b0100_0000u8];
        let img = decode_image(&doc, &dict, &raw, &Dictionary::new(), None).unwrap();
        assert_eq!(img.texel(0, 0).0, [255, 0, 0]);
        assert_eq!(img.texel(1, 0).0, [0, 0, 255]);
    }

    /// ImageMask: 1bpc ステンシル。0=塗る、1=透明（既定 Decode [0 1]）。
    #[test]
    fn decode_image_mask_stencil() {
        let doc = doc();
        let mut dict = Dictionary::new();
        dict.set("Width", Object::Integer(2));
        dict.set("Height", Object::Integer(1));
        dict.set("ImageMask", Object::Boolean(true));
        // ビット列 0b01 → px0=0(塗る), px1=1(透明)。
        let raw = vec![0b0100_0000u8];
        let img = decode_image(&doc, &dict, &raw, &Dictionary::new(), None).unwrap();
        let stencil = img.stencil.as_ref().unwrap();
        assert_eq!(stencil.len(), 2);
        assert!(stencil[0], "px0 は塗る");
        assert!(!stencil[1], "px1 は透明");
        // sample はステンシルでは fill 色 + α。
        let (rgb, a) = img.sample(0.25, 0.5, [10, 20, 30], false);
        assert_eq!(rgb, [10, 20, 30]);
        assert_eq!(a, 255);
        let (_, a2) = img.sample(0.75, 0.5, [10, 20, 30], false);
        assert_eq!(a2, 0);
    }

    /// ImageMask の Decode [1 0] 反転: ビット意味が逆になる。
    #[test]
    fn decode_image_mask_inverted() {
        let doc = doc();
        let mut dict = Dictionary::new();
        dict.set("Width", Object::Integer(2));
        dict.set("Height", Object::Integer(1));
        dict.set("ImageMask", Object::Boolean(true));
        dict.set(
            "Decode",
            Object::Array(vec![Object::Real(1.0), Object::Real(0.0)]),
        );
        let raw = vec![0b0100_0000u8];
        let img = decode_image(&doc, &dict, &raw, &Dictionary::new(), None).unwrap();
        let stencil = img.stencil.as_ref().unwrap();
        // 反転: px0=0→透明, px1=1→塗る。
        assert!(!stencil[0]);
        assert!(stencil[1]);
    }

    /// SMask リサンプル: 本体 2x1 RGB + 1x1 グレー SMask（128）→ アルファ 128。
    #[test]
    fn smask_resample_applies_alpha() {
        use crate::object::Stream;
        let doc = doc();
        // SMask: 1x1 DeviceGray、値 128。
        let mut sm_dict = Dictionary::new();
        sm_dict.set("Width", Object::Integer(1));
        sm_dict.set("Height", Object::Integer(1));
        sm_dict.set("BitsPerComponent", Object::Integer(8));
        sm_dict.set("ColorSpace", Object::name("DeviceGray"));
        let smask = Stream::new(sm_dict, vec![128u8]);

        let mut dict = Dictionary::new();
        dict.set("Width", Object::Integer(2));
        dict.set("Height", Object::Integer(1));
        dict.set("BitsPerComponent", Object::Integer(8));
        dict.set("ColorSpace", Object::name("DeviceRGB"));
        dict.set("SMask", Object::Stream(smask));
        let raw = vec![255u8, 0, 0, 0, 255, 0];
        let img = decode_image(&doc, &dict, &raw, &Dictionary::new(), None).unwrap();
        // 両ピクセルのアルファが 128 になる。
        assert_eq!(img.texel(0, 0).1, 128);
        assert_eq!(img.texel(1, 0).1, 128);
    }

    /// 不正 bpc・長さ不足で panic せず None または縮退。
    #[test]
    fn corrupt_image_no_panic() {
        let doc = doc();
        // bpc=3（不正）。
        let mut dict = Dictionary::new();
        dict.set("Width", Object::Integer(2));
        dict.set("Height", Object::Integer(2));
        dict.set("BitsPerComponent", Object::Integer(3));
        dict.set("ColorSpace", Object::name("DeviceGray"));
        assert!(decode_image(&doc, &dict, &[0u8], &Dictionary::new(), None).is_none());

        // 長さ不足のデータ（2x2 RGB だが 1 バイトしかない）→ 縮退して 0 埋め。
        let mut dict2 = Dictionary::new();
        dict2.set("Width", Object::Integer(2));
        dict2.set("Height", Object::Integer(2));
        dict2.set("BitsPerComponent", Object::Integer(8));
        dict2.set("ColorSpace", Object::name("DeviceRGB"));
        let img = decode_image(&doc, &dict2, &[1u8], &Dictionary::new(), None);
        assert!(img.is_some()); // 縮退でも Some
    }

    /// 巨大サイズはガードで None。
    #[test]
    fn huge_image_guarded() {
        let doc = doc();
        let mut dict = Dictionary::new();
        dict.set("Width", Object::Integer(100000));
        dict.set("Height", Object::Integer(100000));
        dict.set("BitsPerComponent", Object::Integer(8));
        dict.set("ColorSpace", Object::name("DeviceGray"));
        assert!(decode_image(&doc, &dict, &[], &Dictionary::new(), None).is_none());
    }

    /// 逆行列: 恒等は恒等、特異は None。
    #[test]
    fn matrix_inverse() {
        let id = Matrix::identity();
        let inv = invert_matrix(&id).unwrap();
        assert_eq!(inv, Matrix::identity());

        let singular = Matrix {
            a: 0.0,
            b: 0.0,
            c: 0.0,
            d: 0.0,
            e: 1.0,
            f: 1.0,
        };
        assert!(invert_matrix(&singular).is_none());

        // スケール 2 の逆はスケール 0.5。
        let s = Matrix::scale(2.0, 4.0);
        let si = invert_matrix(&s).unwrap();
        assert!((si.a - 0.5).abs() < 1e-9);
        assert!((si.d - 0.25).abs() < 1e-9);
    }
}
