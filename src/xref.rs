//! 相互参照テーブル（クロスリファレンス）の読み取り。
//!
//! 以下の 3 形式すべてに対応する:
//! - 古典的な `xref` テーブル（§7.5.4）
//! - クロスリファレンスストリーム（§7.5.8, PDF 1.5+）
//! - ハイブリッド参照ファイル（`/XRefStm`）
//!
//! また、xref が壊れている場合はファイル全体を走査して
//! `n g obj` を拾い集める再構築（[`reconstruct`]）を行う。

use std::collections::HashMap;

use crate::error::{PdfError, Result};
use crate::lexer::{Lexer, Token};
use crate::object::{Dictionary, Object};
use crate::parser::Parser;

/// 1 オブジェクト分の xref エントリ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XrefEntry {
    /// 空きエントリ（type 0 / `f`）
    Free,
    /// ファイル内に非圧縮で存在（type 1 / `n`）。`offset` はファイル先頭からの位置。
    InFile { offset: usize, gen: u16 },
    /// オブジェクトストリーム内に圧縮格納（type 2）。
    InStream { stream_num: u32, index: u32 },
}

/// 読み取った xref 全体。
#[derive(Debug, Default)]
pub struct Xref {
    /// オブジェクト番号 → エントリ。更新が重なる場合は最新のものだけを保持。
    pub entries: HashMap<u32, XrefEntry>,
    /// マージ済みトレーラ辞書（新しい更新のキーを優先）。
    pub trailer: Dictionary,
}

impl Xref {
    /// `startxref` を探して xref チェーン全体を読み取る。
    pub fn load(data: &[u8]) -> Result<Xref> {
        let start = find_startxref(data)?;
        let mut xref = Xref::default();
        let mut visited = Vec::new();
        let mut next = Some(start);
        while let Some(offset) = next {
            if visited.contains(&offset) {
                break; // ループ防止
            }
            visited.push(offset);
            if offset >= data.len() {
                return Err(PdfError::BrokenXref(format!(
                    "xref offset {offset} out of range"
                )));
            }
            let trailer = parse_section(data, offset, &mut xref)?;
            // ハイブリッド参照: 古典テーブルの trailer が /XRefStm を持つ場合、
            // そのストリームのエントリは古典テーブルより優先度が低い扱いで読む
            // （既存エントリを上書きしない merge 規則で自然に処理される）。
            if let Some(Object::Integer(s)) = trailer.get("XRefStm") {
                let s = *s as usize;
                if !visited.contains(&s) && s < data.len() {
                    visited.push(s);
                    let _ = parse_section(data, s, &mut xref);
                }
            }
            next = match trailer.get("Prev") {
                Some(Object::Integer(p)) if *p >= 0 => Some(*p as usize),
                _ => None,
            };
            merge_trailer(&mut xref.trailer, trailer);
        }
        if xref.entries.is_empty() {
            return Err(PdfError::BrokenXref("no xref entries found".into()));
        }
        Ok(xref)
    }

    /// エントリを追加する。既にあるオブジェクト番号は上書きしない
    /// （新しい更新から順に読むため、先に見つかったものが最新）。
    fn insert(&mut self, num: u32, entry: XrefEntry) {
        self.entries.entry(num).or_insert(entry);
    }
}

/// トレーラのマージ。先に読んだ（=新しい）キーを優先する。
fn merge_trailer(dst: &mut Dictionary, src: Dictionary) {
    for (k, v) in src.iter() {
        if !dst.contains_key(k) {
            dst.set(k, v.clone());
        }
    }
}

/// ファイル末尾から `startxref` を探してオフセットを返す。
pub fn find_startxref(data: &[u8]) -> Result<usize> {
    let tail_len = data.len().min(2048);
    let tail_start = data.len() - tail_len;
    let tail = &data[tail_start..];
    let pos = find_last(tail, b"startxref")
        .ok_or_else(|| PdfError::BrokenXref("'startxref' not found".into()))?;
    let mut lexer = Lexer::new_at(data, tail_start + pos + b"startxref".len());
    match lexer.next_token() {
        Ok(Token::Integer(n)) if n >= 0 => Ok(n as usize),
        _ => Err(PdfError::BrokenXref("invalid startxref offset".into())),
    }
}

fn find_last(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (0..=haystack.len() - needle.len())
        .rev()
        .find(|&i| &haystack[i..i + needle.len()] == needle)
}

