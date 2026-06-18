//! AES-128 / AES-256（FIPS 197）と CBC モード。
//!
//! PDF V4 の AESV2（128bit）と V5 の AESV3（256bit）で使う。
//! PDF 仕様では：
//! - 暗号文の先頭 16 バイトが IV
//! - PKCS#5 / PKCS#7 パディング（V4・V5 ストリーム/文字列）
//! - V5 のファイル鍵検証では IV=0 で 1 ブロックだけ復号（パディング無し）

// shift_rows の行ローテーションは「3 要素を一括循環」する明示的な代入で
// 読みやすさを優先する（mem::swap だと 4 行に増える）。
#![allow(clippy::manual_swap)]

use crate::error::{PdfError, Result};

const NB: usize = 4; // 列数（ブロック = 16 バイト = 4 列）

/// S-box（SubBytes 用）。
const SBOX: [u8; 256] = [
    0x63, 0x7c, 0x77, 0x7b, 0xf2, 0x6b, 0x6f, 0xc5, 0x30, 0x01, 0x67, 0x2b, 0xfe, 0xd7, 0xab, 0x76,
    0xca, 0x82, 0xc9, 0x7d, 0xfa, 0x59, 0x47, 0xf0, 0xad, 0xd4, 0xa2, 0xaf, 0x9c, 0xa4, 0x72, 0xc0,
    0xb7, 0xfd, 0x93, 0x26, 0x36, 0x3f, 0xf7, 0xcc, 0x34, 0xa5, 0xe5, 0xf1, 0x71, 0xd8, 0x31, 0x15,
    0x04, 0xc7, 0x23, 0xc3, 0x18, 0x96, 0x05, 0x9a, 0x07, 0x12, 0x80, 0xe2, 0xeb, 0x27, 0xb2, 0x75,
    0x09, 0x83, 0x2c, 0x1a, 0x1b, 0x6e, 0x5a, 0xa0, 0x52, 0x3b, 0xd6, 0xb3, 0x29, 0xe3, 0x2f, 0x84,
    0x53, 0xd1, 0x00, 0xed, 0x20, 0xfc, 0xb1, 0x5b, 0x6a, 0xcb, 0xbe, 0x39, 0x4a, 0x4c, 0x58, 0xcf,
    0xd0, 0xef, 0xaa, 0xfb, 0x43, 0x4d, 0x33, 0x85, 0x45, 0xf9, 0x02, 0x7f, 0x50, 0x3c, 0x9f, 0xa8,
    0x51, 0xa3, 0x40, 0x8f, 0x92, 0x9d, 0x38, 0xf5, 0xbc, 0xb6, 0xda, 0x21, 0x10, 0xff, 0xf3, 0xd2,
    0xcd, 0x0c, 0x13, 0xec, 0x5f, 0x97, 0x44, 0x17, 0xc4, 0xa7, 0x7e, 0x3d, 0x64, 0x5d, 0x19, 0x73,
    0x60, 0x81, 0x4f, 0xdc, 0x22, 0x2a, 0x90, 0x88, 0x46, 0xee, 0xb8, 0x14, 0xde, 0x5e, 0x0b, 0xdb,
    0xe0, 0x32, 0x3a, 0x0a, 0x49, 0x06, 0x24, 0x5c, 0xc2, 0xd3, 0xac, 0x62, 0x91, 0x95, 0xe4, 0x79,
    0xe7, 0xc8, 0x37, 0x6d, 0x8d, 0xd5, 0x4e, 0xa9, 0x6c, 0x56, 0xf4, 0xea, 0x65, 0x7a, 0xae, 0x08,
    0xba, 0x78, 0x25, 0x2e, 0x1c, 0xa6, 0xb4, 0xc6, 0xe8, 0xdd, 0x74, 0x1f, 0x4b, 0xbd, 0x8b, 0x8a,
    0x70, 0x3e, 0xb5, 0x66, 0x48, 0x03, 0xf6, 0x0e, 0x61, 0x35, 0x57, 0xb9, 0x86, 0xc1, 0x1d, 0x9e,
    0xe1, 0xf8, 0x98, 0x11, 0x69, 0xd9, 0x8e, 0x94, 0x9b, 0x1e, 0x87, 0xe9, 0xce, 0x55, 0x28, 0xdf,
    0x8c, 0xa1, 0x89, 0x0d, 0xbf, 0xe6, 0x42, 0x68, 0x41, 0x99, 0x2d, 0x0f, 0xb0, 0x54, 0xbb, 0x16,
];

