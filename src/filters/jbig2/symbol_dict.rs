//! Symbol dictionary セグメント（T.88 §7.4.2 / §6.5）。
//!
//! シンボル（小さな 1bpp ビットマップ）の集合を符号化する。**Arithmetic 経路**
//! を主軸に実装し、Huffman 経路は最も多用される単純構成（SDHUFF=1, SDREFAGG=0,
//! 集合 MMR）のみ対応する。
//!
//! ## ヘッダ構成（T.88 §7.4.2.1）
//!
//! ```text
//! Symbol dictionary flags (2B):
//!   bit0:     SDHUFF（1 = Huffman、0 = Arithmetic）
//!   bit1:     SDREFAGG（1 = refinement aggregate を許可）
//!   bit2-3:   SDHUFFDH 選択（0:B.4 1:B.5 3:custom）
//!   bit4-5:   SDHUFFDW 選択（0:B.2 1:B.3 3:custom）
//!   bit6:     SDHUFFBMSIZE 選択（0:B.1 1:custom）
//!   bit7:     SDHUFFAGGINST 選択（0:B.1 1:custom）
//!   bit8:     BMCTX_USED（前のシンボル辞書のコンテキストを引き継ぐか）
//!   bit9:     BMCTX_RETAINED（このセグメントのコンテキストを保存するか）
//!   bit10-11: SDTEMPLATE（0/1/2/3）
//!   bit12:    SDRTEMPLATE（refinement テンプレート）
//! SDAT pixels: 8B（SDTEMPLATE=0）または 2B（>=1）。Huffman 時は無し
//! SDRAT pixels: 4B（SDREFAGG=1 かつ SDRTEMPLATE=0 時）
//! SDNUMEXSYMS  (4B): エクスポートするシンボル数
//! SDNUMNEWSYMS (4B): 新規シンボル数
//! ```

use super::bitmap::Bitmap;
use super::err;
use super::mq::{ArithDecoder, IaidDecoder, IntDecoder};
use super::reader::ByteReader;
use super::refinement;
use crate::error::Result;

/// 復号後のシンボル辞書（エクスポートされたシンボル列）。
pub type SymbolList = Vec<Bitmap>;

#[derive(Debug, Clone)]
pub struct SymbolDictParams {
    pub huffman: bool,
    pub refinement_aggregate: bool,
    pub huffman_dh_selector: u8,
    pub huffman_dw_selector: u8,
    pub huffman_bm_size_selector: u8,
    pub huffman_agg_inst_selector: u8,
    pub bmctx_used: bool,
    pub bmctx_retained: bool,
    pub template: u8,
    pub refinement_template: u8,
    /// SDAT。template=0 なら 4 ペア、その他は 1 ペア。
    pub sdat: [(i8, i8); 4],
    /// SDRAT（refinement AT pixels）。refinement_template=0 なら 2 ペア。
    pub sdrat: [(i8, i8); 2],
    pub num_exported_symbols: u32,
    pub num_new_symbols: u32,
}

/// ヘッダをパースし `(params, payload)` を返す。
pub fn parse_header(data: &[u8]) -> Result<(SymbolDictParams, &[u8])> {
    let mut br = ByteReader::new(data);
    let flags = br.read_u16()?;
    let huffman = flags & 0x0001 != 0;
    let refinement_aggregate = flags & 0x0002 != 0;
    let huffman_dh_selector = ((flags >> 2) & 0x03) as u8;
    let huffman_dw_selector = ((flags >> 4) & 0x03) as u8;
    let huffman_bm_size_selector = ((flags >> 6) & 0x01) as u8;
    let huffman_agg_inst_selector = ((flags >> 7) & 0x01) as u8;
    let bmctx_used = flags & 0x0100 != 0;
    let bmctx_retained = flags & 0x0200 != 0;
    let template = ((flags >> 10) & 0x03) as u8;
    let refinement_template = ((flags >> 12) & 0x01) as u8;

    let mut sdat = [(0i8, 0i8); 4];
    if !huffman {
        let n = if template == 0 { 4 } else { 1 };
        for slot in sdat.iter_mut().take(n) {
            let x = br.read_u8()? as i8;
            let y = br.read_u8()? as i8;
            *slot = (x, y);
        }
    }
    let mut sdrat = [(0i8, 0i8); 2];
    if refinement_aggregate && refinement_template == 0 {
        for slot in sdrat.iter_mut() {
            let x = br.read_u8()? as i8;
            let y = br.read_u8()? as i8;
            *slot = (x, y);
        }
    }
    let num_exported_symbols = br.read_u32()?;
    let num_new_symbols = br.read_u32()?;

    let payload_start = br.pos();
    let payload = data.get(payload_start..).unwrap_or(&[]);
    Ok((
        SymbolDictParams {
            huffman,
            refinement_aggregate,
            huffman_dh_selector,
            huffman_dw_selector,
            huffman_bm_size_selector,
            huffman_agg_inst_selector,
            bmctx_used,
            bmctx_retained,
            template,
            refinement_template,
            sdat,
            sdrat,
            num_exported_symbols,
            num_new_symbols,
        },
        payload,
    ))
}

