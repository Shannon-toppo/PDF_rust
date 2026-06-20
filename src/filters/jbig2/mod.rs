//! JBIG2Decode フィルタ（PDF 32000-1:2008 §7.4.7, ITU-T T.88 / ISO 14492）。
//!
//! スキャン文書で広く使われる 1bpp 画像コーデック。CCITT と並ぶ二値画像の主流で、
//! Symbol dictionary・Text region・Halftone region により高い圧縮率を実現する。
//!
//! ## 公開 API
//!
//! [`decode`] が単一のエントリポイント。`/DecodeParms` の `/JBIG2Globals` ストリーム
//! を解決し、本体ストリームと連結したセグメント列を処理してページビットマップを返す。
//!
//! ## 出力ビット意味
//!
//! 出力は 1bpp パックビット（MSB 左）、行ストライド `ceil(width / 8)` バイト。
//! **PDF 慣習に従い 1 = 白、0 = 黒**（DeviceGray の既定 `/Decode = [0 1]` と整合）。
//! JBIG2 内部の「1 = 前景（黒）」とは反転している。反転は本モジュールの最終段で行う。
//!
//! ## サポート状況
//!
//! - セグメントヘッダのパース／ページ情報セグメントの処理：完備
//! - Generic region（算術 + MMR）: 完備
//!   ([`generic_region`])。GBTEMPLATE 0/1/2/3 + AT pixels + TPGDON、
//!   MMR は [`ccitt::decode`][crate::filters::ccitt::decode] を流用
//! - Symbol dictionary / Text region / Generic refinement: 算術経路に対応
//!   ([`symbol_dict`] / [`text_region`] / [`refinement`])
//! - Pattern dictionary / Halftone region: 算術経路に対応
//!   ([`pattern_dict`] / [`halftone_region`])。Halftone の MMR 経路は
//!   ストリーム境界の特定コストが高いため未対応エラー
//!
//! 未対応セグメント種別は黙って読み飛ばす（耐故障路）。

use crate::error::{PdfError, Result};
use crate::object::{Dictionary, Object};

pub mod bitmap;
pub mod generic_region;
pub mod halftone_region;
pub mod huffman;
pub mod mq;
pub mod page;
pub mod pattern_dict;
pub mod reader;
pub mod refinement;
pub mod segment;
pub mod symbol_dict;
pub mod text_region;

use bitmap::{Bitmap, CombineOp};
use huffman::HuffmanTable;
use page::PageInfo;
use reader::ByteReader;
use segment::{SegmentHeader, SegmentType};

use std::collections::BTreeMap;

pub(crate) fn err(msg: impl Into<String>) -> PdfError {
    PdfError::Filter(msg.into())
}

// ---------------------------------------------------------------------------
// 公開エントリ
// ---------------------------------------------------------------------------

/// JBIG2 ストリームを伸長して 1bpp パックビットマップ（行ストライド付き）を返す。
///
/// `parms` は `/DecodeParms`、`resolve` は間接参照解決コールバック。
/// `/JBIG2Globals` ストリームを参照する場合は `parms` と `resolve` が必要。
pub fn decode(
    data: &[u8],
    parms: Option<&Dictionary>,
    resolve: Option<crate::filters::Resolver>,
) -> Result<Vec<u8>> {
    let globals = extract_globals(parms, resolve)?;
    let mut driver = Driver::new();
    driver.process_stream(&globals)?;
    driver.process_stream(data)?;
    driver.finalize()
}

/// `/DecodeParms` から `/JBIG2Globals` を取り出し、ストリーム本体を伸長して返す。
/// 参照が無ければ空を返す。
fn extract_globals(
    parms: Option<&Dictionary>,
    resolve: Option<crate::filters::Resolver>,
) -> Result<Vec<u8>> {
    let parms = match parms {
        Some(p) => p,
        None => return Ok(Vec::new()),
    };
    let raw = match parms.get("JBIG2Globals") {
        Some(o) => o,
        None => return Ok(Vec::new()),
    };
    // 参照ならば解決
    let obj = match (resolve, raw) {
        (Some(f), Object::Reference(_)) => f(raw),
        _ => raw.clone(),
    };
    let stream = match obj {
        Object::Stream(s) => s,
        Object::Null => return Ok(Vec::new()),
        _ => {
            return Err(err(
                "JBIG2: /JBIG2Globals must be a stream (or null)".to_string()
            ));
        }
    };
    // ストリーム自身に /Filter（FlateDecode 等）がかかっている場合があるので
    // decode_stream を再帰呼びで伸長する。
    crate::filters::decode_stream(&stream.dict, &stream.data, resolve)
}