/// 逆 S-box。
const INV_SBOX: [u8; 256] = [
    0x52, 0x09, 0x6a, 0xd5, 0x30, 0x36, 0xa5, 0x38, 0xbf, 0x40, 0xa3, 0x9e, 0x81, 0xf3, 0xd7, 0xfb,
    0x7c, 0xe3, 0x39, 0x82, 0x9b, 0x2f, 0xff, 0x87, 0x34, 0x8e, 0x43, 0x44, 0xc4, 0xde, 0xe9, 0xcb,
    0x54, 0x7b, 0x94, 0x32, 0xa6, 0xc2, 0x23, 0x3d, 0xee, 0x4c, 0x95, 0x0b, 0x42, 0xfa, 0xc3, 0x4e,
    0x08, 0x2e, 0xa1, 0x66, 0x28, 0xd9, 0x24, 0xb2, 0x76, 0x5b, 0xa2, 0x49, 0x6d, 0x8b, 0xd1, 0x25,
    0x72, 0xf8, 0xf6, 0x64, 0x86, 0x68, 0x98, 0x16, 0xd4, 0xa4, 0x5c, 0xcc, 0x5d, 0x65, 0xb6, 0x92,
    0x6c, 0x70, 0x48, 0x50, 0xfd, 0xed, 0xb9, 0xda, 0x5e, 0x15, 0x46, 0x57, 0xa7, 0x8d, 0x9d, 0x84,
    0x90, 0xd8, 0xab, 0x00, 0x8c, 0xbc, 0xd3, 0x0a, 0xf7, 0xe4, 0x58, 0x05, 0xb8, 0xb3, 0x45, 0x06,
    0xd0, 0x2c, 0x1e, 0x8f, 0xca, 0x3f, 0x0f, 0x02, 0xc1, 0xaf, 0xbd, 0x03, 0x01, 0x13, 0x8a, 0x6b,
    0x3a, 0x91, 0x11, 0x41, 0x4f, 0x67, 0xdc, 0xea, 0x97, 0xf2, 0xcf, 0xce, 0xf0, 0xb4, 0xe6, 0x73,
    0x96, 0xac, 0x74, 0x22, 0xe7, 0xad, 0x35, 0x85, 0xe2, 0xf9, 0x37, 0xe8, 0x1c, 0x75, 0xdf, 0x6e,
    0x47, 0xf1, 0x1a, 0x71, 0x1d, 0x29, 0xc5, 0x89, 0x6f, 0xb7, 0x62, 0x0e, 0xaa, 0x18, 0xbe, 0x1b,
    0xfc, 0x56, 0x3e, 0x4b, 0xc6, 0xd2, 0x79, 0x20, 0x9a, 0xdb, 0xc0, 0xfe, 0x78, 0xcd, 0x5a, 0xf4,
    0x1f, 0xdd, 0xa8, 0x33, 0x88, 0x07, 0xc7, 0x31, 0xb1, 0x12, 0x10, 0x59, 0x27, 0x80, 0xec, 0x5f,
    0x60, 0x51, 0x7f, 0xa9, 0x19, 0xb5, 0x4a, 0x0d, 0x2d, 0xe5, 0x7a, 0x9f, 0x93, 0xc9, 0x9c, 0xef,
    0xa0, 0xe0, 0x3b, 0x4d, 0xae, 0x2a, 0xf5, 0xb0, 0xc8, 0xeb, 0xbb, 0x3c, 0x83, 0x53, 0x99, 0x61,
    0x17, 0x2b, 0x04, 0x7e, 0xba, 0x77, 0xd6, 0x26, 0xe1, 0x69, 0x14, 0x63, 0x55, 0x21, 0x0c, 0x7d,
];

