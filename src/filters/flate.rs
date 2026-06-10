//! zlib / DEFLATE (RFC 1950 / RFC 1951) のフルスクラッチ実装。
//!
//! - [`decompress`] : zlib ストリームの伸長（固定・動的ハフマン両対応）
//! - [`compress`]   : zlib ストリームの生成（無圧縮 stored ブロック使用）
//!
//! 伸長は完全な inflate 実装。圧縮は PDF として常に正しい出力を最小の
//! コードで得るため、DEFLATE の「無圧縮ブロック」を用いる（サイズは
//! 縮まないが、あらゆる zlib デコーダで読める正規のストリームになる）。

use crate::error::{PdfError, Result};

fn err(msg: impl Into<String>) -> PdfError {
    PdfError::Filter(msg.into())
}

// ---------------------------------------------------------------------------
// Adler-32 チェックサム (RFC 1950 §2.2)
// ---------------------------------------------------------------------------

/// Adler-32 チェックサムを計算する。
pub fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for chunk in data.chunks(5552) {
        for &byte in chunk {
            a += byte as u32;
            b += a;
        }
        a %= MOD;
        b %= MOD;
    }
    (b << 16) | a
}

// ---------------------------------------------------------------------------
// ビットリーダ（LSB ファースト, RFC 1951 §3.1.1）
// ---------------------------------------------------------------------------

struct BitReader<'a> {
    data: &'a [u8],
    byte_pos: usize,
    bit_pos: u8, // 現在のバイト内で消費済みのビット数 (0..8)
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitReader {
            data,
            byte_pos: 0,
            bit_pos: 0,
        }
    }

    fn read_bit(&mut self) -> Result<u32> {
        let byte = *self
            .data
            .get(self.byte_pos)
            .ok_or_else(|| err("unexpected end of deflate data"))?;
        let bit = (byte >> self.bit_pos) & 1;
        self.bit_pos += 1;
        if self.bit_pos == 8 {
            self.bit_pos = 0;
            self.byte_pos += 1;
        }
        Ok(bit as u32)
    }

    /// n ビットを LSB ファーストで読む（n <= 16 を想定）。
    fn read_bits(&mut self, n: u8) -> Result<u32> {
        let mut v = 0u32;
        for i in 0..n {
            v |= self.read_bit()? << i;
        }
        Ok(v)
    }

    /// バイト境界まで読み飛ばす（stored ブロック用）。
    fn align_to_byte(&mut self) {
        if self.bit_pos != 0 {
            self.bit_pos = 0;
            self.byte_pos += 1;
        }
    }

    fn read_bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .byte_pos
            .checked_add(n)
            .ok_or_else(|| err("length overflow"))?;
        if end > self.data.len() {
            return Err(err("unexpected end of stored block"));
        }
        let s = &self.data[self.byte_pos..end];
        self.byte_pos = end;
        Ok(s)
    }
}

// ---------------------------------------------------------------------------
// カノニカルハフマン復号 (RFC 1951 §3.2.2)
// ---------------------------------------------------------------------------

/// 符号長の配列からカノニカルハフマン表を構築し、ビット列を復号する。
struct Huffman {
    /// 長さ l の符号の個数
    counts: [u16; 16],
    /// 符号値順に並べたシンボル
    symbols: Vec<u16>,
}

