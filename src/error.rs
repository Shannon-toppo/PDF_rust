//! ライブラリ共通のエラー型。

use std::fmt;

/// PDF の読み書きで発生しうるエラー。
#[derive(Debug)]
pub enum PdfError {
    /// 入出力エラー（ファイル読み書き失敗など）。
    Io(std::io::Error),
    /// 構文エラー。`offset` はファイル先頭からのバイト位置。
    Syntax { offset: usize, message: String },
    /// PDF ヘッダ（`%PDF-x.y`）が見つからない。
    NotAPdf,
    /// 相互参照テーブル（xref）が壊れていて復元もできなかった。
    BrokenXref(String),
    /// 参照先のオブジェクトが存在しない。
    MissingObject(u32, u16),
    /// 期待した型と異なるオブジェクトに遭遇した。
    TypeMismatch {
        expected: &'static str,
        found: &'static str,
    },
    /// 辞書に必須キーがない。
    MissingKey(&'static str),
    /// ストリームフィルタの伸長に失敗した。
    Filter(String),
    /// フォントファイル（TTF/TTC）の解析・埋め込みに失敗した。
    Font(String),
    /// 暗号化された PDF（本ライブラリでは未対応）。
    EncryptionNotSupported,
    /// ページ番号が範囲外。
    PageOutOfRange { index: usize, count: usize },
    /// その他の不正な操作・データ。
    Invalid(String),
}

impl fmt::Display for PdfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PdfError::Io(e) => write!(f, "I/O error: {e}"),
            PdfError::Syntax { offset, message } => {
                write!(f, "syntax error at byte {offset}: {message}")
            }
            PdfError::NotAPdf => write!(f, "not a PDF file (missing %PDF header)"),
            PdfError::BrokenXref(m) => write!(f, "broken cross-reference table: {m}"),
            PdfError::MissingObject(n, g) => write!(f, "missing object {n} {g} R"),
            PdfError::TypeMismatch { expected, found } => {
                write!(f, "type mismatch: expected {expected}, found {found}")
            }
            PdfError::MissingKey(k) => write!(f, "missing required dictionary key /{k}"),
            PdfError::Filter(m) => write!(f, "stream filter error: {m}"),
            PdfError::Font(m) => write!(f, "font error: {m}"),
            PdfError::EncryptionNotSupported => {
                write!(f, "encrypted PDF files are not supported")
            }
            PdfError::PageOutOfRange { index, count } => {
                write!(f, "page index {index} out of range (page count = {count})")
            }
            PdfError::Invalid(m) => write!(f, "invalid: {m}"),
        }
    }
}

impl std::error::Error for PdfError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PdfError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for PdfError {
    fn from(e: std::io::Error) -> Self {
        PdfError::Io(e)
    }
}

/// 本ライブラリ標準の `Result` 型。
pub type Result<T> = std::result::Result<T, PdfError>;