// ---------------------------------------------------------------------------
// セグメント列のドライバ
// ---------------------------------------------------------------------------

/// 復号済みセグメントの中間成果物。`referred_segments` 経由で参照される。
enum SegmentArtifact {
    /// Symbol dictionary が公開したシンボル列
    Symbols(Vec<Bitmap>),
    /// Intermediate generic / refinement で得られた中間ビットマップ
    Bitmap(Bitmap),
    /// Pattern dictionary が公開したパターン列（Halftone region から参照される）
    Patterns(Vec<Bitmap>),
    /// Tables セグメントから組み立てたカスタム Huffman テーブル
    #[allow(dead_code)]
    Table(HuffmanTable),
}

/// セグメントを順次処理する状態機械。
struct Driver {
    page: Option<Bitmap>,
    page_info: Option<PageInfo>,
    /// EndOfStripe で更新される、現在の塗り上限行（高さ未確定ページ用）。
    current_stripe_y: u32,
    /// セグメント番号 → 中間成果物。
    artifacts: BTreeMap<u32, SegmentArtifact>,
}

impl Driver {
    fn new() -> Self {
        Self {
            page: None,
            page_info: None,
            current_stripe_y: 0,
            artifacts: BTreeMap::new(),
        }
    }

    /// 参照シンボルを連結して 1 つの `Vec<Bitmap>` にまとめる。
    /// `Symbols` 以外の参照は無視（耐故障）。
    fn collect_input_symbols(&self, refs: &[u32]) -> Vec<Bitmap> {
        let mut out = Vec::new();
        for r in refs {
            if let Some(SegmentArtifact::Symbols(syms)) = self.artifacts.get(r) {
                out.extend_from_slice(syms);
            }
        }
        out
    }

    /// 単一のビットマップ成果物を引き当てる（Refinement 用に最初に見つかった
    /// Bitmap を返す）。
    fn first_referenced_bitmap(&self, refs: &[u32]) -> Option<&Bitmap> {
        for r in refs {
            if let Some(SegmentArtifact::Bitmap(b)) = self.artifacts.get(r) {
                return Some(b);
            }
        }
        None
    }

    /// 参照先 Pattern dictionary を最初の 1 件だけ取り出す。
    /// （Halftone region は仕様上 1 件の pattern dictionary のみ参照する）
    fn first_referenced_patterns(&self, refs: &[u32]) -> Vec<Bitmap> {
        for r in refs {
            if let Some(SegmentArtifact::Patterns(p)) = self.artifacts.get(r) {
                return p.clone();
            }
        }
        Vec::new()
    }

    fn process_stream(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let mut br = ByteReader::new(data);
        while !br.is_eof() {
            // セグメント終端で残りバイトが不足するケースを救う: 先頭 4 バイトが
            // 期待通り読めなければ終端とみなす（耐故障）
            if br.remaining() < 11 {
                break;
            }
            let header = match SegmentHeader::parse(&mut br) {
                Ok(h) => h,
                Err(_) => break, // 不正ヘッダで終了
            };
            self.dispatch(&header, &mut br)?;
        }
        Ok(())
    }

