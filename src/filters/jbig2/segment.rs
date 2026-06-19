//! JBIG2 セグメントヘッダのパーサ（T.88 §7.2）。
//!
//! セグメントは「ヘッダ」「データ」の 2 部構成。ヘッダにはセグメント番号、
//! 種別、参照する他セグメントの番号、ページ関連付け、データ長が入っている。
//! データ長は `0xFFFFFFFF` = 不明（即時 generic region のみ。end-of-stripe 行
//! までストリームを走査して終端を判定する）。

use super::err;
use super::reader::ByteReader;
use crate::error::Result;

// ---------------------------------------------------------------------------
// セグメント種別（T.88 §7.3 表 3）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentType {
    SymbolDictionary,                         // 0
    IntermediateTextRegion,                   // 4
    ImmediateTextRegion,                      // 6
    ImmediateLosslessTextRegion,              // 7
    PatternDictionary,                        // 16
    IntermediateHalftoneRegion,               // 20
    ImmediateHalftoneRegion,                  // 22
    ImmediateLosslessHalftoneRegion,          // 23
    IntermediateGenericRegion,                // 36
    ImmediateGenericRegion,                   // 38
    ImmediateLosslessGenericRegion,           // 39
    IntermediateGenericRefinementRegion,      // 40
    ImmediateGenericRefinementRegion,         // 42
    ImmediateLosslessGenericRefinementRegion, // 43
    PageInformation,                          // 48
    EndOfPage,                                // 49
    EndOfStripe,                              // 50
    EndOfFile,                                // 51
    Profiles,                                 // 52
    Tables,                                   // 53
    Extension,                                // 62
    /// 仕様外コードや将来拡張。
    Unknown(u8),
}

impl SegmentType {
    pub fn from_code(code: u8) -> Self {
        match code {
            0 => Self::SymbolDictionary,
            4 => Self::IntermediateTextRegion,
            6 => Self::ImmediateTextRegion,
            7 => Self::ImmediateLosslessTextRegion,
            16 => Self::PatternDictionary,
            20 => Self::IntermediateHalftoneRegion,
            22 => Self::ImmediateHalftoneRegion,
            23 => Self::ImmediateLosslessHalftoneRegion,
            36 => Self::IntermediateGenericRegion,
            38 => Self::ImmediateGenericRegion,
            39 => Self::ImmediateLosslessGenericRegion,
            40 => Self::IntermediateGenericRefinementRegion,
            42 => Self::ImmediateGenericRefinementRegion,
            43 => Self::ImmediateLosslessGenericRefinementRegion,
            48 => Self::PageInformation,
            49 => Self::EndOfPage,
            50 => Self::EndOfStripe,
            51 => Self::EndOfFile,
            52 => Self::Profiles,
            53 => Self::Tables,
            62 => Self::Extension,
            other => Self::Unknown(other),
        }
    }

    pub fn is_region(&self) -> bool {
        matches!(
            self,
            Self::IntermediateTextRegion
                | Self::ImmediateTextRegion
                | Self::ImmediateLosslessTextRegion
                | Self::IntermediateHalftoneRegion
                | Self::ImmediateHalftoneRegion
                | Self::ImmediateLosslessHalftoneRegion
                | Self::IntermediateGenericRegion
                | Self::ImmediateGenericRegion
                | Self::ImmediateLosslessGenericRegion
                | Self::IntermediateGenericRefinementRegion
                | Self::ImmediateGenericRefinementRegion
                | Self::ImmediateLosslessGenericRefinementRegion
        )
    }

    pub fn is_immediate_region(&self) -> bool {
        matches!(
            self,
            Self::ImmediateTextRegion
                | Self::ImmediateLosslessTextRegion
                | Self::ImmediateHalftoneRegion
                | Self::ImmediateLosslessHalftoneRegion
                | Self::ImmediateGenericRegion
                | Self::ImmediateLosslessGenericRegion
                | Self::ImmediateGenericRefinementRegion
                | Self::ImmediateLosslessGenericRefinementRegion
        )
    }
}

