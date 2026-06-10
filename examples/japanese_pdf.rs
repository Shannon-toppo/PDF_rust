//! 日本語テキストを埋め込みフォントで描画する例。
//!
//! 実行: `cargo run --example japanese_pdf`
//! カレントディレクトリに `japanese.pdf` を出力する。
//!
//! Windows に同梱された日本語フォント（YuGothic / Meiryo / MS Gothic / BIZ UD Gothic）を
//! 優先順位で探し、最初に見つかったものを使用する。
//! フォントが見つからない場合はエラーメッセージを出力して終了する。

use pdf_rust::{Document, EmbeddedFontId, Result, TextOptions};

/// 候補フォントを順番に探し、最初に見つかったパスを返す。
fn find_jp_font() -> Option<std::path::PathBuf> {
    let candidates = [
        r"C:\Windows\Fonts\YuGothM.ttc",
        r"C:\Windows\Fonts\meiryo.ttc",
        r"C:\Windows\Fonts\msgothic.ttc",
        r"C:\Windows\Fonts\BIZ-UDGothicR.ttc",
    ];
    for path in &candidates {
        let p = std::path::PathBuf::from(path);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn main() -> Result<()> {
    // フォントを検索
    let font_path = match find_jp_font() {
        Some(p) => {
            println!("フォント: {}", p.display());
            p
        }
        None => {
            eprintln!(
                "エラー: 日本語フォントが見つかりません。\n\
                 以下のいずれかをインストールしてください:\n\
                 - C:\\Windows\\Fonts\\YuGothM.ttc\n\
                 - C:\\Windows\\Fonts\\meiryo.ttc\n\
                 - C:\\Windows\\Fonts\\msgothic.ttc\n\
                 - C:\\Windows\\Fonts\\BIZ-UDGothicR.ttc"
            );
            std::process::exit(1);
        }
    };

    let mut doc = Document::new();

    // A4 ページ（595 x 842 pt）を追加
    doc.add_page(595.0, 842.0)?;

    // フォントを読み込む（TTC の場合は ttc_index = 0 = 最初の書体）
    let font: EmbeddedFontId = doc.load_font(&font_path)?;

    // --- 見出し ---
    doc.add_text_with_font(
        0,
        "日本語PDFサンプル",
        font,
        &TextOptions {
            size: 28.0,
            x: 72.0,
            y: 760.0,
            color: (0.1, 0.1, 0.5),
            ..Default::default()
        },
    )?;

    // --- 本文（複数行・漢字・仮名・ASCII 混在） ---
    doc.add_text_with_font(
        0,
        "これは pdf_rust で生成された日本語 PDF のサンプルです。\n\
         依存クレートゼロ・フルスクラッチの Rust 実装で、\n\
         TrueType フォントの埋め込みと自動サブセット化に対応しています。\n\
         Unicode 文字 (U+0000〜U+FFFF) を Identity-H エンコーディングで\n\
         CIDFontType2 として埋め込みます。",
        font,
        &TextOptions {
            size: 14.0,
            x: 72.0,
            y: 700.0,
            ..Default::default()
        },
    )?;

    // --- 右寄せの行（text_width を使ったレイアウト例） ---
    let right_text = "右端に揃えたテキスト →";
    let page_width = 595.0_f64;
    let margin = 72.0_f64;
    let text_w = doc.text_width(font, right_text, 12.0);
    let x = page_width - margin - text_w;

    doc.add_text_with_font(
        0,
        right_text,
        font,
        &TextOptions {
            size: 12.0,
            x,
            y: 580.0,
            color: (0.5, 0.0, 0.0),
            ..Default::default()
        },
    )?;

    // --- 日本語タイトル等のメタデータ ---
    doc.set_title("日本語PDFサンプル")?;
    doc.set_info_text("Author", "pdf_rust の例")?;
    doc.set_info_text("Creator", "japanese_pdf.rs")?;
    doc.set_info_text("Subject", "TrueType フォント埋め込みデモ")?;

    // 保存
    doc.save("japanese.pdf")?;

    println!("japanese.pdf を出力しました（{} ページ）", doc.page_count());
    println!("フォントサブセットを埋め込み済みです。");
    Ok(())
}