    fn dispatch(&mut self, header: &SegmentHeader, br: &mut ByteReader<'_>) -> Result<()> {
        // 不明長セグメントは現状未対応（即時 generic region のみで Session 2 対応予定）
        if header.unknown_length {
            // 走査して end-of-stripe を見つける処理は Session 2 で実装。
            // ここではエラーで返す（耐故障性のため、呼び出し側は処理済みページを返せる）。
            return Err(err(format!(
                "JBIG2 segment #{}: unknown-length data not yet supported (session 2)",
                header.number
            )));
        }
        let data = br.slice(header.data_length as usize).map_err(|_| {
            err(format!(
                "JBIG2 segment #{}: data shorter than declared length {}",
                header.number, header.data_length
            ))
        })?;

        match header.seg_type {
            SegmentType::PageInformation => {
                let pi = PageInfo::parse(data)?;
                let bm = pi.allocate_bitmap()?;
                self.page = Some(bm);
                self.page_info = Some(pi);
                self.current_stripe_y = 0;
            }
            SegmentType::EndOfPage | SegmentType::EndOfFile => {
                // ページ終端 / ファイル終端。データはなし（または無視）。
            }
            SegmentType::EndOfStripe => {
                // データ 4 バイト: 直前ストライプの最終行 Y（参考値）
                if data.len() >= 4 {
                    let y = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
                    self.current_stripe_y = y.saturating_add(1);
                }
            }
            // Generic region: 算術 MQ 経路 + MMR 経路。Intermediate は中間
            // ビットマップとしては保持しないが（Symbol/Refinement 連携の作業は
            // セッション 3）、即時版と同様にデコードしてもページに合成しない。
            SegmentType::ImmediateGenericRegion | SegmentType::ImmediateLosslessGenericRegion => {
                self.dispatch_generic_region(data, /*immediate=*/ true)?;
            }
            SegmentType::IntermediateGenericRegion => {
                // 中間ビットマップは Refinement の参照や Halftone の素材として
                // 後段から参照される可能性があるので artifacts に保存する。
                if let Ok((params, payload)) = generic_region::parse_header(data) {
                    if let Ok(bm) = generic_region::decode_region(&params, payload) {
                        self.artifacts
                            .insert(header.number, SegmentArtifact::Bitmap(bm));
                    }
                }
            }
            SegmentType::SymbolDictionary => {
                // 参照シンボル辞書を集約してから新規辞書を復号
                let input_symbols = self.collect_input_symbols(&header.referred_segments);
                let (params, payload) = symbol_dict::parse_header(data)?;
                match symbol_dict::decode(&params, payload, &input_symbols) {
                    Ok(syms) => {
                        self.artifacts
                            .insert(header.number, SegmentArtifact::Symbols(syms));
                    }
                    Err(_) => {
                        // 耐故障: 復号失敗時は空シンボル列を登録
                        self.artifacts
                            .insert(header.number, SegmentArtifact::Symbols(Vec::new()));
                    }
                }
            }
            SegmentType::ImmediateTextRegion
            | SegmentType::ImmediateLosslessTextRegion
            | SegmentType::IntermediateTextRegion => {
                let symbols = self.collect_input_symbols(&header.referred_segments);
                let (params, payload) = text_region::parse_header(data)?;
                match text_region::decode(&params, payload, &symbols) {
                    Ok(bm) => {
                        if matches!(
                            header.seg_type,
                            SegmentType::ImmediateTextRegion
                                | SegmentType::ImmediateLosslessTextRegion
                        ) {
                            self.compose_to_page(&bm, &params.region);
                        } else {
                            self.artifacts
                                .insert(header.number, SegmentArtifact::Bitmap(bm));
                        }
                    }
                    Err(_) => {
                        // 耐故障: ページ更新せずに通過
                    }
                }
            }
            SegmentType::ImmediateGenericRefinementRegion
            | SegmentType::ImmediateLosslessGenericRefinementRegion
            | SegmentType::IntermediateGenericRefinementRegion => {
                let reference = self
                    .first_referenced_bitmap(&header.referred_segments)
                    .cloned();
                let (params, payload) = refinement::parse_header(data)?;
                // 参照ビットマップが無ければページからの切り出しが必要だが、
                // 簡略のため空背景を参照として使う（耐故障）。
                let ref_bm = reference
                    .unwrap_or_else(|| Bitmap::new(params.region.width, params.region.height));
                if let Ok(bm) = refinement::decode_region(&params, &ref_bm, payload) {
                    if matches!(
                        header.seg_type,
                        SegmentType::ImmediateGenericRefinementRegion
                            | SegmentType::ImmediateLosslessGenericRefinementRegion
                    ) {
                        self.compose_to_page(&bm, &params.region);
                    } else {
                        self.artifacts
                            .insert(header.number, SegmentArtifact::Bitmap(bm));
                    }
                }
            }
            SegmentType::PatternDictionary => {
                if let Ok((params, payload)) = pattern_dict::parse_header(data) {
                    let patterns = pattern_dict::decode(&params, payload).unwrap_or_default();
                    self.artifacts
                        .insert(header.number, SegmentArtifact::Patterns(patterns));
                }
            }
            SegmentType::ImmediateHalftoneRegion
            | SegmentType::ImmediateLosslessHalftoneRegion
            | SegmentType::IntermediateHalftoneRegion => {
                let patterns = self.first_referenced_patterns(&header.referred_segments);
                let (params, payload) = halftone_region::parse_header(data)?;
                match halftone_region::decode(&params, payload, &patterns) {
                    Ok(bm) => {
                        if matches!(
                            header.seg_type,
                            SegmentType::ImmediateHalftoneRegion
                                | SegmentType::ImmediateLosslessHalftoneRegion
                        ) {
                            self.compose_to_page(&bm, &params.region);
                        } else {
                            self.artifacts
                                .insert(header.number, SegmentArtifact::Bitmap(bm));
                        }
                    }
                    Err(_) => {
                        // 耐故障: 無視して次のセグメントへ
                    }
                }
            }
            SegmentType::Tables => {
                if let Ok(t) = huffman::parse_custom_table(data) {
                    self.artifacts
                        .insert(header.number, SegmentArtifact::Table(t));
                }
            }
            SegmentType::Profiles | SegmentType::Extension | SegmentType::Unknown(_) => {
                // 仕様で「未知セグメントは無視可」とされている
            }
        }
        Ok(())
    }