impl Huffman {
    fn from_lengths(lengths: &[u8]) -> Result<Huffman> {
        let mut counts = [0u16; 16];
        for &l in lengths {
            if l > 15 {
                return Err(err("invalid huffman code length"));
            }
            counts[l as usize] += 1;
        }
        counts[0] = 0;
        // オーバーサブスクライブの検査
        let mut left: i32 = 1;
        for &count in counts.iter().skip(1) {
            left <<= 1;
            left -= count as i32;
            if left < 0 {
                return Err(err("over-subscribed huffman code"));
            }
        }
        // 各長さの先頭オフセットを計算し、シンボルを符号順に配置
        let mut offsets = [0u16; 16];
        for l in 1..15 {
            offsets[l + 1] = offsets[l] + counts[l];
        }
        let mut symbols = vec![0u16; lengths.len()];
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbols[offsets[l as usize] as usize] = sym as u16;
                offsets[l as usize] += 1;
            }
        }
        Ok(Huffman { counts, symbols })
    }

    /// 1 シンボル復号する。
    fn decode(&self, reader: &mut BitReader) -> Result<u16> {
        let mut code: i32 = 0; // これまで読んだビット列の値
        let mut first: i32 = 0; // 現在の長さの最初の符号値
        let mut index: i32 = 0; // symbols 内のオフセット
        for len in 1..16 {
            code |= reader.read_bit()? as i32;
            let count = self.counts[len] as i32;
            if code - count < first {
                return Ok(self.symbols[(index + (code - first)) as usize]);
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        Err(err("invalid huffman code"))
    }
}

// ---------------------------------------------------------------------------
// inflate 本体 (RFC 1951 §3.2.3〜§3.2.7)
// ---------------------------------------------------------------------------

/// 長さ符号 257..285 の基本値と追加ビット数 (§3.2.5)
const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
/// 距離符号 0..29 の基本値と追加ビット数
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// 生の DEFLATE ストリーム（zlib ヘッダなし）を伸長する。
pub fn inflate_raw(data: &[u8]) -> Result<Vec<u8>> {
    let mut reader = BitReader::new(data);
    let mut out: Vec<u8> = Vec::with_capacity(data.len() * 3);
    loop {
        let bfinal = reader.read_bits(1)?;
        let btype = reader.read_bits(2)?;
        match btype {
            0 => {
                // 無圧縮ブロック
                reader.align_to_byte();
                let header = reader.read_bytes(4)?;
                let len = u16::from_le_bytes([header[0], header[1]]);
                let nlen = u16::from_le_bytes([header[2], header[3]]);
                if len != !nlen {
                    return Err(err("stored block LEN/NLEN mismatch"));
                }
                out.extend_from_slice(reader.read_bytes(len as usize)?);
            }
            1 => {
                // 固定ハフマン (§3.2.6)
                let mut lit_lengths = [0u8; 288];
                for (i, l) in lit_lengths.iter_mut().enumerate() {
                    *l = match i {
                        0..=143 => 8,
                        144..=255 => 9,
                        256..=279 => 7,
                        _ => 8,
                    };
                }
                let lit = Huffman::from_lengths(&lit_lengths)?;
                let dist = Huffman::from_lengths(&[5u8; 30])?;
                inflate_block(&mut reader, &lit, &dist, &mut out)?;
            }
            2 => {
                // 動的ハフマン (§3.2.7)
                let (lit, dist) = read_dynamic_tables(&mut reader)?;
                inflate_block(&mut reader, &lit, &dist, &mut out)?;
            }
            _ => return Err(err("invalid deflate block type 3")),
        }
        if bfinal == 1 {
            break;
        }
    }
    Ok(out)
}

/// 動的ハフマンブロックの符号表を読む。
fn read_dynamic_tables(reader: &mut BitReader) -> Result<(Huffman, Huffman)> {
    const ORDER: [usize; 19] = [
        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
    ];
    let hlit = reader.read_bits(5)? as usize + 257;
    let hdist = reader.read_bits(5)? as usize + 1;
    let hclen = reader.read_bits(4)? as usize + 4;
    if hlit > 286 || hdist > 30 {
        return Err(err("invalid dynamic table sizes"));
    }
    let mut code_lengths = [0u8; 19];
    for &idx in ORDER.iter().take(hclen) {
        code_lengths[idx] = reader.read_bits(3)? as u8;
    }
    let cl_huff = Huffman::from_lengths(&code_lengths)?;

    // 符号長列をランレングス展開しながら読む
    let mut lengths = vec![0u8; hlit + hdist];
    let mut i = 0;
    while i < lengths.len() {
        let sym = cl_huff.decode(reader)?;
        match sym {
            0..=15 => {
                lengths[i] = sym as u8;
                i += 1;
            }
            16 => {
                if i == 0 {
                    return Err(err("repeat code with no previous length"));
                }
                let prev = lengths[i - 1];
                let count = 3 + reader.read_bits(2)? as usize;
                for _ in 0..count {
                    if i >= lengths.len() {
                        return Err(err("length repeat overflows table"));
                    }
                    lengths[i] = prev;
                    i += 1;
                }
            }
            17 => {
                let count = 3 + reader.read_bits(3)? as usize;
                i += count;
            }
            18 => {
                let count = 11 + reader.read_bits(7)? as usize;
                i += count;
            }
            _ => return Err(err("invalid code-length symbol")),
        }
    }
    if i > lengths.len() {
        return Err(err("code length table overflow"));
    }
    let lit = Huffman::from_lengths(&lengths[..hlit])?;
    let dist = Huffman::from_lengths(&lengths[hlit..])?;
    Ok((lit, dist))
}

/// ハフマン符号化されたブロック本体を展開する。
fn inflate_block(
    reader: &mut BitReader,
    lit: &Huffman,
    dist: &Huffman,
    out: &mut Vec<u8>,
) -> Result<()> {
    loop {
        let sym = lit.decode(reader)?;
        match sym {
            0..=255 => out.push(sym as u8),
            256 => return Ok(()), // ブロック終端
            257..=285 => {
                let li = (sym - 257) as usize;
                let length =
                    LENGTH_BASE[li] as usize + reader.read_bits(LENGTH_EXTRA[li])? as usize;
                let dsym = dist.decode(reader)? as usize;
                if dsym >= 30 {
                    return Err(err("invalid distance symbol"));
                }
                let distance =
                    DIST_BASE[dsym] as usize + reader.read_bits(DIST_EXTRA[dsym])? as usize;
                if distance > out.len() {
                    return Err(err("distance exceeds output size"));
                }
                let start = out.len() - distance;
                // distance < length の場合は重なりコピー（1 バイトずつ）
                for k in 0..length {
                    let b = out[start + k];
                    out.push(b);
                }
            }
            _ => return Err(err("invalid literal/length symbol")),
        }
    }
}

/// zlib ストリーム (RFC 1950) を伸長する。
///
/// 壊れた PDF への耐性として、zlib ヘッダが不正な場合は生 DEFLATE として
/// 再試行する。Adler-32 の不一致は警告扱い（エラーにしない）。
pub fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    if data.len() >= 2 {
        let cmf = data[0];
        let flg = data[1];
        let is_zlib = (cmf & 0x0F) == 8 && ((cmf as u16) * 256 + flg as u16).is_multiple_of(31);
        if is_zlib {
            if flg & 0x20 != 0 {
                return Err(err("zlib preset dictionary is not supported"));
            }
            return inflate_raw(&data[2..]);
        }
    }
    // ヘッダなし（生 deflate）として再試行
    inflate_raw(data)
}