/// 鍵展開で使うラウンド定数（10 ラウンド分で十分）。
const RCON: [u8; 11] = [
    0x00, 0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x1b, 0x36,
];

/// 展開済みラウンド鍵。AES-128 は 11 ラウンド、AES-256 は 15 ラウンド。
pub struct RoundKeys {
    keys: Vec<[u8; 16]>,
}

impl RoundKeys {
    /// 16 バイトまたは 32 バイトの鍵を展開する。
    pub fn expand(key: &[u8]) -> Result<RoundKeys> {
        match key.len() {
            16 => Ok(RoundKeys {
                keys: expand_128(key),
            }),
            32 => Ok(RoundKeys {
                keys: expand_256(key),
            }),
            n => Err(PdfError::Invalid(format!(
                "AES key must be 16 or 32 bytes, got {n}"
            ))),
        }
    }

    fn rounds(&self) -> usize {
        self.keys.len() - 1
    }
}

fn expand_128(key: &[u8]) -> Vec<[u8; 16]> {
    // ワード単位（4 バイト）の展開。NK=4, NR=10, NB=4 → 4*(10+1)=44 ワード。
    let nk = 4;
    let nr = 10;
    let mut w = vec![[0u8; 4]; NB * (nr + 1)];
    for i in 0..nk {
        w[i].copy_from_slice(&key[i * 4..i * 4 + 4]);
    }
    for i in nk..NB * (nr + 1) {
        let mut temp = w[i - 1];
        if i % nk == 0 {
            rot_word(&mut temp);
            sub_word(&mut temp);
            temp[0] ^= RCON[i / nk];
        }
        for j in 0..4 {
            w[i][j] = w[i - nk][j] ^ temp[j];
        }
    }
    pack_round_keys(&w, nr)
}

fn expand_256(key: &[u8]) -> Vec<[u8; 16]> {
    let nk = 8;
    let nr = 14;
    let mut w = vec![[0u8; 4]; NB * (nr + 1)];
    for i in 0..nk {
        w[i].copy_from_slice(&key[i * 4..i * 4 + 4]);
    }
    for i in nk..NB * (nr + 1) {
        let mut temp = w[i - 1];
        if i % nk == 0 {
            rot_word(&mut temp);
            sub_word(&mut temp);
            temp[0] ^= RCON[i / nk];
        } else if i % nk == 4 {
            sub_word(&mut temp);
        }
        for j in 0..4 {
            w[i][j] = w[i - nk][j] ^ temp[j];
        }
    }
    pack_round_keys(&w, nr)
}

fn pack_round_keys(w: &[[u8; 4]], nr: usize) -> Vec<[u8; 16]> {
    let mut out = Vec::with_capacity(nr + 1);
    for r in 0..=nr {
        let mut k = [0u8; 16];
        for c in 0..NB {
            k[c * 4..c * 4 + 4].copy_from_slice(&w[r * NB + c]);
        }
        out.push(k);
    }
    out
}

fn rot_word(w: &mut [u8; 4]) {
    let t = w[0];
    w[0] = w[1];
    w[1] = w[2];
    w[2] = w[3];
    w[3] = t;
}

fn sub_word(w: &mut [u8; 4]) {
    for b in w.iter_mut() {
        *b = SBOX[*b as usize];
    }
}

// ---------------------------------------------------------------------------
// 単一ブロック復号（FIPS 197 §5.3）
// ---------------------------------------------------------------------------

