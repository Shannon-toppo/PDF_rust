//! PDF シリアライザ。
//!
//! オブジェクトをファイル形式のバイト列に変換し、ドキュメント全体を
//! 古典的 xref テーブル形式で書き出す（§7.5）。

use crate::error::Result;
use crate::object::{Dictionary, Object, StringFormat};

/// オブジェクト 1 つをシリアライズして `out` に追記する。
pub fn write_object(out: &mut Vec<u8>, obj: &Object) {
    match obj {
        Object::Null => out.extend_from_slice(b"null"),
        Object::Boolean(true) => out.extend_from_slice(b"true"),
        Object::Boolean(false) => out.extend_from_slice(b"false"),
        Object::Integer(i) => out.extend_from_slice(i.to_string().as_bytes()),
        Object::Real(v) => out.extend_from_slice(format_real(*v).as_bytes()),
        Object::String(s, fmt) => write_string(out, s, *fmt),
        Object::Name(n) => write_name(out, n),
        Object::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b' ');
                }
                write_object(out, item);
            }
            out.push(b']');
        }
        Object::Dictionary(d) => write_dict(out, d),
        Object::Stream(s) => {
            write_dict(out, &s.dict);
            out.extend_from_slice(b"\nstream\n");
            out.extend_from_slice(&s.data);
            out.extend_from_slice(b"\nendstream");
        }
        Object::Reference((n, g)) => {
            out.extend_from_slice(format!("{n} {g} R").as_bytes());
        }
    }
}

/// 実数を PDF 表記でフォーマットする（末尾の 0 を除去）。
pub fn format_real(v: f64) -> String {
    if !v.is_finite() {
        return "0".into(); // PDF に NaN/inf は書けない
    }
    if v == v.trunc() && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    let mut s = format!("{v:.6}");
    while s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    s
}

fn write_dict(out: &mut Vec<u8>, d: &Dictionary) {
    out.extend_from_slice(b"<<");
    for (i, (k, v)) in d.iter().enumerate() {
        if i > 0 {
            out.push(b' ');
        }
        write_name(out, k);
        // 名前キーと値の区切り（値が名前や区切り文字で始まる場合は省略可能だが常に空ける）
        out.push(b' ');
        write_object(out, v);
    }
    out.extend_from_slice(b">>");
}

fn write_name(out: &mut Vec<u8>, name: &str) {
    out.push(b'/');
    for &b in name.as_bytes() {
        // 通常文字以外と '#'、非印字文字は #xx でエスケープ（§7.3.5）
        if crate::lexer::is_regular(b) && b != b'#' && (0x21..=0x7E).contains(&b) {
            out.push(b);
        } else {
            out.extend_from_slice(format!("#{b:02X}").as_bytes());
        }
    }
}

fn write_string(out: &mut Vec<u8>, s: &[u8], fmt: StringFormat) {
    // 非印字バイトが多い場合は 16 進形式を選ぶ
    let nonprintable = s.iter().filter(|&&b| !(0x20..=0x7E).contains(&b)).count();
    let use_hex = fmt == StringFormat::Hexadecimal || (!s.is_empty() && nonprintable * 2 > s.len());
    if use_hex {
        out.push(b'<');
        for &b in s {
            out.extend_from_slice(format!("{b:02X}").as_bytes());
        }
        out.push(b'>');
    } else {
        out.push(b'(');
        for &b in s {
            match b {
                b'(' | b')' | b'\\' => {
                    out.push(b'\\');
                    out.push(b);
                }
                b'\n' => out.extend_from_slice(b"\\n"),
                b'\r' => out.extend_from_slice(b"\\r"),
                b'\t' => out.extend_from_slice(b"\\t"),
                0x08 => out.extend_from_slice(b"\\b"),
                0x0C => out.extend_from_slice(b"\\f"),
                b if b < 0x20 => out.extend_from_slice(format!("\\{b:03o}").as_bytes()),
                b => out.push(b),
            }
        }
        out.push(b')');
    }
}

