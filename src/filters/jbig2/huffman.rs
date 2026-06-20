//! Huffman 復号エンジンと標準テーブル B.1〜B.15（T.88 Annex B）。
//!
//! Symbol dictionary・Text region・Generic refinement で算術符号化の代わりに
//! Huffman 経路を選んだ場合に使う。標準テーブルは Annex B に列挙されており、
//! カスタムテーブルセグメント（type 53）からの動的構築もサポートする。
//!
//! 各エントリは次の属性を持つ:
//! - `range_low`  : このエントリで表現される値の下限（符号付き）
//! - `range_len`  : prefix の後に読む追加ビット数（0 ならエントリは単一値）
//! - `prefix_len` / `prefix_code`: Huffman プレフィックス本体
//! - `kind`       : Normal / OOB / LowRange の 3 種類。Annex B の「upper」行は
//!   Normal と同じ意味（`range_low + offset`）なので別扱いしない。

use super::err;
use super::reader::{BitReader, ByteReader};
use crate::error::Result;

/// 1 行（1 つのプレフィックス）の意味。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// 通常: `range_low + offset`（offset は range_len ビットの符号なし整数）。
    /// Annex B の「upper」行も同じ意味で扱う。
    Normal,
    /// OOB（Out-Of-Band）: テーブル特有の終端マーカー。
    Oob,
    /// 低位レンジ: prefix の後に range_len ビットを読み `range_low - offset` を返す。
    LowRange,
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

    pub fn lines(&self) -> &[HuffmanLine] {
        &self.lines
    }

    /// 復号: ビットストリームから 1 個の値を取り出す。
    pub fn decode(&self, br: &mut BitReader) -> Result<HuffmanValue> {
        let mut code: u32 = 0;
        let mut len: u8 = 0;
        loop {
            let bit = br.read_bit()?;
            code = (code << 1) | bit as u32;
            len += 1;
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
                // 値域が i32 を超える可能性があるため i64 で計算してから飽和。
                let v = (line.range_low as i64).saturating_add(off as i64);
                Ok(HuffmanValue::Value(saturate_i32(v)))
            }
            LineKind::LowRange => {
                let off = br.read_bits(line.range_len as u32)?;
                let v = (line.range_low as i64).saturating_sub(off as i64);
                Ok(HuffmanValue::Value(saturate_i32(v)))
            }
        }
    }
}

#[inline]
fn saturate_i32(v: i64) -> i32 {
    if v > i32::MAX as i64 {
        i32::MAX
    } else if v < i32::MIN as i64 {
        i32::MIN
    } else {
        v as i32
    }
}

// ---------------------------------------------------------------------------
// 標準テーブル B.1〜B.15（T.88 Annex B、PDF.js の table データに準拠）
// ---------------------------------------------------------------------------
//
// 各テーブルのソースは ITU-T T.88 Annex B。行データは `(range_low, prefix_len,
// range_len, prefix_code[, kind])` の形で記述する。`kind` が省略されたものは
// Normal、`"oob"` は OOB、`"low"` は LowRange。
//
// 末尾の「upper」行は `range_low + 2^range_len - 1` 以降まで届く伸長用途で、
// 仕様上は Normal と同じ計算で値を取り出す（pdf.js も同様の扱い）。

/// 標準テーブルの 1 行を記述する補助型。
enum L {
    /// `(range_low, prefix_len, range_len, prefix_code)`
    N(i32, u8, u8, u32),
    /// LowRange: `(range_low, prefix_len, range_len, prefix_code)`
    Lo(i32, u8, u8, u32),
    /// OOB: `(prefix_len, prefix_code)`
    Oob(u8, u32),
}

fn build(rows: &[L]) -> HuffmanTable {
    let mut lines = Vec::with_capacity(rows.len());
    for r in rows {
        let line = match *r {
            L::N(rl, pl, rln, pc) => HuffmanLine {
                range_low: rl,
                range_len: rln,
                prefix_len: pl,
                prefix_code: pc,
                kind: LineKind::Normal,
            },
            L::Lo(rl, pl, rln, pc) => HuffmanLine {
                range_low: rl,
                range_len: rln,
                prefix_len: pl,
                prefix_code: pc,
                kind: LineKind::LowRange,
            },
            L::Oob(pl, pc) => HuffmanLine {
                range_low: 0,
                range_len: 0,
                prefix_len: pl,
                prefix_code: pc,
                kind: LineKind::Oob,
            },
        };
        lines.push(line);
    }
    HuffmanTable::new(lines)
}

