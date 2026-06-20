//! Text region セグメント（T.88 §7.4.3 / §6.4）。
//!
//! シンボル辞書のシンボルをストリップに沿って配置し、1bpp 領域ビットマップを
//! 構築する。**Arithmetic 経路のみ**実装し、Huffman 経路と refinement 適用は
//! 単純な単一シンボル置換まで対応する。
//!
//! ## ヘッダ構成（T.88 §7.4.3.1）
//!
//! ```text
//! Region segment info (17B)
//! Text region segment flags (2B):
//!   bit0:    SBHUFF        (0 = arith, 1 = Huffman)
//!   bit1:    SBREFINE      (1 = 各インスタンスに refinement)
//!   bit2-3:  LOGSBSTRIPS   (ストリップ高 = 1 << LOGSBSTRIPS)
//!   bit4-5:  REFCORNER     (シンボル参照点: 0=BL, 1=TL, 2=BR, 3=TR)
//!   bit6:    TRANSPOSED
//!   bit7-8:  SBCOMBOP      (0=OR 1=AND 2=XOR 3=XNOR)
//!   bit9:    SBDEFPIXEL    (背景画素)
//!   bit10-14:SBDSOFFSET    (符号付き 5 ビット、ストリップ間 S オフセット)
//!   bit15:   SBRTEMPLATE   (refinement テンプレート)
//! Huffman flags (2B、SBHUFF=1 時のみ): SBHUFFFS/DS/DT/RDW/RDH/RDX/RDY/RSIZE
//! SBRAT pixels (4B、SBREFINE=1 かつ SBRTEMPLATE=0 時のみ)
//! SBNUMINSTANCES (4B)
//! ```

use super::bitmap::{Bitmap, CombineOp};
use super::err;
use super::mq::{ArithDecoder, IaidDecoder, IntDecoder};
use super::reader::ByteReader;
use super::refinement;
use super::segment::RegionSegmentInfo;
use crate::error::Result;

#[derive(Debug, Clone)]
pub struct TextRegionParams {
    pub region: RegionSegmentInfo,
    pub huffman: bool,
    pub refine: bool,
    /// `log_strip_size`。実ストリップ高は `1 << log_strip_size`。
    pub log_strip_size: u8,
    pub strip_size: u32,
    /// 0=BL 1=TL 2=BR 3=TR
    pub reference_corner: u8,
    pub transposed: bool,
    pub combination_op: u8,
    pub default_pixel: u8,
    /// SBDSOFFSET: 5 ビット符号付き値
    pub ds_offset: i32,
    pub refinement_template: u8,
    pub sbrat: [(i8, i8); 2],
    pub num_instances: u32,
}

/// `data` の先頭からテキスト領域パラメータと残りを返す。
pub fn parse_header(data: &[u8]) -> Result<(TextRegionParams, &[u8])> {
    let mut br = ByteReader::new(data);
    let region = RegionSegmentInfo::parse(&mut br)?;
    let flags = br.read_u16()?;
    let huffman = flags & 0x0001 != 0;
    let refine = flags & 0x0002 != 0;
    let log_strip_size = ((flags >> 2) & 0x03) as u8;
    let strip_size = 1u32 << log_strip_size;
    let reference_corner = ((flags >> 4) & 0x03) as u8;
    let transposed = flags & 0x0040 != 0;
    let combination_op = ((flags >> 7) & 0x03) as u8;
    let default_pixel = ((flags >> 9) & 0x01) as u8;
    // SBDSOFFSET: bits 10..14 を符号付き 5 ビットとして取り出す
    let ds_raw = ((flags >> 10) & 0x1F) as i32;
    let ds_offset = if ds_raw & 0x10 != 0 {
        ds_raw - 32
    } else {
        ds_raw
    };
    let refinement_template = ((flags >> 15) & 0x01) as u8;

    // SBHUFF=1 時のテーブル選択フラグ 2 バイト（読み飛ばし: 我々の実装は
    // 標準テーブルのみ + 単純構成）
    if huffman {
        let _huff_flags = br.read_u16()?;
    }

    let mut sbrat = [(-1i8, -1i8); 2];
    if refine && refinement_template == 0 {
        for slot in sbrat.iter_mut() {
            let x = br.read_u8()? as i8;
            let y = br.read_u8()? as i8;
            *slot = (x, y);
        }
    }
    let num_instances = br.read_u32()?;

    let payload_start = br.pos();
    let payload = data.get(payload_start..).unwrap_or(&[]);
    Ok((
        TextRegionParams {
            region,
            huffman,
            refine,
            log_strip_size,
            strip_size,
            reference_corner,
            transposed,
            combination_op,
            default_pixel,
            ds_offset,
            refinement_template,
            sbrat,
            num_instances,
        },
        payload,
    ))
}

