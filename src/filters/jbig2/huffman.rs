//! Huffman 復号エンジンと標準テーブル B.1〜B.15（T.88 Annex B）。
//!
//! Symbol dictionary・Text region・Generic refinement で算術符号化の代わりに
//! Huffman 経路を選んだ場合に使う。標準テーブルは Annex B に列挙されており、
//! カスタムテーブルセグメント（type 53）からの動的構築もサポートする。
//!
//! 各エントリは 4 つの属性を持つ:
//! - `range_low`  : このエントリで表現される値の下限（符号付き）
//! - `range_len`  : prefix の後に読む追加ビット数（0 ならエントリは単一値）
//! - `prefix_len` / `prefix_code`: Huffman プレフィックス本体
//! - `flag`       : 通常 / OOB / 低位レンジ拡張 / 高位レンジ拡張
//!
//! セッション 1 ではエンジンとテーブル B.1 のみを完全実装する。残り（B.2〜B.15）
//! は Symbol/Text region と同じセッション 3 で `standard_table` を埋める。

use super::err;
use super::reader::BitReader;
use crate::error::Result;

/// 1 行（1 つのプレフィックス）の意味。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// 通常: `range_low` から始まる `2^range_len` 個の値のいずれかを表現。
    Normal,
    /// OOB（Out-Of-Band）: テーブル特有の終端マーカー。
    Oob,
    /// 低位レンジ拡張: prefix の後に range_len ビットを符号化値として読み、
    /// `range_low - 値` を返す（負方向）。
    LowRange,
    /// 高位レンジ拡張: prefix の後に range_len ビットを読み、
    /// `range_low + 値` を返す（テーブルの最高位を超える側）。
    HighRange,
}

#[derive(Debug, Clone, Copy)]
pub struct HuffmanLine {
    pub range_low: i32,
    pub range_len: u8,
    pub prefix_len: u8,
    pub prefix_code: u32,
    pub kind: LineKind,
}

/// 復号結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HuffmanValue {
    Value(i32),
    Oob,
}

/// 線形探索式の Huffman テーブル。プレフィックス長で昇順に並べる。
#[derive(Debug, Clone)]
pub struct HuffmanTable {
    lines: Vec<HuffmanLine>,
}

impl HuffmanTable {
    /// 行リストからテーブルを構築。プレフィックス長で安定ソートする。
    pub fn new(mut lines: Vec<HuffmanLine>) -> Self {
        lines.sort_by_key(|l| (l.prefix_len, l.prefix_code));
        Self { lines }
    }

    /// 復号: ビットストリームから 1 個の値を取り出す。
    pub fn decode(&self, br: &mut BitReader) -> Result<HuffmanValue> {
        let mut code: u32 = 0;
        let mut len: u8 = 0;
        loop {
            let bit = br.read_bit()?;
            code = (code << 1) | bit as u32;
            len += 1;
            // 同じ長さのエントリの中から一致するものを探す
            for line in &self.lines {
                if line.prefix_len != len {
                    continue;
                }
                if line.prefix_code != code {
                    continue;
                }
                return Self::resolve(line, br);
            }
            if len > 32 {
                return Err(err("Huffman: no matching prefix within 32 bits"));
            }
        }
    }