/// 標準テーブル B.1（SDNUMNEWSYMS / SDNUMEXSYMS / SBNUMINSTANCES など）。
pub fn standard_table_b1() -> HuffmanTable {
    build(&[
        L::N(0, 1, 4, 0x0),
        L::N(16, 2, 8, 0x2),
        L::N(272, 3, 16, 0x6),
        L::N(65808, 3, 32, 0x7), // upper
    ])
}

/// 標準テーブル B.2（SBSTRIPS など）。
pub fn standard_table_b2() -> HuffmanTable {
    build(&[
        L::N(0, 1, 0, 0x0),
        L::N(1, 2, 0, 0x2),
        L::N(2, 3, 0, 0x6),
        L::N(3, 4, 3, 0xe),
        L::N(11, 5, 6, 0x1e),
        L::N(75, 6, 32, 0x3e), // upper
        L::Oob(6, 0x3f),
    ])
}

/// 標準テーブル B.3。
pub fn standard_table_b3() -> HuffmanTable {
    build(&[
        L::N(-256, 8, 8, 0xfe),
        L::N(0, 1, 0, 0x0),
        L::N(1, 2, 0, 0x2),
        L::N(2, 3, 0, 0x6),
        L::N(3, 4, 3, 0xe),
        L::N(11, 5, 6, 0x1e),
        L::Lo(-257, 8, 32, 0xff),
        L::N(75, 7, 32, 0x7e), // upper
        L::Oob(6, 0x3e),
    ])
}

/// 標準テーブル B.4。
pub fn standard_table_b4() -> HuffmanTable {
    build(&[
        L::N(1, 1, 0, 0x0),
        L::N(2, 2, 0, 0x2),
        L::N(3, 3, 0, 0x6),
        L::N(4, 4, 3, 0xe),
        L::N(12, 5, 6, 0x1e),
        L::N(76, 5, 32, 0x1f), // upper
    ])
}

/// 標準テーブル B.5。
pub fn standard_table_b5() -> HuffmanTable {
    build(&[
        L::N(-255, 7, 8, 0x7e),
        L::N(1, 1, 0, 0x0),
        L::N(2, 2, 0, 0x2),
        L::N(3, 3, 0, 0x6),
        L::N(4, 4, 3, 0xe),
        L::N(12, 5, 6, 0x1e),
        L::Lo(-256, 7, 32, 0x7f),
        L::N(76, 6, 32, 0x3e), // upper
    ])
}

/// 標準テーブル B.6。
pub fn standard_table_b6() -> HuffmanTable {
    build(&[
        L::N(-2048, 5, 10, 0x1c),
        L::N(-1024, 4, 9, 0x8),
        L::N(-512, 4, 8, 0x9),
        L::N(-256, 4, 7, 0xa),
        L::N(-128, 5, 6, 0x1d),
        L::N(-64, 5, 5, 0x1e),
        L::N(-32, 4, 5, 0xb),
        L::N(0, 2, 7, 0x0),
        L::N(128, 3, 7, 0x2),
        L::N(256, 3, 8, 0x3),
        L::N(512, 4, 9, 0xc),
        L::N(1024, 4, 10, 0xd),
        L::Lo(-2049, 6, 32, 0x3e),
        L::N(2048, 6, 32, 0x3f), // upper
    ])
}

