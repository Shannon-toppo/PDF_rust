//! 既存 PDF を編集する例: 全ページにスタンプ（注釈テキスト）を押し、
//! タイトルを書き換えて別名保存する。
//!
//! 実行: `cargo run --example edit_pdf -- input.pdf output.pdf`

use pdf_rust::{Document, Result, StandardFont, TextOptions};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let (input, output) = match (args.next(), args.next()) {
        (Some(i), Some(o)) => (i, o),
        _ => {
            eprintln!("usage: edit_pdf <input.pdf> <output.pdf>");
            std::process::exit(2);
        }
    };

    let mut doc = Document::load(&input)?;
    println!("{input} を読み込みました（{} ページ）", doc.page_count());

    // 全ページの左下にスタンプを追加
    for i in 0..doc.page_count() {
        doc.add_text(
            i,
            &format!("REVIEWED by pdf_rust - page {}", i + 1),
            &TextOptions {
                font: StandardFont::CourierBold,
                size: 10.0,
                x: 36.0,
                y: 24.0,
                color: (0.8, 0.0, 0.0),
                ..Default::default()
            },
        )?;
    }

    // メタデータの更新
    let old_title = doc.title().unwrap_or_default();
    doc.set_title(&format!("{old_title} (edited)"))?;
    doc.set_info_text("Producer", "pdf_rust edit_pdf example")?;

    doc.save(&output)?;
    println!("{output} に保存しました");
    Ok(())
}
