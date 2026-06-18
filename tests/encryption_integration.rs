//! 暗号化 PDF（V1/V2/V4/V5）の読み込み統合テスト。
//!
//! フィクスチャは `tests/fixtures/encrypted_*.pdf`（PyMuPDF で生成。
//! 再生成スクリプトは `tests/fixtures/gen_encrypted_pdfs.py`）。
//! ユーザーパスワード = 空、オーナーパスワード = "owner-secret"。
//! 期待テキスト: "Hello, Encrypted PDF!"

use pdf_rust::Document;

const EXPECTED: &str = "Hello, Encrypted PDF!";

fn load_fixture(name: &str) -> Vec<u8> {
    let path = format!("tests/fixtures/{name}");
    std::fs::read(&path).unwrap_or_else(|_| panic!("missing fixture {path}"))
}

fn assert_decrypted(name: &str) {
    let bytes = load_fixture(name);
    let doc = Document::from_bytes(&bytes).unwrap_or_else(|e| panic!("failed to load {name}: {e}"));
    assert_eq!(doc.page_count(), 1, "{name}: page count");
    let text = doc
        .extract_text(0)
        .unwrap_or_else(|e| panic!("failed to extract text from {name}: {e}"));
    assert!(
        text.contains(EXPECTED),
        "{name}: expected text not found.\n  got: {text:?}"
    );
}

/// 平文版は対照として正しく読めるはず。
#[test]
fn plain_fixture_baseline() {
    assert_decrypted("encrypted_plain.pdf");
}

/// V1 / R2: RC4-40bit、空ユーザーパスワード。
#[test]
fn decrypts_rc4_40() {
    assert_decrypted("encrypted_rc4_40.pdf");
}

/// V2 / R3: RC4-128bit、空ユーザーパスワード。
#[test]
fn decrypts_rc4_128() {
    assert_decrypted("encrypted_rc4_128.pdf");
}

/// V4 / R4 AESV2: AES-128、空ユーザーパスワード。
#[test]
fn decrypts_aes_128() {
    assert_decrypted("encrypted_aes_128.pdf");
}

/// V5 / R6 AESV3: AES-256、空ユーザーパスワード（PDF 2.0）。
#[test]
fn decrypts_aes_256() {
    assert_decrypted("encrypted_aes_256.pdf");
}

/// オーナーパスワードでも開けるはず（ユーザーパスワード認証経由）。
#[test]
fn opens_with_owner_password() {
    let bytes = load_fixture("encrypted_aes_128.pdf");
    let doc = Document::from_bytes_with_password(&bytes, b"owner-secret").expect("owner password");
    let text = doc.extract_text(0).unwrap();
    assert!(text.contains(EXPECTED));
}

/// 暗号化 PDF を読み込んで再保存すると平文として書き出される（/Encrypt が外れる）。
#[test]
fn saving_loaded_encrypted_pdf_outputs_plaintext() {
    let bytes = load_fixture("encrypted_aes_128.pdf");
    let mut doc = Document::from_bytes(&bytes).unwrap();
    let saved = doc.to_bytes().unwrap();
    // 出力には Encrypt キーが trailer に含まれない（実装は trailer から除去）。
    let trailer_idx = saved
        .windows(7)
        .rposition(|w| w == b"trailer")
        .expect("trailer keyword");
    let trailer = &saved[trailer_idx..];
    assert!(
        !trailer.windows(8).any(|w| w == b"/Encrypt"),
        "trailer still references /Encrypt"
    );
    // 平文化されたバイト列を再読込しても本文が取れる。
    let reloaded = Document::from_bytes(&saved).unwrap();
    assert!(reloaded.extract_text(0).unwrap().contains(EXPECTED));
}
