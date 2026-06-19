//! ページ情報セグメント（T.88 §7.4.8）と end-of-page / end-of-stripe 処理。
//!
//! ページ情報セグメントを処理した時点で「ページビットマップ」が確保され、
//! 以降の領域セグメントが順次このビットマップに合成される。`Page::height` が
//! `0xFFFFFFFF` の場合は不定で、end-of-stripe の段階で確定する。

use super::bitmap::Bitmap;
use super::err;
use super::reader::ByteReader;
use crate::error::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageInfo {
    pub width: u32,
    pub height: u32,
    pub x_resolution: u32,
    pub y_resolution: u32,
    /// バイト境界フラグ。詳細は T.88 §7.4.8.6。
    pub eventually_lossless: bool,
    pub contains_refinements: bool,
    /// 既定画素値（0 = 白背景、1 = 黒背景）。JBIG2 内部規約。
    pub default_pixel: u8,
    /// 既定の領域結合演算子（COMBOP）。
    pub default_combop: u8,
    pub requires_aux_buffers: bool,
    pub combop_override: bool,
    /// 横方向最大ストライプ高（未指定なら 0）。
    pub max_stripe_height: u16,
}

impl PageInfo {
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < 19 {
            return Err(err("JBIG2 page info: data shorter than 19 bytes"));
        }
        let mut br = ByteReader::new(data);
        let width = br.read_u32()?;
        let height = br.read_u32()?;
        let x_res = br.read_u32()?;
        let y_res = br.read_u32()?;
        let flags = br.read_u8()?;
        let eventually_lossless = flags & 0x01 != 0;
        let contains_refinements = flags & 0x02 != 0;
        let default_pixel = (flags >> 2) & 1;
        let default_combop = (flags >> 3) & 0x03;
        let requires_aux_buffers = flags & 0x20 != 0;
        let combop_override = flags & 0x40 != 0;
        let stripe = br.read_u16()?;
        let max_stripe_height = stripe & 0x7FFF;

        Ok(PageInfo {
            width,
            height,
            x_resolution: x_res,
            y_resolution: y_res,
            eventually_lossless,
            contains_refinements,
            default_pixel,
            default_combop,
            requires_aux_buffers,
            combop_override,
            max_stripe_height,
        })
    }

    /// ページビットマップを確保する。`height` 不明（0xFFFFFFFF）の場合は
    /// max_stripe_height を当面の高さとして確保し、後でリサイズする。
    pub fn allocate_bitmap(&self) -> Result<Bitmap> {
        let h = if self.height == 0xFFFF_FFFF {
            self.max_stripe_height as u32
        } else {
            self.height
        };
        // 上限ガード（メモリ爆発防止）
        const MAX_DIM: u32 = 65536;
        if self.width > MAX_DIM || h > MAX_DIM {
            return Err(err(format!(
                "JBIG2 page bitmap too large: {}x{}",
                self.width, h
            )));
        }
        Ok(Bitmap::filled(self.width, h, self.default_pixel))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_data(width: u32, height: u32, flags: u8, stripe: u16) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&width.to_be_bytes());
        v.extend_from_slice(&height.to_be_bytes());
        v.extend_from_slice(&100u32.to_be_bytes()); // xres
        v.extend_from_slice(&100u32.to_be_bytes()); // yres
        v.push(flags);
        v.extend_from_slice(&stripe.to_be_bytes());
        v
    }

    #[test]
    fn parse_basic() {
        let d = make_data(64, 32, 0b0000_0000, 0);
        let pi = PageInfo::parse(&d).unwrap();
        assert_eq!(pi.width, 64);
        assert_eq!(pi.height, 32);
        assert_eq!(pi.default_pixel, 0);
        assert_eq!(pi.default_combop, 0);
        assert!(!pi.eventually_lossless);
    }

    #[test]
    fn parse_flags() {
        // default_pixel=1, default_combop=2, eventually_lossless=1
        let d = make_data(8, 8, 0b0001_0101, 0);
        let pi = PageInfo::parse(&d).unwrap();
        assert_eq!(pi.default_pixel, 1);
        assert_eq!(pi.default_combop, 2);
        assert!(pi.eventually_lossless);
    }

    #[test]
    fn allocate_fills_default_pixel() {
        let d = make_data(8, 4, 0b0000_0100, 0); // default_pixel=1
        let pi = PageInfo::parse(&d).unwrap();
        let bm = pi.allocate_bitmap().unwrap();
        assert_eq!(bm.width, 8);
        assert_eq!(bm.height, 4);
        assert!(bm.data.iter().all(|b| *b == 0xFF));
    }

    #[test]
    fn allocate_rejects_huge() {
        let d = make_data(100_000, 100_000, 0, 0);
        let pi = PageInfo::parse(&d).unwrap();
        assert!(pi.allocate_bitmap().is_err());
    }

    #[test]
    fn unknown_height_uses_stripe() {
        let d = make_data(8, 0xFFFF_FFFF, 0, 16);
        let pi = PageInfo::parse(&d).unwrap();
        let bm = pi.allocate_bitmap().unwrap();
        assert_eq!(bm.height, 16);
    }

    #[test]
    fn too_short_errors() {
        assert!(PageInfo::parse(&[0u8; 10]).is_err());
    }
}