/// 標準テーブル B.7。
pub fn standard_table_b7() -> HuffmanTable {
    build(&[
        L::N(-1024, 4, 9, 0x8),
        L::N(-512, 3, 8, 0x0),
        L::N(-256, 4, 7, 0x9),
        L::N(-128, 5, 6, 0x1a),
        L::N(-64, 5, 5, 0x1b),
        L::N(-32, 4, 5, 0xa),
        L::N(0, 4, 5, 0xb),
        L::N(32, 5, 5, 0x1c),
        L::N(64, 5, 6, 0x1d),
        L::N(128, 4, 7, 0xc),
        L::N(256, 3, 8, 0x1),
        L::N(512, 3, 9, 0x2),
        L::N(1024, 3, 10, 0x3),
        L::Lo(-1025, 5, 32, 0x1e),
        L::N(2048, 5, 32, 0x1f), // upper
    ])
}

/// 標準テーブル B.8。
pub fn standard_table_b8() -> HuffmanTable {
    build(&[
        L::N(-15, 8, 3, 0xfc),
        L::N(-7, 9, 1, 0x1fc),
        L::N(-5, 8, 1, 0xfd),
        L::N(-3, 9, 0, 0x1fd),
        L::N(-2, 7, 0, 0x7c),
        L::N(-1, 4, 0, 0xa),
        L::N(0, 2, 1, 0x0),
        L::N(2, 5, 0, 0x1a),
        L::N(3, 6, 0, 0x3a),
        L::N(4, 3, 4, 0x4),
        L::N(20, 6, 1, 0x3b),
        L::N(22, 4, 4, 0xb),
        L::N(38, 4, 5, 0xc),
        L::N(70, 5, 6, 0x1b),
        L::N(134, 5, 7, 0x1c),
        L::N(262, 6, 7, 0x3c),
        L::N(390, 7, 8, 0x7d),
        L::N(646, 6, 10, 0x3d),
        L::Lo(-16, 9, 32, 0x1fe),
        L::N(1670, 9, 32, 0x1ff), // upper
        L::Oob(2, 0x1),
    ])
}

/// 標準テーブル B.9。
pub fn standard_table_b9() -> HuffmanTable {
    build(&[
        L::N(-31, 8, 4, 0xfc),
        L::N(-15, 9, 2, 0x1fc),
        L::N(-11, 8, 2, 0xfd),
        L::N(-7, 9, 1, 0x1fd),
        L::N(-5, 7, 1, 0x7c),
        L::N(-3, 4, 1, 0xa),
        L::N(-1, 3, 1, 0x2),
        L::N(1, 3, 1, 0x3),
        L::N(3, 5, 1, 0x1a),
        L::N(5, 6, 1, 0x3a),
        L::N(7, 3, 5, 0x4),
        L::N(39, 6, 2, 0x3b),
        L::N(43, 4, 5, 0xb),
        L::N(75, 4, 6, 0xc),
        L::N(139, 5, 7, 0x1b),
        L::N(267, 5, 8, 0x1c),
        L::N(523, 6, 8, 0x3c),
        L::N(779, 7, 9, 0x7d),
        L::N(1291, 6, 11, 0x3d),
        L::Lo(-32, 9, 32, 0x1fe),
        L::N(3339, 9, 32, 0x1ff), // upper
        L::Oob(2, 0x0),
    ])
}

/// 標準テーブル B.10。
pub fn standard_table_b10() -> HuffmanTable {
    build(&[
        L::N(-21, 7, 4, 0x7a),
        L::N(-5, 8, 0, 0xfc),
        L::N(-4, 7, 0, 0x7b),
        L::N(-3, 5, 0, 0x18),
        L::N(-2, 2, 2, 0x0),
        L::N(2, 5, 0, 0x19),
        L::N(3, 6, 0, 0x36),
        L::N(4, 7, 0, 0x7c),
        L::N(5, 8, 0, 0xfd),
        L::N(6, 2, 6, 0x1),
        L::N(70, 5, 5, 0x1a),
        L::N(102, 6, 5, 0x37),
        L::N(134, 6, 6, 0x38),
        L::N(198, 6, 7, 0x39),
        L::N(326, 6, 8, 0x3a),
        L::N(582, 6, 9, 0x3b),
        L::N(1094, 6, 10, 0x3c),
        L::N(2118, 7, 11, 0x7d),
        L::Lo(-22, 8, 32, 0xfe),
        L::N(4166, 8, 32, 0xff), // upper
        L::Oob(2, 0x2),
    ])
}