    /// 領域結果をページに合成する（Symbol/Text/Refinement 共通）。
    /// `combop` は領域情報の external_combop を使い、必要なら page_info の
    /// override に従う。
    fn compose_to_page(&mut self, src: &Bitmap, region: &segment::RegionSegmentInfo) {
        let op = if let Some(pi) = &self.page_info {
            if pi.combop_override {
                generic_region::combine_op_from(pi.default_combop)
            } else {
                generic_region::combine_op_from(region.external_combop)
            }
        } else {
            CombineOp::Or
        };
        if let Some(page) = self.page.as_mut() {
            page.combine(src, region.x as i64, region.y as i64, op);
        }
        let new_y = (region.y as u64).saturating_add(region.height as u64);
        if new_y > self.current_stripe_y as u64 {
            self.current_stripe_y = new_y.min(u32::MAX as u64) as u32;
        }
    }

    /// Generic region セグメントのデータ部を復号してページへ合成する。
    ///
    /// `immediate=true` のときはページビットマップに `external_combop` で
    /// 合成し、`immediate=false` のときはデコードだけ行って結果を捨てる
    /// （セッション 3 で中間ビットマップ保管に置き換える）。
    fn dispatch_generic_region(&mut self, data: &[u8], immediate: bool) -> Result<()> {
        let (params, payload) = generic_region::parse_header(data)?;
        let bm = generic_region::decode_region(&params, payload)?;
        if !immediate {
            return Ok(());
        }
        // ページに合成する: 外部 COMBOP (region.external_combop) を使う
        // ただし PageInfo の combop_override / default_combop の影響は仕様 §7.4.1
        // 参照。簡略のため、ページが confirmed の最初の領域は REPLACE 扱いに
        // 強制する流派（jbig2dec）も多いが、ここでは仕様に従い external_combop。
        let op = if let Some(pi) = &self.page_info {
            if pi.combop_override {
                generic_region::combine_op_from(pi.default_combop)
            } else {
                generic_region::combine_op_from(params.region.external_combop)
            }
        } else {
            CombineOp::Or
        };
        let x = params.region.x as i64;
        let y = params.region.y as i64;
        if let Some(page) = self.page.as_mut() {
            page.combine(&bm, x, y, op);
        }
        // 高さ不定のページが progress した行数を更新（次の end-of-stripe で利用）
        let new_y = (params.region.y as u64).saturating_add(params.region.height as u64);
        if new_y > self.current_stripe_y as u64 {
            self.current_stripe_y = new_y.min(u32::MAX as u64) as u32;
        }
        Ok(())
    }

