//! CCITTFaxDecode フィルタ（PDF 32000-1:2008 §7.4.9, ITU-T T.4 / T.6）。
//!
//! スキャン文書で広く使われる二値画像コーデック。`/K` パラメータで方式を切替える:
//!
//! - `K < 0` : T.6（MMR, Group 4）— EOL なし、純粋 2D
//! - `K = 0` : T.4（MH, Group 3 1D）— 各行 1D（Modified Huffman）
//! - `K > 0` : T.4（MR, Group 3 mixed）— EOL のあとのタグビットで 1D/2D を切替え、
//!   K 行ごとに 1 度は 1D で同期する
//!
//! 出力は `1bpp` パックビット（左 MSB）。行ごとに `ceil(columns/8)` バイトに揃える。
//! `BlackIs1` パラメータ（既定 `false`）に応じて、最終出力のビット解釈は以下:
//!
//! - `BlackIs1 = false`（PDF 既定）: 1 = 白、0 = 黒（一般的な PDF グレースケール慣習）
//! - `BlackIs1 = true`            : 1 = 黒、0 = 白（CCITT 内部表現そのまま）
//!
//! 内部表現は常に 1 = 黒（自然な runs の交互パターン）であり、`BlackIs1 = false` の
//! 場合は最終段でビット反転する。
//!
//! ## 設計上の注意
//!
//! - 入力は信頼できない外部データとして扱う。`data.get(..)` と checked / saturating
//!   演算のみ使用し、未終端や不正符号でも panic しない。
//! - `Columns` / `Rows` は過大値の場合早期にエラー（メモリ爆発防止）。
//! - 拡張 1D / 2D（uncommon）の符号は明示的に拒否する。

use crate::error::{PdfError, Result};
use crate::object::{Dictionary, Object};

fn err(msg: impl Into<String>) -> PdfError {
    PdfError::Filter(msg.into())
}

/// 1 行・1 列あたりの上限。これ以上はメモリ過大としてエラー扱い。
const MAX_DIM: u32 = 65536;

// ---------------------------------------------------------------------------
// パラメータ
// ---------------------------------------------------------------------------

/// `/DecodeParms` から取り出した CCITTFaxDecode のパラメータ。
#[derive(Debug, Clone)]
pub struct CcittParams {
    /// 符号化方式の切替え（負: T.6, 0: T.4 1D, 正: T.4 mixed）。
    pub k: i32,
    /// 1 行のピクセル数。既定 1728。
    pub columns: u32,
    /// 行数（0 = 未指定。`end_of_block` と組み合わせて読み終わる）。
    pub rows: u32,
    /// 各行に EOL コードを要求するか（T.4 の慣習）。
    pub end_of_line: bool,
    /// EOL の前にバイト境界へパディングが入るか。
    pub encoded_byte_align: bool,
    /// 末尾の EOB（End-Of-Block）を見て終端を判定するか。既定 true。
    pub end_of_block: bool,
    /// 出力の 1 ビットを黒として返すか。既定 false（PDF 既定）。
    pub black_is_1: bool,
}

impl Default for CcittParams {
    fn default() -> Self {
        Self {
            k: 0,
            columns: 1728,
            rows: 0,
            end_of_line: false,
            encoded_byte_align: false,
            end_of_block: true,
            black_is_1: false,
        }
    }
}

/// 辞書（DecodeParms）からパラメータを読み出す。欠けているキーは既定値。
pub fn params_from_dict(dict: &Dictionary) -> CcittParams {
    let mut p = CcittParams::default();
    if let Some(o) = dict.get("K") {
        if let Ok(v) = o.as_int() {
            p.k = v.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
        }
    }
    if let Some(o) = dict.get("Columns") {
        if let Ok(v) = o.as_int() {
            p.columns = v.max(0).min(MAX_DIM as i64) as u32;
        }
    }
    if let Some(o) = dict.get("Rows") {
        if let Ok(v) = o.as_int() {
            p.rows = v.max(0).min(MAX_DIM as i64) as u32;
        }
    }
    if let Some(Object::Boolean(b)) = dict.get("EndOfLine") {
        p.end_of_line = *b;
    }
    if let Some(Object::Boolean(b)) = dict.get("EncodedByteAlign") {
        p.encoded_byte_align = *b;
    }
    if let Some(Object::Boolean(b)) = dict.get("EndOfBlock") {
        p.end_of_block = *b;
    }
    if let Some(Object::Boolean(b)) = dict.get("BlackIs1") {
        p.black_is_1 = *b;
    }
    p
}

// ---------------------------------------------------------------------------
// BitReader（MSB ファースト）
// ---------------------------------------------------------------------------

struct BitReader<'a> {
    data: &'a [u8],
    /// 現在のバイト位置。
    byte_pos: usize,
    /// 現在のバイト内で消費済みのビット数（0..8）。
    bit_pos: u8,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitReader {
            data,
            byte_pos: 0,
            bit_pos: 0,
        }
    }

    /// 1 ビット読む（MSB ファースト）。データ末尾を超えたら 0 を返す。
    fn read_bit(&mut self) -> u32 {
        let byte = match self.data.get(self.byte_pos) {
            Some(&b) => b,
            None => return 0,
        };
        let bit = (byte >> (7 - self.bit_pos)) & 1;
        self.bit_pos += 1;
        if self.bit_pos == 8 {
            self.bit_pos = 0;
            self.byte_pos = self.byte_pos.saturating_add(1);
        }
        bit as u32
    }

    /// 先頭 `n` ビット（≤32）を消費せず覗き見る。データ末尾は 0 で埋める。
    fn peek_bits(&self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        let mut bp = self.byte_pos;
        let mut bi = self.bit_pos;
        let mut v: u32 = 0;
        for _ in 0..n {
            let byte = self.data.get(bp).copied().unwrap_or(0);
            let bit = (byte >> (7 - bi)) & 1;
            v = (v << 1) | bit as u32;
            bi += 1;
            if bi == 8 {
                bi = 0;
                bp = bp.saturating_add(1);
            }
        }
        v
    }

    /// `n` ビット進める。
    fn consume(&mut self, n: u32) {
        for _ in 0..n {
            let _ = self.read_bit();
        }
    }

    /// 次のバイト境界まで進める（既に境界なら何もしない）。
    fn byte_align(&mut self) {
        if self.bit_pos != 0 {
            self.bit_pos = 0;
            self.byte_pos = self.byte_pos.saturating_add(1);
        }
    }

    /// データ末尾を完全に過ぎたか。
    fn eof(&self) -> bool {
        self.byte_pos >= self.data.len()
    }
}

// ---------------------------------------------------------------------------
// ハフマンテーブル（T.4 Table 1/2、T.6 共通）
// ---------------------------------------------------------------------------

/// 1 つのコード語: `bits` を MSB ファーストで先頭 `len` ビットに揃えた整数値。
#[derive(Copy, Clone)]
struct HCode {
    bits: u16,
    len: u8,
    val: i32,
}