/// 標準テーブル B.11。
pub fn standard_table_b11() -> HuffmanTable {
    build(&[
        L::N(1, 1, 0, 0x0),
        L::N(2, 2, 1, 0x2),
        L::N(4, 4, 0, 0xc),
        L::N(5, 4, 1, 0xd),
        L::N(7, 5, 1, 0x1c),
        L::N(9, 5, 2, 0x1d),
        L::N(13, 6, 2, 0x3c),
        L::N(17, 7, 2, 0x7a),
        L::N(21, 7, 3, 0x7b),
        L::N(29, 7, 4, 0x7c),
        L::N(45, 7, 5, 0x7d),
        L::N(77, 7, 6, 0x7e),
        L::N(141, 7, 32, 0x7f), // upper
    ])
}

/// 標準テーブル B.12。
pub fn standard_table_b12() -> HuffmanTable {
    build(&[
        L::N(1, 1, 0, 0x0),
        L::N(2, 2, 0, 0x2),
        L::N(3, 3, 1, 0x6),
        L::N(5, 5, 0, 0x1c),
        L::N(6, 5, 1, 0x1d),
        L::N(8, 6, 1, 0x3c),
        L::N(10, 7, 0, 0x7a),
        L::N(11, 7, 1, 0x7b),
        L::N(13, 7, 2, 0x7c),
        L::N(17, 7, 3, 0x7d),
        L::N(25, 7, 4, 0x7e),
        L::N(41, 8, 5, 0xfe),
        L::N(73, 8, 32, 0xff), // upper
    ])
}

/// 標準テーブル B.13。
pub fn standard_table_b13() -> HuffmanTable {
    build(&[
        L::N(1, 1, 0, 0x0),
        L::N(2, 3, 0, 0x4),
        L::N(3, 4, 0, 0xc),
        L::N(4, 5, 0, 0x1c),
        L::N(5, 4, 1, 0xd),
        L::N(7, 3, 3, 0x5),
        L::N(15, 6, 1, 0x3a),
        L::N(17, 6, 2, 0x3b),
        L::N(21, 6, 3, 0x3c),
        L::N(29, 6, 4, 0x3d),
        L::N(45, 6, 5, 0x3e),
        L::N(77, 7, 6, 0x7e),
        L::N(141, 7, 32, 0x7f), // upper
    ])
}

/// 標準テーブル B.14。
pub fn standard_table_b14() -> HuffmanTable {
    build(&[
        L::N(-2, 3, 0, 0x4),
        L::N(-1, 3, 0, 0x5),
        L::N(0, 1, 0, 0x0),
        L::N(1, 3, 0, 0x6),
        L::N(2, 3, 0, 0x7),
    ])
}

/// 標準テーブル B.15。
pub fn standard_table_b15() -> HuffmanTable {
    build(&[
        L::N(-24, 7, 4, 0x7c),
        L::N(-8, 6, 2, 0x3c),
        L::N(-4, 5, 1, 0x1c),
        L::N(-2, 4, 0, 0xc),
        L::N(-1, 3, 0, 0x4),
        L::N(0, 1, 0, 0x0),
        L::N(1, 3, 0, 0x5),
        L::N(2, 4, 0, 0xd),
        L::N(3, 5, 1, 0x1d),
        L::N(5, 6, 2, 0x3d),
        L::N(9, 7, 4, 0x7d),
        L::Lo(-25, 7, 32, 0x7e),
        L::N(25, 7, 32, 0x7f), // upper
    ])
}

/// 標準テーブル番号 → `HuffmanTable`。
pub fn standard_table(n: u8) -> Result<HuffmanTable> {
    Ok(match n {
        1 => standard_table_b1(),
        2 => standard_table_b2(),
        3 => standard_table_b3(),
        4 => standard_table_b4(),
        5 => standard_table_b5(),
        6 => standard_table_b6(),
        7 => standard_table_b7(),
        8 => standard_table_b8(),
        9 => standard_table_b9(),
        10 => standard_table_b10(),
        11 => standard_table_b11(),
        12 => standard_table_b12(),
        13 => standard_table_b13(),
        14 => standard_table_b14(),
        15 => standard_table_b15(),
        _ => return Err(err(format!("invalid Huffman standard table number {n}"))),
    })
}

