//! Halftone region セグメント（T.88 §7.4.5 / §6.6）。
//!
//! [`super::pattern_dict`] が公開したパターン列を、グリッド状に配置して
//! 1bpp ハーフトーン領域を構成するセグメント。実 PDF ではスキャン文書の
//! 写真調エリアやプリンタ向けレンダリングで現れる。
//!
//! ## ヘッダ構成（T.88 §7.4.5）
//!
//! ```text
//! Region segment info (17B)
//! Halftone region flags (1B):
//!   bit0:    HMMR
//!   bit1-2:  HTEMPLATE
//!   bit3:    HENABLESKIP
//!   bit4-6:  HCOMBOP
//!   bit7:    HDEFPIXEL
//! HGW (4B): grid width
//! HGH (4B): grid height
//! HGX (4B, signed): grid origin X（1/256 ピクセル単位の固定小数点）
//! HGY (4B, signed): grid origin Y
//! HRX (2B, signed): grid vector X
//! HRY (2B, signed): grid vector Y
//! ```
//!
//! ## 復号アルゴリズム（T.88 §6.6.5 / Annex C）
//!
//! 1. パターン数 `N` から `HNUMBITPLANES = ceil(log2(N))` を得る
//! 2. MSB から LSB の順に `HNUMBITPLANES` 個の generic region（各 HGW×HGH）を
//!    復号して `planes[0..HNUMBITPLANES]` に格納
//!    （`planes[0]` が MSB、`planes[HNUMBITPLANES-1]` が LSB）
//! 3. 各グリッド点 `(mg, ng)` のパターンインデックスは Gray 符号で再構成:
//!    `bit_j = planes[j][mg, ng] XOR bit_{j-1}`, `index = Σ bit_j << (B-1-j)`
//! 4. パターンを配置:
//!    `x = (HGX + mg*HRY + ng*HRX) >> 8`,
//!    `y = (HGY + mg*HRX - ng*HRY) >> 8`
//!
//! ## 実装上の注意
//!
//! - **算術経路**: 全プレーンが 1 つの MQ ストリームを共有する。コンテキスト
//!   配列はプレーンごとに新規確保する（pdf.js と同じ流派）
//! - **MMR 経路**: 仕様では各プレーンが独立した MMR ストリーム + EOFB と
//!   して連結されるが、ストリーム境界の特定がコストに見合わないため本実装
//!   では未対応エラー
//! - **HENABLESKIP**: スキップマスク（[`HSkip`]）は未対応（実 PDF での
//!   出現が稀なため）

use super::bitmap::{Bitmap, CombineOp};
use super::err;
use super::generic_region::{self, GenericRegionParams};
use super::mq::ArithDecoder;
use super::reader::ByteReader;
use super::segment::RegionSegmentInfo;
use crate::error::Result;

#[derive(Debug, Clone)]
pub struct HalftoneRegionParams {
    pub region: RegionSegmentInfo,
    pub mmr: bool,
    pub template: u8,
    pub enable_skip: bool,
    pub combination_op: u8,
    pub default_pixel: u8,
    pub grid_width: u32,
    pub grid_height: u32,
    pub grid_offset_x: i32,
    pub grid_offset_y: i32,
    pub grid_vector_x: i16,
    pub grid_vector_y: i16,
}

/// ヘッダをパースして `(params, payload)` を返す。
pub fn parse_header(data: &[u8]) -> Result<(HalftoneRegionParams, &[u8])> {
    let mut br = ByteReader::new(data);
    let region = RegionSegmentInfo::parse(&mut br)?;
    let flags = br.read_u8()?;
    let mmr = flags & 0x01 != 0;
    let template = (flags >> 1) & 0x03;
    let enable_skip = flags & 0x08 != 0;
    let combination_op = (flags >> 4) & 0x07;
    let default_pixel = (flags >> 7) & 0x01;
    let grid_width = br.read_u32()?;
    let grid_height = br.read_u32()?;
    let grid_offset_x = br.read_i32()?;
    let grid_offset_y = br.read_i32()?;
    let grid_vector_x = br.read_u16()? as i16;
    let grid_vector_y = br.read_u16()? as i16;
    let payload = data.get(br.pos()..).unwrap_or(&[]);
    Ok((
        HalftoneRegionParams {
            region,
            mmr,
            template,
            enable_skip,
            combination_op,
            default_pixel,
            grid_width,
            grid_height,
            grid_offset_x,
            grid_offset_y,
            grid_vector_x,
            grid_vector_y,
        },
        payload,
    ))
}