// T.4 Table 1: 白ランの終端コード（0..63）
const WHITE_TERMS: &[HCode] = &[
    HCode {
        bits: 0b00110101,
        len: 8,
        val: 0,
    },
    HCode {
        bits: 0b000111,
        len: 6,
        val: 1,
    },
    HCode {
        bits: 0b0111,
        len: 4,
        val: 2,
    },
    HCode {
        bits: 0b1000,
        len: 4,
        val: 3,
    },
    HCode {
        bits: 0b1011,
        len: 4,
        val: 4,
    },
    HCode {
        bits: 0b1100,
        len: 4,
        val: 5,
    },
    HCode {
        bits: 0b1110,
        len: 4,
        val: 6,
    },
    HCode {
        bits: 0b1111,
        len: 4,
        val: 7,
    },
    HCode {
        bits: 0b10011,
        len: 5,
        val: 8,
    },
    HCode {
        bits: 0b10100,
        len: 5,
        val: 9,
    },
    HCode {
        bits: 0b00111,
        len: 5,
        val: 10,
    },
    HCode {
        bits: 0b01000,
        len: 5,
        val: 11,
    },
    HCode {
        bits: 0b001000,
        len: 6,
        val: 12,
    },
    HCode {
        bits: 0b000011,
        len: 6,
        val: 13,
    },
    HCode {
        bits: 0b110100,
        len: 6,
        val: 14,
    },
    HCode {
        bits: 0b110101,
        len: 6,
        val: 15,
    },
    HCode {
        bits: 0b101010,
        len: 6,
        val: 16,
    },
    HCode {
        bits: 0b101011,
        len: 6,
        val: 17,
    },
    HCode {
        bits: 0b0100111,
        len: 7,
        val: 18,
    },
    HCode {
        bits: 0b0001100,
        len: 7,
        val: 19,
    },
    HCode {
        bits: 0b0001000,
        len: 7,
        val: 20,
    },
    HCode {
        bits: 0b0010111,
        len: 7,
        val: 21,
    },
    HCode {
        bits: 0b0000011,
        len: 7,
        val: 22,
    },
    HCode {
        bits: 0b0000100,
        len: 7,
        val: 23,
    },
    HCode {
        bits: 0b0101000,
        len: 7,
        val: 24,
    },
    HCode {
        bits: 0b0101011,
        len: 7,
        val: 25,
    },
    HCode {
        bits: 0b0010011,
        len: 7,
        val: 26,
    },
    HCode {
        bits: 0b0100100,
        len: 7,
        val: 27,
    },
    HCode {
        bits: 0b0011000,
        len: 7,
        val: 28,
    },
    HCode {
        bits: 0b00000010,
        len: 8,
        val: 29,
    },
    HCode {
        bits: 0b00000011,
        len: 8,
        val: 30,
    },
    HCode {
        bits: 0b00011010,
        len: 8,
        val: 31,
    },
    HCode {
        bits: 0b00011011,
        len: 8,
        val: 32,
    },
    HCode {
        bits: 0b00010010,
        len: 8,
        val: 33,
    },
    HCode {
        bits: 0b00010011,
        len: 8,
        val: 34,
    },
    HCode {
        bits: 0b00010100,
        len: 8,
        val: 35,
    },
    HCode {
        bits: 0b00010101,
        len: 8,
        val: 36,
    },
    HCode {
        bits: 0b00010110,
        len: 8,
        val: 37,
    },
    HCode {
        bits: 0b00010111,
        len: 8,
        val: 38,
    },
    HCode {
        bits: 0b00101000,
        len: 8,
        val: 39,
    },
    HCode {
        bits: 0b00101001,
        len: 8,
        val: 40,
    },
    HCode {
        bits: 0b00101010,
        len: 8,
        val: 41,
    },
    HCode {
        bits: 0b00101011,
        len: 8,
        val: 42,
    },
    HCode {
        bits: 0b00101100,
        len: 8,
        val: 43,
    },
    HCode {
        bits: 0b00101101,
        len: 8,
        val: 44,
    },
    HCode {
        bits: 0b00000100,
        len: 8,
        val: 45,
    },
    HCode {
        bits: 0b00000101,
        len: 8,
        val: 46,
    },
    HCode {
        bits: 0b00001010,
        len: 8,
        val: 47,
    },
    HCode {
        bits: 0b00001011,
        len: 8,
        val: 48,
    },
    HCode {
        bits: 0b01010010,
        len: 8,
        val: 49,
    },
    HCode {
        bits: 0b01010011,
        len: 8,
        val: 50,
    },
    HCode {
        bits: 0b01010100,
        len: 8,
        val: 51,
    },
    HCode {
        bits: 0b01010101,
        len: 8,
        val: 52,
    },
    HCode {
        bits: 0b00100100,
        len: 8,
        val: 53,
    },
    HCode {
        bits: 0b00100101,
        len: 8,
        val: 54,
    },
    HCode {
        bits: 0b01011000,
        len: 8,
        val: 55,
    },
    HCode {
        bits: 0b01011001,
        len: 8,
        val: 56,
    },
    HCode {
        bits: 0b01011010,
        len: 8,
        val: 57,
    },
    HCode {
        bits: 0b01011011,
        len: 8,
        val: 58,
    },
    HCode {
        bits: 0b01001010,
        len: 8,
        val: 59,
    },
    HCode {
        bits: 0b01001011,
        len: 8,
        val: 60,
    },
    HCode {
        bits: 0b00110010,
        len: 8,
        val: 61,
    },
    HCode {
        bits: 0b00110011,
        len: 8,
        val: 62,
    },
    HCode {
        bits: 0b00110100,
        len: 8,
        val: 63,
    },
];

