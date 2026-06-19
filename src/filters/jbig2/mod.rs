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
//! ## サポート状況（セッション 1）
//!
//! - セグメントヘッダのパース／ページ情報セグメントの処理：完備
//! - Generic region（算術 + MMR）: セッション 2
//! - Symbol dictionary / Text region / Generic refinement: セッション 3
//! - Pattern dictionary / Halftone region: セッション 4
//!
//! 未対応セグメント種別は黙って読み飛ばす（耐故障路）。これにより、現段階でも
//! 「ページ情報セグメントだけがある最小 JBIG2」は背景一様画像として出力される。

use crate::error::{PdfError, Result};
use crate::object::{Dictionary, Object};

pub mod bitmap;
pub mod huffman;
pub mod mq;
pub mod page;
pub mod reader;
pub mod segment;

use bitmap::Bitmap;
use page::PageInfo;
use reader::ByteReader;
use segment::{SegmentHeader, SegmentType};

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

/// セグメントを順次処理する状態機械。
struct Driver {
    page: Option<Bitmap>,
    page_info: Option<PageInfo>,
    /// EndOfStripe で更新される、現在の塗り上限行（高さ未確定ページ用）。
    current_stripe_y: u32,
}

impl Driver {
    fn new() -> Self {
        Self {
            page: None,
            page_info: None,
            current_stripe_y: 0,
        }
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
            // 以下はセッション 2/3/4 で実装。現段階は読み飛ばす（耐故障）。
            SegmentType::ImmediateGenericRegion
            | SegmentType::ImmediateLosslessGenericRegion
            | SegmentType::IntermediateGenericRegion => {
                // セッション 2 で実装
            }
            SegmentType::SymbolDictionary
            | SegmentType::IntermediateTextRegion
            | SegmentType::ImmediateTextRegion
            | SegmentType::ImmediateLosslessTextRegion
            | SegmentType::IntermediateGenericRefinementRegion
            | SegmentType::ImmediateGenericRefinementRegion
            | SegmentType::ImmediateLosslessGenericRefinementRegion => {
                // セッション 3 で実装
            }
            SegmentType::PatternDictionary
            | SegmentType::IntermediateHalftoneRegion
            | SegmentType::ImmediateHalftoneRegion
            | SegmentType::ImmediateLosslessHalftoneRegion => {
                // セッション 4 で実装
            }
            SegmentType::Tables => {
                // セッション 3 で実装（Huffman カスタムテーブル）
            }
            SegmentType::Profiles | SegmentType::Extension | SegmentType::Unknown(_) => {
                // 仕様で「未知セグメントは無視可」とされている
            }
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