/// ドキュメント全体をシリアライズする。
///
/// すべてのオブジェクトを非圧縮の間接オブジェクトとして書き、
/// 古典的 xref テーブルとトレーラで締める。
pub fn write_document(
    version: &str,
    objects: &std::collections::BTreeMap<crate::object::ObjectId, Object>,
    trailer_extra: &Dictionary,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    // ヘッダ。2 行目のバイナリコメントは「バイナリファイルである」ことを
    // 転送プログラムへ知らせる慣習（§7.5.2 Note）。
    out.extend_from_slice(format!("%PDF-{version}\n").as_bytes());
    out.extend_from_slice(b"%\xE2\xE3\xCF\xD3\n");

    // 本体: オブジェクト番号順に書き出し、オフセットを記録
    let mut offsets: Vec<(u32, u16, usize)> = Vec::with_capacity(objects.len());
    for ((num, gen), obj) in objects {
        offsets.push((*num, *gen, out.len()));
        out.extend_from_slice(format!("{num} {gen} obj\n").as_bytes());
        write_object(&mut out, obj);
        out.extend_from_slice(b"\nendobj\n");
    }

    // xref テーブル（連続区間ごとにサブセクション化）
    let xref_pos = out.len();
    out.extend_from_slice(b"xref\n");
    // オブジェクト 0 は常に空きリストの先頭
    let mut entries: Vec<(u32, String)> = vec![(0, "0000000000 65535 f \n".into())];
    for (num, gen, off) in &offsets {
        entries.push((*num, format!("{off:010} {gen:05} n \n")));
    }
    entries.sort_by_key(|(n, _)| *n);
    let mut i = 0;
    while i < entries.len() {
        let start = entries[i].0;
        let mut j = i;
        while j + 1 < entries.len() && entries[j + 1].0 == entries[j].0 + 1 {
            j += 1;
        }
        out.extend_from_slice(format!("{start} {}\n", j - i + 1).as_bytes());
        for (_, line) in &entries[i..=j] {
            out.extend_from_slice(line.as_bytes());
        }
        i = j + 1;
    }

    // トレーラ
    let max_num = objects.keys().map(|(n, _)| *n).max().unwrap_or(0);
    let mut trailer = Dictionary::new();
    trailer.set("Size", (max_num + 1) as i64);
    for (k, v) in trailer_extra.iter() {
        // 再生成するキーは引き継がない
        if matches!(
            k,
            "Size"
                | "Prev"
                | "XRefStm"
                | "Type"
                | "W"
                | "Index"
                | "Length"
                | "Filter"
                | "DecodeParms"
                | "ID"
        ) {
            continue;
        }
        trailer.set(k, v.clone());
    }
    // /ID: 本体バイト列のハッシュから 16 バイトの識別子を生成（§14.4）
    let h1 = fnv1a64(&out, 0xcbf29ce484222325);
    let h2 = fnv1a64(&out, 0x84222325cbf29ce4);
    let mut id_bytes = Vec::with_capacity(16);
    id_bytes.extend_from_slice(&h1.to_be_bytes());
    id_bytes.extend_from_slice(&h2.to_be_bytes());
    let id = Object::String(id_bytes, StringFormat::Hexadecimal);
    trailer.set("ID", Object::Array(vec![id.clone(), id]));
    out.extend_from_slice(b"trailer\n");
    write_object(&mut out, &Object::Dictionary(trailer));
    out.extend_from_slice(format!("\nstartxref\n{xref_pos}\n%%EOF\n").as_bytes());
    Ok(out)
}

/// FNV-1a 64bit ハッシュ（/ID 生成用。暗号学的強度は不要）。
fn fnv1a64(data: &[u8], seed: u64) -> u64 {
    let mut h = seed;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::Stream;

    fn ser(obj: &Object) -> String {
        let mut v = Vec::new();
        write_object(&mut v, obj);
        String::from_utf8(v).unwrap()
    }

    #[test]
    fn serialize_primitives() {
        assert_eq!(ser(&Object::Null), "null");
        assert_eq!(ser(&Object::Integer(-42)), "-42");
        assert_eq!(ser(&Object::Real(3.5)), "3.5");
        assert_eq!(ser(&Object::Real(2.0)), "2");
        assert_eq!(ser(&Object::Reference((7, 0))), "7 0 R");
        assert_eq!(ser(&Object::Name("A B".into())), "/A#20B");
    }

    #[test]
    fn serialize_strings() {
        assert_eq!(ser(&Object::string_literal("a(b)c\\")), "(a\\(b\\)c\\\\)");
        assert_eq!(
            ser(&Object::String(vec![0xFE, 0xFF], StringFormat::Hexadecimal)),
            "<FEFF>"
        );
    }

    #[test]
    fn serialize_roundtrip_via_parser() {
        let mut d = Dictionary::new();
        d.set("Type", Object::Name("Test".into()));
        d.set(
            "Nums",
            Object::Array(vec![1.into(), 2.5.into(), Object::Reference((3, 0))]),
        );
        d.set("S", Object::string_literal("hello\nworld"));
        let obj = Object::Dictionary(d);
        let bytes = ser(&obj).into_bytes();
        let parsed = crate::parser::Parser::new_at(&bytes, 0)
            .parse_object()
            .unwrap();
        assert_eq!(parsed, obj);
    }

    #[test]
    fn serialize_stream_roundtrip() {
        let s = Stream::new(Dictionary::new(), b"DATA".to_vec());
        let obj = Object::Stream(s);
        let bytes = ser(&obj).into_bytes();
        let parsed = crate::parser::Parser::new_at(&bytes, 0)
            .parse_object()
            .unwrap();
        assert_eq!(parsed.as_stream().unwrap().data, b"DATA");
    }
}
