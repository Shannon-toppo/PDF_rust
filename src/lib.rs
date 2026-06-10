//! # pdf_rust — フルスクラッチ PDF 閲覧・編集ライブラリ
//!
//! 依存クレートゼロ（標準ライブラリのみ）で PDF の読み込み・テキスト抽出・
//! 編集・保存を行うライブラリ。zlib/DEFLATE の伸長器も自前実装している。
//!
//! ## 読む
//!
//! ```no_run
//! use pdf_rust::Document;
//!
//! let doc = Document::load("input.pdf")?;
//! println!("ページ数: {}", doc.page_count());
//! println!("タイトル: {:?}", doc.title());
//! for i in 0..doc.page_count() {
//!     println!("--- page {} ---\n{}", i + 1, doc.extract_text(i)?);
//! }
//! # Ok::<(), pdf_rust::PdfError>(())
//! ```
//!
//! ## 作る・編集する
//!
//! ```no_run
//! use pdf_rust::{Document, TextOptions, StandardFont};
//!
//! let mut doc = Document::new();
//! doc.add_page(595.0, 842.0)?; // A4
//! doc.add_text(0, "Hello, PDF!", &TextOptions {
//!     font: StandardFont::HelveticaBold,
//!     size: 24.0,
//!     x: 72.0,
//!     y: 770.0,
//!     ..Default::default()
//! })?;
//! doc.set_title("My First PDF")?;
//! doc.save("output.pdf")?;
//! # Ok::<(), pdf_rust::PdfError>(())
//! ```
//!
//! ## モジュール構成
//!
//! | モジュール | 役割 |
//! |---|---|
//! | [`document`] | 中心 API（読み込み・ページ操作・編集・保存） |
//! | [`object`] | PDF オブジェクトモデル（辞書・配列・ストリーム…） |
//! | [`lexer`] / [`parser`] | 字句解析・構文解析 |
//! | [`xref`] | 相互参照テーブル（古典 / xref ストリーム / 再構築） |
//! | [`filters`] | ストリームフィルタ（Flate, LZW, ASCII85, RunLength…） |
//! | [`content`] | コンテントストリームの解析・生成 |
//! | [`text`] | テキスト抽出（ToUnicode CMap 対応） |
//! | [`font`] | 標準 14 フォントのメトリクスと WinAnsi 変換 |
//! | [`writer`] | シリアライザ（保存処理の実体） |
//!
//! ## 制限事項
//!
//! - 暗号化 PDF は読めない（[`PdfError::EncryptionNotSupported`]）
//! - 保存は常に完全書き直し（増分更新・電子署名の保持は不可）
//! - テキスト抽出は ToUnicode CMap か WinAnsi 相当の単純フォントが対象。
//!   ToUnicode を持たない CID フォントや `/Differences` は近似になる
//! - 画像のデコード（DCT/JPX など）は行わない（生データの取得は可能）

pub mod content;
pub mod document;
pub mod error;
pub mod filters;
pub mod font;
pub mod lexer;
pub mod object;
pub mod parser;
pub mod subset;
pub mod text;
pub mod truetype;
pub mod writer;
pub mod xref;

pub use content::Operation;
pub use document::{
    decode_text_string, encode_text_string, Document, DrawOptions, EmbeddedFontId, TextOptions,
};
pub use error::{PdfError, Result};
pub use font::StandardFont;
pub use object::{Dictionary, Object, ObjectId, Stream, StringFormat};
pub use truetype::TrueTypeFont;
