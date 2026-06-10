//! PDF の中身を表示する例（簡易 pdfinfo + pdftotext）。
//!
//! 実行: `cargo run --example inspect -- path/to/file.pdf`

use pdf_rust::Document;

fn main() {
    let path = match std::env::args().nth(1) {
        Some(p) => p,
        None => {
            eprintln!("usage: inspect <file.pdf>");
            std::process::exit(2);
        }
    };
    let doc = match Document::load(&path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("読み込み失敗: {e}");
            std::process::exit(1);
        }
    };

    println!("=== {path} ===");
    println!("PDF バージョン : {}", doc.version);
    println!("オブジェクト数 : {}", doc.objects.len());
    println!("ページ数       : {}", doc.page_count());
    for key in [
        "Title",
        "Author",
        "Subject",
        "Creator",
        "Producer",
        "CreationDate",
    ] {
        if let Some(v) = doc.info_text(key) {
            println!("{key:<14} : {v}");
        }
    }

    for i in 0..doc.page_count() {
        let id = doc.page_id(i).unwrap();
        let mb = doc.page_media_box(id);
        println!(
            "\n--- ページ {} ({} x {} pt) ---",
            i + 1,
            mb[2] - mb[0],
            mb[3] - mb[1]
        );
        match doc.extract_text(i) {
            Ok(text) if text.is_empty() => println!("(テキストなし)"),
            Ok(text) => println!("{text}"),
            Err(e) => println!("(抽出失敗: {e})"),
        }
    }
}
