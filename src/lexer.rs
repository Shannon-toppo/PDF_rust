//! PDF 字句解析器。
//!
//! バイト列をトークン列に分解する。PDF 32000-1:2008 §7.2（字句規約）に従い、
//! 空白・区切り文字・コメント・エスケープ付き文字列・`#xx` 付き名前などを扱う。

use crate::error::{PdfError, Result};

/// トークン。
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Integer(i64),
    Real(f64),
    /// `(...)` リテラル文字列（エスケープ解決済みのバイト列）
    LiteralString(Vec<u8>),
    /// `<...>` 16 進文字列（デコード済みのバイト列）
    HexString(Vec<u8>),
    /// `/Name`（`#xx` 解決済み）
    Name(String),
    /// `[`
    ArrayStart,
    /// `]`
    ArrayEnd,
    /// `<<`
    DictStart,
    /// `>>`
    DictEnd,
    /// 予約語・演算子（`obj` `endobj` `stream` `R` `true` や content stream の `Tj` など）
    Keyword(String),
    /// 入力の終わり
    Eof,
}

/// PDF の空白文字か（§7.2.2 Table 1）。
pub fn is_whitespace(b: u8) -> bool {
    matches!(b, b'\0' | b'\t' | b'\n' | b'\x0C' | b'\r' | b' ')
}