// T.4 Table 1: 白ラン Make-up コード（64 の倍数, 64..1728）
const WHITE_MAKEUPS: &[HCode] = &[
    HCode {
        bits: 0b11011,
        len: 5,
        val: 64,
    },
    HCode {
        bits: 0b10010,
        len: 5,
        val: 128,
    },
    HCode {
        bits: 0b010111,
        len: 6,
        val: 192,
    },
    HCode {
        bits: 0b0110111,
        len: 7,
        val: 256,
    },
    HCode {
        bits: 0b00110110,
        len: 8,
        val: 320,
    },
    HCode {
        bits: 0b00110111,
        len: 8,
        val: 384,
    },
    HCode {
        bits: 0b01100100,
        len: 8,
        val: 448,
    },
    HCode {
        bits: 0b01100101,
        len: 8,
        val: 512,
    },
    HCode {
        bits: 0b01101000,
        len: 8,
        val: 576,
    },
    HCode {
        bits: 0b01100111,
        len: 8,
        val: 640,
    },
    HCode {
        bits: 0b011001100,
        len: 9,
        val: 704,
    },
    HCode {
        bits: 0b011001101,
        len: 9,
        val: 768,
    },
    HCode {
        bits: 0b011010010,
        len: 9,
        val: 832,
    },
    HCode {
        bits: 0b011010011,
        len: 9,
        val: 896,
    },
    HCode {
        bits: 0b011010100,
        len: 9,
        val: 960,
    },
    HCode {
        bits: 0b011010101,
        len: 9,
        val: 1024,
    },
    HCode {
        bits: 0b011010110,
        len: 9,
        val: 1088,
    },
    HCode {
        bits: 0b011010111,
        len: 9,
        val: 1152,
    },
    HCode {
        bits: 0b011011000,
        len: 9,
        val: 1216,
    },
    HCode {
        bits: 0b011011001,
        len: 9,
        val: 1280,
    },
    HCode {
        bits: 0b011011010,
        len: 9,
        val: 1344,
    },
    HCode {
        bits: 0b011011011,
        len: 9,
        val: 1408,
    },
    HCode {
        bits: 0b010011000,
        len: 9,
        val: 1472,
    },
    HCode {
        bits: 0b010011001,
        len: 9,
        val: 1536,
    },
    HCode {
        bits: 0b010011010,
        len: 9,
        val: 1600,
    },
    HCode {
        bits: 0b011000,
        len: 6,
        val: 1664,
    },
    HCode {
        bits: 0b010011011,
        len: 9,
        val: 1728,
    },
];

// T.4 Table 1: 黒ランの終端コード（0..63）
const BLACK_TERMS: &[HCode] = &[
    HCode {
        bits: 0b0000110111,
        len: 10,
        val: 0,
    },
    HCode {
        bits: 0b010,
        len: 3,
        val: 1,
    },
    HCode {
        bits: 0b11,
        len: 2,
        val: 2,
    },
    HCode {
        bits: 0b10,
        len: 2,
        val: 3,
    },
    HCode {
        bits: 0b011,
        len: 3,
        val: 4,
    },
    HCode {
        bits: 0b0011,
        len: 4,
        val: 5,
    },
    HCode {
        bits: 0b0010,
        len: 4,
        val: 6,
    },
    HCode {
        bits: 0b00011,
        len: 5,
        val: 7,
    },
    HCode {
        bits: 0b000101,
        len: 6,
        val: 8,
    },
    HCode {
        bits: 0b000100,
        len: 6,
        val: 9,
    },
    HCode {
        bits: 0b0000100,
        len: 7,
        val: 10,
    },
    HCode {
        bits: 0b0000101,
        len: 7,
        val: 11,
    },
    HCode {
        bits: 0b0000111,
        len: 7,
        val: 12,
    },
    HCode {
        bits: 0b00000100,
        len: 8,
        val: 13,
    },
    HCode {
        bits: 0b00000111,
        len: 8,
        val: 14,
    },
    HCode {
        bits: 0b000011000,
        len: 9,
        val: 15,
    },
    HCode {
        bits: 0b0000010111,
        len: 10,
        val: 16,
    },
    HCode {
        bits: 0b0000011000,
        len: 10,
        val: 17,
    },
    HCode {
        bits: 0b0000001000,
        len: 10,
        val: 18,
    },
    HCode {
        bits: 0b00001100111,
        len: 11,
        val: 19,
    },
    HCode {
        bits: 0b00001101000,
        len: 11,
        val: 20,
    },
    HCode {
        bits: 0b00001101100,
        len: 11,
        val: 21,
    },
    HCode {
        bits: 0b00000110111,
        len: 11,
        val: 22,
    },
    HCode {
        bits: 0b00000101000,
        len: 11,
        val: 23,
    },
    HCode {
        bits: 0b00000010111,
        len: 11,
        val: 24,
    },
    HCode {
        bits: 0b00000011000,
        len: 11,
        val: 25,
    },
    HCode {
        bits: 0b000011001010,
        len: 12,
        val: 26,
    },
    HCode {
        bits: 0b000011001011,
        len: 12,
        val: 27,
    },
    HCode {
        bits: 0b000011001100,
        len: 12,
        val: 28,
    },
    HCode {
        bits: 0b000011001101,
        len: 12,
        val: 29,
    },
    HCode {
        bits: 0b000001101000,
        len: 12,
        val: 30,
    },
    HCode {
        bits: 0b000001101001,
        len: 12,
        val: 31,
    },
    HCode {
        bits: 0b000001101010,
        len: 12,
        val: 32,
    },
    HCode {
        bits: 0b000001101011,
        len: 12,
        val: 33,
    },
    HCode {
        bits: 0b000011010010,
        len: 12,
        val: 34,
    },
    HCode {
        bits: 0b000011010011,
        len: 12,
        val: 35,
    },
    HCode {
        bits: 0b000011010100,
        len: 12,
        val: 36,
    },
    HCode {
        bits: 0b000011010101,
        len: 12,
        val: 37,
    },
    HCode {
        bits: 0b000011010110,
        len: 12,
        val: 38,
    },
    HCode {
        bits: 0b000011010111,
        len: 12,
        val: 39,
    },
    HCode {
        bits: 0b000001101100,
        len: 12,
        val: 40,
    },
    HCode {
        bits: 0b000001101101,
        len: 12,
        val: 41,
    },
    HCode {
        bits: 0b000011011010,
        len: 12,
        val: 42,
    },
    HCode {
        bits: 0b000011011011,
        len: 12,
        val: 43,
    },
    HCode {
        bits: 0b000001010100,
        len: 12,
        val: 44,
    },
    HCode {
        bits: 0b000001010101,
        len: 12,
        val: 45,
    },
    HCode {
        bits: 0b000001010110,
        len: 12,
        val: 46,
    },
    HCode {
        bits: 0b000001010111,
        len: 12,
        val: 47,
    },
    HCode {
        bits: 0b000001100100,
        len: 12,
        val: 48,
    },
    HCode {
        bits: 0b000001100101,
        len: 12,
        val: 49,
    },
    HCode {
        bits: 0b000001010010,
        len: 12,
        val: 50,
    },
    HCode {
        bits: 0b000001010011,
        len: 12,
        val: 51,
    },
    HCode {
        bits: 0b000000100100,
        len: 12,
        val: 52,
    },
    HCode {
        bits: 0b000000110111,
        len: 12,
        val: 53,
    },
    HCode {
        bits: 0b000000111000,
        len: 12,
        val: 54,
    },
    HCode {
        bits: 0b000000100111,
        len: 12,
        val: 55,
    },
    HCode {
        bits: 0b000000101000,
        len: 12,
        val: 56,
    },
    HCode {
        bits: 0b000001011000,
        len: 12,
        val: 57,
    },
    HCode {
        bits: 0b000001011001,
        len: 12,
        val: 58,
    },
    HCode {
        bits: 0b000000101011,
        len: 12,
        val: 59,
    },
    HCode {
        bits: 0b000000101100,
        len: 12,
        val: 60,
    },
    HCode {
        bits: 0b000001011010,
        len: 12,
        val: 61,
    },
    HCode {
        bits: 0b000001100110,
        len: 12,
        val: 62,
    },
    HCode {
        bits: 0b000001100111,
        len: 12,
        val: 63,
    },
];

