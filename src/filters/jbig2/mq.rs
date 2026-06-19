//! MQ 算術復号器（T.88 Annex E）と整数復号 IADEC / シンボル ID 復号 IAID
//! （T.88 Annex A.2 / A.3）。
//!
//! JBIG2 のあらゆる算術符号化領域はこのデコーダを共有する。コンテキスト配列
//! (`Cx`) は呼び出し側が確保し `&mut [u8]` で渡す。各バイトは
//! `(index << 1) | MPS` を保持する。
//!
//! 入力末尾を超えた読み出しは 0xFF で埋める（T.88 慣習）。これにより
//! セグメント末尾の終端処理（仕様で 0xAC が現れるまで読み飛ばす流儀）に
//! も追従できる。

use super::err;
use crate::error::Result;

// ---------------------------------------------------------------------------
// 確率推定テーブル（T.88 Annex E §E.1.2 表 24）
// ---------------------------------------------------------------------------

/// `(Qe, NMPS, NLPS, SWITCH)` の組。SWITCH=true で LPS 経路移動時に MPS を反転。
struct QeEntry(u32, u8, u8, bool);

#[rustfmt::skip]
const QE: [QeEntry; 47] = [
    QeEntry(0x5601,  1,  1, true),
    QeEntry(0x3401,  2,  6, false),
    QeEntry(0x1801,  3,  9, false),
    QeEntry(0x0AC1,  4, 12, false),
    QeEntry(0x0521,  5, 29, false),
    QeEntry(0x0221, 38, 33, false),
    QeEntry(0x5601,  7,  6, true),
    QeEntry(0x5401,  8, 14, false),
    QeEntry(0x4801,  9, 14, false),
    QeEntry(0x3801, 10, 14, false),
    QeEntry(0x3001, 11, 17, false),
    QeEntry(0x2401, 12, 18, false),
    QeEntry(0x1C01, 13, 20, false),
    QeEntry(0x1601, 29, 21, false),
    QeEntry(0x5601, 15, 14, true),
    QeEntry(0x5401, 16, 14, false),
    QeEntry(0x5101, 17, 15, false),
    QeEntry(0x4801, 18, 16, false),
    QeEntry(0x3801, 19, 17, false),
    QeEntry(0x3401, 20, 18, false),
    QeEntry(0x3001, 21, 19, false),
    QeEntry(0x2801, 22, 19, false),
    QeEntry(0x2401, 23, 20, false),
    QeEntry(0x2201, 24, 21, false),
    QeEntry(0x1C01, 25, 22, false),
    QeEntry(0x1801, 26, 23, false),
    QeEntry(0x1601, 27, 24, false),
    QeEntry(0x1401, 28, 25, false),
    QeEntry(0x1201, 29, 26, false),
    QeEntry(0x1101, 30, 27, false),
    QeEntry(0x0AC1, 31, 28, false),
    QeEntry(0x09C1, 32, 29, false),
    QeEntry(0x08A1, 33, 30, false),
    QeEntry(0x0521, 34, 31, false),
    QeEntry(0x0441, 35, 32, false),
    QeEntry(0x02A1, 36, 33, false),
    QeEntry(0x0221, 37, 34, false),
    QeEntry(0x0141, 38, 35, false),
    QeEntry(0x0111, 39, 36, false),
    QeEntry(0x0085, 40, 37, false),
    QeEntry(0x0049, 41, 38, false),
    QeEntry(0x0025, 42, 39, false),
    QeEntry(0x0015, 43, 40, false),
    QeEntry(0x0009, 44, 41, false),
    QeEntry(0x0005, 45, 42, false),
    QeEntry(0x0001, 45, 43, false),
    QeEntry(0x5601, 46, 46, false),
];

// ---------------------------------------------------------------------------
// MQ 算術復号器（T.88 Annex E.3）
// ---------------------------------------------------------------------------

/// MQ 算術復号器の状態。
pub struct ArithDecoder<'a> {
    data: &'a [u8],
    bp: usize,
    c: u32,
    a: u32,
    ct: i32,
    b: u8,
}

impl<'a> ArithDecoder<'a> {
    /// INITDEC（T.88 Annex E.3.5）。`data` 先頭からビットストリーム読み出しを開始する。
    pub fn new(data: &'a [u8]) -> Result<Self> {
        if data.is_empty() {
            return Err(err("MQ: empty input"));
        }
        let mut d = ArithDecoder {
            data,
            bp: 0,
            c: 0,
            a: 0x8000,
            ct: 0,
            b: data[0],
        };
        d.c = (d.b as u32) << 16;
        d.byte_in();
        d.c <<= 7;
        d.ct -= 7;
        Ok(d)
    }