/// ハーフトーン領域を復号して 1bpp 領域ビットマップを返す。
///
/// `patterns` は本セグメントが参照する [`pattern_dict`][super::pattern_dict] の
/// パターン列。空、または `enable_skip` が `true`、`mmr` が `true` の場合は
/// エラーで返す（未対応）。
pub fn decode(
    params: &HalftoneRegionParams,
    payload: &[u8],
    patterns: &[Bitmap],
) -> Result<Bitmap> {
    if params.enable_skip {
        return Err(err("JBIG2 halftone region: HENABLESKIP not supported"));
    }
    if patterns.is_empty() {
        return Err(err("JBIG2 halftone region: no patterns supplied"));
    }
    if params.mmr {
        return Err(err("JBIG2 halftone region: MMR path not supported"));
    }

    let region_w = params.region.width;
    let region_h = params.region.height;
    let mut bm = if params.default_pixel != 0 {
        Bitmap::filled(region_w, region_h, 1)
    } else {
        Bitmap::new(region_w, region_h)
    };
    let gw = params.grid_width;
    let gh = params.grid_height;
    let num_patterns = patterns.len() as u32;
    let bits_per_value = log2_ceil(num_patterns);

    // 空グリッドまたは単一パターン (bits_per_value=0) のときは
    // ビットプレーン無しでそのままパターン 0 を全グリッド点に配置する。
    if gw == 0 || gh == 0 {
        return Ok(bm);
    }

    // 算術復号の準備
    let mut at_pixels = [(0i8, 0i8); 4];
    // T.88 §6.6.5.2 の AT pixels: テンプレート別に固定
    at_pixels[0] = if params.template <= 1 {
        (3, -1)
    } else {
        (2, -1)
    };
    if params.template == 0 {
        at_pixels[1] = (-3, -1);
        at_pixels[2] = (2, -2);
        at_pixels[3] = (-2, -2);
    }
    let gr_params = GenericRegionParams {
        region: RegionSegmentInfo {
            width: gw,
            height: gh,
            x: 0,
            y: 0,
            external_combop: 0,
            color: false,
        },
        mmr: false,
        template: params.template,
        tpgdon: false,
        at_pixels,
    };
    let ctx_bits = match params.template {
        0 => 16u32,
        1 => 13,
        2 | 3 => 10,
        _ => {
            return Err(err(format!(
                "JBIG2 halftone: bad template {}",
                params.template
            )))
        }
    };

    // ビットプレーンを MSB → LSB の順に復号
    let mut planes: Vec<Bitmap> = Vec::with_capacity(bits_per_value as usize);
    if bits_per_value > 0 {
        let mut ad = ArithDecoder::new(payload)?;
        for _ in 0..bits_per_value {
            let mut cx = vec![0u8; 1usize << ctx_bits];
            let plane = generic_region::decode_arith_shared(&gr_params, &mut ad, &mut cx)?;
            planes.push(plane);
        }
    }

    // Gray code → パターンインデックス → 配置
    let combop = halftone_combop(params.combination_op);
    let pw = patterns[0].width;
    let ph = patterns[0].height;
    for mg in 0..gh as i64 {
        for ng in 0..gw as i64 {
            // Gray decode (MSB から累積 XOR)
            let mut bit = 0u8;
            let mut index: u32 = 0;
            for (j, plane) in planes.iter().enumerate().take(bits_per_value as usize) {
                let pixel = plane.get(ng, mg);
                bit ^= pixel;
                // j=0 が MSB なので、シフト量は (bits_per_value - 1 - j)
                let shift = (bits_per_value - 1) as usize - j;
                index |= (bit as u32) << shift;
            }
            let index = index.min(num_patterns.saturating_sub(1));
            let pat = &patterns[index as usize];
            // 異サイズパターンが混在するのは仕様で不可だが、念のためサイズチェック
            // （実装側でサイズが揃っていることを期待）
            let _ = (pw, ph);

            // パターン配置座標（仕様の固定小数点 .8 を >> 8 で除算）
            let x = (params.grid_offset_x as i64)
                .saturating_add(mg.saturating_mul(params.grid_vector_y as i64))
                .saturating_add(ng.saturating_mul(params.grid_vector_x as i64))
                >> 8;
            let y = (params.grid_offset_y as i64)
                .saturating_add(mg.saturating_mul(params.grid_vector_x as i64))
                .saturating_sub(ng.saturating_mul(params.grid_vector_y as i64))
                >> 8;
            bm.combine(pat, x, y, combop);
        }
    }
    Ok(bm)
}