// T.4 Table 1: 黒ラン Make-up コード（64..1728）
const BLACK_MAKEUPS: &[HCode] = &[
    HCode {
        bits: 0b0000001111,
        len: 10,
        val: 64,
    },
    HCode {
        bits: 0b000011001000,
        len: 12,
        val: 128,
    },
    HCode {
        bits: 0b000011001001,
        len: 12,
        val: 192,
    },
    HCode {
        bits: 0b000001011011,
        len: 12,
        val: 256,
    },
    HCode {
        bits: 0b000000110011,
        len: 12,
        val: 320,
    },
    HCode {
        bits: 0b000000110100,
        len: 12,
        val: 384,
    },
    HCode {
        bits: 0b000000110101,
        len: 12,
        val: 448,
    },
    HCode {
        bits: 0b0000001101100,
        len: 13,
        val: 512,
    },
    HCode {
        bits: 0b0000001101101,
        len: 13,
        val: 576,
    },
    HCode {
        bits: 0b0000001001010,
        len: 13,
        val: 640,
    },
    HCode {
        bits: 0b0000001001011,
        len: 13,
        val: 704,
    },
    HCode {
        bits: 0b0000001001100,
        len: 13,
        val: 768,
    },
    HCode {
        bits: 0b0000001001101,
        len: 13,
        val: 832,
    },
    HCode {
        bits: 0b0000001110010,
        len: 13,
        val: 896,
    },
    HCode {
        bits: 0b0000001110011,
        len: 13,
        val: 960,
    },
    HCode {
        bits: 0b0000001110100,
        len: 13,
        val: 1024,
    },
    HCode {
        bits: 0b0000001110101,
        len: 13,
        val: 1088,
    },
    HCode {
        bits: 0b0000001110110,
        len: 13,
        val: 1152,
    },
    HCode {
        bits: 0b0000001110111,
        len: 13,
        val: 1216,
    },
    HCode {
        bits: 0b0000001010010,
        len: 13,
        val: 1280,
    },
    HCode {
        bits: 0b0000001010011,
        len: 13,
        val: 1344,
    },
    HCode {
        bits: 0b0000001010100,
        len: 13,
        val: 1408,
    },
    HCode {
        bits: 0b0000001010101,
        len: 13,
        val: 1472,
    },
    HCode {
        bits: 0b0000001011010,
        len: 13,
        val: 1536,
    },
    HCode {
        bits: 0b0000001011011,
        len: 13,
        val: 1600,
    },
    HCode {
        bits: 0b0000001100100,
        len: 13,
        val: 1664,
    },
    HCode {
        bits: 0b0000001100101,
        len: 13,
        val: 1728,
    },
];

// T.4 Table 1: 拡張 Make-up コード（1792..2560）。白黒共通。
const COMMON_MAKEUPS: &[HCode] = &[
    HCode {
        bits: 0b00000001000,
        len: 11,
        val: 1792,
    },
    HCode {
        bits: 0b00000001100,
        len: 11,
        val: 1856,
    },
    HCode {
        bits: 0b00000001101,
        len: 11,
        val: 1920,
    },
    HCode {
        bits: 0b000000010010,
        len: 12,
        val: 1984,
    },
    HCode {
        bits: 0b000000010011,
        len: 12,
        val: 2048,
    },
    HCode {
        bits: 0b000000010100,
        len: 12,
        val: 2112,
    },
    HCode {
        bits: 0b000000010101,
        len: 12,
        val: 2176,
    },
    HCode {
        bits: 0b000000010110,
        len: 12,
        val: 2240,
    },
    HCode {
        bits: 0b000000010111,
        len: 12,
        val: 2304,
    },
    HCode {
        bits: 0b000000011100,
        len: 12,
        val: 2368,
    },
    HCode {
        bits: 0b000000011101,
        len: 12,
        val: 2432,
    },
    HCode {
        bits: 0b000000011110,
        len: 12,
        val: 2496,
    },
    HCode {
        bits: 0b000000011111,
        len: 12,
        val: 2560,
    },
];

/// EOL コード（12 ビット: 11 個の 0 + 1）。
const EOL_BITS: u32 = 0x001;
const EOL_LEN: u32 = 12;

/// 2D モードコード（T.4 Table 2 / T.6）。
#[derive(Debug, Clone, Copy)]
enum Mode2D {
    /// Pass モード（コード `0001`）
    Pass,
    /// Horizontal モード（コード `001`）
    H,
    /// Vertical モード（V0 / V±1 / V±2 / V±3）
    V(i32),
    /// 1D 拡張（uncommon, 未対応）
    Ext1d,
    /// 2D 拡張（uncommon, 未対応）
    Ext2d,
}

const MODES_2D: &[(u16, u8, Mode2D)] = &[
    (0b1, 1, Mode2D::V(0)),
    (0b011, 3, Mode2D::V(1)),
    (0b010, 3, Mode2D::V(-1)),
    (0b000011, 6, Mode2D::V(2)),
    (0b000010, 6, Mode2D::V(-2)),
    (0b0000011, 7, Mode2D::V(3)),
    (0b0000010, 7, Mode2D::V(-3)),
    (0b001, 3, Mode2D::H),
    (0b0001, 4, Mode2D::Pass),
    (0b0000001, 7, Mode2D::Ext2d),
    (0b000000001, 9, Mode2D::Ext1d),
];

// ---------------------------------------------------------------------------
// ハフマンデコーダ
// ---------------------------------------------------------------------------

/// 与えられたテーブル群から 1 つのコードを読み取り、(値, 終端か否か) を返す。
///
/// 終端コード→makeup→共通 makeup の順に試す。最大 13 ビットまで読んで一致なし
/// なら `None`（壊れたストリーム）。
fn read_run_code(
    reader: &mut BitReader,
    terms: &[HCode],
    makeups: &[HCode],
) -> Option<(i32, bool)> {
    let mut buf: u16 = 0;
    for nbits in 1..=13u8 {
        buf = (buf << 1) | reader.read_bit() as u16;
        for e in terms {
            if e.len == nbits && e.bits == buf {
                return Some((e.val, true));
            }
        }
        for e in makeups {
            if e.len == nbits && e.bits == buf {
                return Some((e.val, false));
            }
        }
        for e in COMMON_MAKEUPS {
            if e.len == nbits && e.bits == buf {
                return Some((e.val, false));
            }
        }
    }
    None
}

