//! Generic refinement region デコーダ（T.88 §6.3 / §7.4.7）。
//!
//! 既存の「参照ビットマップ」を低解像度として、その近傍画素を文脈に加えて
//! 新しいビットマップを算術復号する。Symbol dictionary や Text region で
//! 1 シンボル単位の細部修正（refinement）にも使う。
//!
//! ## コンテキスト構成
//!
//! - GRTEMPLATE=0: 13 ビット = 出力 3 画素 + 参照 8 画素 + AT 2 ペアぶん 2 画素
//! - GRTEMPLATE=1: 10 ビット = 出力 4 画素 + 参照 6 画素（AT pixel 無し）
//!
//! 既定 AT pixels:
//! - A1（出力側）= (-1, -1)
//! - A2（参照側）= (-1, -1)
//!
//! ## TPGRON（typical prediction、§6.3.5.6）
//!
//! 行頭で SLTP ビットを 1 つ読み、SLTP=1 ならその行はスキップ（参照そのまま）。
//! `RefinementReusedContexts` は GRTEMPLATE 0/1 でそれぞれ `0x0020`, `0x0008`。

use super::bitmap::Bitmap;
use super::err;
use super::mq::ArithDecoder;
use super::reader::ByteReader;
use super::segment::RegionSegmentInfo;
use crate::error::Result;

/// 出力／参照テンプレートの 1 要素（相対座標）。
#[derive(Debug, Clone, Copy)]
struct Pt {
    x: i32,
    y: i32,
}

/// GRTEMPLATE 別の出力／参照テンプレート定義。
struct RefTmpl {
    /// 出力ビットマップから取る画素（既に復号済みの近傍）。
    coding: &'static [Pt],
    /// 参照ビットマップから取る画素。
    reference: &'static [Pt],
    /// TPGRON 用の固定コンテキストインデックス。
    sltp_cx: u32,
}

const T0_CODING: &[Pt] = &[Pt { x: 0, y: -1 }, Pt { x: 1, y: -1 }, Pt { x: -1, y: 0 }];
const T0_REFERENCE: &[Pt] = &[
    Pt { x: 0, y: -1 },
    Pt { x: 1, y: -1 },
    Pt { x: -1, y: 0 },
    Pt { x: 0, y: 0 },
    Pt { x: 1, y: 0 },
    Pt { x: -1, y: 1 },
    Pt { x: 0, y: 1 },
    Pt { x: 1, y: 1 },
];

const T1_CODING: &[Pt] = &[
    Pt { x: -1, y: -1 },
    Pt { x: 0, y: -1 },
    Pt { x: 1, y: -1 },
    Pt { x: -1, y: 0 },
];
const T1_REFERENCE: &[Pt] = &[
    Pt { x: 0, y: -1 },
    Pt { x: -1, y: 0 },
    Pt { x: 0, y: 0 },
    Pt { x: 1, y: 0 },
    Pt { x: 0, y: 1 },
    Pt { x: 1, y: 1 },
];

const T0: RefTmpl = RefTmpl {
    coding: T0_CODING,
    reference: T0_REFERENCE,
    sltp_cx: 0x0020,
};

const T1: RefTmpl = RefTmpl {
    coding: T1_CODING,
    reference: T1_REFERENCE,
    sltp_cx: 0x0008,
};

fn template_def(t: u8) -> Result<&'static RefTmpl> {
    Ok(match t {
        0 => &T0,
        1 => &T1,
        _ => return Err(err(format!("JBIG2 refinement: invalid GRTEMPLATE {t}"))),
    })
}

// ---------------------------------------------------------------------------
// パラメータ
// ---------------------------------------------------------------------------

/// セグメントヘッダ＋本体から取り出した refinement region のパラメータ。
#[derive(Debug, Clone)]
pub struct RefinementParams {
    pub region: RegionSegmentInfo,
    pub template: u8,
    pub tpgron: bool,
    /// AT pixels（GRTEMPLATE=0 のときのみ意味を持つ）。
    /// `at_pixels[0]` が出力側 A1、`at_pixels[1]` が参照側 A2。
    pub at_pixels: [(i8, i8); 2],
    /// 参照ビットマップの原点を出力(0,0) を基準にしたオフセット。
    pub reference_dx: i32,
    pub reference_dy: i32,
}