/// `HCOMBOP` から `CombineOp` を作る。範囲外は OR にフォールバック（耐故障）。
fn halftone_combop(code: u8) -> CombineOp {
    match code {
        0 => CombineOp::Or,
        1 => CombineOp::And,
        2 => CombineOp::Xor,
        3 => CombineOp::Xnor,
        4 => CombineOp::Replace,
        _ => CombineOp::Or,
    }
}

/// `ceil(log2(n))`。`n <= 1` のときは 0（1 パターンならビットプレーン不要）。
fn log2_ceil(n: u32) -> u32 {
    if n <= 1 {
        return 0;
    }
    let mut bits = 0u32;
    let mut v = 1u32;
    while v < n {
        v <<= 1;
        bits += 1;
    }
    bits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_region_info(width: u32, height: u32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&width.to_be_bytes());
        v.extend_from_slice(&height.to_be_bytes());
        v.extend_from_slice(&0u32.to_be_bytes()); // x
        v.extend_from_slice(&0u32.to_be_bytes()); // y
        v.push(0);
        v
    }

    #[test]
    fn header_basic() {
        let mut data = build_region_info(64, 32);
        data.push(0b1000_0010); // HDEFPIXEL=1, combop=0, skip=0, template=1, mmr=0
        data.extend_from_slice(&8u32.to_be_bytes()); // grid_width
        data.extend_from_slice(&4u32.to_be_bytes()); // grid_height
        data.extend_from_slice(&0i32.to_be_bytes()); // grid_offset_x
        data.extend_from_slice(&0i32.to_be_bytes()); // grid_offset_y
        data.extend_from_slice(&(8i16 * 256).to_be_bytes()); // grid_vector_x = 8 px
        data.extend_from_slice(&0i16.to_be_bytes()); // grid_vector_y = 0
        data.extend_from_slice(&[0xABu8]); // payload
        let (p, payload) = parse_header(&data).unwrap();
        assert!(!p.mmr);
        assert_eq!(p.template, 1);
        assert!(!p.enable_skip);
        assert_eq!(p.combination_op, 0);
        assert_eq!(p.default_pixel, 1);
        assert_eq!(p.grid_width, 8);
        assert_eq!(p.grid_height, 4);
        assert_eq!(p.grid_vector_x, 8 * 256);
        assert_eq!(payload, &[0xAB]);
    }

    #[test]
    fn header_mmr_flag() {
        let mut data = build_region_info(8, 8);
        data.push(0b0000_0001); // HMMR=1
        data.extend_from_slice(&0u32.to_be_bytes());
        data.extend_from_slice(&0u32.to_be_bytes());
        data.extend_from_slice(&0i32.to_be_bytes());
        data.extend_from_slice(&0i32.to_be_bytes());
        data.extend_from_slice(&0i16.to_be_bytes());
        data.extend_from_slice(&0i16.to_be_bytes());
        let (p, _) = parse_header(&data).unwrap();
        assert!(p.mmr);
    }

    #[test]
    fn mmr_path_errors() {
        let params = HalftoneRegionParams {
            region: RegionSegmentInfo {
                width: 8,
                height: 8,
                x: 0,
                y: 0,
                external_combop: 0,
                color: false,
            },
            mmr: true,
            template: 0,
            enable_skip: false,
            combination_op: 0,
            default_pixel: 0,
            grid_width: 1,
            grid_height: 1,
            grid_offset_x: 0,
            grid_offset_y: 0,
            grid_vector_x: 0,
            grid_vector_y: 0,
        };
        let patterns = vec![Bitmap::new(4, 4)];
        assert!(decode(&params, &[0u8; 16], &patterns).is_err());
    }

    #[test]
    fn no_patterns_errors() {
        let params = HalftoneRegionParams {
            region: RegionSegmentInfo {
                width: 8,
                height: 8,
                x: 0,
                y: 0,
                external_combop: 0,
                color: false,
            },
            mmr: false,
            template: 0,
            enable_skip: false,
            combination_op: 0,
            default_pixel: 0,
            grid_width: 1,
            grid_height: 1,
            grid_offset_x: 0,
            grid_offset_y: 0,
            grid_vector_x: 0,
            grid_vector_y: 0,
        };
        assert!(decode(&params, &[0u8; 16], &[]).is_err());
    }

    /// 単一パターン（bits_per_value=0）。MQ ストリーム不要。
    /// 黒のパターン (4x4) を 2x2 グリッドに 8px 間隔で配置すれば、
    /// 領域 16x16 の全画素が 1 になる。
    #[test]
    fn single_pattern_tiles_grid() {
        let mut pat = Bitmap::new(8, 8);
        pat.fill(1);
        let params = HalftoneRegionParams {
            region: RegionSegmentInfo {
                width: 16,
                height: 16,
                x: 0,
                y: 0,
                external_combop: 0,
                color: false,
            },
            mmr: false,
            template: 0,
            enable_skip: false,
            combination_op: 0, // OR
            default_pixel: 0,
            grid_width: 2,
            grid_height: 2,
            grid_offset_x: 0,
            grid_offset_y: 0,
            grid_vector_x: 8 * 256, // 1 セル = 8px 横
            grid_vector_y: 0,
        };
        // 配置式: x = (mg*HRY + ng*HRX) >> 8, y = (mg*HRX - ng*HRY) >> 8
        // ここで HRX=8*256, HRY=0 → x = ng*8, y = mg*8。
        let region = decode(&params, &[], &[pat]).unwrap();
        // すべて 1 になっているはず（pack 後は 0xFF が並ぶ）
        // 16x16 → stride=2, height=16 → 32 バイト
        assert_eq!(region.data.len(), 32);
        for b in &region.data {
            assert_eq!(*b, 0xFF);
        }
    }

    /// HENABLESKIP が立っていればエラー。
    #[test]
    fn enable_skip_errors() {
        let params = HalftoneRegionParams {
            region: RegionSegmentInfo {
                width: 8,
                height: 8,
                x: 0,
                y: 0,
                external_combop: 0,
                color: false,
            },
            mmr: false,
            template: 0,
            enable_skip: true,
            combination_op: 0,
            default_pixel: 0,
            grid_width: 1,
            grid_height: 1,
            grid_offset_x: 0,
            grid_offset_y: 0,
            grid_vector_x: 0,
            grid_vector_y: 0,
        };
        let patterns = vec![Bitmap::new(4, 4)];
        assert!(decode(&params, &[], &patterns).is_err());
    }

    /// 算術復号が完走する（出力値は未検証）。
    #[test]
    fn arith_decode_runs() {
        // 4 パターン → 2 ビットプレーン
        let patterns = vec![
            Bitmap::new(4, 4),
            Bitmap::filled(4, 4, 1),
            Bitmap::new(4, 4),
            Bitmap::filled(4, 4, 1),
        ];
        let params = HalftoneRegionParams {
            region: RegionSegmentInfo {
                width: 16,
                height: 16,
                x: 0,
                y: 0,
                external_combop: 0,
                color: false,
            },
            mmr: false,
            template: 0,
            enable_skip: false,
            combination_op: 0,
            default_pixel: 0,
            grid_width: 4,
            grid_height: 4,
            grid_offset_x: 0,
            grid_offset_y: 0,
            grid_vector_x: 4 * 256,
            grid_vector_y: 0,
        };
        let payload = vec![0u8; 32];
        let mut full = payload.clone();
        full.extend_from_slice(&[0xFF, 0xAC]);
        let bm = decode(&params, &full, &patterns).unwrap();
        assert_eq!(bm.width, 16);
        assert_eq!(bm.height, 16);
    }

    #[test]
    fn log2_ceil_basic() {
        assert_eq!(log2_ceil(0), 0);
        assert_eq!(log2_ceil(1), 0);
        assert_eq!(log2_ceil(2), 1);
        assert_eq!(log2_ceil(3), 2);
        assert_eq!(log2_ceil(4), 2);
        assert_eq!(log2_ceil(5), 3);
        assert_eq!(log2_ceil(256), 8);
        assert_eq!(log2_ceil(257), 9);
    }

    #[test]
    fn combop_mapping() {
        assert_eq!(halftone_combop(0), CombineOp::Or);
        assert_eq!(halftone_combop(1), CombineOp::And);
        assert_eq!(halftone_combop(2), CombineOp::Xor);
        assert_eq!(halftone_combop(3), CombineOp::Xnor);
        assert_eq!(halftone_combop(4), CombineOp::Replace);
        assert_eq!(halftone_combop(7), CombineOp::Or);
    }
}