/// 16 バイトの単一ブロックを復号。状態は列優先で保持する。
fn decrypt_block(block: &mut [u8; 16], rk: &RoundKeys) {
    let nr = rk.rounds();
    add_round_key(block, &rk.keys[nr]);
    for round in (1..nr).rev() {
        inv_shift_rows(block);
        inv_sub_bytes(block);
        add_round_key(block, &rk.keys[round]);
        inv_mix_columns(block);
    }
    inv_shift_rows(block);
    inv_sub_bytes(block);
    add_round_key(block, &rk.keys[0]);
}

fn add_round_key(block: &mut [u8; 16], rk: &[u8; 16]) {
    for i in 0..16 {
        block[i] ^= rk[i];
    }
}

fn inv_sub_bytes(block: &mut [u8; 16]) {
    for b in block.iter_mut() {
        *b = INV_SBOX[*b as usize];
    }
}

fn inv_shift_rows(block: &mut [u8; 16]) {
    // state[row, col] = block[col * 4 + row]
    // Row r は r バイト右シフト（暗号化時の左シフトの逆）。
    // 行 1
    let t = block[13];
    block[13] = block[9];
    block[9] = block[5];
    block[5] = block[1];
    block[1] = t;
    // 行 2（入れ替え 2 ペア）
    let t = block[2];
    block[2] = block[10];
    block[10] = t;
    let t = block[6];
    block[6] = block[14];
    block[14] = t;
    // 行 3
    let t = block[3];
    block[3] = block[7];
    block[7] = block[11];
    block[11] = block[15];
    block[15] = t;
}

fn xtime(b: u8) -> u8 {
    (b << 1) ^ if b & 0x80 != 0 { 0x1b } else { 0 }
}

/// GF(2^8) の乗算（任意定数）。
fn gmul(mut a: u8, mut b: u8) -> u8 {
    let mut p: u8 = 0;
    for _ in 0..8 {
        if b & 1 != 0 {
            p ^= a;
        }
        a = xtime(a);
        b >>= 1;
    }
    p
}

fn inv_mix_columns(block: &mut [u8; 16]) {
    for c in 0..4 {
        let i = c * 4;
        let s0 = block[i];
        let s1 = block[i + 1];
        let s2 = block[i + 2];
        let s3 = block[i + 3];
        block[i] = gmul(s0, 0x0e) ^ gmul(s1, 0x0b) ^ gmul(s2, 0x0d) ^ gmul(s3, 0x09);
        block[i + 1] = gmul(s0, 0x09) ^ gmul(s1, 0x0e) ^ gmul(s2, 0x0b) ^ gmul(s3, 0x0d);
        block[i + 2] = gmul(s0, 0x0d) ^ gmul(s1, 0x09) ^ gmul(s2, 0x0e) ^ gmul(s3, 0x0b);
        block[i + 3] = gmul(s0, 0x0b) ^ gmul(s1, 0x0d) ^ gmul(s2, 0x09) ^ gmul(s3, 0x0e);
    }
}

// ---------------------------------------------------------------------------
// 暗号化（鍵検証ベクタの再計算で使うことがある）
// ---------------------------------------------------------------------------

fn encrypt_block(block: &mut [u8; 16], rk: &RoundKeys) {
    let nr = rk.rounds();
    add_round_key(block, &rk.keys[0]);
    for round in 1..nr {
        sub_bytes(block);
        shift_rows(block);
        mix_columns(block);
        add_round_key(block, &rk.keys[round]);
    }
    sub_bytes(block);
    shift_rows(block);
    add_round_key(block, &rk.keys[nr]);
}

fn sub_bytes(block: &mut [u8; 16]) {
    for b in block.iter_mut() {
        *b = SBOX[*b as usize];
    }
}

fn shift_rows(block: &mut [u8; 16]) {
    // 行 1: 左 1
    let t = block[1];
    block[1] = block[5];
    block[5] = block[9];
    block[9] = block[13];
    block[13] = t;
    // 行 2: 左 2
    let t = block[2];
    block[2] = block[10];
    block[10] = t;
    let t = block[6];
    block[6] = block[14];
    block[14] = t;
    // 行 3: 左 3（右 1 と等価）
    let t = block[15];
    block[15] = block[11];
    block[11] = block[7];
    block[7] = block[3];
    block[3] = t;
}

