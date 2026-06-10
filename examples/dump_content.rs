//! デバッグ用: ページのコンテントストリームを伸長してそのまま表示する。
//!
//! 実行: `cargo run --example dump_content -- file.pdf [page]`

use pdf_rust::Document;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: dump_content <file.pdf> [page]");
    let page: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let doc = Document::load(&path).expect("load failed");
    let id = doc.page_id(page).expect("page out of range");
    let content = doc.page_content_bytes(id).expect("content failed");
    println!("{}", String::from_utf8_lossy(&content));
}