/// 領域セグメント本体から `(params, payload)` を分離する。
///
/// `data` の先頭は領域セグメント情報フィールド（17 バイト）。
/// 続いて 1 バイト flags、GRTEMPLATE=0 なら 4 バイト AT pixels、そして
/// 4 バイトずつ GRREFERENCEDX、GRREFERENCEDY。
pub fn parse_header(data: &[u8]) -> Result<(RefinementParams, &[u8])> {
    let mut br = ByteReader::new(data);
    let region = RegionSegmentInfo::parse(&mut br)?;
    let flags = br.read_u8()?;
    let template = flags & 0x01;
    let tpgron = flags & 0x02 != 0;
    let mut at_pixels = [(-1i8, -1i8); 2];
    if template == 0 {
        for slot in at_pixels.iter_mut() {
            let x = br.read_u8()? as i8;
            let y = br.read_u8()? as i8;
            *slot = (x, y);
        }
    }
    let reference_dx = br.read_i32()?;
    let reference_dy = br.read_i32()?;
    let payload_start = br.pos();
    let payload = data.get(payload_start..).unwrap_or(&[]);
    Ok((
        RefinementParams {
            region,
            template,
            tpgron,
            at_pixels,
            reference_dx,
            reference_dy,
        },
        payload,
    ))
}

// ---------------------------------------------------------------------------
// デコード本体
// ---------------------------------------------------------------------------

/// 関数 API: パラメータ＋参照ビットマップ＋MQ 入力ストリームから出力 Bitmap を返す。
///
/// セグメント単独で使う場合は `decode_region`、シンボル/テキスト経由で他の MQ
/// 状態を共有したい場合は [`decode_with_decoder`] を使う。
pub fn decode_region(
    params: &RefinementParams,
    reference: &Bitmap,
    payload: &[u8],
) -> Result<Bitmap> {
    // 出力が空ならペイロード読み出し無しで終了（耐故障）。
    if params.region.width == 0 || params.region.height == 0 {
        return Ok(Bitmap::new(params.region.width, params.region.height));
    }
    let mut ad = ArithDecoder::new(payload)?;
    decode_with_decoder(
        params.region.width,
        params.region.height,
        params.template,
        params.tpgron,
        &params.at_pixels,
        params.reference_dx,
        params.reference_dy,
        reference,
        &mut ad,
        None,
    )
}

/// 任意の MQ デコーダ（既存のコンテキスト配列を持ち越せる）を使って
/// refinement を実行する。`cx_external` が `Some` なら専用配列を共有
/// （symbol dictionary 全体でコンテキストを保持する場合に使う）。
#[allow(clippy::too_many_arguments)]
pub fn decode_with_decoder(
    width: u32,
    height: u32,
    template: u8,
    tpgron: bool,
    at_pixels: &[(i8, i8); 2],
    reference_dx: i32,
    reference_dy: i32,
    reference: &Bitmap,
    ad: &mut ArithDecoder,
    cx_external: Option<&mut Vec<u8>>,
) -> Result<Bitmap> {
    let tmpl = template_def(template)?;
    if width == 0 || height == 0 {
        return Ok(Bitmap::new(width, height));
    }

    // 文脈ビット数: GRTEMPLATE=0 は 13 ビット、=1 は 10 ビット。
    let ctx_bits = match template {
        0 => 13u32,
        1 => 10u32,
        _ => unreachable!(),
    };
    let cx_size = 1usize << ctx_bits;
    let mut owned: Vec<u8>;
    let cx: &mut Vec<u8> = match cx_external {
        Some(c) => {
            if c.len() < cx_size {
                c.resize(cx_size, 0);
            }
            c
        }
        None => {
            owned = vec![0u8; cx_size];
            &mut owned
        }
    };
    let mut sltp_cx = vec![0u8; cx_size];

    let mut bm = Bitmap::new(width, height);
    let mut sltp = 0u8;

    for y in 0..height as i64 {
        if tpgron {
            let bit = ad.decode(&mut sltp_cx, tmpl.sltp_cx as usize);
            sltp ^= bit;
            if sltp == 1 {
                // T.88 §6.3.5.6: SLTP=1 行は「typical prediction」で参照行をコピー。
                // 実装の単純化のため、参照と出力が同サイズでないケースでも
                // 同じ X 範囲を逐一参照する。
                let ry = y - reference_dy as i64;
                for x in 0..width {
                    let rx = x as i64 - reference_dx as i64;
                    bm.set(x, y as u32, reference.get(rx, ry));
                }
                continue;
            }
        }
        decode_row(
            &mut bm,
            y,
            tmpl,
            template,
            at_pixels,
            reference,
            reference_dx,
            reference_dy,
            cx,
            ad,
        );
    }
    Ok(bm)
}