fn mix_columns(block: &mut [u8; 16]) {
    for c in 0..4 {
        let i = c * 4;
        let s0 = block[i];
        let s1 = block[i + 1];
        let s2 = block[i + 2];
        let s3 = block[i + 3];
        let t = s0 ^ s1 ^ s2 ^ s3;
        block[i] ^= t ^ xtime(s0 ^ s1);
        block[i + 1] ^= t ^ xtime(s1 ^ s2);
        block[i + 2] ^= t ^ xtime(s2 ^ s3);
        block[i + 3] ^= t ^ xtime(s3 ^ s0);
    }
}

// ---------------------------------------------------------------------------
// CBC モード
// ---------------------------------------------------------------------------

/// AES-CBC で復号して PKCS#5/PKCS#7 パディングを除去する。
/// 入力先頭 16 バイトを IV、残りを暗号文として扱う（PDF V4/V5 の規約）。
pub fn aes_cbc_decrypt_pkcs5(key: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    if data.len() < 16 || !data.len().is_multiple_of(16) {
        return Err(PdfError::Invalid(format!(
            "AES ciphertext length {} invalid (need IV + non-zero multiple of 16)",
            data.len()
        )));
    }
    let rk = RoundKeys::expand(key)?;
    let mut iv = [0u8; 16];
    iv.copy_from_slice(&data[..16]);
    let cipher = &data[16..];
    let mut out = Vec::with_capacity(cipher.len());
    let mut prev = iv;
    for chunk in cipher.chunks(16) {
        let mut block = [0u8; 16];
        block.copy_from_slice(chunk);
        let saved = block;
        decrypt_block(&mut block, &rk);
        for i in 0..16 {
            block[i] ^= prev[i];
        }
        out.extend_from_slice(&block);
        prev = saved;
    }
    // PKCS#5 パディング除去。
    if let Some(&pad) = out.last() {
        if pad == 0 || pad > 16 {
            // 壊れたパディングは耐故障性のためそのまま返す（PDF は耐性優先）
            return Ok(out);
        }
        let n = out.len();
        if n >= pad as usize && out[n - pad as usize..].iter().all(|&b| b == pad) {
            out.truncate(n - pad as usize);
        }
    }
    Ok(out)
}

/// IV 込みではなく、生の暗号文 1 ブロックを IV=0 で復号する。
/// PDF V5 (R6) のファイル鍵検証で使用。
pub fn aes_decrypt_block_no_iv(key: &[u8], block: &[u8; 16]) -> Result<[u8; 16]> {
    let rk = RoundKeys::expand(key)?;
    let mut b = *block;
    decrypt_block(&mut b, &rk);
    Ok(b)
}

/// CBC モードでパディング無しに復号する（V5 R6 のハッシュ計算で使う）。
/// `iv` 込みではなく、別途指定。長さは 16 の倍数でなければならない。
pub fn aes_cbc_decrypt_nopad(key: &[u8], iv: &[u8; 16], cipher: &[u8]) -> Result<Vec<u8>> {
    if !cipher.len().is_multiple_of(16) {
        return Err(PdfError::Invalid(format!(
            "AES no-pad cipher length {} not multiple of 16",
            cipher.len()
        )));
    }
    let rk = RoundKeys::expand(key)?;
    let mut prev = *iv;
    let mut out = Vec::with_capacity(cipher.len());
    for chunk in cipher.chunks(16) {
        let mut block = [0u8; 16];
        block.copy_from_slice(chunk);
        let saved = block;
        decrypt_block(&mut block, &rk);
        for i in 0..16 {
            block[i] ^= prev[i];
        }
        out.extend_from_slice(&block);
        prev = saved;
    }
    Ok(out)
}

