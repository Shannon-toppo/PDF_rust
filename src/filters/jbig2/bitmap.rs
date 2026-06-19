//! JBIG2 内部用 1bpp パックビットマップ（MSB ファースト、行ストライド付き）。
//!
//! ビット意味は **JBIG2 内部規約に従い 1 = 前景（黒）**。最終出力時に
//! `mod.rs::decode` でビット反転して PDF 慣習（1 = 白）へ揃える。
//!
//! 結合演算子は T.88 §7 のテーブル: OR / AND / XOR / XNOR / REPLACE。

use super::err;
use crate::error::Result;

/// 領域結合演算子（T.88 表 64・表 18 等の COMBOP / EXTCOMBOP 共通）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CombineOp {
    Or = 0,
    And = 1,
    Xor = 2,
    Xnor = 3,
    Replace = 4,
}

impl CombineOp {
    pub fn from_int(v: u32) -> Result<Self> {
        match v {
            0 => Ok(Self::Or),
            1 => Ok(Self::And),
            2 => Ok(Self::Xor),
            3 => Ok(Self::Xnor),
            4 => Ok(Self::Replace),
            _ => Err(err(format!("invalid combination operator {v}"))),
        }
    }
}

/// 1bpp パックビットマップ。`data` は `stride * height` バイト、MSB が左端。
#[derive(Debug, Clone)]
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub data: Vec<u8>,
}

impl Bitmap {
    /// 全ビット 0（背景）で確保する。
    pub fn new(width: u32, height: u32) -> Self {
        let stride = width.div_ceil(8);
        let size = (stride as usize).saturating_mul(height as usize);
        Self {
            width,
            height,
            stride,
            data: vec![0u8; size],
        }
    }

    /// `value`（0 または 1）で全画素を初期化する。
    pub fn filled(width: u32, height: u32, value: u8) -> Self {
        let mut bm = Self::new(width, height);
        bm.fill(value);
        bm
    }

    /// 全画素を `value`（0/1）で塗りつぶす。
    pub fn fill(&mut self, value: u8) {
        let b = if value & 1 != 0 { 0xFFu8 } else { 0x00 };
        for x in self.data.iter_mut() {
            *x = b;
        }
        self.mask_trailing();
    }

    /// 行末の余剰ビット（width が 8 の倍数でないとき）を 0 にする。
    fn mask_trailing(&mut self) {
        let extra = (self.stride * 8).saturating_sub(self.width);
        if extra == 0 {
            return;
        }
        let mask = 0xFFu8.wrapping_shl(extra);
        for r in 0..self.height as usize {
            let last = (r + 1) * self.stride as usize;
            if last == 0 {
                continue;
            }
            if let Some(b) = self.data.get_mut(last - 1) {
                *b &= mask;
            }
        }
    }

    /// 安全な画素取得。範囲外は 0 を返す（generic region のテンプレ参照で
    /// 領域外を 0 と見なす要件をここで担保する）。
    #[inline]
    pub fn get(&self, x: i64, y: i64) -> u8 {
        if x < 0 || y < 0 {
            return 0;
        }
        let (xu, yu) = (x as u64, y as u64);
        if xu >= self.width as u64 || yu >= self.height as u64 {
            return 0;
        }
        let idx = yu * self.stride as u64 + (xu / 8);
        let shift = 7 - (xu as u32 % 8);
        (self.data[idx as usize] >> shift) & 1
    }

    /// 安全な画素設定。範囲外は無視。
    #[inline]
    pub fn set(&mut self, x: u32, y: u32, value: u8) {
        if x >= self.width || y >= self.height {
            return;
        }
        let idx = (y * self.stride + x / 8) as usize;
        let shift = 7 - (x % 8);
        let bit = (value & 1) << shift;
        let mask = !(1u8 << shift);
        self.data[idx] = (self.data[idx] & mask) | bit;
    }

    /// 別ビットマップを指定座標を始点に組み合わせる。
    ///
    /// `src` の (0,0) が dst の (x,y) に重なる前提。座標範囲外は無視。
    /// パフォーマンス重視ではなく正しさ重視（実装第 1 段）。
    pub fn combine(&mut self, src: &Bitmap, x: i64, y: i64, op: CombineOp) {
        for sy in 0..src.height as i64 {
            let dy = y + sy;
            if dy < 0 || dy as u32 >= self.height {
                continue;
            }
            for sx in 0..src.width as i64 {
                let dx = x + sx;
                if dx < 0 || dx as u32 >= self.width {
                    continue;
                }
                let s = src.get(sx, sy);
                let d = self.get(dx, dy);
                let r = match op {
                    CombineOp::Or => s | d,
                    CombineOp::And => s & d,
                    CombineOp::Xor => s ^ d,
                    CombineOp::Xnor => !(s ^ d) & 1,
                    CombineOp::Replace => s,
                };
                self.set(dx as u32, dy as u32, r);
            }
        }
    }

    /// 全ビット反転。最終出力で 1=黒（JBIG2 内部）→ 1=白（PDF 慣習）へ変換するために使う。
    pub fn invert(&mut self) {
        for b in self.data.iter_mut() {
            *b = !*b;
        }
        self.mask_trailing();
    }

    /// 出力バイト列（MSB ファースト・行ストライド込み）。
    pub fn into_packed(self) -> Vec<u8> {
        self.data
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_zero() {
        let bm = Bitmap::new(13, 5);
        assert_eq!(bm.stride, 2);
        assert_eq!(bm.data.len(), 10);
        assert!(bm.data.iter().all(|b| *b == 0));
    }

    #[test]
    fn fill_masks_trailing() {
        let mut bm = Bitmap::new(13, 2);
        bm.fill(1);
        // width 13 → 各行 2 バイト、最終バイトの下位 3 ビットは 0
        assert_eq!(bm.data, vec![0xFF, 0xF8, 0xFF, 0xF8]);
    }

    #[test]
    fn set_and_get() {
        let mut bm = Bitmap::new(8, 2);
        bm.set(0, 0, 1);
        bm.set(7, 0, 1);
        bm.set(3, 1, 1);
        assert_eq!(bm.get(0, 0), 1);
        assert_eq!(bm.get(7, 0), 1);
        assert_eq!(bm.get(3, 1), 1);
        assert_eq!(bm.get(4, 1), 0);
        // 範囲外は 0
        assert_eq!(bm.get(-1, 0), 0);
        assert_eq!(bm.get(0, 100), 0);
    }

    #[test]
    fn combine_replace() {
        let mut dst = Bitmap::new(8, 4);
        let mut src = Bitmap::new(4, 2);
        src.fill(1);
        dst.combine(&src, 2, 1, CombineOp::Replace);
        assert_eq!(dst.get(2, 1), 1);
        assert_eq!(dst.get(5, 2), 1);
        assert_eq!(dst.get(1, 1), 0);
        assert_eq!(dst.get(2, 0), 0);
    }

    #[test]
    fn invert_clears_trailing() {
        let mut bm = Bitmap::new(13, 1);
        bm.invert();
        // 全 0 → 全 1（末尾 3 ビットはマスクされ 0）
        assert_eq!(bm.data, vec![0xFF, 0xF8]);
    }
}
