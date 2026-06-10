//! PDF 構文解析器。
//!
//! トークン列から [`Object`] を組み立てる。間接参照（`n g R`）の先読み、
//! ストリームデータの読み取り（`/Length` が間接参照の場合の解決と、
//! 壊れている場合の `endstream` スキャンによる復元）を行う。

use crate::error::{PdfError, Result};
use crate::lexer::{Lexer, Token};
use crate::object::{Dictionary, Object, ObjectId, Stream, StringFormat};

/// `/Length` の間接参照を解決するコールバック。
pub type LengthResolver<'b> = &'b dyn Fn(ObjectId) -> Option<i64>;

/// 構文解析器。
pub struct Parser<'a, 'b> {
    pub lexer: Lexer<'a>,
    /// ストリームの `/Length` が間接参照だった場合に使う解決関数。
    pub length_resolver: Option<LengthResolver<'b>>,
}

impl<'a, 'b> Parser<'a, 'b> {
    /// データの指定位置から解析するパーサを作る。
    pub fn new_at(data: &'a [u8], pos: usize) -> Self {
        Parser {
            lexer: Lexer::new_at(data, pos),
            length_resolver: None,
        }
    }

    /// 現在のバイト位置。
    pub fn pos(&self) -> usize {
        self.lexer.pos
    }

    fn syntax(&self, message: impl Into<String>) -> PdfError {
        PdfError::Syntax {
            offset: self.lexer.pos,
            message: message.into(),
        }
    }

    /// オブジェクトを 1 つ解析する。
    ///
    /// 整数の後に `整数 R` が続く場合は間接参照として解釈する。
    /// 辞書の直後に `stream` が続く場合はストリームとして読み込む。
    pub fn parse_object(&mut self) -> Result<Object> {
        let token = self.lexer.next_token()?;
        self.parse_object_from(token)
    }

    /// 既に読んだトークンを起点にオブジェクトを解析する。
    pub fn parse_object_from(&mut self, token: Token) -> Result<Object> {
        match token {
            Token::Integer(n) => {
                // `n g R` の先読み
                let save = self.lexer.pos;
                if let Some(r) = self.try_reference(n) {
                    return Ok(r);
                }
                self.lexer.pos = save;
                Ok(Object::Integer(n))
            }
            Token::Real(v) => Ok(Object::Real(v)),
            Token::LiteralString(s) => Ok(Object::String(s, StringFormat::Literal)),
            Token::HexString(s) => Ok(Object::String(s, StringFormat::Hexadecimal)),
            Token::Name(n) => Ok(Object::Name(n)),
            Token::ArrayStart => {
                let mut items = Vec::new();
                loop {
                    let t = self.lexer.next_token()?;
                    match t {
                        Token::ArrayEnd => break,
                        Token::Eof => return Err(self.syntax("unterminated array")),
                        t => items.push(self.parse_object_from(t)?),
                    }
                }
                Ok(Object::Array(items))
            }
            Token::DictStart => {
                let dict = self.parse_dict_body()?;
                // 直後に `stream` が続くか
                let save = self.lexer.pos;
                match self.lexer.next_token() {
                    Ok(Token::Keyword(ref k)) if k == "stream" => {
                        let stream = self.read_stream_data(dict)?;
                        Ok(Object::Stream(stream))
                    }
                    _ => {
                        self.lexer.pos = save;
                        Ok(Object::Dictionary(dict))
                    }
                }
            }
            Token::Keyword(k) => match k.as_str() {
                "true" => Ok(Object::Boolean(true)),
                "false" => Ok(Object::Boolean(false)),
                "null" => Ok(Object::Null),
                other => Err(self.syntax(format!("unexpected keyword '{other}'"))),
            },
            Token::ArrayEnd => Err(self.syntax("unexpected ']'")),
            Token::DictEnd => Err(self.syntax("unexpected '>>'")),
            Token::Eof => Err(self.syntax("unexpected end of file")),
        }
    }

    /// `整数 R` の続きを試し読みする。成功したら参照を返す。
    fn try_reference(&mut self, num: i64) -> Option<Object> {
        if num < 0 || num > u32::MAX as i64 {
            return None;
        }
        let gen = match self.lexer.next_token() {
            Ok(Token::Integer(g)) if (0..=u16::MAX as i64).contains(&g) => g as u16,
            _ => return None,
        };
        match self.lexer.next_token() {
            Ok(Token::Keyword(ref k)) if k == "R" => Some(Object::Reference((num as u32, gen))),
            _ => None,
        }
    }

    /// `<<` 消費済みの状態から辞書本体を解析する。
    fn parse_dict_body(&mut self) -> Result<Dictionary> {
        let mut dict = Dictionary::new();
        loop {
            match self.lexer.next_token()? {
                Token::DictEnd => break,
                Token::Name(key) => {
                    let value = self.parse_object()?;
                    dict.set(key, value);
                }
                Token::Eof => return Err(self.syntax("unterminated dictionary")),
                t => return Err(self.syntax(format!("expected name key in dictionary, got {t:?}"))),
            }
        }
        Ok(dict)
    }