/// `offset` にある xref セクション 1 つ（古典 or ストリーム）を読み、
/// そのセクションのトレーラ辞書を返す。
fn parse_section(data: &[u8], offset: usize, xref: &mut Xref) -> Result<Dictionary> {
    let mut lexer = Lexer::new_at(data, offset);
    let save = lexer.pos;
    match lexer.next_token() {
        Ok(Token::Keyword(ref k)) if k == "xref" => parse_classic_table(data, lexer.pos, xref),
        _ => {
            // クロスリファレンスストリーム（間接オブジェクト）
            let mut parser = Parser::new_at(data, save);
            let (_, obj) = parser.parse_indirect_object()?;
            let stream = obj.as_stream()?;
            parse_xref_stream(stream, xref)?;
            Ok(stream.dict.clone())
        }
    }
}

/// 古典的な xref テーブルを読む。`xref` キーワードは消費済み。
fn parse_classic_table(data: &[u8], pos: usize, xref: &mut Xref) -> Result<Dictionary> {
    let mut lexer = Lexer::new_at(data, pos);
    loop {
        let save = lexer.pos;
        match lexer.next_token() {
            // サブセクション: `開始番号 個数`
            Ok(Token::Integer(start)) => {
                let count = match lexer.next_token() {
                    Ok(Token::Integer(c)) if c >= 0 => c as u32,
                    t => {
                        return Err(PdfError::BrokenXref(format!(
                            "invalid xref subsection header: {t:?}"
                        )))
                    }
                };
                if start < 0 {
                    return Err(PdfError::BrokenXref("negative subsection start".into()));
                }
                // 各エントリは「10 桁オフセット 5 桁世代 n/f」の固定 20 バイトだが、
                // 改行の乱れに耐えるためトークン単位で読む。
                for i in 0..count {
                    let num = start as u32 + i;
                    let f1 = match lexer.next_token() {
                        Ok(Token::Integer(v)) if v >= 0 => v as usize,
                        t => return Err(PdfError::BrokenXref(format!("bad xref entry: {t:?}"))),
                    };
                    let f2 = match lexer.next_token() {
                        Ok(Token::Integer(v)) if v >= 0 => v as u16,
                        Ok(Token::Integer(_)) => 0,
                        t => return Err(PdfError::BrokenXref(format!("bad xref entry: {t:?}"))),
                    };
                    match lexer.next_token() {
                        Ok(Token::Keyword(ref k)) if k == "n" => {
                            xref.insert(
                                num,
                                XrefEntry::InFile {
                                    offset: f1,
                                    gen: f2,
                                },
                            );
                        }
                        Ok(Token::Keyword(ref k)) if k == "f" => {
                            xref.insert(num, XrefEntry::Free);
                        }
                        t => {
                            return Err(PdfError::BrokenXref(format!("bad xref entry type: {t:?}")))
                        }
                    }
                }
            }
            // trailer
            Ok(Token::Keyword(ref k)) if k == "trailer" => {
                let mut parser = Parser::new_at(data, lexer.pos);
                let obj = parser.parse_object()?;
                return Ok(obj.as_dict()?.clone());
            }
            t => {
                let _ = save;
                return Err(PdfError::BrokenXref(format!(
                    "unexpected token in xref table: {t:?}"
                )));
            }
        }
    }
}

/// クロスリファレンスストリーム（§7.5.8）を読む。
fn parse_xref_stream(stream: &crate::object::Stream, xref: &mut Xref) -> Result<()> {
    let dict = &stream.dict;
    let decoded = crate::filters::decode_stream(dict, &stream.data, None)?;

    // /W: 各フィールドのバイト幅
    let w = dict.require("W")?.as_array()?;
    let widths: Vec<usize> = w
        .iter()
        .map(|o| o.as_int().map(|v| v as usize))
        .collect::<Result<_>>()
        .map_err(|_| PdfError::BrokenXref("invalid /W array".into()))?;
    if widths.len() < 3 {
        return Err(PdfError::BrokenXref("/W must have 3 elements".into()));
    }
    let row_len: usize = widths.iter().sum();
    if row_len == 0 {
        return Err(PdfError::BrokenXref("zero-width xref rows".into()));
    }

    // /Index: [開始 個数 ...]（省略時は [0 Size]）
    let size = dict.require("Size")?.as_int()?;
    let index: Vec<i64> = match dict.get("Index") {
        Some(Object::Array(a)) => a.iter().map(|o| o.as_int()).collect::<Result<_>>()?,
        _ => vec![0, size],
    };

    let read_field =
        |bytes: &[u8]| -> u64 { bytes.iter().fold(0u64, |acc, &b| (acc << 8) | b as u64) };

    let mut row_iter = decoded.chunks_exact(row_len);
    for pair in index.chunks(2) {
        let (start, count) = (pair[0], *pair.get(1).unwrap_or(&0));
        for i in 0..count {
            let row = match row_iter.next() {
                Some(r) => r,
                None => return Err(PdfError::BrokenXref("xref stream too short".into())),
            };
            let mut p = 0;
            // type フィールド幅 0 のときの既定値は 1（§7.5.8.3）
            let ftype = if widths[0] == 0 {
                1
            } else {
                read_field(&row[..widths[0]])
            };
            p += widths[0];
            let f2 = read_field(&row[p..p + widths[1]]);
            p += widths[1];
            let f3 = read_field(&row[p..p + widths[2]]);
            let num = (start + i) as u32;
            match ftype {
                0 => xref.insert(num, XrefEntry::Free),
                1 => xref.insert(
                    num,
                    XrefEntry::InFile {
                        offset: f2 as usize,
                        gen: f3 as u16,
                    },
                ),
                2 => xref.insert(
                    num,
                    XrefEntry::InStream {
                        stream_num: f2 as u32,
                        index: f3 as u32,
                    },
                ),
                _ => {} // 未知タイプは無視（将来拡張、§7.5.8.3）
            }
        }
    }
    Ok(())
}

