//! バイト／ビット読み出しユーティリティ。
//!
//! - [`ByteReader`][] : 大エンディアン整数とスライスの安全な切り出し。セグメントヘッダ／
//!   セグメントデータの構造化パースで使う。
//! - [`BitReader`][] : MSB ファーストのビット読み出し。Huffman 復号や Refinement の
//!   一部で使う。
//!
//! いずれも `data.get(..)` と checked 演算のみで実装し、不正入力でも panic しない。

use super::err;
use crate::error::Result;

/// バイト単位の読み出しカーソル。
pub struct ByteReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> ByteReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }
    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }
    pub fn is_eof(&self) -> bool {
        self.pos >= self.data.len()
    }

    pub fn set_pos(&mut self, pos: usize) -> Result<()> {
        if pos > self.data.len() {
            return Err(err("byte reader: set_pos past end"));
        }
        self.pos = pos;
        Ok(())
    }

    pub fn skip(&mut self, n: usize) -> Result<()> {
        let np = self
            .pos
            .checked_add(n)
            .ok_or_else(|| err("byte reader: overflow on skip"))?;
        if np > self.data.len() {
            return Err(err("byte reader: skip past end"));
        }
        self.pos = np;
        Ok(())
    }

    pub fn slice(&mut self, n: usize) -> Result<&'a [u8]> {
        let np = self
            .pos
            .checked_add(n)
            .ok_or_else(|| err("byte reader: overflow on slice"))?;
        if np > self.data.len() {
            return Err(err("byte reader: slice past end"));
        }
        let s = &self.data[self.pos..np];
        self.pos = np;
        Ok(s)
    }

    /// 残りすべて。
    pub fn remaining_slice(&self) -> &'a [u8] {
        &self.data[self.pos..]
    }

    pub fn read_u8(&mut self) -> Result<u8> {
        let v = *self
            .data
            .get(self.pos)
            .ok_or_else(|| err("byte reader: EOF on u8"))?;
        self.pos += 1;
        Ok(v)
    }

    pub fn read_u16(&mut self) -> Result<u16> {
        let b = self.slice(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    pub fn read_u32(&mut self) -> Result<u32> {
        let b = self.slice(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn read_i32(&mut self) -> Result<i32> {
        Ok(self.read_u32()? as i32)
    }
}

/// MSB ファーストのビット読み出し。バイト境界アライン操作付き。
pub struct BitReader<'a> {
    data: &'a [u8],
    byte_pos: usize,
    /// 次に読むビットの位置（7 = MSB, 0 = LSB）
    bit_pos: i8,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_pos: 0,
            bit_pos: 7,
        }
    }

    pub fn byte_pos(&self) -> usize {
        self.byte_pos
    }

    pub fn is_eof(&self) -> bool {
        self.byte_pos >= self.data.len()
    }

    /// 1 ビット読む（EOF 時はエラー）。
    pub fn read_bit(&mut self) -> Result<u8> {
        let b = *self
            .data
            .get(self.byte_pos)
            .ok_or_else(|| err("bit reader: EOF"))?;
        let bit = (b >> self.bit_pos as u32) & 1;
        if self.bit_pos == 0 {
            self.byte_pos += 1;
            self.bit_pos = 7;
        } else {
            self.bit_pos -= 1;
        }
        Ok(bit)
    }

    /// `n` ビット読み出し、MSB → LSB の順に値へ詰める（最大 32 ビット）。
    pub fn read_bits(&mut self, n: u32) -> Result<u32> {
        if n > 32 {
            return Err(err("bit reader: read_bits > 32"));
        }
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.read_bit()? as u32;
        }
        Ok(v)
    }

    /// バイト境界へ進める。次の `read_bit` は次バイトの MSB から。
    pub fn align_to_byte(&mut self) {
        if self.bit_pos != 7 {
            self.byte_pos += 1;
            self.bit_pos = 7;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_reader_be_integers() {
        let mut r = ByteReader::new(&[0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0]);
        assert_eq!(r.read_u8().unwrap(), 0x12);
        assert_eq!(r.read_u16().unwrap(), 0x3456);
        assert_eq!(r.read_u32().unwrap(), 0x789A_BCDE);
        assert_eq!(r.remaining(), 1);
        assert_eq!(r.read_u8().unwrap(), 0xF0);
        assert!(r.is_eof());
        assert!(r.read_u8().is_err());
    }

    #[test]
    fn byte_reader_slice() {
        let mut r = ByteReader::new(&[1, 2, 3, 4, 5]);
        let s = r.slice(3).unwrap();
        assert_eq!(s, &[1, 2, 3]);
        assert_eq!(r.remaining(), 2);
        assert!(r.slice(10).is_err());
    }

    #[test]
    fn bit_reader_msb_first() {
        // 0b1011_0100 = 0xB4
        let mut r = BitReader::new(&[0xB4]);
        assert_eq!(r.read_bit().unwrap(), 1);
        assert_eq!(r.read_bit().unwrap(), 0);
        assert_eq!(r.read_bit().unwrap(), 1);
        assert_eq!(r.read_bit().unwrap(), 1);
        assert_eq!(r.read_bits(4).unwrap(), 0b0100);
    }

    #[test]
    fn bit_reader_align() {
        let mut r = BitReader::new(&[0xFF, 0x00]);
        let _ = r.read_bits(3).unwrap(); // byte 0 を 3 ビット消費（bit_pos=4）
        r.align_to_byte(); // byte_pos=1, bit_pos=7
        assert_eq!(r.read_bit().unwrap(), 0); // byte 1 の MSB
        assert_eq!(r.byte_pos(), 1); // 残り 7 ビットあるので byte_pos はまだ 1
    }

    #[test]
    fn bit_reader_eof() {
        let mut r = BitReader::new(&[0x80]);
        let _ = r.read_bits(8).unwrap();
        assert!(r.read_bit().is_err());
    }
}
