//! テキスト抽出。
//!
//! コンテントストリームのテキスト表示演算子（`Tj` `TJ` `'` `"`）を解釈し、
//! フォントのエンコーディング情報を使ってバイト列を Unicode に変換する。
//!
//! 対応している変換手段（優先順）:
//! 1. フォントの `/ToUnicode` CMap（`bfchar` / `bfrange`）
//! 2. 1 バイトフォントの WinAnsiEncoding（≒ CP1252）フォールバック
//!
//! 改行・空白は演算子（`Td` `TD` `T*` `Tm` と `TJ` 内の字送り調整）から
//! ヒューリスティックに復元する。レイアウトの完全な再現は目的としない。

use std::collections::HashMap;

use crate::content::parse_content;
use crate::document::Document;
use crate::error::Result;
use crate::lexer::{Lexer, Token};
use crate::object::{Dictionary, Object, ObjectId};

/// フォントの文字幅情報（空白検出のための advance 計算に使う）。
enum WidthSource {
    /// 単純フォント: `/FirstChar` + `/Widths`
    Simple { first_char: u32, widths: Vec<f64> },
    /// CID フォント: `/DW`（既定幅）+ `/W` から作ったマップ
    Cid {
        default: f64,
        map: HashMap<u32, f64>,
    },
    /// 標準 14 フォント（組み込みメトリクス）
    Standard(crate::font::StandardFont),
    /// 不明（advance 計算ができない）
    Unknown,
}

/// 1 フォント分のデコード情報。
struct FontDecoder {
    /// コード長が 2 バイト（Type0/CID フォント）か。
    two_byte: bool,
    /// `/ToUnicode` CMap から構築した コード → 文字列 のマップ。
    to_unicode: Option<HashMap<u32, String>>,
    /// 文字幅情報。
    widths: WidthSource,
}

impl FontDecoder {
    fn fallback() -> FontDecoder {
        FontDecoder {
            two_byte: false,
            to_unicode: None,
            widths: WidthSource::Unknown,
        }
    }

    /// フォント辞書から構築する。
    fn from_font_dict(doc: &Document, font: &Dictionary) -> FontDecoder {
        let subtype = font
            .get("Subtype")
            .and_then(|o| o.as_name().ok())
            .unwrap_or("");
        let two_byte = subtype == "Type0";
        let to_unicode = doc
            .dict_get(font, "ToUnicode")
            .and_then(|o| o.as_stream().ok())
            .and_then(|s| doc.get_stream_data(s).ok())
            .map(|data| parse_tounicode_cmap(&data));
        let widths = Self::load_widths(doc, font, two_byte);
        FontDecoder {
            two_byte,
            to_unicode,
            widths,
        }
    }

    fn load_widths(doc: &Document, font: &Dictionary, two_byte: bool) -> WidthSource {
        if two_byte {
            // Type0: /DescendantFonts [0] に CID フォントの幅がある
            let desc = doc
                .dict_get(font, "DescendantFonts")
                .and_then(|o| o.as_array().ok())
                .and_then(|a| a.first())
                .map(|o| doc.resolve(o))
                .and_then(|o| o.as_dict().ok());
            if let Some(desc) = desc {
                let default = doc
                    .dict_get(desc, "DW")
                    .and_then(|o| o.as_number().ok())
                    .unwrap_or(1000.0);
                let mut map = HashMap::new();
                if let Some(Object::Array(w)) = doc.dict_get(desc, "W") {
                    parse_cid_w_array(doc, w, &mut map);
                }
                return WidthSource::Cid { default, map };
            }
            return WidthSource::Unknown;
        }
        // 単純フォント: /Widths
        if let Some(Object::Array(ws)) = doc.dict_get(font, "Widths") {
            let first_char = doc
                .dict_get(font, "FirstChar")
                .and_then(|o| o.as_int().ok())
                .unwrap_or(0) as u32;
            let widths: Vec<f64> = ws
                .iter()
                .map(|o| doc.resolve(o).as_number().unwrap_or(500.0))
                .collect();
            return WidthSource::Simple { first_char, widths };
        }
        // /Widths の無い標準 14 フォント（サブセット接頭辞 "ABCDEF+" を除去）
        if let Some(base) = font.get("BaseFont").and_then(|o| o.as_name().ok()) {
            let base = base.split('+').next_back().unwrap_or(base);
            use crate::font::StandardFont::*;
            let std = match base {
                "Helvetica" => Some(Helvetica),
                "Helvetica-Bold" => Some(HelveticaBold),
                "Helvetica-Oblique" => Some(HelveticaOblique),
                "Helvetica-BoldOblique" => Some(HelveticaBoldOblique),
                "Times-Roman" => Some(TimesRoman),
                "Times-Bold" => Some(TimesBold),
                "Times-Italic" => Some(TimesItalic),
                "Times-BoldItalic" => Some(TimesBoldItalic),
                "Courier" => Some(Courier),
                "Courier-Bold" => Some(CourierBold),
                "Courier-Oblique" => Some(CourierOblique),
                "Courier-BoldOblique" => Some(CourierBoldOblique),
                _ => None,
            };
            if let Some(f) = std {
                return WidthSource::Standard(f);
            }
        }
        WidthSource::Unknown
    }

