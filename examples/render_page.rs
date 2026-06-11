//! ページを PNG にラスタライズするサンプル。
//!
//! ```powershell
//! cargo run --example render_page -- <input.pdf> <output.png> [page] [scale]
//! ```
//!
//! `page` は 0 始まり（既定 0）、`scale` は 72dpi を 1.0 とする倍率（既定 2.0）。

use pdf_rust::Document;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: render_page <input.pdf> <output.png> [page] [scale]");
        std::process::exit(2);
    }
    let page: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);
    let scale: f64 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(2.0);

    let doc = match Document::load(&args[1]) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("読み込み失敗: {e}");
            std::process::exit(1);
        }
    };
    let pm = match doc.render_page(page, scale) {
        Ok(pm) => pm,
        Err(e) => {
            eprintln!("描画失敗: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = pm.save_png(&args[2]) {
        eprintln!("保存失敗: {e}");
        std::process::exit(1);
    }
    println!(
        "{} ページ {} → {} ({}x{} px, scale {})",
        args[1],
        page,
        args[2],
        pm.width(),
        pm.height(),
        scale
    );
}
