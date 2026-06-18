//! 暗号プリミティブ（PDF 標準セキュリティハンドラ用）。
//!
//! - [`md5`] — RFC 1321。V1/V2/V4 の鍵生成。
//! - [`rc4`] — ARC4 ストリーム暗号。V1/V2/V4（AESV2 でない方）。
//! - [`aes`] — AES-128 / AES-256 + CBC。V4 AESV2 / V5 AESV3。
//! - [`sha2`] — SHA-256 / SHA-384 / SHA-512。V5 R6 鍵検証。
//!
//! いずれも依存ゼロで自前実装。テストは FIPS 197 / FIPS 180-4 / RFC 1321 /
//! RFC 6229 の公式テストベクタで検証する。

pub mod aes;
pub mod md5;
pub mod rc4;
pub mod sha2;