    /// 文字コードの幅（1000 分の 1 em 単位）。不明なら `None`。
    fn code_width(&self, code: u32) -> Option<f64> {
        match &self.widths {
            WidthSource::Simple { first_char, widths } => {
                let idx = code.checked_sub(*first_char)? as usize;
                widths.get(idx).copied()
            }
            WidthSource::Cid { default, map } => Some(*map.get(&code).unwrap_or(default)),
            WidthSource::Standard(f) => {
                Some(f.char_width(crate::font::winansi_to_char(code as u8)) as f64)
            }
            WidthSource::Unknown => None,
        }
    }

    /// バイト列をコード列に分解する。
    fn codes(&self, bytes: &[u8]) -> Vec<u32> {
        if self.two_byte {
            bytes
                .chunks(2)
                .map(|p| {
                    if p.len() == 2 {
                        u16::from_be_bytes([p[0], p[1]]) as u32
                    } else {
                        p[0] as u32
                    }
                })
                .collect()
        } else {
            bytes.iter().map(|&b| b as u32).collect()
        }
    }

    /// 表示文字列のバイト列を Unicode 文字列へ変換する。
    fn decode(&self, bytes: &[u8], out: &mut String) {
        for code in self.codes(bytes) {
            match self.to_unicode.as_ref().and_then(|m| m.get(&code)) {
                Some(s) => out.push_str(s),
                None if self.two_byte => out.push('\u{FFFD}'), // マップ不明の CID
                None => out.push(crate::font::winansi_to_char(code as u8)),
            }
        }
    }

    /// 表示文字列全体の advance（1000 分の 1 em 単位）。幅不明なら `None`。
    fn advance_units(&self, bytes: &[u8]) -> Option<f64> {
        let mut total = 0.0;
        for code in self.codes(bytes) {
            total += self.code_width(code)?;
        }
        Some(total)
    }
}

/// CID フォントの `/W` 配列をパースする。
/// 形式: `c [w1 w2 ...]` または `c_first c_last w` の繰り返し（§9.7.4.3）。
fn parse_cid_w_array(doc: &Document, w: &[Object], map: &mut HashMap<u32, f64>) {
    let mut i = 0;
    while i < w.len() {
        let c1 = match doc.resolve(&w[i]).as_int() {
            Ok(v) if v >= 0 => v as u32,
            _ => break,
        };
        match w.get(i + 1).map(|o| doc.resolve(o)) {
            Some(Object::Array(ws)) => {
                for (k, wo) in ws.iter().enumerate() {
                    if let Ok(width) = doc.resolve(wo).as_number() {
                        map.insert(c1 + k as u32, width);
                    }
                }
                i += 2;
            }
            Some(o) => {
                let c2 = match o.as_int() {
                    Ok(v) if v >= c1 as i64 && v - (c1 as i64) < 65536 => v as u32,
                    _ => break,
                };
                let width = match w.get(i + 2).map(|o| doc.resolve(o).as_number()) {
                    Some(Ok(v)) => v,
                    _ => break,
                };
                for c in c1..=c2 {
                    map.insert(c, width);
                }
                i += 3;
            }
            None => break,
        }
    }
}