    /// `stream` キーワード消費済みの状態からストリームデータを読む。
    fn read_stream_data(&mut self, dict: Dictionary) -> Result<Stream> {
        let data = self.lexer.data;
        // stream の後は CRLF または LF（§7.3.8.1）。CR 単独の壊れた PDF も許容。
        let mut p = self.lexer.pos;
        if data.get(p) == Some(&b'\r') {
            p += 1;
        }
        if data.get(p) == Some(&b'\n') {
            p += 1;
        }
        let start = p;

        // /Length の決定（直接値 or 間接参照の解決）
        let length: Option<usize> = match dict.get("Length") {
            Some(Object::Integer(n)) if *n >= 0 => Some(*n as usize),
            Some(Object::Reference(id)) => self
                .length_resolver
                .and_then(|f| f(*id))
                .and_then(|n| if n >= 0 { Some(n as usize) } else { None }),
            _ => None,
        };

        // Length が信用できるか検証しつつデータ範囲を決める
        let end = match length {
            Some(len) if start + len <= data.len() && endstream_follows(data, start + len) => {
                start + len
            }
            // Length が無い/壊れている場合は endstream を探す
            _ => scan_for_endstream(data, start)
                .ok_or_else(|| self.syntax("cannot find 'endstream'"))?,
        };

        let mut stream_dict = dict;
        stream_dict.set("Length", (end - start) as i64);
        let stream = Stream {
            dict: stream_dict,
            data: data[start..end].to_vec(),
        };

        // endstream キーワードを消費
        self.lexer.pos = end;
        match self.lexer.next_token()? {
            Token::Keyword(ref k) if k == "endstream" => {}
            t => return Err(self.syntax(format!("expected 'endstream', got {t:?}"))),
        }
        Ok(stream)
    }

    /// `n g obj ... endobj` 形式の間接オブジェクトを解析する。
    ///
    /// 戻り値は `(オブジェクト ID, 中身)`。
    pub fn parse_indirect_object(&mut self) -> Result<(ObjectId, Object)> {
        let num = match self.lexer.next_token()? {
            Token::Integer(n) if n >= 0 => n as u32,
            t => return Err(self.syntax(format!("expected object number, got {t:?}"))),
        };
        let gen = match self.lexer.next_token()? {
            Token::Integer(g) if (0..=u16::MAX as i64).contains(&g) => g as u16,
            t => return Err(self.syntax(format!("expected generation number, got {t:?}"))),
        };
        match self.lexer.next_token()? {
            Token::Keyword(ref k) if k == "obj" => {}
            t => return Err(self.syntax(format!("expected 'obj', got {t:?}"))),
        }
        let object = self.parse_object()?;
        // endobj は省略されている壊れた PDF もあるため、無くてもエラーにしない
        let save = self.lexer.pos;
        match self.lexer.next_token() {
            Ok(Token::Keyword(ref k)) if k == "endobj" => {}
            _ => self.lexer.pos = save,
        }
        Ok(((num, gen), object))
    }
}

/// `pos` 以降（空白を挟んで）`endstream` が現れるか。
fn endstream_follows(data: &[u8], mut pos: usize) -> bool {
    // 多少の空白・改行は許容
    let limit = (pos + 4).min(data.len());
    while pos < limit && crate::lexer::is_whitespace(data[pos]) {
        pos += 1;
    }
    data[pos..].starts_with(b"endstream")
}

/// `start` 以降で最初の `endstream` を探し、ストリームデータの終端位置を返す。
fn scan_for_endstream(data: &[u8], start: usize) -> Option<usize> {
    let needle = b"endstream";
    let mut i = start;
    while i + needle.len() <= data.len() {
        if &data[i..i + needle.len()] == needle {
            // 直前の EOL はストリームデータに含めない（§7.3.8.1）
            let mut end = i;
            if end > start && data[end - 1] == b'\n' {
                end -= 1;
            }
            if end > start && data[end - 1] == b'\r' {
                end -= 1;
            }
            return Some(end);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(input: &[u8]) -> Object {
        Parser::new_at(input, 0).parse_object().unwrap()
    }

    #[test]
    fn parses_reference_vs_integers() {
        assert_eq!(parse(b"12 0 R"), Object::Reference((12, 0)));
        // 配列内の数値と参照の混在
        let o = parse(b"[1 2 3 0 R 4]");
        assert_eq!(
            o,
            Object::Array(vec![
                Object::Integer(1),
                Object::Integer(2),
                Object::Reference((3, 0)),
                Object::Integer(4),
            ])
        );
    }

    #[test]
    fn parses_nested_dict() {
        let o = parse(b"<< /A << /B [1 2.5] >> /C (str) /D null >>");
        let d = o.as_dict().unwrap();
        let inner = d.get("A").unwrap().as_dict().unwrap();
        assert_eq!(inner.get("B").unwrap().as_array().unwrap().len(), 2);
        assert_eq!(d.get("C").unwrap().as_string().unwrap(), b"str");
        assert_eq!(d.get("D"), Some(&Object::Null));
    }

    #[test]
    fn parses_stream_with_direct_length() {
        let data = b"1 0 obj\n<< /Length 5 >>\nstream\nHELLO\nendstream\nendobj";
        let mut p = Parser::new_at(data, 0);
        let (id, obj) = p.parse_indirect_object().unwrap();
        assert_eq!(id, (1, 0));
        assert_eq!(obj.as_stream().unwrap().data, b"HELLO");
    }

    #[test]
    fn parses_stream_with_broken_length_by_scanning() {
        let data = b"1 0 obj << /Length 9999 >> stream\nWORLD\nendstream endobj";
        let mut p = Parser::new_at(data, 0);
        let (_, obj) = p.parse_indirect_object().unwrap();
        assert_eq!(obj.as_stream().unwrap().data, b"WORLD");
    }

    #[test]
    fn parses_stream_with_indirect_length() {
        let data = b"1 0 obj << /Length 2 0 R >> stream\nABCDE\nendstream endobj";
        let resolver = |id: ObjectId| if id == (2, 0) { Some(5i64) } else { None };
        let mut p = Parser::new_at(data, 0);
        p.length_resolver = Some(&resolver);
        let (_, obj) = p.parse_indirect_object().unwrap();
        assert_eq!(obj.as_stream().unwrap().data, b"ABCDE");
    }
}