    /// 末尾を超えた読み出しは 0xFF で埋める。
    #[inline]
    fn peek_byte(&self, idx: usize) -> u8 {
        self.data.get(idx).copied().unwrap_or(0xFF)
    }

    /// BYTEIN（T.88 Annex E.3.4）。
    fn byte_in(&mut self) {
        if self.b == 0xFF {
            let b1 = self.peek_byte(self.bp + 1);
            if b1 > 0x8F {
                // ターミネータ近傍。バイトを消費せず CT=8 にする
                self.ct = 8;
            } else {
                self.bp += 1;
                self.b = self.peek_byte(self.bp);
                self.c = self
                    .c
                    .wrapping_add(0xFE00)
                    .wrapping_sub((self.b as u32) << 9);
                self.ct = 7;
            }
        } else {
            self.bp += 1;
            self.b = self.peek_byte(self.bp);
            self.c = self
                .c
                .wrapping_add(0xFF00)
                .wrapping_sub((self.b as u32) << 8);
            self.ct = 8;
        }
    }

    /// RENORMD（T.88 Annex E.3.3）。
    #[inline]
    fn renorm(&mut self) {
        loop {
            if self.ct == 0 {
                self.byte_in();
            }
            self.a <<= 1;
            self.c <<= 1;
            self.ct -= 1;
            if self.a & 0x8000 != 0 {
                break;
            }
        }
    }

    /// 1 ビットを指定コンテキスト（`cx[idx]`）で復号する。
    /// `cx[idx]` は `(state << 1) | MPS` を保持。
    pub fn decode(&mut self, cx: &mut [u8], idx: usize) -> u8 {
        let raw = cx[idx];
        let mut state = (raw >> 1) as usize;
        let mut mps = raw & 1;
        let qe = QE[state].0;

        self.a = self.a.wrapping_sub(qe);
        let c_high = self.c >> 16;
        let d;

        if c_high < qe {
            // LPS 経路
            d = self.lps_exchange(&mut state, &mut mps, qe);
            self.renorm();
        } else {
            self.c = self.c.wrapping_sub(qe << 16);
            if self.a & 0x8000 == 0 {
                d = self.mps_exchange(&mut state, &mut mps, qe);
                self.renorm();
            } else {
                d = mps;
            }
        }

        cx[idx] = ((state as u8) << 1) | mps;
        d
    }

    #[inline]
    fn lps_exchange(&mut self, state: &mut usize, mps: &mut u8, qe: u32) -> u8 {
        let entry = &QE[*state];
        let d;
        if self.a < qe {
            // 主従入れ替えなし
            self.a = qe;
            d = *mps;
            *state = entry.1 as usize;
        } else {
            self.a = qe;
            d = 1 - *mps;
            if entry.3 {
                *mps = 1 - *mps;
            }
            *state = entry.2 as usize;
        }
        d
    }

    #[inline]
    fn mps_exchange(&mut self, state: &mut usize, mps: &mut u8, qe: u32) -> u8 {
        let entry = &QE[*state];
        let d;
        if self.a < qe {
            d = 1 - *mps;
            if entry.3 {
                *mps = 1 - *mps;
            }
            *state = entry.2 as usize;
        } else {
            d = *mps;
            *state = entry.1 as usize;
        }
        d
    }
}

// ---------------------------------------------------------------------------
// 整数復号 IADEC（T.88 Annex A.2）
// ---------------------------------------------------------------------------

/// 整数復号用コンテキスト（512 バイト）。OOB を扱うため戻り値は `Option<i32>`。
pub struct IntDecoder {
    cx: Vec<u8>,
}

impl IntDecoder {
    pub fn new() -> Self {
        Self { cx: vec![0u8; 512] }
    }

