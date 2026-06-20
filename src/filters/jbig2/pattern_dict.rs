//! Pattern dictionary セグメント（T.88 §7.4.4 / §6.7）。
//!
//! ハーフトーン領域（[`super::halftone_region`]）が使うパターンビットマップ列を
//! 符号化する辞書セグメント。全パターンを「幅 = `pattern_width * num_patterns`、
//! 高さ = `pattern_height`」の **1 つの大きな generic region** として復号し、
//! 横方向に `pattern_width` ごとに分割して個別の `Bitmap` 列として返す。
//!
//! ## ヘッダ構成（T.88 §7.4.4）
//!
//! ```text
//! Pattern dictionary flags (1B):
//!   bit0:    HDMMR (1 = MMR、0 = Arithmetic)
//!   bit1-2:  HDTEMPLATE (0-3)
//!   bit3-7:  予約
//! HDPW (1B): パターン幅 (1-255)
//! HDPH (1B): パターン高 (1-255)
//! GRAYMAX (4B): パターン数 - 1
//! ```
//!
//! ## AT pixels（T.88 §6.7.5）
//!
//! Generic region で参照する AT pixels はテンプレート別に固定値。
//!
//! - `HDTEMPLATE = 0`: `A1 = (-HDPW, 0)`, `A2 = (-3, -1)`, `A3 = (2, -2)`,
//!   `A4 = (-2, -2)`
//! - `HDTEMPLATE = 1/2/3`: `A1 = (-HDPW, 0)`
//!
//! A1 を `(-HDPW, 0)` にすることで、直前パターンの同じ列が文脈に入る
//! （連結ビットマップを横に並べたときに「左隣のパターンの同位置」を見る）。

use super::bitmap::Bitmap;
use super::err;
use super::generic_region::{decode_region, GenericRegionParams};
use super::reader::ByteReader;
use super::segment::RegionSegmentInfo;
use crate::error::Result;

#[derive(Debug, Clone)]
pub struct PatternDictParams {
    pub mmr: bool,
    pub template: u8,
    pub pattern_width: u8,
    pub pattern_height: u8,
    /// 最大パターンインデックス。パターン数 = `gray_max + 1`。
    pub gray_max: u32,
}

/// ヘッダをパースし、続く符号化データのスライスを返す。
pub fn parse_header(data: &[u8]) -> Result<(PatternDictParams, &[u8])> {
    let mut br = ByteReader::new(data);
    let flags = br.read_u8()?;
    let mmr = flags & 0x01 != 0;
    let template = (flags >> 1) & 0x03;
    let pattern_width = br.read_u8()?;
    let pattern_height = br.read_u8()?;
    let gray_max = br.read_u32()?;
    let payload = data.get(br.pos()..).unwrap_or(&[]);
    Ok((
        PatternDictParams {
            mmr,
            template,
            pattern_width,
            pattern_height,
            gray_max,
        },
        payload,
    ))
}

