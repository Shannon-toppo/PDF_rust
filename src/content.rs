//! コンテントストリーム（ページ記述）の解析と生成。
//!
//! コンテントストリームは「オペランド列 + 演算子」の繰り返しで構成される
//! （§7.8.2）。本モジュールはこれを [`Operation`] の列に分解する。
//!
//! インライン画像（`BI ... ID <バイナリ> EI`）は次の 2 要素オペランドを持つ
//! `BI` 演算として返す:
//! 1. `Object::Dictionary` — 画像属性辞書（`/W`, `/H`, `/BPC`, `/CS` 等）
//! 2. `Object::String(data, StringFormat::Hexadecimal)` — エンコード済み生データ
//!    （フィルタ伸長せず保持。`Stream.data` と同じ方針）
//!
//! 書き出し時は `BI /Key val ... ID <生データ> EI` の形式に戻す（§8.9.7）。

use crate::error::{PdfError, Result};
use crate::lexer::Token;
use crate::object::{Dictionary, Object, StringFormat};
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
        let before = parser.lexer.pos;
        let token = match parser.lexer.next_token() {
            Ok(t) => t,
            Err(_) => {
                // 不正バイトは読み飛ばす。未終端文字列などで字句解析器が
                // 位置を進めずにエラーを返すと無限ループになるため、
                // 進んでいなければ 1 バイト強制前進する
                if parser.lexer.pos == before {
                    parser.lexer.pos = before + 1;
                }
                continue;
            }
        };
        match token {
            Token::Eof => break,
            Token::Keyword(kw) => match kw.as_str() {
                "true" => operands.push(Object::Boolean(true)),
                "false" => operands.push(Object::Boolean(false)),
                "null" => operands.push(Object::Null),
                "BI" => {
                    // インライン画像: 辞書と生データを 2 要素オペランドとして保持
                    match parse_inline_image(&mut parser) {
                        Ok(bi_ops) => {
                            ops.push(Operation::new("BI", bi_ops));
                        }
                        Err(_) => {
                            // 壊れた BI ブロックは読み飛ばして継続
                        }
                    }
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

/// `BI` 消費済みの状態からインライン画像を読み込む。
///
/// 戻り値は 2 要素:
/// - `[0]` `Object::Dictionary` — `/W`, `/H`, `/BPC`, `/CS` 等の画像属性
/// - `[1]` `Object::String(data, StringFormat::Hexadecimal)` — エンコード済み生データ
///
/// キーが名前でない・値が欠けるなどの不正は読み飛ばして継続する。
/// EI が見つからない場合は [`PdfError::Syntax`] を返す（呼び出し側は読み飛ばす）。
fn parse_inline_image(parser: &mut Parser) -> Result<Vec<Object>> {
    // BI〜ID 間の属性をキー/値ペアとして辞書に詰める
    let mut dict = Dictionary::new();
    loop {
        match parser.lexer.next_token()? {
            Token::Keyword(ref k) if k == "ID" => break,
            Token::Eof => {
                return Err(PdfError::Syntax {
                    offset: parser.pos(),
                    message: "unterminated inline image (no ID)".into(),
                });
            }
            t => {
                // キートークンを取得: 名前でなければ読み飛ばす
                let key_obj = match parser.parse_object_from(t) {
                    Ok(o) => o,
                    Err(_) => continue,
                };
                let key = match key_obj.as_name() {
                    Ok(n) => n.to_owned(),
                    Err(_) => continue, // 名前でないキーは読み飛ばす
                };
                // 値トークンを取得
                let val_tok = match parser.lexer.next_token() {
                    Ok(Token::Eof) => {
                        return Err(PdfError::Syntax {
                            offset: parser.pos(),
                            message: "unterminated inline image (no ID)".into(),
                        });
                    }
                    Ok(t) => t,
                    Err(_) => continue,
                };
                // "ID" が値位置に来た場合（BI /W 1 ID のような省略形）は終端とみなす
                if let Token::Keyword(ref k) = val_tok {
                    if k == "ID" {
                        dict.set(key, Object::Null);
                        break;
                    }
                }
                // 値不正は無視して継続
                if let Ok(val) = parser.parse_object_from(val_tok) {
                    dict.set(key, val);
                }
            }
        }
    }

    // ID の直後 1 バイトの空白を読み飛ばす（§8.9.7: "a single white-space character"）
    let data = parser.lexer.data;
    let mut p = parser.pos();
    if p < data.len() && crate::lexer::is_whitespace(data[p]) {
        p += 1;
    }

    // バイナリデータの終端 EI を探す
    // フィルタなし（/F・/Filter キーなし）の場合は /W・/H・/BPC から期待バイト長を
    // 計算し、その位置から EI を探す（バイナリ中に偶然 "EI" が現れる誤検出対策）。
    let data_start = p;
    let ei_pos = find_ei(data, &dict, data_start);

    match ei_pos {
        Some(pos) => {
            // pos は "EI" の先頭位置。
            // "EI" の直前に空白デリミタがある場合（is_ei_boundary の条件）、
            // その空白はデータの一部ではなく区切り文字なので除去する。
            let data_end = if pos > data_start && crate::lexer::is_whitespace(data[pos - 1]) {
                pos - 1
            } else {
                pos
            };
            let raw = data
                .get(data_start..data_end)
                .ok_or_else(|| PdfError::Syntax {
                    offset: data_start,
                    message: "inline image data out of bounds".into(),
                })?;
            parser.lexer.pos = pos + 2; // "EI" の 2 バイト分を消費
            Ok(vec![
                Object::Dictionary(dict),
                Object::String(raw.to_vec(), StringFormat::Hexadecimal),
            ])
        }
        None => Err(PdfError::Syntax {
            offset: parser.pos(),
            message: "missing EI for inline image".into(),
        }),
    }
}

/// インライン画像データの終端 "EI" 位置を返す。
///
/// 1. フィルタなし（`/F`・`/Filter` キーなし）かつ `/W`/`/H`/`/BPC` が揃っている
///    場合は期待バイト長を計算し、その直後の "EI" を優先的に探す。
/// 2. 条件を満たさない、または計算位置に "EI" が存在しない場合は
///    「空白に挟まれた EI を線形走査」にフォールバックする。
///
/// 戻り値は `"EI"` の先頭バイト位置。見つからない場合は `None`。
fn find_ei(data: &[u8], dict: &Dictionary, start: usize) -> Option<usize> {
    // フィルタなし且つ画像サイズが判明している場合は長さ計算で定位
    let has_filter = dict.contains_key("F") || dict.contains_key("Filter");
    if !has_filter {
        if let Some(expected_len) = calc_expected_len(dict) {
            let candidate = start + expected_len;
            if is_ei_boundary(data, candidate) {
                return Some(candidate);
            }
            // 計算位置が外れた場合はフォールバックへ
        }
    }
    // 線形走査フォールバック: 空白に挟まれた "EI" を探す
    scan_for_ei(data, start)
}

/// 線形走査で「空白に挟まれた EI」の先頭位置を返す。
fn scan_for_ei(data: &[u8], start: usize) -> Option<usize> {
    let mut p = start;
    while p + 2 <= data.len() {
        if is_ei_boundary(data, p) {
            return Some(p);
        }
        p += 1;
    }
    None
}

/// 位置 `pos` が "EI" の先頭であり、前後が適切な区切りかを確認する。
fn is_ei_boundary(data: &[u8], pos: usize) -> bool {
    if pos + 2 > data.len() {
        return false;
    }
    if &data[pos..pos + 2] != b"EI" {
        return false;
    }
    // 直前が空白またはデータ先頭
    let prev_ok = pos == 0 || crate::lexer::is_whitespace(data[pos - 1]);
    // 直後が空白・デリミタまたはデータ末尾
    let next_ok = pos + 2 == data.len() || !crate::lexer::is_regular(data[pos + 2]);
    prev_ok && next_ok
}

/// フィルタなし画像の期待データバイト数を計算する。
///
/// `/W`（幅）・`/H`（高さ）・`/BPC`（既定 8）・`/CS`（色空間）から算出する。
/// 行は **8 の倍数ビット**にパディングされる。
/// `/CS` が不明（Indexed 等）の場合は `None` を返す（走査フォールバック）。
fn calc_expected_len(dict: &Dictionary) -> Option<usize> {
    let w = dict_int(dict, "W")? as usize;
    let h = dict_int(dict, "H")? as usize;
    // /BPC の既定値は 8
    let bpc = dict_int(dict, "BPC").unwrap_or(8) as usize;
    let components = inline_cs_components(dict)?;
    // 行ビット数 = w * components * bpc; 8 の倍数に切り上げてバイト数に
    let row_bits = w * components * bpc;
    let row_bytes = row_bits.div_ceil(8);
    Some(row_bytes * h)
}

/// インライン画像辞書の `/CS`（または `/ColorSpace`）から成分数を返す。
///
/// 認識できない色空間（Indexed 等）は `None` を返す。
fn inline_cs_components(dict: &Dictionary) -> Option<usize> {
    // インライン画像では短縮名 (/CS) と正規名 (/ColorSpace) の両方が使われる
    let cs = dict.get("CS").or_else(|| dict.get("ColorSpace"))?;
    let name = cs.as_name().ok()?;
    match name {
        "G" | "DeviceGray" => Some(1),
        "RGB" | "DeviceRGB" => Some(3),
        "CMYK" | "DeviceCMYK" => Some(4),
        // Indexed 等は走査フォールバックへ
        _ => None,
    }
}

/// 辞書から整数値を取り出す補助関数（正規名・短縮名は呼び出し側が使い分ける）。
fn dict_int(dict: &Dictionary, key: &str) -> Option<i64> {
    dict.get(key)?.as_int().ok()
}

/// 演算列をコンテントストリームのバイト列に直列化する。
///
/// `BI` 演算は特別扱いし、インライン画像記法で出力する:
/// ```text
/// BI /Key val ... ID
/// <生データ>
/// EI
/// ```
pub fn write_content(ops: &[Operation]) -> Vec<u8> {
    let mut out = Vec::new();
    for op in ops {
        if op.operator == "BI" {
            write_inline_image(&mut out, &op.operands);
        } else {
            for operand in &op.operands {
                crate::writer::write_object(&mut out, operand);
                out.push(b' ');
            }
            out.extend_from_slice(op.operator.as_bytes());
            out.push(b'\n');
        }
    }
    out
}

/// `BI` 演算をインライン画像記法で書き出す。
///
/// `operands` は `[Dictionary, String]` の 2 要素を期待するが、
/// 不正な場合でも panic せず可能な範囲で出力する。
fn write_inline_image(out: &mut Vec<u8>, operands: &[Object]) {
    out.extend_from_slice(b"BI");

    // 辞書部: /Key val の繰り返し
    if let Some(dict_obj) = operands.first() {
        if let Ok(dict) = dict_obj.as_dict() {
            for (k, v) in dict.iter() {
                out.push(b'\n');
                crate::writer::write_object(out, &Object::Name(k.to_owned()));
                out.push(b' ');
                crate::writer::write_object(out, v);
            }
        }
    }

    out.extend_from_slice(b"\nID ");

    // データ部: 生バイト列をそのまま書く
    if let Some(data_obj) = operands.get(1) {
        if let Ok(raw) = data_obj.as_string() {
            out.extend_from_slice(raw);
        }
    }

    // EI の前に改行を置いて区切りを明確化
    out.extend_from_slice(b"\nEI\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::Dictionary;

    #[test]
    fn parses_simple_content() {
        let src = b"BT /F1 12 Tf 72 700 Td (Hello) Tj ET 0 0 100 50 re S";
        let ops = parse_content(src).unwrap();
        let names: Vec<&str> = ops.iter().map(|o| o.operator.as_str()).collect();
        assert_eq!(names, vec!["BT", "Tf", "Td", "Tj", "ET", "re", "S"]);
        assert_eq!(ops[3].operands[0].as_string().unwrap(), b"Hello");
        assert_eq!(ops[5].operands.len(), 4);
    }

    /// BI 演算のオペランドが [Dictionary, String] の 2 要素であり、
    /// データが正しく保持されること。
    #[test]
    fn inline_image_holds_dict_and_data() {
        let src = b"q BI /W 2 /H 2 /BPC 8 /CS /G ID \x00\x01\x02\x03 EI Q (x) Tj";
        let ops = parse_content(src).unwrap();
        let names: Vec<&str> = ops.iter().map(|o| o.operator.as_str()).collect();
        assert_eq!(names, vec!["q", "BI", "Q", "Tj"]);

        let bi = &ops[1];
        assert_eq!(bi.operands.len(), 2, "BI オペランドは 2 要素");

        // 辞書確認
        let dict = bi.operands[0].as_dict().expect("operands[0] は辞書");
        assert_eq!(dict.get("W").unwrap().as_int().unwrap(), 2);
        assert_eq!(dict.get("H").unwrap().as_int().unwrap(), 2);
        assert_eq!(dict.get("BPC").unwrap().as_int().unwrap(), 8);
        assert_eq!(dict.get("CS").unwrap().as_name().unwrap(), "G");

        // データ確認: \x00\x01\x02\x03（4 バイト = 2x2x1ch）
        let raw = bi.operands[1].as_string().expect("operands[1] は文字列");
        assert_eq!(raw, b"\x00\x01\x02\x03", "生データが保持されている");
    }

    /// parse → write_content → parse の往復一致テスト。
    #[test]
    fn inline_image_roundtrip() {
        let src = b"q BI /W 2 /H 2 /BPC 8 /CS /G ID \x00\x01\x02\x03 EI Q";
        let ops1 = parse_content(src).unwrap();
        let written = write_content(&ops1);
        let ops2 = parse_content(&written).unwrap();
        assert_eq!(ops1, ops2, "往復後の演算列が一致しない");
    }

    /// バイナリデータ中に "EI" を含むが、フィルタなしで長さ計算により
    /// 正しい EI を特定できるケース（誤検出対策の検証）。
    #[test]
    fn inline_image_ei_in_binary_length_calculation() {
        // /W 3 /H 1 /BPC 8 /CS /G → 期待バイト数 = 3*1*1 = 3
        // データ = b"EI\xff" (先頭 2 バイトが "EI" だが長さ計算で正しく終端を特定)
        let src = b"BI /W 3 /H 1 /BPC 8 /CS /G ID EI\xff EI";
        let ops = parse_content(src).unwrap();
        let bi_ops: Vec<_> = ops.iter().filter(|o| o.operator == "BI").collect();
        assert_eq!(bi_ops.len(), 1);
        let raw = bi_ops[0].operands[1].as_string().unwrap();
        // 期待: 3 バイト目まで = b"EI\xff"
        assert_eq!(raw, b"EI\xff", "バイナリ中の EI は終端とみなさない");
    }

    /// /BPC 1 で行パディングがあるケース: /W 3 /H 2 /BPC 1 → 行 1 バイト × 2 = 2 バイト。
    #[test]
    fn inline_image_bpc1_row_padding() {
        // /W 3 /H 2 /BPC 1 /CS /G:
        // 行ビット = 3*1*1 = 3 → ceil(3/8) = 1 バイト; 2 行で合計 2 バイト
        let src = b"BI /W 3 /H 2 /BPC 1 /CS /G ID \xA0\xC0 EI";
        let ops = parse_content(src).unwrap();
        let bi_ops: Vec<_> = ops.iter().filter(|o| o.operator == "BI").collect();
        assert_eq!(bi_ops.len(), 1);
        let raw = bi_ops[0].operands[1].as_string().unwrap();
        assert_eq!(raw, b"\xA0\xC0");
    }

    /// EI 欠落・ID 直後 EOF などの壊れた入力で panic しないこと。
    #[test]
    fn inline_image_broken_input_no_panic() {
        // EI が存在しないケース
        let src_no_ei = b"BI /W 1 /H 1 /BPC 8 /CS /G ID \xff";
        let ops = parse_content(src_no_ei).unwrap(); // panic しない
                                                     // BI は読み飛ばされる（エラーを無視して継続）
        assert!(
            ops.iter().all(|o| o.operator != "BI"),
            "壊れた BI は読み飛ばされる"
        );

        // ID 直後 EOF
        let src_id_eof = b"BI /W 1 /H 1 ID";
        let _ops = parse_content(src_id_eof).unwrap(); // panic しない

        // BI の辞書部に不正なキー（名前でない）があるケース
        let src_bad_key = b"BI 42 1 /W 1 ID \xff EI";
        let _ops = parse_content(src_bad_key).unwrap(); // panic しない
    }

    /// 未終端リテラル文字列で無限ループしない（回帰テスト）。
    #[test]
    fn unterminated_string_terminates() {
        let ops = parse_content(b"1 0 0 1 10 10 cm (never closed").unwrap();
        let names: Vec<&str> = ops.iter().map(|o| o.operator.as_str()).collect();
        assert_eq!(names, vec!["cm"]);
        // 未終端の 16 進文字列・名前のみの断片でも終了すること
        parse_content(b"<48656").unwrap();
        parse_content(b"q (a\\").unwrap();
    }

    #[test]
    fn terminates_on_malformed_content() {
        // 不正バイト・対応しない閉じ括弧・未終端文字列を含む壊れたストリームで
        // 無限ループしないこと（回帰テスト）。ハング時にテストランナーごと
        // 固まらないよう別スレッド + タイムアウトで検証する
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let src = b"\xff\x00q \xde\xad\n1 2 cm garbage )(] f Q Q Q";
            tx.send(parse_content(src)).ok();
        });
        let ops = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("parse_content が終了しない")
            .unwrap();
        assert!(ops.iter().any(|o| o.operator == "cm"));
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

    /// write_inline_image の出力形式を直接検証する。
    #[test]
    fn write_inline_image_format() {
        let mut dict = Dictionary::new();
        dict.set("W", Object::Integer(2));
        dict.set("H", Object::Integer(2));
        dict.set("BPC", Object::Integer(8));
        dict.set("CS", Object::name("G"));
        let operands = vec![
            Object::Dictionary(dict),
            Object::String(vec![0x00, 0x01, 0x02, 0x03], StringFormat::Hexadecimal),
        ];
        let mut out = Vec::new();
        write_inline_image(&mut out, &operands);
        let s = String::from_utf8_lossy(&out);
        assert!(s.starts_with("BI\n"), "BI で始まる");
        assert!(s.contains("ID "), "ID を含む");
        assert!(s.ends_with("\nEI\n"), "EI で終わる");
    }
}
