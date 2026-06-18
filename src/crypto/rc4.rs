//! RC4（ARC4）ストリーム暗号。
//!
//! PDF 標準セキュリティハンドラ V1/V2/V4（AES でない方）で使う。
//! 暗号学的に弱いが互換のために必要。

/// `key` で RC4 を初期化し、`data` を暗号化（または復号）した結果を返す。
/// RC4 は対称なので、同じ関数で暗号化・復号の両方を行える。
pub fn rc4(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut s = [0u8; 256];
    for (i, slot) in s.iter_mut().enumerate() {
        *slot = i as u8;
    }
    let klen = key.len().max(1);
    let mut j: usize = 0;
    for i in 0..256 {
        j = (j + s[i] as usize + key[i % klen] as usize) & 0xff;
        s.swap(i, j);
    }
    let mut i: usize = 0;
    let mut j: usize = 0;
    let mut out = Vec::with_capacity(data.len());
    for &b in data {
        i = (i + 1) & 0xff;
        j = (j + s[i] as usize) & 0xff;
        s.swap(i, j);
        let k = s[(s[i] as usize + s[j] as usize) & 0xff];
        out.push(b ^ k);
    }
    out
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

    /// RFC 6229 テストベクタ（抜粋）。
    #[test]
    fn rfc6229_key_40bits() {
        // Key = 0102030405, plaintext = "Plaintext" でも著名なベクタ
        // Key=Key, Plaintext=Plaintext -> "BBF316E8D940AF0AD3"
        assert_eq!(hex(&rc4(b"Key", b"Plaintext")), "bbf316e8d940af0ad3");
        // Key=Wiki, Plaintext=pedia -> "1021BF0420"
        assert_eq!(hex(&rc4(b"Wiki", b"pedia")), "1021bf0420");
        // Key=Secret, Plaintext=Attack at dawn -> "45A01F645FC35B383552544B9BF5"
        assert_eq!(
            hex(&rc4(b"Secret", b"Attack at dawn")),
            "45a01f645fc35b383552544b9bf5"
        );
    }

    #[test]
    fn rc4_is_symmetric() {
        let key = b"hello";
        let plain = b"the quick brown fox jumps over the lazy dog";
        let cipher = rc4(key, plain);
        let back = rc4(key, &cipher);
        assert_eq!(back, plain);
    }
}