/// PDF の区切り文字か（§7.2.2 Table 2）。
pub fn is_delimiter(b: u8) -> bool {
    matches!(
        b,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

/// 通常文字（トークンを構成できる文字）か。
pub fn is_regular(b: u8) -> bool {
    !is_whitespace(b) && !is_delimiter(b)
}

/// 字句解析器。`data` 上をバイト位置 `pos` で走査する。
pub struct Lexer<'a> {
    pub data: &'a [u8],
    pub pos: usize,
}

impl<'a> Lexer<'a> {
    /// 新しいレキサを作る。
    pub fn new(data: &'a [u8]) -> Self {
        Lexer { data, pos: 0 }
    }

    /// 指定位置から開始するレキサを作る。
    pub fn new_at(data: &'a [u8], pos: usize) -> Self {
        Lexer { data, pos }
    }

    fn peek_byte(&self) -> Option<u8> {
        self.data.get(self.pos).copied()
    }

    fn syntax(&self, message: impl Into<String>) -> PdfError {
        PdfError::Syntax {
            offset: self.pos,
            message: message.into(),
        }
    }

    /// 空白とコメント（`%` から行末まで）を読み飛ばす。
    pub fn skip_whitespace(&mut self) {
        loop {
            match self.peek_byte() {
                Some(b) if is_whitespace(b) => self.pos += 1,
                Some(b'%') => {
                    // コメント: 行末（CR か LF）まで
                    while let Some(b) = self.peek_byte() {
                        if b == b'\n' || b == b'\r' {
                            break;
                        }
                        self.pos += 1;
                    }
                }
                _ => break,
            }
        }
    }

    /// 次のトークンを返す。
    pub fn next_token(&mut self) -> Result<Token> {
        self.skip_whitespace();
        let b = match self.peek_byte() {
            None => return Ok(Token::Eof),
            Some(b) => b,
        };
        match b {
            b'[' => {
                self.pos += 1;
                Ok(Token::ArrayStart)
            }
            b']' => {
                self.pos += 1;
                Ok(Token::ArrayEnd)
            }
            b'<' => {
                if self.data.get(self.pos + 1) == Some(&b'<') {
                    self.pos += 2;
                    Ok(Token::DictStart)
                } else {
                    self.pos += 1;
                    self.read_hex_string()
                }
            }
            b'>' => {
                if self.data.get(self.pos + 1) == Some(&b'>') {
                    self.pos += 2;
                    Ok(Token::DictEnd)
                } else {
                    Err(self.syntax("unexpected '>'"))
                }
            }
            b'(' => {
                self.pos += 1;
                self.read_literal_string()
            }
            b'/' => {
                self.pos += 1;
                self.read_name()
            }
            b'+' | b'-' | b'.' | b'0'..=b'9' => self.read_number(),
            b'{' | b'}' => {
                // PostScript 関数（Type 4 function）でのみ現れる。キーワード扱い。
                self.pos += 1;
                Ok(Token::Keyword((b as char).to_string()))
            }
            b')' => Err(self.syntax("unexpected ')'")),
            _ => self.read_keyword(),
        }
    }

    /// 数値トークン（整数または実数）を読む。
    fn read_number(&mut self) -> Result<Token> {
        let start = self.pos;
        let mut has_dot = false;
        if matches!(self.peek_byte(), Some(b'+') | Some(b'-')) {
            self.pos += 1;
        }
        while let Some(b) = self.peek_byte() {
            match b {
                b'0'..=b'9' => self.pos += 1,
                b'.' => {
                    has_dot = true;
                    self.pos += 1;
                }
                // `1.2.3` のような壊れた数値や `6-` などは数値部分だけ読む
                _ => break,
            }
        }
        let text = std::str::from_utf8(&self.data[start..self.pos])
            .map_err(|_| self.syntax("invalid number"))?;
        if has_dot {
            // ".5" "-.5" "4." なども許容される
            let v: f64 = parse_real(text).ok_or_else(|| self.syntax("invalid real number"))?;
            Ok(Token::Real(v))
        } else {
            match text.parse::<i64>() {
                Ok(v) => Ok(Token::Integer(v)),
                // 桁あふれは実数として扱う（壊れた PDF への耐性）
                Err(_) => match parse_real(text) {
                    Some(v) => Ok(Token::Real(v)),
                    None => Err(self.syntax("invalid number")),
                },
            }
        }
    }

    /// `(...)` リテラル文字列を読む。開き括弧は消費済みであること。
    fn read_literal_string(&mut self) -> Result<Token> {
        let mut out = Vec::new();
        let mut depth = 1usize; // 対応の取れた括弧はエスケープ不要（§7.3.4.2）
        loop {
            let b = self
                .peek_byte()
                .ok_or_else(|| self.syntax("unterminated string"))?;
            self.pos += 1;
            match b {
                b'(' => {
                    depth += 1;
                    out.push(b);
                }
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    out.push(b);
                }
                b'\\' => {
                    let e = self
                        .peek_byte()
                        .ok_or_else(|| self.syntax("unterminated string"))?;
                    self.pos += 1;
                    match e {
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0C),
                        b'(' => out.push(b'('),
                        b')' => out.push(b')'),
                        b'\\' => out.push(b'\\'),
                        b'0'..=b'7' => {
                            // 8 進数 1〜3 桁
                            let mut v = (e - b'0') as u32;
                            for _ in 0..2 {
                                match self.peek_byte() {
                                    Some(d @ b'0'..=b'7') => {
                                        v = v * 8 + (d - b'0') as u32;
                                        self.pos += 1;
                                    }
                                    _ => break,
                                }
                            }
                            out.push((v & 0xFF) as u8);
                        }
                        b'\r' => {
                            // 行継続: \<EOL> は無視。CRLF は 2 バイトとも消費
                            if self.peek_byte() == Some(b'\n') {
                                self.pos += 1;
                            }
                        }
                        b'\n' => {}
                        // 規格上、未知のエスケープはバックスラッシュを無視
                        other => out.push(other),
                    }
                }
                b'\r' => {
                    // 文字列中の EOL は LF に正規化（§7.3.4.2）
                    if self.peek_byte() == Some(b'\n') {
                        self.pos += 1;
                    }
                    out.push(b'\n');
                }
                other => out.push(other),
            }
        }
        Ok(Token::LiteralString(out))
    }

    /// `<...>` 16 進文字列を読む。開き `<` は消費済みであること。
    fn read_hex_string(&mut self) -> Result<Token> {
        let mut out = Vec::new();
        let mut hi: Option<u8> = None;
        loop {
            let b = self
                .peek_byte()
                .ok_or_else(|| self.syntax("unterminated hex string"))?;
            self.pos += 1;
            match b {
                b'>' => break,
                b if is_whitespace(b) => {}
                b => {
                    let v = hex_value(b).ok_or_else(|| self.syntax("invalid hex digit"))?;
                    match hi.take() {
                        Some(h) => out.push((h << 4) | v),
                        None => hi = Some(v),
                    }
                }
            }
        }
        // 奇数桁なら最後は下位 0 とみなす（§7.3.4.3）
        if let Some(h) = hi {
            out.push(h << 4);
        }
        Ok(Token::HexString(out))
    }

    /// 名前オブジェクトを読む。`/` は消費済みであること。
    fn read_name(&mut self) -> Result<Token> {
        let mut bytes = Vec::new();
        while let Some(b) = self.peek_byte() {
            if !is_regular(b) {
                break;
            }
            self.pos += 1;
            if b == b'#' {
                // #xx エスケープ（§7.3.5）
                let h = self.peek_byte().and_then(hex_value);
                let l = self.data.get(self.pos + 1).copied().and_then(hex_value);
                match (h, l) {
                    (Some(h), Some(l)) => {
                        bytes.push((h << 4) | l);
                        self.pos += 2;
                    }
                    // 不正な # は文字どおり扱う（耐性優先）
                    _ => bytes.push(b'#'),
                }
            } else {
                bytes.push(b);
            }
        }
        Ok(Token::Name(String::from_utf8_lossy(&bytes).into_owned()))
    }

    /// キーワード（英字などの並び）を読む。
    fn read_keyword(&mut self) -> Result<Token> {
        let start = self.pos;
        while let Some(b) = self.peek_byte() {
            if !is_regular(b) {
                break;
            }
            self.pos += 1;
        }
        if start == self.pos {
            // 通常文字でない未知のバイト: 1 バイト読み捨ててエラー
            self.pos += 1;
            return Err(self.syntax(format!("unexpected byte 0x{:02X}", self.data[start])));
        }
        Ok(Token::Keyword(
            String::from_utf8_lossy(&self.data[start..self.pos]).into_owned(),
        ))
    }
}