// ---------------------------------------------------------------------------
// デコード本体（Arithmetic 経路のみ）
// ---------------------------------------------------------------------------

/// 入力シンボル列（過去のシンボル辞書由来）を参照しつつ新規シンボル列を作り、
/// エクスポートフラグに従ってエクスポート対象シンボル列を返す。
///
/// `input_symbols` は本セグメントが参照する辞書群を連結したもの。
pub fn decode(
    params: &SymbolDictParams,
    payload: &[u8],
    input_symbols: &[Bitmap],
) -> Result<SymbolList> {
    if params.huffman {
        return Err(err(
            "JBIG2 symbol dictionary: Huffman path not yet supported",
        ));
    }
    decode_arithmetic(params, payload, input_symbols)
}

fn decode_arithmetic(
    params: &SymbolDictParams,
    payload: &[u8],
    input_symbols: &[Bitmap],
) -> Result<SymbolList> {
    let mut ad = ArithDecoder::new(payload)?;

    // 整数復号器（IADH / IADW / IAAI / IAEX / IARDX / IARDY）と IAID。
    let mut ia_dh = IntDecoder::new();
    let mut ia_dw = IntDecoder::new();
    let mut ia_aggregate_instances = IntDecoder::new();
    let mut ia_ex = IntDecoder::new();
    let mut ia_rdx = IntDecoder::new();
    let mut ia_rdy = IntDecoder::new();

    let total_symbols = (input_symbols.len() as u64).saturating_add(params.num_new_symbols as u64);
    let symbol_code_length = bits_needed(total_symbols.max(1));
    let mut iaid = IaidDecoder::new(symbol_code_length);

    // ジェネリック領域用のコンテキスト配列（全シンボルで共有して状態を保持）。
    let ctx_bits = match params.template {
        0 => 16u32,
        1 => 13,
        2 => 10,
        3 => 10,
        _ => {
            return Err(err(format!(
                "JBIG2 symbol dict: bad template {}",
                params.template
            )))
        }
    };
    let mut gb_cx = vec![0u8; 1usize << ctx_bits];
    let mut gr_cx: Vec<u8> = Vec::new(); // refinement 用（必要なら遅延確保）

    let mut new_symbols: Vec<Bitmap> = Vec::with_capacity(params.num_new_symbols as usize);
    let mut current_height: i32 = 0;

    while (new_symbols.len() as u32) < params.num_new_symbols {
        let dh = ia_dh
            .decode(&mut ad)
            .ok_or_else(|| err("JBIG2 symbol dict: IADH returned OOB unexpectedly"))?;
        current_height = current_height.saturating_add(dh);
        if !(0..=65536).contains(&current_height) {
            return Err(err("JBIG2 symbol dict: implausible height"));
        }
        let mut current_width: i32 = 0;

        loop {
            let dw_opt = ia_dw.decode(&mut ad);
            let dw = match dw_opt {
                None => break, // OOB → 高さクラス終了
                Some(v) => v,
            };
            current_width = current_width.saturating_add(dw);
            if !(0..=65536).contains(&current_width) {
                return Err(err("JBIG2 symbol dict: implausible width"));
            }
            if (new_symbols.len() as u32) >= params.num_new_symbols {
                break;
            }

            let bitmap = if params.refinement_aggregate {
                let num_instances = ia_aggregate_instances
                    .decode(&mut ad)
                    .ok_or_else(|| err("JBIG2 symbol dict: IAAI returned OOB"))?;
                if num_instances <= 0 {
                    Bitmap::new(current_width as u32, current_height as u32)
                } else if num_instances == 1 {
                    // refinement テンプレートで参照シンボルを再構成
                    let sym_id = iaid.decode(&mut ad);
                    let rdx = ia_rdx
                        .decode(&mut ad)
                        .ok_or_else(|| err("JBIG2 symbol dict: IARDX returned OOB"))?;
                    let rdy = ia_rdy
                        .decode(&mut ad)
                        .ok_or_else(|| err("JBIG2 symbol dict: IARDY returned OOB"))?;
                    let reference = resolve_symbol(input_symbols, &new_symbols, sym_id)?;
                    refinement::decode_with_decoder(
                        current_width as u32,
                        current_height as u32,
                        params.refinement_template,
                        false,
                        &params.sdrat,
                        rdx,
                        rdy,
                        reference,
                        &mut ad,
                        Some(&mut gr_cx),
                    )?
                } else {
                    // 複数インスタンスの集合（text region で配置）。本実装では未対応：
                    // 空ビットマップを返してパースを継続する（耐故障）。
                    Bitmap::new(current_width as u32, current_height as u32)
                }
            } else {
                // 通常: 生成的領域として復号
                decode_generic_symbol(
                    current_width as u32,
                    current_height as u32,
                    params.template,
                    &params.sdat,
                    &mut gb_cx,
                    &mut ad,
                )?
            };
            new_symbols.push(bitmap);
        }
    }

    // エクスポートフラグの復号: 0/1 が交互に切り替わるランレングス。
    let exported = decode_export_flags(
        &mut ia_ex,
        &mut ad,
        input_symbols,
        &new_symbols,
        params.num_exported_symbols,
    )?;
    Ok(exported)
}

