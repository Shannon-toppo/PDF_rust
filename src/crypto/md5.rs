//! MD5 ハッシュ（RFC 1321）。暗号学的には壊れているが、PDF
//! 標準セキュリティハンドラ（V1/V2/V4）の鍵生成に必須。

/// MD5 ダイジェスト（16 バイト）を返す。
pub fn md5(data: &[u8]) -> [u8; 16] {
    let mut s = State::new();
    s.update(data);
    s.finalize()
}

/// 複数バイト列を連結して MD5 を計算するヘルパ。
pub fn md5_many(parts: &[&[u8]]) -> [u8; 16] {
    let mut s = State::new();
    for p in parts {
        s.update(p);
    }
    s.finalize()
}

struct State {
    a: u32,
    b: u32,
    c: u32,
    d: u32,
    buf: [u8; 64],
    buf_len: usize,
    bits: u64,
}

impl State {
    fn new() -> State {
        State {
            a: 0x67452301,
            b: 0xefcdab89,
            c: 0x98badcfe,
            d: 0x10325476,
            buf: [0; 64],
            buf_len: 0,
            bits: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.bits = self.bits.wrapping_add((data.len() as u64) * 8);
        if self.buf_len > 0 {
            let take = (64 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == 64 {
                let block = self.buf;
                self.compress(&block);
                self.buf_len = 0;
            }
        }
        while data.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&data[..64]);
            self.compress(&block);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buf_len = data.len();
        }
    }