    /// 1 整数を復号。OOB は `None`。
    pub fn decode(&mut self, ad: &mut ArithDecoder) -> Option<i32> {
        let mut prev: u32 = 1;
        let s = ad.decode(&mut self.cx, prev as usize);
        prev = (prev << 1) | s as u32;

        let b0 = ad.decode(&mut self.cx, prev as usize);
        prev = (prev << 1) | b0 as u32;

        let (nbits, offset);
        if b0 == 0 {
            nbits = 2;
            offset = 0;
        } else {
            let b1 = ad.decode(&mut self.cx, prev as usize);
            prev = (prev << 1) | b1 as u32;
            if b1 == 0 {
                nbits = 4;
                offset = 4;
            } else {
                let b2 = ad.decode(&mut self.cx, prev as usize);
                prev = (prev << 1) | b2 as u32;
                if b2 == 0 {
                    nbits = 6;
                    offset = 20;
                } else {
                    let b3 = ad.decode(&mut self.cx, prev as usize);
                    prev = (prev << 1) | b3 as u32;
                    if b3 == 0 {
                        nbits = 8;
                        offset = 84;
                    } else {
                        let b4 = ad.decode(&mut self.cx, prev as usize);
                        prev = (prev << 1) | b4 as u32;
                        if b4 == 0 {
                            nbits = 12;
                            offset = 340;
                        } else {
                            nbits = 32;
                            offset = 4436;
                        }
                    }
                }
            }
        }

        let mut v: u32 = 0;
        for _ in 0..nbits {
            let bit = ad.decode(&mut self.cx, prev as usize);
            prev = ((prev << 1) | bit as u32) & 0x1FF;
            // 反転防止: prev が 256 以上に育ったら下位 9 ビットでマスク
            if prev < 256 {
                prev |= 256;
            }
            v = (v << 1) | bit as u32;
        }
        let v = v.wrapping_add(offset);

        if s == 1 {
            if v == 0 {
                None // OOB
            } else {
                Some(-(v as i32))
            }
        } else {
            Some(v as i32)
        }
    }
}

impl Default for IntDecoder {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// シンボル ID 復号 IAID（T.88 Annex A.3）
// ---------------------------------------------------------------------------

/// シンボル ID 復号器。コードビット長は呼び出し時固定（symbol dictionary のサイズで決まる）。
pub struct IaidDecoder {
    cx: Vec<u8>,
    nbits: u32,
}

impl IaidDecoder {
    pub fn new(nbits: u32) -> Self {
        let size = 1usize.checked_shl(nbits + 1).unwrap_or(usize::MAX);
        Self {
            cx: vec![0u8; size.min(1 << 24)],
            nbits,
        }
    }

    pub fn decode(&mut self, ad: &mut ArithDecoder) -> u32 {
        let mut prev: u32 = 1;
        let mask = self.cx.len() - 1;
        for _ in 0..self.nbits {
            let idx = (prev as usize) & mask;
            let bit = ad.decode(&mut self.cx, idx);
            prev = (prev << 1) | bit as u32;
        }
        prev - (1u32 << self.nbits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// INITDEC が状態を壊さず、初回 DECODE で panic しないこと。
    #[test]
    fn initdec_roundtrip() {
        let data = [0x84u8, 0xC7, 0x3B, 0xFC, 0xE1, 0xA1, 0x43, 0x04, 0x02, 0x20];
        let mut ad = ArithDecoder::new(&data).unwrap();
        let mut cx = [0u8; 1];
        for _ in 0..16 {
            let _ = ad.decode(&mut cx, 0);
        }
    }

    /// 末尾 0xFF...0xAC マーカーの近傍でも panic しないこと。
    #[test]
    fn terminator_robustness() {
        // 0xFF が連続する終端でも 0xFF 埋めで継続できる
        let data = [0x00u8, 0x00, 0xFF, 0xAC];
        let mut ad = ArithDecoder::new(&data).unwrap();
        let mut cx = vec![0u8; 64];
        for _ in 0..32 {
            let _ = ad.decode(&mut cx, 0);
        }
    }

    /// IntDecoder の OOB / 符号付き挙動の基本動作確認。
    #[test]
    fn intdec_does_not_panic() {
        let data = [0x00u8, 0x00, 0x00, 0x00, 0xFF, 0xAC];
        let mut ad = ArithDecoder::new(&data).unwrap();
        let mut id = IntDecoder::new();
        for _ in 0..4 {
            let _ = id.decode(&mut ad);
        }
    }

    /// IaidDecoder が指定ビット長を超えた値を返さないこと。
    #[test]
    fn iaid_bound() {
        let data = [0x00u8, 0x00, 0x00, 0x00, 0xFF, 0xAC];
        let mut ad = ArithDecoder::new(&data).unwrap();
        let mut iaid = IaidDecoder::new(5);
        for _ in 0..4 {
            let v = iaid.decode(&mut ad);
            assert!(v < (1 << 5));
        }
    }
}