/// `.5` `4.` `-.7` などを含む PDF 実数のパース。
fn parse_real(text: &str) -> Option<f64> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    // Rust の parse は "4." を受理するが ".5" も受理する。先頭 '+' も OK。
    let normalized = if let Some(stripped) = t.strip_prefix('+') {
        stripped
    } else {
        t
    };
    let candidate = if normalized.starts_with('.') {
        format!("0{normalized}")
    } else {
        normalized.to_string()
    };
    let candidate = if candidate.ends_with('.') {
        format!("{candidate}0")
    } else {
        candidate
    };
    let candidate = candidate.replace("-.", "-0.");
    candidate.parse::<f64>().ok()
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(input: &[u8]) -> Vec<Token> {
        let mut lex = Lexer::new(input);
        let mut out = Vec::new();
        loop {
            let t = lex.next_token().unwrap();
            if t == Token::Eof {
                break;
            }
            out.push(t);
        }
        out
    }

    #[test]
    fn numbers() {
        assert_eq!(
            tokens(b"123 -7 +5 3.25 -.5 4. 0"),
            vec![
                Token::Integer(123),
                Token::Integer(-7),
                Token::Integer(5),
                Token::Real(3.25),
                Token::Real(-0.5),
                Token::Real(4.0),
                Token::Integer(0),
            ]
        );
    }

    #[test]
    fn literal_string_escapes() {
        let t = tokens(br"(a\nb\(c\)d\\e\101 (nested) f)");
        assert_eq!(
            t,
            vec![Token::LiteralString(b"a\nb(c)d\\eA (nested) f".to_vec())]
        );
    }

    #[test]
    fn literal_string_line_continuation_and_octal() {
        let t = tokens(b"(ab\\\ncd) (\\053)");
        assert_eq!(
            t,
            vec![
                Token::LiteralString(b"abcd".to_vec()),
                Token::LiteralString(b"+".to_vec())
            ]
        );
    }

    #[test]
    fn hex_string() {
        let t = tokens(b"<48 65 6C6C 6F> <414>");
        assert_eq!(
            t,
            vec![
                Token::HexString(b"Hello".to_vec()),
                Token::HexString(vec![0x41, 0x40]),
            ]
        );
    }

    #[test]
    fn names_with_hash_escape() {
        let t = tokens(b"/Name1 /A#20B /Type");
        assert_eq!(
            t,
            vec![
                Token::Name("Name1".into()),
                Token::Name("A B".into()),
                Token::Name("Type".into()),
            ]
        );
    }

    #[test]
    fn dict_array_keyword_comment() {
        let t = tokens(b"<< /K [1 2] >> % comment\ntrue R obj");
        assert_eq!(
            t,
            vec![
                Token::DictStart,
                Token::Name("K".into()),
                Token::ArrayStart,
                Token::Integer(1),
                Token::Integer(2),
                Token::ArrayEnd,
                Token::DictEnd,
                Token::Keyword("true".into()),
                Token::Keyword("R".into()),
                Token::Keyword("obj".into()),
            ]
        );
    }
}