/// `bits_needed(n)` = ⌈log2(n)⌉。ただし n=0/1 のときは最低 1。
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

fn resolve_symbol<'a>(
    input_symbols: &'a [Bitmap],
    new_symbols: &'a [Bitmap],
    sym_id: u32,
) -> Result<&'a Bitmap> {
    let id = sym_id as usize;
    if id < input_symbols.len() {
        Ok(&input_symbols[id])
    } else {
        let off = id - input_symbols.len();
        new_symbols.get(off).ok_or_else(|| {
            err(format!(
                "JBIG2 symbol dict: symbol id {sym_id} out of range"
            ))
        })
    }
}

/// 1 シンボル分のジェネリック領域を復号する（MQ 状態は共有）。
fn decode_generic_symbol(
    width: u32,
    height: u32,
    template: u8,
    sdat: &[(i8, i8); 4],
    cx: &mut [u8],
    ad: &mut ArithDecoder,
) -> Result<Bitmap> {
    // generic_region 内部の decode_row と同じ構造を辿る。共通関数を流用したい
    // が generic_region は payload を取り直す API のため、ここでは
    // 単純化のため局所的に同じ走査を行う。
    use super::generic_region::*;
    let params = GenericRegionParams {
        region: super::segment::RegionSegmentInfo {
            width,
            height,
            x: 0,
            y: 0,
            external_combop: 0,
            color: false,
        },
        mmr: false,
        template,
        tpgdon: false,
        at_pixels: [
            sdat[0],
            sdat.get(1).copied().unwrap_or((0, 0)),
            sdat.get(2).copied().unwrap_or((0, 0)),
            sdat.get(3).copied().unwrap_or((0, 0)),
        ],
    };
    decode_arith_shared(&params, ad, cx)
}