/// CBC モードでパディング無しに暗号化する（V5 R6 の中間ハッシュ計算で使う）。
pub fn aes_cbc_encrypt_nopad(key: &[u8], iv: &[u8; 16], plain: &[u8]) -> Result<Vec<u8>> {
    if !plain.len().is_multiple_of(16) {
        return Err(PdfError::Invalid(format!(
            "AES no-pad plain length {} not multiple of 16",
            plain.len()
        )));
    }
    let rk = RoundKeys::expand(key)?;
    let mut prev = *iv;
    let mut out = Vec::with_capacity(plain.len());
    for chunk in plain.chunks(16) {
        let mut block = [0u8; 16];
        for i in 0..16 {
            block[i] = chunk[i] ^ prev[i];
        }
        encrypt_block(&mut block, &rk);
        out.extend_from_slice(&block);
        prev = block;
    }
    Ok(out)
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

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// FIPS 197 付録 B のテストベクタ（AES-128）。
    #[test]
    fn fips197_aes128_example() {
        let key = unhex("000102030405060708090a0b0c0d0e0f");
        let plain = unhex("00112233445566778899aabbccddeeff");
        let expected_cipher = unhex("69c4e0d86a7b0430d8cdb78070b4c55a");

        let rk = RoundKeys::expand(&key).unwrap();
        let mut block = [0u8; 16];
        block.copy_from_slice(&plain);
        encrypt_block(&mut block, &rk);
        assert_eq!(hex(&block), hex(&expected_cipher));

        decrypt_block(&mut block, &rk);
        assert_eq!(hex(&block), hex(&plain));
    }

    /// FIPS 197 付録 C のテストベクタ（AES-256）。
    #[test]
    fn fips197_aes256_example() {
        let key = unhex("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f");
        let plain = unhex("00112233445566778899aabbccddeeff");
        let expected_cipher = unhex("8ea2b7ca516745bfeafc49904b496089");

        let rk = RoundKeys::expand(&key).unwrap();
        let mut block = [0u8; 16];
        block.copy_from_slice(&plain);
        encrypt_block(&mut block, &rk);
        assert_eq!(hex(&block), hex(&expected_cipher));

        decrypt_block(&mut block, &rk);
        assert_eq!(hex(&block), hex(&plain));
    }

    /// CBC 自己往復: 暗号化 → 復号で元に戻ること。
    #[test]
    fn cbc_roundtrip_128() {
        let key = unhex("2b7e151628aed2a6abf7158809cf4f3c");
        let iv = [0u8; 16];
        let plain = b"PDF rust library bytes test 1234"; // 32 bytes
        let cipher = aes_cbc_encrypt_nopad(&key, &iv, plain).unwrap();
        let back = aes_cbc_decrypt_nopad(&key, &iv, &cipher).unwrap();
        assert_eq!(back, plain);
    }

    /// PKCS#5 パディング付き CBC: IV 込みデータで復号できる。
    #[test]
    fn cbc_pkcs5_roundtrip() {
        let key = unhex("2b7e151628aed2a6abf7158809cf4f3c");
        let iv = unhex("000102030405060708090a0b0c0d0e0f");

        // 自前で平文にパディングを付けて CBC 暗号化（IV を先頭に付ける）
        let plain = b"hello world"; // 11 バイト → パディング 5
        let pad = 16 - plain.len() % 16;
        let mut padded = plain.to_vec();
        padded.extend(std::iter::repeat_n(pad as u8, pad));

        let mut iv_arr = [0u8; 16];
        iv_arr.copy_from_slice(&iv);
        let cipher_body = aes_cbc_encrypt_nopad(&key, &iv_arr, &padded).unwrap();
        let mut combined = iv.clone();
        combined.extend(cipher_body);

        let back = aes_cbc_decrypt_pkcs5(&key, &combined).unwrap();
        assert_eq!(back, plain);
    }
}