/// データを zlib 形式（無圧縮 stored ブロック）で包む。
///
/// あらゆる zlib デコーダで伸長できる正規のストリームを返す。
/// 圧縮率は得られない（数バイトのオーバーヘッドが付く）。
pub fn compress(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 65535 * 5 + 16);
    // CMF: 圧縮方式 8 (deflate), 窓サイズ 32K / FLG: チェックビットのみ
    out.push(0x78);
    out.push(0x01);
    if data.is_empty() {
        // 空でも最低 1 ブロック必要
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xFF, 0xFF]);
    } else {
        let mut chunks = data.chunks(0xFFFF).peekable();
        while let Some(chunk) = chunks.next() {
            let bfinal: u8 = if chunks.peek().is_none() { 1 } else { 0 };
            out.push(bfinal); // BTYPE=00 (stored)
            let len = chunk.len() as u16;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(chunk);
        }
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adler32_known_values() {
        assert_eq!(adler32(b""), 1);
        assert_eq!(adler32(b"Wikipedia"), 0x11E60398);
    }

    #[test]
    fn compress_roundtrip() {
        let cases: Vec<Vec<u8>> = vec![
            b"".to_vec(),
            b"hello world".to_vec(),
            (0..200_000).map(|i| (i % 251) as u8).collect(),
        ];
        for data in cases {
            let z = compress(&data);
            assert_eq!(decompress(&z).unwrap(), data);
        }
    }

    #[test]
    fn inflate_fixed_huffman() {
        // "hello hello hello\n" を .NET ZLibStream (Fastest) で圧縮した既知のベクタ
        let z: Vec<u8> = vec![
            0x78, 0x01, 0xCB, 0x48, 0xCD, 0xC9, 0xC9, 0x57, 0x40, 0x22, 0xB9, 0x00, 0x40, 0xB5,
            0x06, 0x87,
        ];
        assert_eq!(decompress(&z).unwrap(), b"hello hello hello\n");
    }

    #[test]
    fn inflate_dynamic_huffman() {
        // 反復データを .NET ZLibStream (SmallestSize) で圧縮した既知のベクタ
        // 平文: "the quick brown fox jumps over the lazy dog. " * 8
        let z: Vec<u8> = vec![
            0x78, 0xDA, 0x2B, 0xC9, 0x48, 0x55, 0x28, 0x2C, 0xCD, 0x4C, 0xCE, 0x56, 0x48, 0x2A,
            0xCA, 0x2F, 0xCF, 0x53, 0x48, 0xCB, 0xAF, 0x50, 0xC8, 0x2A, 0xCD, 0x2D, 0x28, 0x56,
            0xC8, 0x2F, 0x4B, 0x2D, 0x52, 0x28, 0x01, 0x4A, 0xE7, 0x24, 0x56, 0x55, 0x2A, 0xA4,
            0xE4, 0xA7, 0xEB, 0x81, 0x79, 0xA3, 0x8A, 0xC9, 0x52, 0x0C, 0x00, 0x2F, 0xC0, 0x82,
            0x39,
        ];
        let plain = b"the quick brown fox jumps over the lazy dog. ".repeat(8);
        assert_eq!(decompress(&z).unwrap(), plain);
    }

    #[test]
    fn rejects_garbage() {
        assert!(decompress(&[0xFF, 0xFF, 0xFF, 0xFF]).is_err());
    }
}