/// MH 方式で 1 つのラン長（白または黒）を読む。Make-up を繰り返し読み、
/// 終端コードが来たら累積を返す。`columns_left` はクランプ用上限。
fn read_mh_run(reader: &mut BitReader, color_white: bool, columns_left: u32) -> Result<u32> {
    let (terms, makeups) = if color_white {
        (WHITE_TERMS, WHITE_MAKEUPS)
    } else {
        (BLACK_TERMS, BLACK_MAKEUPS)
    };
    let mut total: u32 = 0;
    // 異常ループ防止: make-up の累計でも一度にコラム数を超えたら打ち切る。
    for _ in 0..64 {
        let (val, is_term) = read_run_code(reader, terms, makeups)
            .ok_or_else(|| err("ccitt: 不正な MH ラン符号"))?;
        if val < 0 {
            return Err(err("ccitt: 不正なラン長"));
        }
        total = total.saturating_add(val as u32);
        if is_term {
            return Ok(total.min(columns_left));
        }
        if total > MAX_DIM {
            return Err(err("ccitt: ラン長が大きすぎる"));
        }
    }
    Err(err("ccitt: makeup の連続が多すぎる"))
}

/// 2D モードコードを 1 つ読む。
fn read_mode_2d(reader: &mut BitReader) -> Result<Mode2D> {
    let mut buf: u16 = 0;
    for nbits in 1..=10u8 {
        buf = (buf << 1) | reader.read_bit() as u16;
        for &(bits, len, mode) in MODES_2D {
            if len == nbits && bits == buf {
                return Ok(mode);
            }
        }
    }
    Err(err("ccitt: 不正な 2D モード符号"))
}

// ---------------------------------------------------------------------------
// 行デコード
// ---------------------------------------------------------------------------

/// 参照ライン上で `a0` の右隣にある「a0 と反対色への遷移点」b1 と、その次 b2 を返す。
///
/// `ref_changes` は前行の遷移位置リスト（昇順、白→黒→白→… の順）。`a0_white` は
/// 現在の符号化ライン上で a0 のすぐ右側にあるピクセル色（最初は white = true）。
fn find_b1_b2(ref_changes: &[u32], a0: i32, a0_white: bool, columns: u32) -> (u32, u32) {
    // a0 より右の最初の遷移点を探す。
    let mut i = 0;
    while i < ref_changes.len() && (ref_changes[i] as i32) <= a0 {
        i += 1;
    }
    // 参照ラインの遷移点 i は: i が偶数なら「→黒」、奇数なら「→白」。
    // a0 と反対色（a0 白なら黒、a0 黒なら白）への遷移にスキップ。
    while i < ref_changes.len() {
        let i_even = i % 2 == 0;
        let new_color_black = i_even;
        let want_black = a0_white;
        if new_color_black == want_black {
            break;
        }
        i += 1;
    }
    let b1 = if i < ref_changes.len() {
        ref_changes[i].min(columns)
    } else {
        columns
    };
    let b2 = if i + 1 < ref_changes.len() {
        ref_changes[i + 1].min(columns)
    } else {
        columns
    };
    (b1, b2)
}

/// 1D（Modified Huffman）方式で 1 行をデコードする。
///
/// `out` に遷移位置を昇順で詰める。最後の位置 = `columns` の場合は番兵として落とす。
fn decode_row_1d(reader: &mut BitReader, columns: u32, out: &mut Vec<u32>) -> Result<()> {
    out.clear();
    let mut pos: u32 = 0;
    let mut color_white = true;
    while pos < columns {
        let remaining = columns - pos;
        let run = read_mh_run(reader, color_white, remaining)?;
        pos = pos.saturating_add(run);
        if pos > columns {
            pos = columns;
        }
        out.push(pos);
        color_white = !color_white;
    }
    // 末尾が columns のときは描画上の意味がないため番兵として除去する。
    if out.last().copied() == Some(columns) {
        out.pop();
    }
    Ok(())
}

