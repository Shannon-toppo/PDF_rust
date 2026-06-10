//! コンテントストリーム（ページ記述）の解析と生成。
//!
//! コンテントストリームは「オペランド列 + 演算子」の繰り返しで構成される
//! （§7.8.2）。本モジュールはこれを [`Operation`] の列に分解する。
//! インライン画像（`BI ... ID <バイナリ> EI`）はデータ部をスキップして
//! 1 つの `BI` 演算として返す。

use crate::error::{PdfError, Result};
use crate::lexer::Token;
use crate::object::Object;
use crate::parser::Parser;

/// コンテントストリーム中の演算 1 つ。
#[derive(Debug, Clone, PartialEq)]
pub struct Operation {
    /// 演算子（`Tj` `re` `cm` など）。
    pub operator: String,
    /// オペランド（演算子の前に置かれた値）。
    pub operands: Vec<Object>,
}

impl Operation {
    /// 演算を作る補助関数。
    pub fn new(operator: impl Into<String>, operands: Vec<Object>) -> Self {
        Operation {
            operator: operator.into(),
            operands,
        }
    }
}

/// コンテントストリームを解析して演算列を返す。
///
/// 多少の構文エラー（不明な演算子など）は読み飛ばして継続する。
pub fn parse_content(data: &[u8]) -> Result<Vec<Operation>> {
    let mut parser = Parser::new_at(data, 0);
    let mut ops = Vec::new();
    let mut operands: Vec<Object> = Vec::new();
    loop {
        let token = match parser.lexer.next_token() {
            Ok(t) => t,
            Err(_) => continue, // 不正バイトは読み飛ばす
        };
        match token {
            Token::Eof => break,
            Token::Keyword(kw) => match kw.as_str() {
                "true" => operands.push(Object::Boolean(true)),
                "false" => operands.push(Object::Boolean(false)),
                "null" => operands.push(Object::Null),
                "BI" => {
                    // インライン画像: 辞書部を読み、ID の後のバイナリを EI までスキップ
                    let dict_ops = parse_inline_image(&mut parser)?;
                    ops.push(Operation::new("BI", dict_ops));
                    operands.clear();
                }
                _ => {
                    ops.push(Operation::new(kw, std::mem::take(&mut operands)));
                }
            },
            t => {
                // 値はオペランドとして積む
                match parser.parse_object_from(t) {
                    Ok(o) => operands.push(o),
                    Err(_) => operands.clear(),
                }
            }
        }
    }
    Ok(ops)
}

/// `BI` 消費済みの状態からインライン画像を読み飛ばす。
/// 戻り値は画像辞書のキーと値を交互に並べたオペランド列。
fn parse_inline_image(parser: &mut Parser) -> Result<Vec<Object>> {
    let mut kv = Vec::new();
    loop {
        match parser.lexer.next_token()? {
            Token::Keyword(ref k) if k == "ID" => break,
            Token::Eof => {
                return Err(PdfError::Syntax {
                    offset: parser.pos(),
                    message: "unterminated inline image".into(),
                })
            }
            t => kv.push(parser.parse_object_from(t)?),
        }
    }
    // ID の直後 1 バイトの空白を挟んでバイナリデータが始まる
    let data = parser.lexer.data;
    let mut p = parser.pos();
    if p < data.len() && crate::lexer::is_whitespace(data[p]) {
        p += 1;
    }
    // 空白に挟まれた "EI" を探す
    while p + 2 <= data.len() {
        if &data[p..p + 2] == b"EI"
            && (p == 0 || crate::lexer::is_whitespace(data[p - 1]))
            && (p + 2 == data.len() || !crate::lexer::is_regular(data[p + 2]))
        {
            parser.lexer.pos = p + 2;
            return Ok(kv);
        }
        p += 1;
    }
    Err(PdfError::Syntax {
        offset: parser.pos(),
        message: "missing EI for inline image".into(),
    })
}

/// 演算列をコンテントストリームのバイト列に直列化する。
pub fn write_content(ops: &[Operation]) -> Vec<u8> {
    let mut out = Vec::new();
    for op in ops {
        for operand in &op.operands {
            crate::writer::write_object(&mut out, operand);
            out.push(b' ');
        }
        out.extend_from_slice(op.operator.as_bytes());
        out.push(b'\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_content() {
        let src = b"BT /F1 12 Tf 72 700 Td (Hello) Tj ET 0 0 100 50 re S";
        let ops = parse_content(src).unwrap();
        let names: Vec<&str> = ops.iter().map(|o| o.operator.as_str()).collect();
        assert_eq!(names, vec!["BT", "Tf", "Td", "Tj", "ET", "re", "S"]);
        assert_eq!(ops[3].operands[0].as_string().unwrap(), b"Hello");
        assert_eq!(ops[5].operands.len(), 4);
    }

    #[test]
    fn skips_inline_image() {
        let src = b"q BI /W 2 /H 2 /BPC 8 /CS /G ID \x00\x01\x02\x03 EI Q (x) Tj";
        let ops = parse_content(src).unwrap();
        let names: Vec<&str> = ops.iter().map(|o| o.operator.as_str()).collect();
        assert_eq!(names, vec!["q", "BI", "Q", "Tj"]);
    }

    #[test]
    fn content_roundtrip() {
        let ops = vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec![Object::name("F1"), 12.into()]),
            Operation::new("Td", vec![72.into(), 700.into()]),
            Operation::new("Tj", vec![Object::string_literal("Hi")]),
            Operation::new("ET", vec![]),
        ];
        let bytes = write_content(&ops);
        assert_eq!(parse_content(&bytes).unwrap(), ops);
    }
}