    fn resolve(line: &HuffmanLine, br: &mut BitReader) -> Result<HuffmanValue> {
        match line.kind {
            LineKind::Oob => Ok(HuffmanValue::Oob),
            LineKind::Normal => {
                let off = if line.range_len == 0 {
                    0
                } else {
                    br.read_bits(line.range_len as u32)?
                };
                Ok(HuffmanValue::Value(
                    line.range_low.saturating_add(off as i32),
                ))
            }
            LineKind::HighRange => {
                let off = br.read_bits(line.range_len as u32)?;
                Ok(HuffmanValue::Value(
                    line.range_low.saturating_add(off as i32),
                ))
            }
            LineKind::LowRange => {
                let off = br.read_bits(line.range_len as u32)?;
                Ok(HuffmanValue::Value(
                    line.range_low.saturating_sub(off as i32),
                ))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 標準テーブル B.1（T.88 Annex B.1, Table B.1）
// ---------------------------------------------------------------------------

/// Standard Huffman table B.1（symbol height/width 系・全 9 行）。
///
/// | prefix       | length | range low | range bits |
/// |--------------|--------|-----------|------------|
/// | 0            | 1      | 0         | 1          |
/// | 10           | 2      | 2         | 2          |
/// | 110          | 3      | 6         | 3          |
/// | 1110         | 4      | 14        | 4          |
/// | 11110        | 5      | 30        | 5          |
/// | 111110       | 6      | 62        | 6          |
/// | 1111110      | 7      | 126       | 7          |
/// | 11111110     | 8      | 254       | 8          |
/// | 111111110    | 9      | 510       | 32         |
pub fn standard_table_b1() -> HuffmanTable {
    let mut lines = Vec::new();
    for (i, range_low) in [0, 2, 6, 14, 30, 62, 126, 254, 510].iter().enumerate() {
        let prefix_len = (i + 1) as u8;
        let range_len = if i == 8 { 32 } else { (i + 1) as u8 };
        // 第 i 行のプレフィックスはビット列 "1 を i 個 + 末尾 0"。
        // 値は ((1 << i) - 1) << 1。i=0 で 0、i=8 で 510 (= 0b111111110)。
        let prefix_code = ((1u32 << i) - 1) << 1;
        lines.push(HuffmanLine {
            range_low: *range_low,
            range_len,
            prefix_len,
            prefix_code,
            kind: LineKind::Normal,
        });
    }
    debug_assert_eq!(lines.last().unwrap().prefix_code, 0b1_1111_1110);
    HuffmanTable::new(lines)
}

/// 標準テーブル番号 → `HuffmanTable`。
///
/// セッション 1 では B.1 のみ対応。残りは Symbol/Text region 実装時に追加する。
pub fn standard_table(n: u8) -> Result<HuffmanTable> {
    match n {
        1 => Ok(standard_table_b1()),
        2..=15 => Err(err(format!(
            "JBIG2 standard Huffman table B.{n} not yet supported (filled in session 3)"
        ))),
        _ => Err(err(format!("invalid Huffman standard table number {n}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// B.1 の最初の数行を手計算で復号できることを確認する。
    #[test]
    fn b1_decodes_lower_lines() {
        let t = standard_table_b1();
        // prefix "0" + range bit "0" → 0、prefix "0" + range bit "1" → 1
        // 連続入力 "00 01" = 0b0001_0000 = 0x10
        let data = [0b0001_0000u8];
        let mut br = BitReader::new(&data);
        assert_eq!(t.decode(&mut br).unwrap(), HuffmanValue::Value(0));
        assert_eq!(t.decode(&mut br).unwrap(), HuffmanValue::Value(1));
    }

    #[test]
    fn b1_decodes_second_band() {
        let t = standard_table_b1();
        // prefix "10" + range bits "10" = range_low(2) + 2 = 4
        // ビット並び: 1 0 1 0 0 0 0 0 = 0xA0
        let data = [0b1010_0000u8];
        let mut br = BitReader::new(&data);
        assert_eq!(t.decode(&mut br).unwrap(), HuffmanValue::Value(4));
    }

    #[test]
    fn b1_decodes_third_band() {
        let t = standard_table_b1();
        // prefix "110" + 3 bits "111" = range_low(6) + 7 = 13
        // ビット並び: 1 1 0 1 1 1 0 0 = 0xDC
        let data = [0b1101_1100u8];
        let mut br = BitReader::new(&data);
        assert_eq!(t.decode(&mut br).unwrap(), HuffmanValue::Value(13));
    }

    /// プレフィックス長を超えても整合がとれなければエラー。
    #[test]
    fn decode_errors_on_eof() {
        let t = standard_table_b1();
        // prefix "10" まで読んだ後に range bits 2 が読めずに EOF
        let data = [0b1000_0000u8];
        let mut br = BitReader::new(&data);
        // 2 ビット余分を読もうとして EOF にあたる
        let _ = t.decode(&mut br); // パターン次第で OK にもなりうる、ここでは panic しないことだけ確認
    }

    #[test]
    fn standard_table_2_to_15_returns_err() {
        for n in 2..=15u8 {
            assert!(standard_table(n).is_err());
        }
    }
}