    fn finalize(mut self) -> [u8; 16] {
        let bits = self.bits;
        let mut tail = [0u8; 64 + 56];
        tail[0] = 0x80;
        let pad = if self.buf_len < 56 {
            56 - self.buf_len
        } else {
            120 - self.buf_len
        };
        tail[pad..pad + 8].copy_from_slice(&bits.to_le_bytes());
        self.update(&tail[..pad + 8]);
        let mut out = [0u8; 16];
        out[0..4].copy_from_slice(&self.a.to_le_bytes());
        out[4..8].copy_from_slice(&self.b.to_le_bytes());
        out[8..12].copy_from_slice(&self.c.to_le_bytes());
        out[12..16].copy_from_slice(&self.d.to_le_bytes());
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut x = [0u32; 16];
        for (i, w) in x.iter_mut().enumerate() {
            *w = u32::from_le_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        let (mut a, mut b, mut c, mut d) = (self.a, self.b, self.c, self.d);

        // Round 1: F(b,c,d) = (b & c) | (!b & d)
        macro_rules! r1 {
            ($a:ident,$b:ident,$c:ident,$d:ident,$k:expr,$s:expr,$ac:expr) => {
                $a = $b.wrapping_add(
                    $a.wrapping_add(($b & $c) | (!$b & $d))
                        .wrapping_add(x[$k])
                        .wrapping_add($ac)
                        .rotate_left($s),
                );
            };
        }
        r1!(a, b, c, d, 0, 7, 0xd76aa478);
        r1!(d, a, b, c, 1, 12, 0xe8c7b756);
        r1!(c, d, a, b, 2, 17, 0x242070db);
        r1!(b, c, d, a, 3, 22, 0xc1bdceee);
        r1!(a, b, c, d, 4, 7, 0xf57c0faf);
        r1!(d, a, b, c, 5, 12, 0x4787c62a);
        r1!(c, d, a, b, 6, 17, 0xa8304613);
        r1!(b, c, d, a, 7, 22, 0xfd469501);
        r1!(a, b, c, d, 8, 7, 0x698098d8);
        r1!(d, a, b, c, 9, 12, 0x8b44f7af);
        r1!(c, d, a, b, 10, 17, 0xffff5bb1);
        r1!(b, c, d, a, 11, 22, 0x895cd7be);
        r1!(a, b, c, d, 12, 7, 0x6b901122);
        r1!(d, a, b, c, 13, 12, 0xfd987193);
        r1!(c, d, a, b, 14, 17, 0xa679438e);
        r1!(b, c, d, a, 15, 22, 0x49b40821);

        // Round 2: G(b,c,d) = (b & d) | (c & !d)
        macro_rules! r2 {
            ($a:ident,$b:ident,$c:ident,$d:ident,$k:expr,$s:expr,$ac:expr) => {
                $a = $b.wrapping_add(
                    $a.wrapping_add(($b & $d) | ($c & !$d))
                        .wrapping_add(x[$k])
                        .wrapping_add($ac)
                        .rotate_left($s),
                );
            };
        }
        r2!(a, b, c, d, 1, 5, 0xf61e2562);
        r2!(d, a, b, c, 6, 9, 0xc040b340);
        r2!(c, d, a, b, 11, 14, 0x265e5a51);
        r2!(b, c, d, a, 0, 20, 0xe9b6c7aa);
        r2!(a, b, c, d, 5, 5, 0xd62f105d);
        r2!(d, a, b, c, 10, 9, 0x02441453);
        r2!(c, d, a, b, 15, 14, 0xd8a1e681);
        r2!(b, c, d, a, 4, 20, 0xe7d3fbc8);
        r2!(a, b, c, d, 9, 5, 0x21e1cde6);
        r2!(d, a, b, c, 14, 9, 0xc33707d6);
        r2!(c, d, a, b, 3, 14, 0xf4d50d87);
        r2!(b, c, d, a, 8, 20, 0x455a14ed);
        r2!(a, b, c, d, 13, 5, 0xa9e3e905);
        r2!(d, a, b, c, 2, 9, 0xfcefa3f8);
        r2!(c, d, a, b, 7, 14, 0x676f02d9);
        r2!(b, c, d, a, 12, 20, 0x8d2a4c8a);

        // Round 3: H(b,c,d) = b ^ c ^ d
        macro_rules! r3 {
            ($a:ident,$b:ident,$c:ident,$d:ident,$k:expr,$s:expr,$ac:expr) => {
                $a = $b.wrapping_add(
                    $a.wrapping_add($b ^ $c ^ $d)
                        .wrapping_add(x[$k])
                        .wrapping_add($ac)
                        .rotate_left($s),
                );
            };
        }
        r3!(a, b, c, d, 5, 4, 0xfffa3942);
        r3!(d, a, b, c, 8, 11, 0x8771f681);
        r3!(c, d, a, b, 11, 16, 0x6d9d6122);
        r3!(b, c, d, a, 14, 23, 0xfde5380c);
        r3!(a, b, c, d, 1, 4, 0xa4beea44);
        r3!(d, a, b, c, 4, 11, 0x4bdecfa9);
        r3!(c, d, a, b, 7, 16, 0xf6bb4b60);
        r3!(b, c, d, a, 10, 23, 0xbebfbc70);
        r3!(a, b, c, d, 13, 4, 0x289b7ec6);
        r3!(d, a, b, c, 0, 11, 0xeaa127fa);
        r3!(c, d, a, b, 3, 16, 0xd4ef3085);
        r3!(b, c, d, a, 6, 23, 0x04881d05);
        r3!(a, b, c, d, 9, 4, 0xd9d4d039);
        r3!(d, a, b, c, 12, 11, 0xe6db99e5);
        r3!(c, d, a, b, 15, 16, 0x1fa27cf8);
        r3!(b, c, d, a, 2, 23, 0xc4ac5665);

        // Round 4: I(b,c,d) = c ^ (b | !d)
        macro_rules! r4 {
            ($a:ident,$b:ident,$c:ident,$d:ident,$k:expr,$s:expr,$ac:expr) => {
                $a = $b.wrapping_add(
                    $a.wrapping_add($c ^ ($b | !$d))
                        .wrapping_add(x[$k])
                        .wrapping_add($ac)
                        .rotate_left($s),
                );
            };
        }
        r4!(a, b, c, d, 0, 6, 0xf4292244);
        r4!(d, a, b, c, 7, 10, 0x432aff97);
        r4!(c, d, a, b, 14, 15, 0xab9423a7);
        r4!(b, c, d, a, 5, 21, 0xfc93a039);
        r4!(a, b, c, d, 12, 6, 0x655b59c3);
        r4!(d, a, b, c, 3, 10, 0x8f0ccc92);
        r4!(c, d, a, b, 10, 15, 0xffeff47d);
        r4!(b, c, d, a, 1, 21, 0x85845dd1);
        r4!(a, b, c, d, 8, 6, 0x6fa87e4f);
        r4!(d, a, b, c, 15, 10, 0xfe2ce6e0);
        r4!(c, d, a, b, 6, 15, 0xa3014314);
        r4!(b, c, d, a, 13, 21, 0x4e0811a1);
        r4!(a, b, c, d, 4, 6, 0xf7537e82);
        r4!(d, a, b, c, 11, 10, 0xbd3af235);
        r4!(c, d, a, b, 2, 15, 0x2ad7d2bb);
        r4!(b, c, d, a, 9, 21, 0xeb86d391);

        self.a = self.a.wrapping_add(a);
        self.b = self.b.wrapping_add(b);
        self.c = self.c.wrapping_add(c);
        self.d = self.d.wrapping_add(d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        let mut s = String::new();
        for byte in b {
            s.push_str(&format!("{:02x}", byte));
        }
        s
    }

    /// RFC 1321 付録 A.5 のテストスイート。
    #[test]
    fn rfc1321_test_suite() {
        let cases = [
            ("", "d41d8cd98f00b204e9800998ecf8427e"),
            ("a", "0cc175b9c0f1b6a831c399e269772661"),
            ("abc", "900150983cd24fb0d6963f7d28e17f72"),
            ("message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
            (
                "abcdefghijklmnopqrstuvwxyz",
                "c3fcd3d76192e4007dfb496cca67e13b",
            ),
            (
                "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
                "d174ab98d277d9f5a5611c2c9f419d9f",
            ),
            (
                "12345678901234567890123456789012345678901234567890123456789012345678901234567890",
                "57edf4a22be3c955ac49da2e2107b67a",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(hex(&md5(input.as_bytes())), expected);
        }
    }

    /// `md5_many` の連結が単一呼び出しと一致する。
    #[test]
    fn many_concat_eq_single() {
        let parts: &[&[u8]] = &[b"Hello, ", b"World", b"!"];
        let joined: Vec<u8> = parts.iter().flat_map(|p| p.iter().copied()).collect();
        assert_eq!(md5_many(parts), md5(&joined));
    }
}