/// パターン辞書をデコードしてパターンビットマップ列を返す。
pub fn decode(params: &PatternDictParams, payload: &[u8]) -> Result<Vec<Bitmap>> {
    if params.pattern_width == 0 || params.pattern_height == 0 {
        return Err(err("JBIG2 pattern dict: pattern dimensions must be > 0"));
    }
    let num_patterns = (params.gray_max as u64).saturating_add(1);
    if num_patterns > 65536 {
        return Err(err(format!(
            "JBIG2 pattern dict: too many patterns {num_patterns}"
        )));
    }
    let pw = params.pattern_width as u32;
    let ph = params.pattern_height as u32;
    let collective_w = (pw as u64).saturating_mul(num_patterns);
    if collective_w > 65536 {
        return Err(err(format!(
            "JBIG2 pattern dict: collective width too large {collective_w}"
        )));
    }
    let collective_w = collective_w as u32;

    // T.88 §6.7.5 の AT pixels
    let mut at_pixels = [(0i8, 0i8); 4];
    // パターン幅は最大 255 なので i8 へキャストして負値化する際にラップを避ける
    let neg_pw = -(params.pattern_width as i16) as i8;
    at_pixels[0] = (neg_pw, 0);
    if params.template == 0 {
        at_pixels[1] = (-3, -1);
        at_pixels[2] = (2, -2);
        at_pixels[3] = (-2, -2);
    }

    let gr_params = GenericRegionParams {
        region: RegionSegmentInfo {
            width: collective_w,
            height: ph,
            x: 0,
            y: 0,
            external_combop: 0,
            color: false,
        },
        mmr: params.mmr,
        template: params.template,
        tpgdon: false,
        at_pixels,
    };
    let collective = decode_region(&gr_params, payload)?;

    // 横方向に pattern_width ごとに分割
    let mut patterns = Vec::with_capacity(num_patterns as usize);
    for i in 0..num_patterns as u32 {
        let mut bm = Bitmap::new(pw, ph);
        let x0 = i.saturating_mul(pw);
        for y in 0..ph {
            for x in 0..pw {
                let v = collective.get((x0 + x) as i64, y as i64);
                bm.set(x, y, v);
            }
        }
        patterns.push(bm);
    }
    Ok(patterns)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_arith_default() {
        let mut data = Vec::new();
        data.push(0b0000_0000); // HDMMR=0, HDTEMPLATE=0
        data.push(8); // HDPW
        data.push(8); // HDPH
        data.extend_from_slice(&15u32.to_be_bytes()); // GRAYMAX = 15 → 16 patterns
        data.extend_from_slice(&[0xAB, 0xCD]); // payload
        let (p, payload) = parse_header(&data).unwrap();
        assert!(!p.mmr);
        assert_eq!(p.template, 0);
        assert_eq!(p.pattern_width, 8);
        assert_eq!(p.pattern_height, 8);
        assert_eq!(p.gray_max, 15);
        assert_eq!(payload, &[0xAB, 0xCD]);
    }

    #[test]
    fn header_mmr_template_high() {
        let mut data = Vec::new();
        data.push(0b0000_0101); // HDMMR=1, HDTEMPLATE=2
        data.push(4);
        data.push(4);
        data.extend_from_slice(&3u32.to_be_bytes()); // 4 patterns
        let (p, payload) = parse_header(&data).unwrap();
        assert!(p.mmr);
        assert_eq!(p.template, 2);
        assert_eq!(p.gray_max, 3);
        assert!(payload.is_empty());
    }

    /// 算術デコードは中身に依存せず、N パターンに分割された Bitmap 列を返す。
    /// 値は未検証だが、構造（数、サイズ）と panic 無しを保証する。
    #[test]
    fn decode_arith_splits_into_n_patterns() {
        let params = PatternDictParams {
            mmr: false,
            template: 0,
            pattern_width: 4,
            pattern_height: 4,
            gray_max: 3, // 4 patterns
        };
        // 全ゼロ + ターミネータ
        let mut payload = vec![0u8; 32];
        payload.extend_from_slice(&[0xFF, 0xAC]);
        let patterns = decode(&params, &payload).unwrap();
        assert_eq!(patterns.len(), 4);
        for pat in &patterns {
            assert_eq!(pat.width, 4);
            assert_eq!(pat.height, 4);
        }
    }

    #[test]
    fn invalid_zero_dimensions_errors() {
        let params = PatternDictParams {
            mmr: false,
            template: 0,
            pattern_width: 0,
            pattern_height: 4,
            gray_max: 3,
        };
        assert!(decode(&params, &[0u8; 16]).is_err());
    }

    #[test]
    fn too_many_patterns_errors() {
        let params = PatternDictParams {
            mmr: false,
            template: 0,
            pattern_width: 100,
            pattern_height: 100,
            gray_max: 10_000, // 100 * 10001 = 1_000_100 > 65536
        };
        assert!(decode(&params, &[0u8; 16]).is_err());
    }
}