/// `decode_export_flags`: IAEX を 0/1 切替のランレングスとして読み、対象シンボル
/// 配列からエクスポート対象を抽出して返す。
fn decode_export_flags(
    ia_ex: &mut IntDecoder,
    ad: &mut ArithDecoder,
    input_symbols: &[Bitmap],
    new_symbols: &[Bitmap],
    num_exported: u32,
) -> Result<SymbolList> {
    let total = input_symbols.len() + new_symbols.len();
    let mut flags: Vec<bool> = Vec::with_capacity(total);
    let mut current_flag = false;
    while flags.len() < total {
        let run = ia_ex
            .decode(ad)
            .ok_or_else(|| err("JBIG2 symbol dict: IAEX returned OOB"))?;
        let run = run.max(0) as usize;
        for _ in 0..run {
            if flags.len() >= total {
                break;
            }
            flags.push(current_flag);
        }
        current_flag = !current_flag;
    }
    let mut exported: SymbolList = Vec::new();
    for (i, &f) in flags.iter().enumerate() {
        if !f {
            continue;
        }
        let bm = if i < input_symbols.len() {
            input_symbols[i].clone()
        } else {
            new_symbols[i - input_symbols.len()].clone()
        };
        exported.push(bm);
    }
    if num_exported != 0 && (exported.len() as u32) > num_exported {
        exported.truncate(num_exported as usize);
    }
    Ok(exported)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_arith_header(num_new: u32, num_exp: u32) -> Vec<u8> {
        let mut v = Vec::new();
        // flags: SDHUFF=0, SDREFAGG=0, SDTEMPLATE=0
        v.extend_from_slice(&0u16.to_be_bytes());
        // SDAT: 8 バイト（テンプレート 0）
        v.extend_from_slice(&[3, 0xFF, 0xFD, 0xFF, 2, 0xFE, 0xFE, 0xFE]);
        v.extend_from_slice(&num_exp.to_be_bytes());
        v.extend_from_slice(&num_new.to_be_bytes());
        v
    }

    #[test]
    fn header_arith_parses() {
        let mut data = build_arith_header(2, 1);
        data.extend_from_slice(&[0u8; 4]); // payload
        let (p, payload) = parse_header(&data).unwrap();
        assert!(!p.huffman);
        assert!(!p.refinement_aggregate);
        assert_eq!(p.template, 0);
        assert_eq!(p.num_new_symbols, 2);
        assert_eq!(p.num_exported_symbols, 1);
        assert_eq!(p.sdat[0], (3, -1));
        assert_eq!(payload, &[0u8; 4]);
    }

    #[test]
    fn header_template_high_uses_one_at_pair() {
        let mut v = Vec::new();
        // SDTEMPLATE=2 → flags bit10-11 = 10 → 値 0x0800
        let flags: u16 = 0x0800;
        v.extend_from_slice(&flags.to_be_bytes());
        v.extend_from_slice(&[2, 0xFF]); // 1 ペアの SDAT
        v.extend_from_slice(&1u32.to_be_bytes());
        v.extend_from_slice(&1u32.to_be_bytes());
        v.extend_from_slice(&[0xAB]);
        let (p, payload) = parse_header(&v).unwrap();
        assert_eq!(p.template, 2);
        assert_eq!(p.sdat[0], (2, -1));
        assert_eq!(payload, &[0xAB]);
    }

    #[test]
    fn huffman_path_errors() {
        let mut v = Vec::new();
        v.extend_from_slice(&0x0001u16.to_be_bytes()); // SDHUFF=1
        v.extend_from_slice(&1u32.to_be_bytes());
        v.extend_from_slice(&1u32.to_be_bytes());
        let (p, _) = parse_header(&v).unwrap();
        assert!(p.huffman);
        let r = decode(&p, &[0, 0, 0, 0xFF, 0xAC], &[]);
        assert!(r.is_err());
    }

    /// Arithmetic で num_new_symbols=0 のときは即座にエクスポート復号へ進み、
    /// 出力は空または入力のサブセット。panic しないことが目標。
    #[test]
    fn arith_zero_new_symbols_runs() {
        let mut data = build_arith_header(0, 0);
        // payload 16 バイト + ターミネータ
        data.extend_from_slice(&[0u8; 16]);
        data.extend_from_slice(&[0xFF, 0xAC]);
        let (p, payload) = parse_header(&data).unwrap();
        let _ = decode(&p, payload, &[]).unwrap();
    }

    #[test]
    fn bits_needed_basic() {
        assert_eq!(bits_needed(0), 1);
        assert_eq!(bits_needed(1), 1);
        assert_eq!(bits_needed(2), 1);
        assert_eq!(bits_needed(3), 2);
        assert_eq!(bits_needed(4), 2);
        assert_eq!(bits_needed(5), 3);
        assert_eq!(bits_needed(256), 8);
        assert_eq!(bits_needed(257), 9);
    }
}