/// 壊れた PDF のための xref 再構築。
///
/// ファイル全体を走査して `n g obj` の出現位置を収集する。
/// 同じオブジェクト番号が複数回現れた場合は最後（=最新）を採用する。
pub fn reconstruct(data: &[u8]) -> Result<Xref> {
    let mut xref = Xref::default();
    let mut i = 0usize;
    while i < data.len() {
        // "obj" を探す
        if data[i..].starts_with(b"obj")
            && (i + 3 >= data.len() || !crate::lexer::is_regular(data[i + 3]))
        {
            // 後ろ向きに `n g` を読む
            if let Some((num, gen, start)) = scan_back_obj_header(data, i) {
                xref.entries
                    .insert(num, XrefEntry::InFile { offset: start, gen });
            }
        }
        i += 1;
    }
    if xref.entries.is_empty() {
        return Err(PdfError::BrokenXref(
            "reconstruction found no objects".into(),
        ));
    }
    // trailer 辞書も探す（最後のものを採用）
    if let Some(tpos) = find_last(data, b"trailer") {
        let mut parser = Parser::new_at(data, tpos + b"trailer".len());
        if let Ok(obj) = parser.parse_object() {
            if let Ok(d) = obj.as_dict() {
                xref.trailer = d.clone();
            }
        }
    }
    Ok(xref)
}

/// `obj` キーワードの位置から後ろ向きに「番号 世代」を読み取る。
/// 戻り値は `(番号, 世代, オブジェクト開始位置)`。
fn scan_back_obj_header(data: &[u8], obj_pos: usize) -> Option<(u32, u16, usize)> {
    let mut p = obj_pos;
    // 空白を戻る
    let skip_ws_back = |p: &mut usize| {
        while *p > 0 && crate::lexer::is_whitespace(data[*p - 1]) {
            *p -= 1;
        }
    };
    let read_int_back = |p: &mut usize| -> Option<(u64, usize)> {
        let end = *p;
        while *p > 0 && data[*p - 1].is_ascii_digit() {
            *p -= 1;
        }
        if *p == end {
            return None;
        }
        let s = std::str::from_utf8(&data[*p..end]).ok()?;
        Some((s.parse().ok()?, *p))
    };
    skip_ws_back(&mut p);
    let (gen, _) = read_int_back(&mut p)?;
    skip_ws_back(&mut p);
    let (num, start) = read_int_back(&mut p)?;
    if gen > u16::MAX as u64 || num > u32::MAX as u64 {
        return None;
    }
    Some((num as u32, gen as u16, start))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classic_table() {
        let pdf = b"xref\n0 3\n0000000000 65535 f \n0000000017 00000 n \n0000000081 00000 n \ntrailer\n<< /Size 3 /Root 1 0 R >>\nstartxref\n0\n%%EOF";
        let mut xref = Xref::default();
        let trailer = parse_section(pdf, 0, &mut xref).unwrap();
        assert_eq!(xref.entries[&0], XrefEntry::Free);
        assert_eq!(xref.entries[&1], XrefEntry::InFile { offset: 17, gen: 0 });
        assert_eq!(trailer.get("Size").unwrap().as_int().unwrap(), 3);
    }

    #[test]
    fn reconstruct_finds_objects() {
        let pdf = b"%PDF-1.4\n1 0 obj << /A 1 >> endobj\n2 0 obj << /B 2 >> endobj\n1 0 obj << /A 9 >> endobj\n";
        let xref = reconstruct(pdf).unwrap();
        // 1 0 obj は 2 回現れる: 最後の出現（offset 62）が勝つ
        match xref.entries[&1] {
            XrefEntry::InFile { offset, .. } => assert!(offset > 35),
            _ => panic!(),
        }
        assert!(matches!(xref.entries[&2], XrefEntry::InFile { .. }));
    }
}