// ---------------------------------------------------------------------------
// セグメントヘッダ
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SegmentHeader {
    pub number: u32,
    pub seg_type: SegmentType,
    pub raw_type_code: u8,
    pub deferred_non_retain: bool,
    pub retain_flags: Vec<bool>,
    pub referred_segments: Vec<u32>,
    pub page_association: u32,
    pub data_length: u32,
    /// `data_length == 0xFFFFFFFF` の場合の「不明長」フラグ。
    pub unknown_length: bool,
    /// ヘッダ全体のバイト数（セグメント番号の先頭からデータ直前まで）。
    pub header_size: usize,
}

impl SegmentHeader {
    /// `br` のカーソルがセグメントヘッダ先頭にある状態で呼び、ヘッダを読み終え
    /// た位置までカーソルを進める。
    pub fn parse(br: &mut ByteReader<'_>) -> Result<Self> {
        let start = br.pos();

        let number = br.read_u32()?;
        let flags = br.read_u8()?;
        let type_code = flags & 0x3F;
        let seg_type = SegmentType::from_code(type_code);
        let page_assoc_size_4 = (flags >> 6) & 1 != 0;
        let deferred_non_retain = (flags >> 7) & 1 != 0;

        // 参照セグメント数 + retention flags
        let count_byte = br.read_u8()?;
        let short_count = (count_byte >> 5) & 0x07;
        let (referred_count, retain_bytes) = if short_count <= 4 {
            // 短形式: 残り 5 ビットは retention の先頭部
            let count = short_count as u32;
            // retention flag は (count + 1) ビット必要だが、短形式では先頭バイトに
            // 詰め込まれている。仕様: 残り 5 ビットを上位とし、続く 0 バイト。
            // 全体ビット数 = count + 1。
            let total_bits = count + 1;
            let _used_in_first = 5u32; // 上記 byte の下位 5 ビット
                                       // 短形式では追加バイトは読まない（5 ビットで count+1 (≤5) ビットを必ず収容できる）
            let mut flags_vec = Vec::with_capacity(total_bits as usize);
            for i in 0..total_bits {
                let bit = (count_byte >> (4 - i)) & 1;
                flags_vec.push(bit != 0);
            }
            (count, flags_vec)
        } else if short_count == 7 {
            // 長形式: count は次の 3 バイトと組み合わせる 32 ビット値。
            // ただし spec では「先頭バイトの上位 3 ビット=111、残り 5 ビットは長形式の予約」と定義され、
            // 続く 4 バイトの 32 ビットが実際の count。
            // count_byte の下位 5 ビットは予約。
            let b1 = br.read_u8()?;
            let b2 = br.read_u8()?;
            let b3 = br.read_u8()?;
            let count = (((count_byte & 0x1F) as u32) << 24)
                | ((b1 as u32) << 16)
                | ((b2 as u32) << 8)
                | (b3 as u32);
            // retention flags: (count + 1) ビット、バイト境界アライン
            let nbytes = ((count + 1) as usize).div_ceil(8);
            let bytes = br.slice(nbytes)?;
            let mut flags_vec = Vec::with_capacity(count as usize + 1);
            for i in 0..=count as usize {
                let byte = bytes[i / 8];
                let bit = (byte >> (7 - (i % 8))) & 1;
                flags_vec.push(bit != 0);
            }
            (count, flags_vec)
        } else {
            return Err(err(format!(
                "JBIG2 segment {number}: invalid short referred-count {short_count}"
            )));
        };

        // 参照セグメント番号: number の値域に応じてバイト幅が変わる
        let ref_bytes = if number <= 0xFF {
            1
        } else if number <= 0xFFFF {
            2
        } else {
            4
        };
        let mut referred = Vec::with_capacity(referred_count as usize);
        for _ in 0..referred_count {
            let v = match ref_bytes {
                1 => br.read_u8()? as u32,
                2 => br.read_u16()? as u32,
                _ => br.read_u32()?,
            };
            referred.push(v);
        }

        // ページ関連付け
        let page_association = if page_assoc_size_4 {
            br.read_u32()?
        } else {
            br.read_u8()? as u32
        };

        // セグメントデータ長
        let raw_len = br.read_u32()?;
        let unknown_length = raw_len == 0xFFFF_FFFF;
        let header_size = br.pos() - start;

        Ok(SegmentHeader {
            number,
            seg_type,
            raw_type_code: type_code,
            deferred_non_retain,
            retain_flags: retain_bytes,
            referred_segments: referred,
            page_association,
            data_length: raw_len,
            unknown_length,
            header_size,
        })
    }
}