/// ページのテキストを抽出する。
pub fn extract_page_text(doc: &Document, page_id: ObjectId) -> Result<String> {
    let content = doc.page_content_bytes(page_id)?;
    let resources = doc.page_resources(page_id);
    let mut out = String::new();
    extract_from_content(doc, &content, &resources, &mut out, 0)?;
    // 末尾の余分な改行を整理
    while out.ends_with('\n') {
        out.pop();
    }
    Ok(out)
}

/// コンテントストリーム 1 本分を処理する。Form XObject は再帰する。
fn extract_from_content(
    doc: &Document,
    content: &[u8],
    resources: &Dictionary,
    out: &mut String,
    depth: usize,
) -> Result<()> {
    if depth > 8 {
        return Ok(()); // フォーム再帰の暴走防止
    }
    let ops = parse_content(content)?;

    // リソースからフォント辞書を引く（遅延構築でキャッシュ）
    let fonts_dict = doc
        .dict_get(resources, "Font")
        .and_then(|o| o.as_dict().ok())
        .cloned()
        .unwrap_or_default();
    let mut decoders: HashMap<String, FontDecoder> = HashMap::new();
    let mut current_font: Option<String> = None;

    // 行・空白検出用の状態
    let mut last_ty: Option<f64> = None; // Tm の縦位置
    let mut pending_newline = false;
    let mut font_size: f64 = 12.0;
    // 直近の行送り演算子（Td/TD/Tm/T*）以降に表示したテキストの advance（pt）。
    // フォント幅が不明な場合は None（計算不能）。
    let mut advance_pt: Option<f64> = Some(0.0);

    let push_newline = |out: &mut String| {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
    };
    let push_space = |out: &mut String| {
        if !out.is_empty() && !out.ends_with(char::is_whitespace) {
            out.push(' ');
        }
    };

    for op in &ops {
        match op.operator.as_str() {
            "BT" => {
                last_ty = None;
                advance_pt = Some(0.0);
            }
            "Tf" => {
                if let Some(Object::Name(n)) = op.operands.first() {
                    if !decoders.contains_key(n) {
                        let decoder = doc
                            .dict_get(&fonts_dict, n)
                            .and_then(|o| o.as_dict().ok())
                            .map(|fd| FontDecoder::from_font_dict(doc, fd))
                            .unwrap_or_else(FontDecoder::fallback);
                        decoders.insert(n.clone(), decoder);
                    }
                    current_font = Some(n.clone());
                }
                if let Some(s) = op.operands.get(1).and_then(|o| o.as_number().ok()) {
                    if s > 0.0 {
                        font_size = s;
                    }
                }
            }
            // 行送り系: 縦移動があれば改行。横移動は「直前に表示した
            // テキストの advance との差」が大きいときだけ空白とみなす
            // （Chromium/Skia などはグリフ 1 つごとに Td で位置決めする）。
            "Td" | "TD" => {
                let tx = op
                    .operands
                    .first()
                    .and_then(|o| o.as_number().ok())
                    .unwrap_or(0.0);
                let ty = op
                    .operands
                    .get(1)
                    .and_then(|o| o.as_number().ok())
                    .unwrap_or(0.0);
                if ty != 0.0 {
                    pending_newline = true;
                } else {
                    match advance_pt {
                        Some(consumed) => {
                            let extra = tx - consumed;
                            if extra > (0.2 * font_size).max(0.5) {
                                push_space(out);
                            }
                        }
                        // 幅情報が無いフォント: 正の横移動は空白とみなす
                        None => {
                            if tx > 0.0 {
                                push_space(out);
                            }
                        }
                    }
                }
                advance_pt = Some(0.0);
            }
            "T*" => {
                pending_newline = true;
                advance_pt = Some(0.0);
            }
            "Tm" => {
                if let Some(f) = op.operands.get(5).and_then(|o| o.as_number().ok()) {
                    if let Some(prev) = last_ty {
                        if (f - prev).abs() > 0.5 {
                            pending_newline = true;
                        }
                    }
                    last_ty = Some(f);
                }
                advance_pt = Some(0.0);
            }
            "Tj" | "'" | "\"" => {
                if op.operator != "Tj" {
                    pending_newline = true; // ' と " は次行へ移ってから表示
                    advance_pt = Some(0.0);
                }
                if pending_newline {
                    push_newline(out);
                    pending_newline = false;
                }
                // " のオペランドは (aw ac string)
                let s = op.operands.iter().rev().find_map(|o| o.as_string().ok());
                if let Some(bytes) = s {
                    show_text(
                        &decoders,
                        &current_font,
                        bytes,
                        out,
                        font_size,
                        &mut advance_pt,
                    );
                }
            }
            "TJ" => {
                if pending_newline {
                    push_newline(out);
                    pending_newline = false;
                }
                if let Some(Object::Array(items)) = op.operands.first() {
                    for item in items {
                        match item {
                            Object::String(bytes, _) => show_text(
                                &decoders,
                                &current_font,
                                bytes,
                                out,
                                font_size,
                                &mut advance_pt,
                            ),
                            // 大きな負の字送り調整は単語間の空白とみなす
                            Object::Integer(_) | Object::Real(_) => {
                                let adj = item.as_number().unwrap_or(0.0);
                                if adj < -180.0 {
                                    push_space(out);
                                }
                                // 調整もペン位置を動かす
                                if let Some(a) = advance_pt {
                                    advance_pt = Some(a - adj / 1000.0 * font_size);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            // Form XObject の再帰
            "Do" => {
                if let Some(Object::Name(n)) = op.operands.first() {
                    let xobj = doc
                        .dict_get(resources, "XObject")
                        .and_then(|o| o.as_dict().ok())
                        .and_then(|x| doc.dict_get(x, n))
                        .and_then(|o| o.as_stream().ok().cloned());
                    if let Some(stream) = xobj {
                        let is_form = stream.dict.get("Subtype").and_then(|o| o.as_name().ok())
                            == Some("Form");
                        if is_form {
                            if let Ok(data) = doc.get_stream_data(&stream) {
                                let sub_res = match doc.dict_get(&stream.dict, "Resources") {
                                    Some(Object::Dictionary(d)) => d.clone(),
                                    _ => resources.clone(),
                                };
                                extract_from_content(doc, &data, &sub_res, out, depth + 1)?;
                            }
                        }
                    }
                }
            }
            "ET" => {
                pending_newline = false;
                push_newline(out);
            }
            _ => {}
        }
    }
    Ok(())
}

/// テキストを表示し、advance（pt）の累積を更新する。
fn show_text(
    decoders: &HashMap<String, FontDecoder>,
    current: &Option<String>,
    bytes: &[u8],
    out: &mut String,
    font_size: f64,
    advance_pt: &mut Option<f64>,
) {
    static FALLBACK: std::sync::OnceLock<FontDecoder> = std::sync::OnceLock::new();
    let fallback = FALLBACK.get_or_init(FontDecoder::fallback);
    let decoder = current
        .as_ref()
        .and_then(|n| decoders.get(n))
        .unwrap_or(fallback);
    decoder.decode(bytes, out);
    *advance_pt = match (*advance_pt, decoder.advance_units(bytes)) {
        (Some(a), Some(units)) => Some(a + units / 1000.0 * font_size),
        _ => None,
    };
}

// ---------------------------------------------------------------------------
// ToUnicode CMap の解析（§9.10.3）
// ---------------------------------------------------------------------------

/// ToUnicode CMap ストリームから コード → Unicode 文字列 マップを作る。
///
/// `beginbfchar`/`endbfchar` と `beginbfrange`/`endbfrange` のみ解釈する
/// （ToUnicode 用途ではこれで十分）。
fn parse_tounicode_cmap(data: &[u8]) -> HashMap<u32, String> {
    let mut map = HashMap::new();
    let mut lexer = Lexer::new(data);
    // 直近のオペランドを溜めるスタック
    let mut stack: Vec<Token> = Vec::new();
    loop {
        let token = match lexer.next_token() {
            Ok(Token::Eof) => break,
            Ok(t) => t,
            Err(_) => continue,
        };
        match &token {
            Token::Keyword(k) if k == "beginbfchar" => {
                stack.clear();
                // <src> <dst> の対が endbfchar まで続く
                while let Ok(Token::HexString(src)) = lexer.next_token() {
                    match lexer.next_token() {
                        Ok(Token::HexString(dst)) => {
                            if let Some(code) = bytes_to_code(&src) {
                                map.insert(code, utf16be_to_string(&dst));
                            }
                        }
                        _ => break,
                    }
                }
            }
            Token::Keyword(k) if k == "beginbfrange" => {
                stack.clear();
                // <lo> <hi> <dst...> の組が endbfrange まで続く
                while let Ok(Token::HexString(lo)) = lexer.next_token() {
                    let hi = match lexer.next_token() {
                        Ok(Token::HexString(s)) => s,
                        _ => break,
                    };
                    let (lo_c, hi_c) = match (bytes_to_code(&lo), bytes_to_code(&hi)) {
                        (Some(a), Some(b)) if b >= a && b - a < 65536 => (a, b),
                        _ => break,
                    };
                    match lexer.next_token() {
                        // 連続マッピング: <lo> <hi> <dstStart>
                        Ok(Token::HexString(dst)) => {
                            for i in 0..=(hi_c - lo_c) {
                                let mut d = dst.clone();
                                // 最後の 16bit 単位に加算
                                if d.len() >= 2 {
                                    let l = d.len();
                                    let unit = u16::from_be_bytes([d[l - 2], d[l - 1]])
                                        .wrapping_add(i as u16);
                                    d[l - 2..].copy_from_slice(&unit.to_be_bytes());
                                }
                                map.insert(lo_c + i, utf16be_to_string(&d));
                            }
                        }
                        // 個別マッピング: <lo> <hi> [<d0> <d1> ...]
                        Ok(Token::ArrayStart) => {
                            let mut i = 0u32;
                            loop {
                                match lexer.next_token() {
                                    Ok(Token::HexString(d)) => {
                                        map.insert(lo_c + i, utf16be_to_string(&d));
                                        i += 1;
                                    }
                                    Ok(Token::ArrayEnd) => break,
                                    _ => break,
                                }
                            }
                        }
                        _ => break,
                    }
                }
            }
            t => {
                stack.push(t.clone());
                if stack.len() > 16 {
                    stack.remove(0);
                }
            }
        }
    }
    map
}

/// CMap のコード（1〜4 バイトのビッグエンディアン）を u32 へ。
fn bytes_to_code(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() || bytes.len() > 4 {
        return None;
    }
    Some(bytes.iter().fold(0u32, |acc, &b| (acc << 8) | b as u32))
}

/// UTF-16BE のバイト列を文字列へ（サロゲートペア対応）。
fn utf16be_to_string(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bfchar_and_bfrange() {
        let cmap = br"
/CIDInit /ProcSet findresource begin
12 dict begin begincmap
1 begincodespacerange <0000> <FFFF> endcodespacerange
2 beginbfchar
<0041> <0061>
<0042> <3042>
endbfchar
1 beginbfrange
<0050> <0052> <0070>
endbfrange
endcmap end end";
        let map = parse_tounicode_cmap(cmap);
        assert_eq!(map[&0x41], "a");
        assert_eq!(map[&0x42], "あ");
        assert_eq!(map[&0x50], "p");
        assert_eq!(map[&0x51], "q");
        assert_eq!(map[&0x52], "r");
    }

    #[test]
    fn bfrange_with_array() {
        let cmap = b"1 beginbfrange <01> <03> [<0058> <0059> <005A>] endbfrange";
        let map = parse_tounicode_cmap(cmap);
        assert_eq!(map[&1], "X");
        assert_eq!(map[&2], "Y");
        assert_eq!(map[&3], "Z");
    }

    #[test]
    fn surrogate_pair_target() {
        // U+1F600 (😀) は UTF-16BE で D83D DE00
        let cmap = b"1 beginbfchar <01> <D83DDE00> endbfchar";
        let map = parse_tounicode_cmap(cmap);
        assert_eq!(map[&1], "\u{1F600}");
    }
}