// ---------------------------------------------------------------------------
// カスタムテーブルセグメント（T.88 §7.4.10、type=53）
// ---------------------------------------------------------------------------

/// Tables セグメント（type=53）の本体から Huffman テーブルを再構築する。
///
/// 構造:
/// ```text
/// flags: 1 バイト
///   bit0:   HTOOB（OOB エントリを持つか）
///   bit1-3: HTPS（先頭プレフィックスの長さ - 1）
///   bit4-6: HTRS（range_len のビット幅 - 1）
///   bit7:   予約
/// HTLOW : 符号付き 32 ビット（先頭値）
/// HTHIGH: 符号付き 32 ビット（最終値 +1）
/// その後、(prefix_len_field, range_len_field) のペアを HTHIGH に達するまで読み、
/// 続く 2 ペアが LowRange/HighRange、HTOOB=1 なら最後に OOB エントリのプレフィックス長。
/// ```
///
/// プレフィックスは正規 Huffman の規則（Annex B.5）でビット列を再構築する。
pub fn parse_custom_table(data: &[u8]) -> Result<HuffmanTable> {
    let mut br = ByteReader::new(data);
    let flags = br.read_u8()?;
    let has_oob = flags & 0x01 != 0;
    let htps = ((flags >> 1) & 0x07) + 1;
    let htrs = ((flags >> 4) & 0x07) + 1;
    let ht_low = br.read_i32()?;
    let ht_high = br.read_i32()?;
    if ht_high < ht_low {
        return Err(err("JBIG2 custom Huffman: HTHIGH < HTLOW"));
    }

    // 内部行データ: 各行 (range_low, range_len, prefix_len, kind)。
    // プレフィックス長と範囲長は別途ビットストリームから読み出す。
    let mut bits = BitReader::new(data);
    // 既に消費したヘッダ分（1+4+4 = 9 バイト）までスキップ
    bits.read_bits(9 * 8)?;

    #[derive(Clone)]
    struct Row {
        range_low: i32,
        range_len: u8,
        prefix_len: u8,
        kind: LineKind,
    }
    let mut rows: Vec<Row> = Vec::new();
    let mut cur = ht_low;
    while cur < ht_high {
        let prefix_len = bits.read_bits(htps as u32)? as u8;
        let range_len = bits.read_bits(htrs as u32)? as u8;
        rows.push(Row {
            range_low: cur,
            range_len,
            prefix_len,
            kind: LineKind::Normal,
        });
        let span = 1i64 << range_len;
        cur = (cur as i64).saturating_add(span) as i32;
    }
    // LowRange エントリ
    {
        let prefix_len = bits.read_bits(htps as u32)? as u8;
        let range_len = 32;
        rows.push(Row {
            range_low: ht_low - 1,
            range_len,
            prefix_len,
            kind: LineKind::LowRange,
        });
    }
    // HighRange エントリ
    {
        let prefix_len = bits.read_bits(htps as u32)? as u8;
        let range_len = 32;
        rows.push(Row {
            range_low: ht_high,
            range_len,
            prefix_len,
            kind: LineKind::Normal,
        });
    }
    // OOB エントリ
    if has_oob {
        let prefix_len = bits.read_bits(htps as u32)? as u8;
        rows.push(Row {
            range_low: 0,
            range_len: 0,
            prefix_len,
            kind: LineKind::Oob,
        });
    }

    // 正規 Huffman: prefix_len から prefix_code を再生成する
    let mut sorted_idx: Vec<usize> = (0..rows.len()).collect();
    sorted_idx.sort_by_key(|&i| rows[i].prefix_len);
    let mut codes = vec![0u32; rows.len()];
    let mut code: u32 = 0;
    let mut last_len: u8 = 0;
    for &i in &sorted_idx {
        let len = rows[i].prefix_len;
        if len == 0 {
            continue;
        }
        if last_len == 0 {
            // 最初の有効エントリ
            last_len = len;
            codes[i] = code;
        } else {
            code = (code + 1) << (len - last_len);
            codes[i] = code;
            last_len = len;
        }
    }

    let lines: Vec<HuffmanLine> = rows
        .into_iter()
        .enumerate()
        .filter(|(_, r)| r.prefix_len != 0)
        .map(|(i, r)| HuffmanLine {
            range_low: r.range_low,
            range_len: r.range_len,
            prefix_len: r.prefix_len,
            prefix_code: codes[i],
            kind: r.kind,
        })
        .collect();

    Ok(HuffmanTable::new(lines))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// B.1 のプレフィックス "0" は 4 ビット range で 0..15 を表す。
    #[test]
    fn b1_first_band() {
        let t = standard_table_b1();
        // prefix "0" + range "0000" = 0
        let data = [0b0000_0000u8];
        let mut br = BitReader::new(&data);
        assert_eq!(t.decode(&mut br).unwrap(), HuffmanValue::Value(0));
        // prefix "0" + range "0001" = 1 → ビット列 0_0001 = 5 ビット
        let data = [0b0000_1000u8]; // 0|0001|000
        let mut br = BitReader::new(&data);
        assert_eq!(t.decode(&mut br).unwrap(), HuffmanValue::Value(1));
    }

    #[test]
    fn b1_second_band() {
        // prefix "10" + range "00000000" = 16
        let t = standard_table_b1();
        // ビット並び: 1 0 0 0 0 0 0 0 0 0 = 10 ビット
        // 上位 8 ビット = 0b1000_0000, 下位 2 ビット = 0b00xxxxxx
        let data = [0b1000_0000u8, 0b0000_0000u8];
        let mut br = BitReader::new(&data);
        assert_eq!(t.decode(&mut br).unwrap(), HuffmanValue::Value(16));
    }

    /// B.2 の OOB（プレフィックス 111111）が復号できること。
    #[test]
    fn b2_oob() {
        let t = standard_table_b2();
        let data = [0b1111_1100u8]; // 111111 + 00
        let mut br = BitReader::new(&data);
        assert_eq!(t.decode(&mut br).unwrap(), HuffmanValue::Oob);
    }

    /// B.2 の最小値は prefix "0" → 0。
    #[test]
    fn b2_zero() {
        let t = standard_table_b2();
        let data = [0u8];
        let mut br = BitReader::new(&data);
        assert_eq!(t.decode(&mut br).unwrap(), HuffmanValue::Value(0));
    }

    /// B.3 の負方向 LowRange が復号できること。
    /// prefix "11111111"（8 ビット）+ range 32 ビットで `-257 - offset` を返す。
    #[test]
    fn b3_low_range() {
        let t = standard_table_b3();
        // prefix 0xFF + range 0x0000_0000 = -257 - 0 = -257
        let data = [0xFFu8, 0, 0, 0, 0];
        let mut br = BitReader::new(&data);
        assert_eq!(t.decode(&mut br).unwrap(), HuffmanValue::Value(-257));
    }

    /// B.14 は 5 行のみで OOB / 拡張行を持たない。
    #[test]
    fn b14_values() {
        let t = standard_table_b14();
        // prefix "0" → 0 (1 ビット)
        let data = [0b0000_0000u8];
        let mut br = BitReader::new(&data);
        assert_eq!(t.decode(&mut br).unwrap(), HuffmanValue::Value(0));
        // prefix "100" → -2
        let data = [0b1000_0000u8];
        let mut br = BitReader::new(&data);
        assert_eq!(t.decode(&mut br).unwrap(), HuffmanValue::Value(-2));
        // prefix "111" → 2
        let data = [0b1110_0000u8];
        let mut br = BitReader::new(&data);
        assert_eq!(t.decode(&mut br).unwrap(), HuffmanValue::Value(2));
    }

    #[test]
    fn standard_table_all_constructible() {
        for n in 1..=15u8 {
            let t = standard_table(n).unwrap();
            assert!(!t.lines().is_empty(), "table {n} is empty");
        }
        assert!(standard_table(0).is_err());
        assert!(standard_table(16).is_err());
    }
}