// ---------------------------------------------------------------------------
// 領域セグメント情報フィールド（T.88 §7.4.1, region segment information field）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct RegionSegmentInfo {
    pub width: u32,
    pub height: u32,
    pub x: u32,
    pub y: u32,
    /// 外部結合演算子（COMBOP）。
    pub external_combop: u8,
    /// 色フラグ（前景値の選択）。
    pub color: bool,
}

impl RegionSegmentInfo {
    pub fn parse(br: &mut ByteReader<'_>) -> Result<Self> {
        let width = br.read_u32()?;
        let height = br.read_u32()?;
        let x = br.read_u32()?;
        let y = br.read_u32()?;
        let flags = br.read_u8()?;
        let external_combop = flags & 0x07;
        let color = (flags >> 3) & 1 != 0;
        Ok(Self {
            width,
            height,
            x,
            y,
            external_combop,
            color,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_short_form() {
        // セグメント番号 1、type=48（PageInformation）、参照 0 件、ページ 1、データ長 19
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1u32.to_be_bytes()); // 番号
        bytes.push(48); // 種別フラグ（type=48、page_assoc_size=0、retain=0）
        bytes.push(0); // 参照数=0、retention=1 ビット（自分の retain）
        bytes.push(1); // page association（1 バイト）
        bytes.extend_from_slice(&19u32.to_be_bytes()); // data length
        bytes.extend_from_slice(&[0u8; 19]); // ダミーデータ

        let mut br = ByteReader::new(&bytes);
        let h = SegmentHeader::parse(&mut br).unwrap();
        assert_eq!(h.number, 1);
        assert_eq!(h.seg_type, SegmentType::PageInformation);
        assert_eq!(h.raw_type_code, 48);
        assert_eq!(h.referred_segments.len(), 0);
        assert_eq!(h.page_association, 1);
        assert_eq!(h.data_length, 19);
        assert!(!h.unknown_length);
        assert!(!h.deferred_non_retain);
        assert_eq!(h.header_size, 4 + 1 + 1 + 1 + 4);
    }

    #[test]
    fn header_with_refs() {
        // セグメント番号 5、type=38（即時 generic）、参照=2 (5,3)、ページ 1、長さ 100
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&5u32.to_be_bytes());
        bytes.push(38);
        // 短形式 count=2 → 上位 3 ビット 010、残り 5 ビットは retention 3 ビット相当（count+1=3）
        bytes.push(0b010_00000);
        // 参照番号: number=5 < 256 → 1 バイトずつ
        bytes.push(2);
        bytes.push(3);
        bytes.push(1); // page assoc
        bytes.extend_from_slice(&100u32.to_be_bytes());
        bytes.extend_from_slice(&[0u8; 10]); // 余分

        let mut br = ByteReader::new(&bytes);
        let h = SegmentHeader::parse(&mut br).unwrap();
        assert_eq!(h.number, 5);
        assert_eq!(h.seg_type, SegmentType::ImmediateGenericRegion);
        assert_eq!(h.referred_segments, vec![2, 3]);
        assert_eq!(h.page_association, 1);
        assert_eq!(h.data_length, 100);
        assert_eq!(h.retain_flags.len(), 3);
    }

    #[test]
    fn unknown_length_flag() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&7u32.to_be_bytes());
        bytes.push(38);
        bytes.push(0);
        bytes.push(1);
        bytes.extend_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
        let mut br = ByteReader::new(&bytes);
        let h = SegmentHeader::parse(&mut br).unwrap();
        assert!(h.unknown_length);
        assert_eq!(h.data_length, 0xFFFF_FFFF);
    }

    #[test]
    fn region_info_parse() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&100u32.to_be_bytes()); // width
        bytes.extend_from_slice(&50u32.to_be_bytes()); // height
        bytes.extend_from_slice(&10u32.to_be_bytes()); // x
        bytes.extend_from_slice(&20u32.to_be_bytes()); // y
        bytes.push(0b0000_1010); // color=1, combop=2 (XOR)
        let mut br = ByteReader::new(&bytes);
        let r = RegionSegmentInfo::parse(&mut br).unwrap();
        assert_eq!(r.width, 100);
        assert_eq!(r.height, 50);
        assert_eq!(r.x, 10);
        assert_eq!(r.y, 20);
        assert_eq!(r.external_combop, 2);
        assert!(r.color);
    }
}