// ---------------------------------------------------------------------------
// デコード本体（Arithmetic）
// ---------------------------------------------------------------------------

/// 入力シンボル列を使ってテキスト領域 1bpp ビットマップを構築する。
pub fn decode(params: &TextRegionParams, payload: &[u8], symbols: &[Bitmap]) -> Result<Bitmap> {
    if params.huffman {
        return Err(err("JBIG2 text region: Huffman path not yet supported"));
    }
    decode_arithmetic(params, payload, symbols)
}

fn decode_arithmetic(
    params: &TextRegionParams,
    payload: &[u8],
    symbols: &[Bitmap],
) -> Result<Bitmap> {
    if symbols.is_empty() {
        return Err(err("JBIG2 text region: no input symbols"));
    }
    let w = params.region.width;
    let h = params.region.height;
    let bm = if params.default_pixel != 0 {
        Bitmap::filled(w, h, 1)
    } else {
        Bitmap::new(w, h)
    };
    if params.num_instances == 0 {
        return Ok(bm);
    }
    let mut bm = bm;

    let mut ad = ArithDecoder::new(payload)?;
    let mut ia_dt = IntDecoder::new();
    let mut ia_fs = IntDecoder::new();
    let mut ia_ds = IntDecoder::new();
    let mut ia_it = IntDecoder::new();
    let mut ia_ri = IntDecoder::new();
    let mut ia_rdw = IntDecoder::new();
    let mut ia_rdh = IntDecoder::new();
    let mut ia_rdx = IntDecoder::new();
    let mut ia_rdy = IntDecoder::new();

    let symbol_code_length = bits_needed(symbols.len() as u64);
    let mut iaid = IaidDecoder::new(symbol_code_length);
    let mut gr_cx: Vec<u8> = Vec::new();

    // T.88 §6.4.5 のループ。NIstrip までを順に配置する。
    let mut strip_t: i32 = {
        let v = ia_dt
            .decode(&mut ad)
            .ok_or_else(|| err("JBIG2 text region: IADT initial OOB"))?;
        -v
    };
    let mut first_s: i32 = 0;
    let mut count: u32 = 0;
    let combop = combine_op_from(params.combination_op);

    while count < params.num_instances {
        let dt = ia_dt
            .decode(&mut ad)
            .ok_or_else(|| err("JBIG2 text region: IADT mid OOB"))?;
        strip_t = strip_t.saturating_add(dt);
        let dfs = ia_fs
            .decode(&mut ad)
            .ok_or_else(|| err("JBIG2 text region: IAFS OOB"))?;
        first_s = first_s.saturating_add(dfs);
        let mut current_s: i32 = first_s;

        loop {
            let current_t = if params.strip_size > 1 {
                ia_it
                    .decode(&mut ad)
                    .ok_or_else(|| err("JBIG2 text region: IAIT OOB"))?
            } else {
                0
            };
            let t = (params.strip_size as i32)
                .saturating_mul(strip_t)
                .saturating_add(current_t);
            let sym_id = iaid.decode(&mut ad);
            if sym_id as usize >= symbols.len() {
                return Err(err(format!(
                    "JBIG2 text region: symbol id {sym_id} out of range (have {})",
                    symbols.len()
                )));
            }
            let mut symbol = symbols[sym_id as usize].clone();
            let apply_ref = if params.refine {
                let v = ia_ri
                    .decode(&mut ad)
                    .ok_or_else(|| err("JBIG2 text region: IARI OOB"))?;
                v != 0
            } else {
                false
            };
            if apply_ref {
                let rdw = ia_rdw
                    .decode(&mut ad)
                    .ok_or_else(|| err("JBIG2 text region: IARDW OOB"))?;
                let rdh = ia_rdh
                    .decode(&mut ad)
                    .ok_or_else(|| err("JBIG2 text region: IARDH OOB"))?;
                let rdx = ia_rdx
                    .decode(&mut ad)
                    .ok_or_else(|| err("JBIG2 text region: IARDX OOB"))?;
                let rdy = ia_rdy
                    .decode(&mut ad)
                    .ok_or_else(|| err("JBIG2 text region: IARDY OOB"))?;
                let new_w = (symbol.width as i32).saturating_add(rdw).max(0) as u32;
                let new_h = (symbol.height as i32).saturating_add(rdh).max(0) as u32;
                let refined = refinement::decode_with_decoder(
                    new_w,
                    new_h,
                    params.refinement_template,
                    false,
                    &params.sbrat,
                    (rdw >> 1).saturating_add(rdx),
                    (rdh >> 1).saturating_add(rdy),
                    &symbol,
                    &mut ad,
                    Some(&mut gr_cx),
                )?;
                symbol = refined;
            }

            let sym_w = symbol.width as i32;
            let sym_h = symbol.height as i32;
            let offset_t = t - if params.reference_corner & 1 != 0 {
                0
            } else {
                sym_h - 1
            };
            let offset_s = current_s
                - if params.reference_corner & 2 != 0 {
                    sym_w - 1
                } else {
                    0
                };

            // 領域への合成
            if params.transposed {
                // S と T が入れ替わる: シンボルの (sx, sy) を領域の (offset_t+sy, offset_s+sx)
                bm.combine(&symbol, offset_t as i64, offset_s as i64, combop);
                current_s = current_s.saturating_add(sym_h - 1);
            } else {
                bm.combine(&symbol, offset_s as i64, offset_t as i64, combop);
                current_s = current_s.saturating_add(sym_w - 1);
            }

            count += 1;
            if count >= params.num_instances {
                break;
            }
            let ds = match ia_ds.decode(&mut ad) {
                None => break, // OOB → ストリップ終端
                Some(v) => v,
            };
            current_s = current_s
                .saturating_add(ds)
                .saturating_add(params.ds_offset);
        }
    }

    Ok(bm)
}