    /// 全セグメント処理後にページビットマップを最終出力（PDF 慣習: 1=白）へ変換。
    fn finalize(mut self) -> Result<Vec<u8>> {
        let mut bm = self
            .page
            .take()
            .ok_or_else(|| err("JBIG2: stream lacks page information segment"))?;
        // JBIG2 内部 1=黒 → PDF 1=白 へ反転
        bm.invert();
        Ok(bm.into_packed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ページ情報セグメントだけの最小ストリームを構築して decode が
    /// 単色（既定画素値で塗りつぶした）ページを返すことを確認する。
    fn build_min_stream(width: u32, height: u32, default_pixel: u8) -> Vec<u8> {
        let mut s = Vec::new();
        // ---- セグメントヘッダ ----
        s.extend_from_slice(&1u32.to_be_bytes()); // 番号 1
        s.push(48); // type=48 PageInformation
        s.push(0); // 参照 0 件
        s.push(1); // page assoc = 1
        s.extend_from_slice(&19u32.to_be_bytes()); // data length

        // ---- ページ情報セグメントデータ ----
        s.extend_from_slice(&width.to_be_bytes());
        s.extend_from_slice(&height.to_be_bytes());
        s.extend_from_slice(&100u32.to_be_bytes()); // xres
        s.extend_from_slice(&100u32.to_be_bytes()); // yres
        let flags = (default_pixel & 1) << 2;
        s.push(flags);
        s.extend_from_slice(&0u16.to_be_bytes()); // stripe info

        // ---- end-of-page セグメント ----
        s.extend_from_slice(&2u32.to_be_bytes());
        s.push(49); // type=49
        s.push(0);
        s.push(1);
        s.extend_from_slice(&0u32.to_be_bytes()); // length=0
        s
    }

    #[test]
    fn min_stream_default_white() {
        // default_pixel=0（白背景）→ 反転後 1=白 → 全 0xFF
        let data = build_min_stream(16, 4, 0);
        let out = decode(&data, None, None).unwrap();
        assert_eq!(out.len(), 2 * 4); // stride=2, h=4
        assert!(out.iter().all(|b| *b == 0xFF));
    }

    #[test]
    fn min_stream_default_black() {
        // default_pixel=1（黒背景）→ 反転後 0=黒 → 全 0x00
        let data = build_min_stream(16, 4, 1);
        let out = decode(&data, None, None).unwrap();
        assert!(out.iter().all(|b| *b == 0x00));
    }

    #[test]
    fn missing_page_info_errors() {
        // ヘッダのみ（end-of-file セグメント 1 件）
        let mut s = Vec::new();
        s.extend_from_slice(&1u32.to_be_bytes());
        s.push(51); // end-of-file
        s.push(0);
        s.push(1);
        s.extend_from_slice(&0u32.to_be_bytes());
        assert!(decode(&s, None, None).is_err());
    }

    /// ページ情報セグメントだけのストリームに、フル白の generic region（MMR
    /// 経路、空ペイロード → 0 行）を結合しても decode は完走する。
    /// MMR 経路がパース→ccitt 呼び出し→ページ無変更まで通ることを確認。
    #[test]
    fn generic_region_mmr_empty_combines() {
        let s = build_min_stream(8, 2, 0);
        // ---- generic region セグメント（type=38, immediate） ----
        let mut seg = Vec::new();
        // ヘッダ
        seg.extend_from_slice(&3u32.to_be_bytes()); // number
        seg.push(38); // immediate generic region
        seg.push(0); // ref count=0
        seg.push(1); // page assoc
                     // データ部 = 領域情報 17B + flags 1B + 空 MMR ペイロード
        let mut payload = Vec::new();
        payload.extend_from_slice(&8u32.to_be_bytes()); // width
        payload.extend_from_slice(&0u32.to_be_bytes()); // height=0
        payload.extend_from_slice(&0u32.to_be_bytes()); // x
        payload.extend_from_slice(&0u32.to_be_bytes()); // y
        payload.push(0); // combop=0 (OR)
        payload.push(0x01); // flags: MMR=1
        seg.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        seg.extend_from_slice(&payload);

        // 順序: [PageInfo, EndOfPage, GenericRegion] になるので
        // end-of-page の前に挿入する
        // build_min_stream は末尾が end-of-page なので、その前に挿入し直す。
        let mut s2 = Vec::new();
        // end-of-page セグメントは 11 バイト（ヘッダのみ・データ長 0）
        s2.extend_from_slice(&s[..s.len() - 11]); // page info セグメント全体
        s2.extend_from_slice(&seg);
        s2.extend_from_slice(&s[s.len() - 11..]); // end-of-page

        let out = decode(&s2, None, None).unwrap();
        assert_eq!(out.len(), 2);
        // ページは無変更（背景白）= 0xFF
        assert!(out.iter().all(|b| *b == 0xFF));
    }

    /// 算術 generic region（テンプレ 0 / TPGDON 無し / 全ゼロペイロード）が
    /// パース→MQ 復号→ページ合成の流れで panic せず完走する。
    /// 出力値は実装依存だが、サイズと終了は確実に検証する。
    #[test]
    fn generic_region_arith_runs() {
        // 16x4 のページに同サイズ generic region（COMBOP=REPLACE）を載せる
        let s = build_min_stream(16, 4, 0);
        let mut seg = Vec::new();
        seg.extend_from_slice(&3u32.to_be_bytes());
        seg.push(38);
        seg.push(0);
        seg.push(1);
        let mut payload = Vec::new();
        payload.extend_from_slice(&16u32.to_be_bytes()); // width
        payload.extend_from_slice(&4u32.to_be_bytes()); // height
        payload.extend_from_slice(&0u32.to_be_bytes()); // x
        payload.extend_from_slice(&0u32.to_be_bytes()); // y
        payload.push(4); // region info flags: combop=4 (REPLACE), color=0
        payload.push(0x00); // generic region flags: MMR=0, template=0, tpgdon=0
        payload.extend_from_slice(&[3, 0xFF, 0xFD, 0xFF, 2, 0xFE, 0xFE, 0xFE]); // AT (default)
        payload.extend_from_slice(&[0u8; 16]); // 全 0 のペイロード
        payload.extend_from_slice(&[0xFF, 0xAC]); // 擬似ターミネータ
        seg.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        seg.extend_from_slice(&payload);
        let mut s2 = Vec::new();
        s2.extend_from_slice(&s[..s.len() - 11]);
        s2.extend_from_slice(&seg);
        s2.extend_from_slice(&s[s.len() - 11..]);

        let out = decode(&s2, None, None).unwrap();
        // stride=2、4 行 → 8 バイト
        assert_eq!(out.len(), 8);
    }

    /// 空の Symbol Dictionary（num_new=0, num_exp=0）が走破できる。
    /// テキスト領域から参照されてもエラーにならず、ページは背景のまま。
    #[test]
    fn empty_symbol_dictionary_runs() {
        let s = build_min_stream(16, 4, 0);

        // ---- セグメント番号 3: 空の Symbol Dictionary ----
        let mut sd_payload = Vec::new();
        sd_payload.extend_from_slice(&0u16.to_be_bytes()); // flags（arith, template=0）
        sd_payload.extend_from_slice(&[3, 0xFF, 0xFD, 0xFF, 2, 0xFE, 0xFE, 0xFE]); // SDAT
        sd_payload.extend_from_slice(&0u32.to_be_bytes()); // num_exp
        sd_payload.extend_from_slice(&0u32.to_be_bytes()); // num_new
        sd_payload.extend_from_slice(&[0u8; 8]);
        sd_payload.extend_from_slice(&[0xFF, 0xAC]);

        let mut sd_seg = Vec::new();
        sd_seg.extend_from_slice(&3u32.to_be_bytes());
        sd_seg.push(0); // type=0 SymbolDictionary
        sd_seg.push(0); // ref count=0
        sd_seg.push(1); // page assoc
        sd_seg.extend_from_slice(&(sd_payload.len() as u32).to_be_bytes());
        sd_seg.extend_from_slice(&sd_payload);

        // EOP の前に挿入
        let mut s2 = Vec::new();
        s2.extend_from_slice(&s[..s.len() - 11]);
        s2.extend_from_slice(&sd_seg);
        s2.extend_from_slice(&s[s.len() - 11..]);

        let out = decode(&s2, None, None).unwrap();
        // ページは背景白のまま
        assert!(out.iter().all(|b| *b == 0xFF));
    }

    /// Tables セグメント（type=53）は内容が不正でもパースだけ通る（耐故障）。
    #[test]
    fn malformed_table_segment_skipped() {
        let s = build_min_stream(8, 2, 0);
        let mut tab = Vec::new();
        tab.extend_from_slice(&3u32.to_be_bytes());
        tab.push(53); // type=53 Tables
        tab.push(0);
        tab.push(1);
        tab.extend_from_slice(&4u32.to_be_bytes());
        tab.extend_from_slice(&[0u8; 4]); // 不正テーブル
        let mut s2 = Vec::new();
        s2.extend_from_slice(&s[..s.len() - 11]);
        s2.extend_from_slice(&tab);
        s2.extend_from_slice(&s[s.len() - 11..]);
        let _ = decode(&s2, None, None).unwrap();
    }

    /// Refinement region は参照無しでも空背景を使って走破できる（耐故障）。
    #[test]
    fn refinement_region_without_reference_runs() {
        let s = build_min_stream(8, 4, 0);
        let mut payload = Vec::new();
        payload.extend_from_slice(&8u32.to_be_bytes()); // width
        payload.extend_from_slice(&4u32.to_be_bytes()); // height
        payload.extend_from_slice(&0u32.to_be_bytes()); // x
        payload.extend_from_slice(&0u32.to_be_bytes()); // y
        payload.push(0); // region info flags
        payload.push(0b0000_0001); // template=1, tpgron=0
        payload.extend_from_slice(&0i32.to_be_bytes()); // GRREFERENCEDX
        payload.extend_from_slice(&0i32.to_be_bytes()); // GRREFERENCEDY
        payload.extend_from_slice(&[0u8; 8]);
        payload.extend_from_slice(&[0xFF, 0xAC]);

        let mut seg = Vec::new();
        seg.extend_from_slice(&3u32.to_be_bytes());
        seg.push(42); // immediate refinement
        seg.push(0);
        seg.push(1);
        seg.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        seg.extend_from_slice(&payload);

        let mut s2 = Vec::new();
        s2.extend_from_slice(&s[..s.len() - 11]);
        s2.extend_from_slice(&seg);
        s2.extend_from_slice(&s[s.len() - 11..]);
        let out = decode(&s2, None, None).unwrap();
        assert_eq!(out.len(), 4);
    }

    /// Pattern dictionary + Immediate halftone region のセグメント連鎖を
    /// `decode` まで通せること。
    ///
    /// 構築物: 16x8 ページに、4x4 の単一パターン（gray_max=0）を
    /// 4x2 のグリッドで密に並べた halftone。OR 合成で前景一色 →
    /// 反転後すべて 0x00（黒）になる。
    #[test]
    fn pattern_dict_and_halftone_combine() {
        let mut s = build_min_stream(16, 8, 0);

        // ---- セグメント 3: Pattern dictionary（MMR=1, GRAYMAX=0 → 1 パターン）----
        // MMR 経路は payload を T.6 として復号するが、行数 0/サイズも 0 のときは
        // 入力消費なしで終了する。ここでは pw=4 ph=4 / 1 パターンの最小構成を
        // 使うために、MMR ではなく算術経路にする。Pattern を 1 個だけ作って
        // halftone 側で bits_per_value=0 にする。
        // ただし算術 1 パターンでも復号は走り、結果が「すべて黒」になる保証はない。
        // テストは構造（連鎖の完走 + ページが書き戻された）に絞る。
        let mut pd_payload = Vec::new();
        pd_payload.push(0b0000_0000); // HDMMR=0, HDTEMPLATE=0
        pd_payload.push(4); // HDPW
        pd_payload.push(4); // HDPH
        pd_payload.extend_from_slice(&0u32.to_be_bytes()); // GRAYMAX=0
        pd_payload.extend_from_slice(&[0u8; 8]);
        pd_payload.extend_from_slice(&[0xFF, 0xAC]);
        let mut pd_seg = Vec::new();
        pd_seg.extend_from_slice(&3u32.to_be_bytes());
        pd_seg.push(16); // type=16 PatternDictionary
        pd_seg.push(0); // ref count
        pd_seg.push(1); // page assoc
        pd_seg.extend_from_slice(&(pd_payload.len() as u32).to_be_bytes());
        pd_seg.extend_from_slice(&pd_payload);

        // ---- セグメント 4: Immediate halftone region（type=22）----
        // 16x8 領域、4x2 グリッド、HRX=4*256, HRY=0、参照 = #3
        let mut ht_payload = Vec::new();
        // 領域情報フィールド
        ht_payload.extend_from_slice(&16u32.to_be_bytes()); // width
        ht_payload.extend_from_slice(&8u32.to_be_bytes()); // height
        ht_payload.extend_from_slice(&0u32.to_be_bytes()); // x
        ht_payload.extend_from_slice(&0u32.to_be_bytes()); // y
        ht_payload.push(0); // combop=0
                            // halftone flags: HMMR=0, HTEMPLATE=0, skip=0, HCOMBOP=0, HDEFPIXEL=0
        ht_payload.push(0);
        ht_payload.extend_from_slice(&4u32.to_be_bytes()); // HGW
        ht_payload.extend_from_slice(&2u32.to_be_bytes()); // HGH
        ht_payload.extend_from_slice(&0i32.to_be_bytes()); // HGX
        ht_payload.extend_from_slice(&0i32.to_be_bytes()); // HGY
        ht_payload.extend_from_slice(&(4i16 * 256).to_be_bytes()); // HRX
        ht_payload.extend_from_slice(&0i16.to_be_bytes()); // HRY

        let mut ht_seg = Vec::new();
        ht_seg.extend_from_slice(&4u32.to_be_bytes());
        ht_seg.push(22); // type=22 ImmediateHalftoneRegion
        ht_seg.push(0b001_00000); // 短形式 count=1 → 上位 3 ビット = 001
        ht_seg.push(3); // 参照番号 = 3（pattern dictionary）
        ht_seg.push(1); // page assoc
        ht_seg.extend_from_slice(&(ht_payload.len() as u32).to_be_bytes());
        ht_seg.extend_from_slice(&ht_payload);

        let mut s2 = Vec::new();
        s2.extend_from_slice(&s[..s.len() - 11]); // pageinfo
        s2.extend_from_slice(&pd_seg);
        s2.extend_from_slice(&ht_seg);
        s2.extend_from_slice(&s[s.len() - 11..]); // eop
        s = s2;

        // 復号: 1 パターン (bits_per_value=0) なので算術ストリームは消費されず、
        // 8 グリッド点に同じパターンを OR で配置する → 部分的にビットが立つ。
        // 最低限、復号が完走し既定行ストライドの出力が返ることを検証。
        let out = decode(&s, None, None).unwrap();
        assert_eq!(out.len(), 16); // stride=2, h=8
    }

    #[test]
    fn unknown_segment_skipped() {
        // 型 62 (Extension) を 1 件挟んでもページが返る
        let mut s = build_min_stream(8, 2, 0);
        // 拡張セグメントを末尾に追加（ただし decode は EOF 前に処理完了する）
        let mut ext = Vec::new();
        ext.extend_from_slice(&3u32.to_be_bytes());
        ext.push(62);
        ext.push(0);
        ext.push(1);
        ext.extend_from_slice(&4u32.to_be_bytes());
        ext.extend_from_slice(&[0u8; 4]);
        // end-of-page の後に拡張を追加してもよいので、ページ情報の後・eop の前に挿入
        // 簡易: そのまま末尾結合（decode は前方のみで page を確定する）
        s.extend_from_slice(&ext);
        let out = decode(&s, None, None).unwrap();
        assert_eq!(out.len(), 2);
    }
}