#[allow(clippy::too_many_arguments)]
fn decode_row(
    bm: &mut Bitmap,
    y: i64,
    tmpl: &RefTmpl,
    template_idx: u8,
    at_pixels: &[(i8, i8); 2],
    reference: &Bitmap,
    rdx: i32,
    rdy: i32,
    cx: &mut [u8],
    ad: &mut ArithDecoder,
) {
    let w = bm.width as i64;
    for x in 0..w {
        let mut ctx: u32 = 0;
        // 出力側テンプレート
        for p in tmpl.coding {
            ctx <<= 1;
            ctx |= bm.get(x + p.x as i64, y + p.y as i64) as u32;
        }
        // GRTEMPLATE=0 のときは出力側 AT pixel を 1 つ追加（A1）
        if template_idx == 0 {
            ctx <<= 1;
            let (ax, ay) = at_pixels[0];
            ctx |= bm.get(x + ax as i64, y + ay as i64) as u32;
        }
        // 参照側テンプレート
        for p in tmpl.reference {
            ctx <<= 1;
            let rx = x + p.x as i64 - rdx as i64;
            let ry = y + p.y as i64 - rdy as i64;
            ctx |= reference.get(rx, ry) as u32;
        }
        // GRTEMPLATE=0 のときは参照側 AT pixel を 1 つ追加（A2）
        if template_idx == 0 {
            ctx <<= 1;
            let (ax, ay) = at_pixels[1];
            let rx = x + ax as i64 - rdx as i64;
            let ry = y + ay as i64 - rdy as i64;
            ctx |= reference.get(rx, ry) as u32;
        }
        let bit = ad.decode(cx, ctx as usize);
        bm.set(x as u32, y as u32, bit);
    }
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
    fn header_template0_parses_at_and_refdelta() {
        let mut data = build_region_info(16, 4);
        data.push(0b0000_0000); // template=0, tpgron=0
        data.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]); // AT pixels (-1,-1)(-1,-1)
        data.extend_from_slice(&3i32.to_be_bytes()); // reference dx
        data.extend_from_slice(&(-2i32).to_be_bytes()); // reference dy
        data.extend_from_slice(&[0u8; 4]);

        let (p, payload) = parse_header(&data).unwrap();
        assert_eq!(p.template, 0);
        assert!(!p.tpgron);
        assert_eq!(p.at_pixels, [(-1, -1), (-1, -1)]);
        assert_eq!(p.reference_dx, 3);
        assert_eq!(p.reference_dy, -2);
        assert_eq!(payload, &[0u8; 4]);
    }

    #[test]
    fn header_template1_skips_at_pixels() {
        let mut data = build_region_info(16, 4);
        data.push(0b0000_0011); // template=1, tpgron=1
        data.extend_from_slice(&0i32.to_be_bytes());
        data.extend_from_slice(&0i32.to_be_bytes());
        data.extend_from_slice(&[0xABu8]);

        let (p, payload) = parse_header(&data).unwrap();
        assert_eq!(p.template, 1);
        assert!(p.tpgron);
        assert_eq!(payload, &[0xAB]);
    }

    /// 参照が同サイズで TPGRON=1 + 全行 SLTP=1 のときは参照をそのままコピーする。
    /// （任意の入力で SLTP=1 を強制するのは難しいので、ゼロ height で
    ///  panic しないことだけ確認する。）
    #[test]
    fn decode_zero_height() {
        let params = RefinementParams {
            region: RegionSegmentInfo {
                width: 8,
                height: 0,
                x: 0,
                y: 0,
                external_combop: 0,
                color: false,
            },
            template: 0,
            tpgron: false,
            at_pixels: [(-1, -1), (-1, -1)],
            reference_dx: 0,
            reference_dy: 0,
        };
        let reference = Bitmap::new(8, 4);
        let bm = decode_region(&params, &reference, &[]).unwrap();
        assert_eq!(bm.height, 0);
    }

    /// 算術復号が完走することを確認する（出力値は未検証）。
    #[test]
    fn decode_arith_runs() {
        let params = RefinementParams {
            region: RegionSegmentInfo {
                width: 8,
                height: 4,
                x: 0,
                y: 0,
                external_combop: 0,
                color: false,
            },
            template: 1,
            tpgron: false,
            at_pixels: [(-1, -1), (-1, -1)],
            reference_dx: 0,
            reference_dy: 0,
        };
        let mut reference = Bitmap::new(8, 4);
        reference.fill(1);
        let payload = vec![0u8, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xAC];
        let bm = decode_region(&params, &reference, &payload).unwrap();
        assert_eq!(bm.width, 8);
        assert_eq!(bm.height, 4);
    }

    #[test]
    fn invalid_template_errors() {
        assert!(template_def(2).is_err());
    }
}