/// 2D（Modified READ / MMR）方式で 1 行をデコードする。
fn decode_row_2d(
    reader: &mut BitReader,
    ref_changes: &[u32],
    columns: u32,
    out: &mut Vec<u32>,
) -> Result<()> {
    out.clear();
    let mut a0: i32 = -1;
    // 無限ループ防止: 1 行あたり最大 `2*columns + 16` 個の遷移までしか作らない。
    let max_changes = (columns as usize).saturating_mul(2).saturating_add(16);
    while a0 < columns as i32 {
        if out.len() > max_changes {
            return Err(err("ccitt: 1 行の遷移数が多すぎる"));
        }
        let a0_white = out.len().is_multiple_of(2);
        let (b1, b2) = find_b1_b2(ref_changes, a0, a0_white, columns);
        let mode = read_mode_2d(reader)?;
        match mode {
            Mode2D::Pass => {
                a0 = b2 as i32;
            }
            Mode2D::H => {
                let base = a0.max(0) as u32;
                let r1 = read_mh_run(reader, a0_white, columns.saturating_sub(base))?;
                let a1 = base.saturating_add(r1).min(columns);
                let r2 = read_mh_run(reader, !a0_white, columns.saturating_sub(a1))?;
                let a2 = a1.saturating_add(r2).min(columns);
                // 単調性を保つために push 前にチェック（壊れた入力対策）。
                let mut last = out.last().copied().unwrap_or(0);
                if a1 >= last {
                    out.push(a1);
                    last = a1;
                }
                if a2 >= last {
                    out.push(a2);
                }
                a0 = a2 as i32;
            }
            Mode2D::V(d) => {
                let a1 = (b1 as i32 + d).max(0).min(columns as i32) as u32;
                let last = out.last().copied().unwrap_or(0);
                if a1 >= last {
                    out.push(a1);
                }
                a0 = a1 as i32;
            }
            Mode2D::Ext1d | Mode2D::Ext2d => {
                return Err(err("ccitt: 拡張モード（uncommon）は未対応"));
            }
        }
    }
    // 末尾の columns 番兵は外す。
    while out.last().copied() == Some(columns) {
        out.pop();
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 出力パッキング
// ---------------------------------------------------------------------------

/// 1 行分の遷移位置から 1bpp パックビット（行末まで `ceil(columns/8)` バイト）を生成。
///
/// 内部表現は常に「1 = 黒、0 = 白」。
fn write_row(changes: &[u32], columns: u32, out: &mut Vec<u8>) {
    let row_bytes = (columns as usize).div_ceil(8);
    let off = out.len();
    out.resize(off + row_bytes, 0);
    // 行の塗りは「白 → 黒 → 白 → …」と交互。
    let mut color_black = false;
    let mut last: u32 = 0;
    for &p in changes {
        let end = p.min(columns).max(last);
        if color_black {
            fill_bits(&mut out[off..off + row_bytes], last, end);
        }
        last = end;
        color_black = !color_black;
    }
    if color_black {
        fill_bits(&mut out[off..off + row_bytes], last, columns);
    }
}

/// `row[start..end]` のビットを 1 で塗る（MSB ファースト）。
fn fill_bits(row: &mut [u8], start: u32, end: u32) {
    for c in start..end {
        let i = (c as usize) / 8;
        let bit = 7 - ((c % 8) as u8);
        if let Some(byte) = row.get_mut(i) {
            *byte |= 1 << bit;
        }
    }
}

/// `BlackIs1 = false` のときに使う: 全バイトをビット反転して、行末のパディング
/// ビットを 0 に戻す。
fn invert_to_pdf_default(out: &mut [u8], columns: u32, rows_written: u32) {
    let row_bytes = (columns as usize).div_ceil(8);
    let pad_bits = (row_bytes as u32) * 8 - columns;
    let last_mask: u8 = if pad_bits == 0 {
        0xFF
    } else {
        // 上位 columns%8 ビットを残し、下位 pad_bits ビットを 0 にする。
        !((1u8 << pad_bits) - 1)
    };
    for r in 0..rows_written as usize {
        let off = r * row_bytes;
        for (i, byte) in out[off..off + row_bytes].iter_mut().enumerate() {
            *byte = !*byte;
            if i == row_bytes - 1 {
                *byte &= last_mask;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// EOL / EOB の検出
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// パブリック API
// ---------------------------------------------------------------------------

/// CCITTFaxDecode を実行して 1bpp パックビットの出力を返す。
///
/// 出力サイズは `rows × ceil(columns/8)` バイト。`rows = 0`（未指定）の場合は
/// 入力を読み切れるだけ読み、EOB（T.6 の EOFB / T.4 の連続 EOL）で終端する。
pub fn decode(data: &[u8], params: &CcittParams) -> Result<Vec<u8>> {
    if params.columns == 0 {
        return Ok(Vec::new());
    }
    if params.columns > MAX_DIM {
        return Err(err("ccitt: columns が大きすぎる"));
    }

    let mut reader = BitReader::new(data);
    let row_bytes = (params.columns as usize).div_ceil(8);
    let row_cap = if params.rows > 0 {
        params.rows as usize
    } else {
        // 未指定時の概算: 入力長から行数の上限を推定（最低 1 ビット/行と仮定）。
        ((data.len() * 8) / params.columns.max(1) as usize).saturating_add(16)
    };
    let mut out: Vec<u8> = Vec::with_capacity(row_cap.saturating_mul(row_bytes));

    let mut prev: Vec<u32> = Vec::new();
    let mut cur: Vec<u32> = Vec::new();
    let max_rows = if params.rows > 0 {
        params.rows
    } else {
        u32::MAX
    };
    let mut rows_written: u32 = 0;

    let k = params.k;
    // T.4 でファイル先頭にも EOL を持つ実装が多いため、最初の EOL があれば消費する。
    if k >= 0 && params.end_of_line {
        // 失敗しても問題ない（無ければ無しで進む）。
        let _ = peek_and_consume_eol(&mut reader, params.encoded_byte_align);
    }

    while rows_written < max_rows {
        // 終端判定: T.6 は EOFB（2 連続 EOL = 24 ビットの `001 001`）、
        // T.4 は RTC（連続 EOL）または EOL なしで EOF。
        if params.end_of_block {
            if k < 0 {
                // EOFB: 24 ビット先読み
                if reader.peek_bits(EOL_LEN * 2) == ((EOL_BITS << EOL_LEN) | EOL_BITS) {
                    reader.consume(EOL_LEN * 2);
                    break;
                }
            } else {
                // RTC: 6 連続 EOL
                let mut rtc = true;
                for i in 0..6 {
                    let off = i * EOL_LEN;
                    if reader.peek_bits(off + EOL_LEN) & ((1u32 << EOL_LEN) - 1) != EOL_BITS {
                        rtc = false;
                        break;
                    }
                }
                if rtc {
                    reader.consume(EOL_LEN * 6);
                    break;
                }
            }
        }

        // T.4: 行頭で EOL を消費（end_of_line または rows 未指定時の通常運用）。
        if k >= 0 && (params.end_of_line || params.encoded_byte_align) {
            // 行頭の EOL を消費。見つからなくても、消費せず素通り。
            let _ = peek_and_consume_eol(&mut reader, params.encoded_byte_align);
        }

        // T.4 mixed: タグビット（1=1D, 0=2D）を読む
        let use_1d = if k < 0 {
            false
        } else if k == 0 {
            true
        } else {
            // T.4 mixed: EOL の直後にタグが 1 bit。EOL を消費していない場合でも
            // 通常は EOL があるはず（PDF 実装次第）。タグ 1 bit を読む。
            reader.read_bit() == 1
        };

        // EOF 直前で空入力になっていれば打ち切る。
        if reader.eof() && data.is_empty() {
            break;
        }

        cur.clear();
        let row_result = if use_1d {
            decode_row_1d(&mut reader, params.columns, &mut cur)
        } else {
            decode_row_2d(&mut reader, &prev, params.columns, &mut cur)
        };
        match row_result {
            Ok(()) => {}
            Err(_) if rows_written > 0 && !params.end_of_block => {
                // rows 指定時は中途でエラーが出ても、出力を縮めて返す。
                break;
            }
            Err(e) => {
                // 0 行も読めずに失敗、または EOB 期待だが壊れている: rows 指定なら
                // 一部行を返す、未指定ならエラー。
                if rows_written == 0 {
                    return Err(e);
                }
                break;
            }
        }
        write_row(&cur, params.columns, &mut out);
        std::mem::swap(&mut prev, &mut cur);
        rows_written = rows_written.saturating_add(1);

        // EOF に達して `rows` 未指定ならここで打ち切る（無限ループ対策）。
        if reader.eof() && params.rows == 0 {
            break;
        }
    }

    // PDF 既定（BlackIs1 = false）では 1 = 白へ反転する。
    if !params.black_is_1 {
        invert_to_pdf_default(&mut out, params.columns, rows_written);
    }

    Ok(out)
}

/// 行頭の EOL を見つけたら消費する。なかったら何もしない。
fn peek_and_consume_eol(reader: &mut BitReader, encoded_byte_align: bool) -> bool {
    let save_byte = reader.byte_pos;
    let save_bit = reader.bit_pos;
    if encoded_byte_align {
        reader.byte_align();
    }
    if reader.peek_bits(EOL_LEN) == EOL_BITS {
        reader.consume(EOL_LEN);
        true
    } else {
        // 失敗時は元に戻す（byte_align で進んだ分も）。
        reader.byte_pos = save_byte;
        reader.bit_pos = save_bit;
        false
    }
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    /// ヘルパ: MSB ファーストのビット列文字列（"0" "1" のみ）からバイト列を生成。
    fn bits(s: &str) -> Vec<u8> {
        let mut out = Vec::new();
        let mut byte: u8 = 0;
        let mut n: u8 = 0;
        for c in s.chars() {
            if c == ' ' {
                continue;
            }
            byte <<= 1;
            if c == '1' {
                byte |= 1;
            }
            n += 1;
            if n == 8 {
                out.push(byte);
                byte = 0;
                n = 0;
            }
        }
        if n > 0 {
            byte <<= 8 - n;
            out.push(byte);
        }
        out
    }

    #[test]
    fn bitreader_msb_first_and_eof_is_zero() {
        let data = [0b1010_0110, 0b1100_0000];
        let mut r = BitReader::new(&data);
        assert_eq!(r.read_bit(), 1);
        assert_eq!(r.read_bit(), 0);
        assert_eq!(r.read_bit(), 1);
        assert_eq!(r.read_bit(), 0);
        assert_eq!(r.peek_bits(4), 0b0110);
        assert_eq!(r.read_bit(), 0);
        assert_eq!(r.read_bit(), 1);
        assert_eq!(r.read_bit(), 1);
        assert_eq!(r.read_bit(), 0);
        // 次のバイト
        assert_eq!(r.read_bit(), 1);
        assert_eq!(r.read_bit(), 1);
        // 末尾を超えたら 0
        for _ in 0..6 {
            assert_eq!(r.read_bit(), 0);
        }
        assert_eq!(r.read_bit(), 0);
    }

    #[test]
    fn read_white_run_term() {
        // 白終端コード 4: 1011 → run=4
        let data = bits("1011");
        let mut r = BitReader::new(&data);
        let run = read_mh_run(&mut r, true, 100).unwrap();
        assert_eq!(run, 4);
    }

    #[test]
    fn read_white_run_makeup_then_term() {
        // makeup 64 (11011) + 終端 4 (1011) → run = 64 + 4 = 68
        let data = bits("11011 1011");
        let mut r = BitReader::new(&data);
        let run = read_mh_run(&mut r, true, 1000).unwrap();
        assert_eq!(run, 68);
    }

    #[test]
    fn read_black_run_term() {
        // 黒終端 3: "10" → run=3
        let data = bits("10");
        let mut r = BitReader::new(&data);
        let run = read_mh_run(&mut r, false, 100).unwrap();
        assert_eq!(run, 3);
    }

    #[test]
    fn read_mode_2d_v0_and_h() {
        let d = bits("1");
        let mut r = BitReader::new(&d);
        assert!(matches!(read_mode_2d(&mut r).unwrap(), Mode2D::V(0)));

        let d = bits("001");
        let mut r = BitReader::new(&d);
        assert!(matches!(read_mode_2d(&mut r).unwrap(), Mode2D::H));

        let d = bits("0001");
        let mut r = BitReader::new(&d);
        assert!(matches!(read_mode_2d(&mut r).unwrap(), Mode2D::Pass));

        let d = bits("011");
        let mut r = BitReader::new(&d);
        assert!(matches!(read_mode_2d(&mut r).unwrap(), Mode2D::V(1)));

        let d = bits("010");
        let mut r = BitReader::new(&d);
        assert!(matches!(read_mode_2d(&mut r).unwrap(), Mode2D::V(-1)));
    }

    /// T.6 で 8x1 の全白行: V(0) を 1 つ書くだけ。
    #[test]
    fn t6_all_white_row() {
        let mut p = CcittParams::default();
        p.k = -1;
        p.columns = 8;
        p.rows = 1;
        p.end_of_block = false;
        // V0 = "1" + EOFB あれば無くてもよい。rows=1, end_of_block=false なら 1 行で終わる。
        let data = bits("1");
        let out = decode(&data, &p).unwrap();
        // BlackIs1=false なので 1=白。全白 → 全バイト 0xFF（columns=8 ぴったり）。
        assert_eq!(out, vec![0xFF]);
    }

    /// T.6 で 8x1 の全黒行: H モードで白 0 + 黒 8。
    #[test]
    fn t6_all_black_row() {
        let mut p = CcittParams::default();
        p.k = -1;
        p.columns = 8;
        p.rows = 1;
        p.end_of_block = false;
        // H = "001", 白終端 0 = "00110101" (8 bit), 黒終端 8 = "000101" (6 bit)
        let data = bits("001 00110101 000101");
        let out = decode(&data, &p).unwrap();
        // 全黒 → BlackIs1=false で 0x00
        assert_eq!(out, vec![0x00]);
    }

    /// T.6 で 8x1: 先頭 5 白 + 3 黒。
    #[test]
    fn t6_5w_3b_row() {
        let mut p = CcittParams::default();
        p.k = -1;
        p.columns = 8;
        p.rows = 1;
        p.end_of_block = false;
        // H + 白 5 + 黒 3
        // 白 5 終端: "1100" (4 bit), 黒 3 終端: "10" (2 bit)
        let data = bits("001 1100 10");
        let out = decode(&data, &p).unwrap();
        // 5 白 + 3 黒 → ビット内部表現 0b00000111 → 反転 0b11111000 = 0xF8
        assert_eq!(out, vec![0xF8]);
    }

    /// T.6 で BlackIs1=true: 内部表現そのまま。
    #[test]
    fn t6_black_is_1() {
        let mut p = CcittParams::default();
        p.k = -1;
        p.columns = 8;
        p.rows = 1;
        p.end_of_block = false;
        p.black_is_1 = true;
        // H + 白 5 + 黒 3
        let data = bits("001 1100 10");
        let out = decode(&data, &p).unwrap();
        // BlackIs1=true → 1=黒の表現そのまま。0b00000111 = 0x07
        assert_eq!(out, vec![0x07]);
    }

    /// T.6 で 2 行: 1 行目は H モードで一部黒、2 行目は V(0) で同一パターンを継承。
    #[test]
    fn t6_two_rows_v0_inherits() {
        let mut p = CcittParams::default();
        p.k = -1;
        p.columns = 8;
        p.rows = 2;
        p.end_of_block = false;
        // 1 行目: 5W + 3B（前テストと同じ）"001 1100 10"
        // 2 行目: 同パターンを V(0) で表現。a0=-1 white, b1 = 5（前行の遷移 5）→ a1=5。
        //         pass モードは使わず、もう一度 V(0): a0=5 black, b1 = 8 (列終端) → a1=8。
        //         実際は V(0) → V(0): "1 1"。a0=-1→5（V0 with b1=5）→ a0=5 → 次 V0 with
        //         b1 = end of ref changes（columns=8）= 8 → a1=8、行終端。
        let data = bits("001 1100 10  1 1");
        let out = decode(&data, &p).unwrap();
        // 両行とも 5W + 3B → 0xF8 × 2
        assert_eq!(out, vec![0xF8, 0xF8]);
    }

    /// T.6 全白行を Pass モードで表現（純粋 V(0) と等価）。
    #[test]
    fn t6_pass_mode_white() {
        let mut p = CcittParams::default();
        p.k = -1;
        p.columns = 8;
        p.rows = 1;
        p.end_of_block = false;
        // Pass = "0001"。a0=-1, ref 全白 → b1=8, b2=8。a0=b2=8 で終端。
        let data = bits("0001");
        let out = decode(&data, &p).unwrap();
        assert_eq!(out, vec![0xFF]);
    }

    /// T.4 1D（K=0）で 1 行を符号化: 5W + 3B。先頭 EOL なし。
    #[test]
    fn t4_1d_single_row_no_eol() {
        let mut p = CcittParams::default();
        p.k = 0;
        p.columns = 8;
        p.rows = 1;
        p.end_of_block = false;
        p.end_of_line = false;
        // 1D: 白 5 終端 + 黒 3 終端
        let data = bits("1100 10");
        let out = decode(&data, &p).unwrap();
        assert_eq!(out, vec![0xF8]);
    }

    /// T.4 1D で 2 行を EOL 区切りでデコード。
    #[test]
    fn t4_1d_two_rows_with_eol() {
        let mut p = CcittParams::default();
        p.k = 0;
        p.columns = 8;
        p.rows = 2;
        p.end_of_block = false;
        p.end_of_line = true;
        // EOL + 白 5 + 黒 3 + EOL + 白 0 + 黒 8
        let data = bits(
            "000000000001  1100 10  \
             000000000001  00110101 000101",
        );
        let out = decode(&data, &p).unwrap();
        // 1 行目: 0xF8, 2 行目: 全黒 0x00
        assert_eq!(out, vec![0xF8, 0x00]);
    }

    /// EOFB（2 連続 EOL）で終端。
    #[test]
    fn t6_eofb_terminates() {
        let mut p = CcittParams::default();
        p.k = -1;
        p.columns = 8;
        p.rows = 0; // 未指定 → EOFB で終端
        p.end_of_block = true;
        // 1 行 V(0) で全白 + EOFB
        let data = bits("1  000000000001 000000000001");
        let out = decode(&data, &p).unwrap();
        assert_eq!(out, vec![0xFF]);
    }

    /// columns が 8 の倍数でないとき: 最後のバイトはパディングを含む。
    #[test]
    fn t6_non_byte_aligned_columns() {
        let mut p = CcittParams::default();
        p.k = -1;
        p.columns = 5;
        p.rows = 1;
        p.end_of_block = false;
        // 全黒 5px: H + 白 0 + 黒 5
        // 白 0: "00110101", 黒 5: "0011"
        let data = bits("001 00110101 0011");
        let out = decode(&data, &p).unwrap();
        // 内部表現 0b11111000（上位 5 ビット黒）→ 反転 0b00000000 → さらにパディング 0 化 → 0x00
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], 0x00);
    }

    /// 1 行 8 ピクセル: 黒 1 個 + 白 7。CCITT で最小ビット列を作って検証。
    #[test]
    fn t6_pixel_at_position_0() {
        let mut p = CcittParams::default();
        p.k = -1;
        p.columns = 8;
        p.rows = 1;
        p.end_of_block = false;
        // a0=-1 white. H: 白 0 ("00110101") + 黒 1 ("010"). a0=1.
        // 次に V(0): b1 = 8 (no more changes) → a1=8, 終了。"1"
        let data = bits("001 00110101 010  1");
        let out = decode(&data, &p).unwrap();
        // 内部: 1 黒 + 7 白 = 0b10000000 → 反転 0b01111111 = 0x7F
        assert_eq!(out, vec![0x7F]);
    }

    /// 壊れたコード（13 ビット先まで一致なし）でエラー。panic しないこと。
    #[test]
    fn t6_corrupt_code_returns_error() {
        let mut p = CcittParams::default();
        p.k = -1;
        p.columns = 8;
        p.rows = 1;
        p.end_of_block = false;
        // H モードを開始させ、白ランで完全に不正な 13bit「0」を入れる
        let data = bits("001 0000000000000");
        assert!(decode(&data, &p).is_err());
    }

    /// 16 ピクセル幅 4 行: 様々なパターン
    #[test]
    fn t6_16x4_mixed() {
        let mut p = CcittParams::default();
        p.k = -1;
        p.columns = 16;
        p.rows = 4;
        p.end_of_block = false;
        // Row 0: 全白 → V(0)
        // Row 1: 全黒 → H + 白 0 + 黒 16
        //   - 黒 16 は make-up 不要（< 64 の終端 16 の符号: 0000010111 = 10 bit）
        // Row 2: 4 黒 + 12 白 → H + 白 0 + 黒 4 + V(0)
        //   - 黒 4 終端: "011"
        // Row 3: V(0) を 3 回（ref=[0,4] の各遷移点を V0 で踏み、最後に列終端へ）
        let data = bits(
            "1  \
             001 00110101 0000010111  \
             001 00110101 011  1  \
             1 1 1",
        );
        let out = decode(&data, &p).unwrap();
        assert_eq!(out.len(), 4 * 2);
        // Row 0 全白 → 0xFF 0xFF
        assert_eq!(&out[0..2], &[0xFF, 0xFF]);
        // Row 1 全黒 → 0x00 0x00
        assert_eq!(&out[2..4], &[0x00, 0x00]);
        // Row 2 4黒 + 12白 → 内部 0b1111000000000000 → 反転 0b0000111111111111 = 0x0F 0xFF
        assert_eq!(&out[4..6], &[0x0F, 0xFF]);
        // Row 3 同上
        assert_eq!(&out[6..8], &[0x0F, 0xFF]);
    }

    /// params_from_dict: 主要キーを読み取れる。
    #[test]
    fn params_from_dict_reads_all_keys() {
        let mut d = Dictionary::new();
        d.set("K", Object::Integer(-1));
        d.set("Columns", Object::Integer(1024));
        d.set("Rows", Object::Integer(100));
        d.set("BlackIs1", Object::Boolean(true));
        d.set("EndOfBlock", Object::Boolean(false));
        d.set("EncodedByteAlign", Object::Boolean(true));
        let p = params_from_dict(&d);
        assert_eq!(p.k, -1);
        assert_eq!(p.columns, 1024);
        assert_eq!(p.rows, 100);
        assert!(p.black_is_1);
        assert!(!p.end_of_block);
        assert!(p.encoded_byte_align);
    }

    /// Columns = 0 のときは空出力（過大値ガードと併せて panic しないことを確認）。
    #[test]
    fn zero_columns_returns_empty() {
        let mut p = CcittParams::default();
        p.k = -1;
        p.columns = 0;
        let out = decode(&[0xFFu8; 4], &p).unwrap();
        assert!(out.is_empty());
    }

    /// 全データが 0 でも panic しない（Err でもよい、要は壊れずに戻ること）。
    #[test]
    fn all_zeros_input_no_panic() {
        let mut p = CcittParams::default();
        p.k = -1;
        p.columns = 8;
        p.rows = 0;
        p.end_of_block = true;
        let data = vec![0u8; 6];
        let _ = decode(&data, &p);
    }

    /// 0 行と指定して任意のゴミ入力でも panic しない。
    #[test]
    fn zero_rows_corrupt_no_panic() {
        let mut p = CcittParams::default();
        p.k = -1;
        p.columns = 8;
        p.rows = 4;
        p.end_of_block = false;
        let data = vec![0xA5u8; 16];
        let _ = decode(&data, &p);
    }
}