fn combine_op_from(code: u8) -> CombineOp {
    match code {
        0 => CombineOp::Or,
        1 => CombineOp::And,
        2 => CombineOp::Xor,
        3 => CombineOp::Xnor,
        _ => CombineOp::Or,
    }
}

fn bits_needed(n: u64) -> u32 {
    if n <= 1 {
        return 1;
    }
    let mut b = 0u32;
    let mut v = n - 1;
    while v > 0 {
        v >>= 1;
        b += 1;
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_region_info(width: u32, height: u32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&width.to_be_bytes());
        v.extend_from_slice(&height.to_be_bytes());
        v.extend_from_slice(&0u32.to_be_bytes());
        v.extend_from_slice(&0u32.to_be_bytes());
        v.push(0);
        v
    }

    #[test]
    fn header_arith_basic() {
        let mut data = build_region_info(64, 32);
        // flags: log_strip_size=1 (bit2-3=01), refcorner=1 (bit4-5=01), combop=0
        let flags: u16 = (1u16 << 2) | (1u16 << 4);
        data.extend_from_slice(&flags.to_be_bytes());
        data.extend_from_slice(&5u32.to_be_bytes()); // num instances
        data.extend_from_slice(&[0u8; 4]);

        let (p, payload) = parse_header(&data).unwrap();
        assert!(!p.huffman);
        assert_eq!(p.log_strip_size, 1);
        assert_eq!(p.strip_size, 2);
        assert_eq!(p.reference_corner, 1);
        assert_eq!(p.num_instances, 5);
        assert_eq!(payload, &[0u8; 4]);
    }

    #[test]
    fn header_with_refine_reads_sbrat() {
        let mut data = build_region_info(8, 8);
        // refine=1, refinement_template=0
        let flags: u16 = 0x0002;
        data.extend_from_slice(&flags.to_be_bytes());
        data.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]); // SBRAT
        data.extend_from_slice(&1u32.to_be_bytes());
        let (p, _) = parse_header(&data).unwrap();
        assert!(p.refine);
        assert_eq!(p.sbrat, [(-1, -1), (-1, -1)]);
    }

    #[test]
    fn huffman_path_errors() {
        let mut data = build_region_info(8, 8);
        let flags: u16 = 0x0001;
        data.extend_from_slice(&flags.to_be_bytes());
        data.extend_from_slice(&0u16.to_be_bytes()); // huffman selector flags
        data.extend_from_slice(&1u32.to_be_bytes());
        let (p, payload) = parse_header(&data).unwrap();
        assert!(p.huffman);
        let r = decode(&p, payload, &[Bitmap::new(4, 4)]);
        assert!(r.is_err());
    }

    /// 入力シンボル無しの arithmetic 経路はエラー。
    #[test]
    fn arith_no_symbols_errors() {
        let mut data = build_region_info(8, 8);
        data.extend_from_slice(&0u16.to_be_bytes());
        data.extend_from_slice(&1u32.to_be_bytes());
        data.extend_from_slice(&[0u8; 8]);
        data.extend_from_slice(&[0xFF, 0xAC]);
        let (p, payload) = parse_header(&data).unwrap();
        let r = decode(&p, payload, &[]);
        assert!(r.is_err());
    }
}
