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
///
/// テキスト抽出（本モジュール）と描画（[`crate::render`]）で共有するため
/// `pub(crate)` で公開する。レンダラはこの幅情報を字送り（w0）の決定に使う。
pub(crate) enum WidthSource {
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

impl WidthSource {
    /// フォント辞書から文字幅情報を構築する（抽出・描画の共通入口）。
    ///
    /// `two_byte` は Type0/CID フォント（2 バイトコード）かどうか。
    pub(crate) fn from_font_dict(doc: &Document, font: &Dictionary, two_byte: bool) -> WidthSource {
        FontDecoder::load_widths(doc, font, two_byte)
    }

    /// 文字コードの幅（1000 分の 1 em 単位）。不明なら `None`。
    pub(crate) fn width_of(&self, code: u32) -> Option<f64> {
        match self {
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
}

/// 1 フォント分のデコード情報。
struct FontDecoder {
    /// コード長が 2 バイト（Type0/CID フォント）か。
    two_byte: bool,
    /// `/ToUnicode` CMap から構築した コード → 文字列 のマップ。
    to_unicode: Option<HashMap<u32, String>>,
    /// 文字幅情報。
    widths: WidthSource,
    /// アセント（em 単位。FontDescriptor /Ascent ÷ 1000。不明なら近似値）。
    ascent: f64,
    /// ディセント（em 単位。負値）。
    descent: f64,
}

/// FontDescriptor が引けないときのアセント/ディセント近似値（em 単位）。
const DEFAULT_ASCENT: f64 = 0.8;
const DEFAULT_DESCENT: f64 = -0.2;

impl FontDecoder {
    fn fallback() -> FontDecoder {
        FontDecoder {
            two_byte: false,
            to_unicode: None,
            widths: WidthSource::Unknown,
            ascent: DEFAULT_ASCENT,
            descent: DEFAULT_DESCENT,
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
        let (ascent, descent) = Self::load_vertical_metrics(doc, font, two_byte);
        FontDecoder {
            two_byte,
            to_unicode,
            widths,
            ascent,
            descent,
        }
    }

    /// FontDescriptor の /Ascent /Descent を em 単位（÷1000）で取得する。
    /// Type0 は /DescendantFonts [0] の記述子を見る。引けない・異常値は近似値。
    fn load_vertical_metrics(doc: &Document, font: &Dictionary, two_byte: bool) -> (f64, f64) {
        let owner = if two_byte {
            doc.dict_get(font, "DescendantFonts")
                .and_then(|o| o.as_array().ok())
                .and_then(|a| a.first())
                .map(|o| doc.resolve(o))
                .and_then(|o| o.as_dict().ok())
        } else {
            Some(font)
        };
        let desc = owner
            .and_then(|d| doc.dict_get(d, "FontDescriptor"))
            .and_then(|o| o.as_dict().ok());
        let desc = match desc {
            Some(d) => d,
            None => return (DEFAULT_ASCENT, DEFAULT_DESCENT),
        };
        let ascent = doc
            .dict_get(desc, "Ascent")
            .and_then(|o| o.as_number().ok())
            .map(|v| v / 1000.0)
            .filter(|v| v.is_finite() && *v > 0.0 && *v < 4.0)
            .unwrap_or(DEFAULT_ASCENT);
        let descent = doc
            .dict_get(desc, "Descent")
            .and_then(|o| o.as_number().ok())
            .map(|v| v / 1000.0)
            .filter(|v| v.is_finite() && *v < 0.0 && *v > -4.0)
            .unwrap_or(DEFAULT_DESCENT);
        (ascent, descent)
    }

    pub(crate) fn load_widths(doc: &Document, font: &Dictionary, two_byte: bool) -> WidthSource {
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
        self.widths.width_of(code)
    }

    /// バイト列をコード列に分解する。
    fn codes(&self, bytes: &[u8]) -> Vec<u32> {
        split_codes(bytes, self.two_byte)
    }

    /// コード 1 つを Unicode へ変換して追記する。
    fn decode_code(&self, code: u32, out: &mut String) {
        match self.to_unicode.as_ref().and_then(|m| m.get(&code)) {
            Some(s) => out.push_str(s),
            None if self.two_byte => out.push('\u{FFFD}'), // マップ不明の CID
            None => out.push(crate::font::winansi_to_char(code as u8)),
        }
    }

    /// 表示文字列のバイト列を Unicode 文字列へ変換する。
    fn decode(&self, bytes: &[u8], out: &mut String) {
        for code in self.codes(bytes) {
            self.decode_code(code, out);
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

/// 表示文字列のバイト列をコード列へ分解する（抽出・描画の共通ヘルパ）。
///
/// `two_byte` が真なら 2 バイト big-endian（Type0/CID）として、偽なら
/// 1 バイトずつコードへ分解する。末尾が奇数の場合は最後の 1 バイトを
/// そのままコードとして扱う（耐故障性）。
pub(crate) fn split_codes(bytes: &[u8], two_byte: bool) -> Vec<u32> {
    if two_byte {
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
// 位置付きテキスト抽出（テキストスパン）
// ---------------------------------------------------------------------------

use crate::render::path::Matrix;

/// 位置付きテキストスパン（テキスト選択・検索ハイライト用）。
///
/// 1 つの表示演算（`Tj` `'` `"` または `TJ` 1 回分）が 1 スパンになる。
/// 座標はページのユーザー空間（原点は左下、y 軸上向き、ポイント単位）で、
/// `cm` や Form XObject の `/Matrix` は適用済み。ページの `/Rotate` は
/// 適用しない（描画時の回転はビューワー側の責務）。
#[derive(Debug, Clone)]
pub struct TextSpan {
    /// 抽出テキスト（[`Document::extract_text`] と同じ変換規則）。
    pub text: String,
    /// 軸平行境界箱 `[x0, y0, x1, y1]`（x0<x1, y0<y1）。高さはフォントの
    /// アセント/ディセント（FontDescriptor、無ければ近似値）から推定する。
    pub bbox: [f64; 4],
    /// 実効フォントサイズ（テキスト行列・CTM のスケール込み、ポイント）。
    pub font_size: f64,
    /// グリフ（コード）単位の境界箱列（テキスト選択・キャレット用）。
    ///
    /// 各要素の `text` を連結したものが [`text`](Self::text) と一致する。
    /// 幅はフォントメトリクスの advance から、高さはスパンと同じ
    /// アセント/ディセントから推定する（グリフ実測ではない）。
    pub glyphs: Vec<SpanGlyph>,
}

/// [`TextSpan`] 内の 1 グリフ（1 文字コード）分の位置情報。
///
/// 座標系は [`TextSpan::bbox`] と同じページのユーザー空間。合字や
/// 多バイトコードでは `text` が複数文字になることがあり、ToUnicode で
/// 対応が引けないコードでは空文字列になることもある。
#[derive(Debug, Clone)]
pub struct SpanGlyph {
    /// このコードのデコード結果。
    pub text: String,
    /// 軸平行境界箱 `[x0, y0, x1, y1]`（x0<x1, y0<y1）。
    pub bbox: [f64; 4],
}

/// ページの位置付きテキストスパンを抽出する。
pub fn extract_page_text_spans(doc: &Document, page_id: ObjectId) -> Result<Vec<TextSpan>> {
    let content = doc.page_content_bytes(page_id)?;
    let resources = doc.page_resources(page_id);
    let mut out = Vec::new();
    spans_from_content(doc, &content, &resources, Matrix::identity(), &mut out, 0)?;
    Ok(out)
}

/// スパン抽出用のグラフィックス/テキスト状態（`q`/`Q` で退避・復元する分）。
#[derive(Clone)]
struct SpanState {
    /// CTM（ユーザー空間 → ページ空間。`cm` と Form の `/Matrix` を合成）。
    ctm: Matrix,
    /// 現在フォントのリソース名。
    font: Option<String>,
    /// フォントサイズ `Tfs`。
    font_size: f64,
    /// 字間 `Tc`。
    char_spacing: f64,
    /// 語間 `Tw`（1 バイトコード 32 のみに作用）。
    word_spacing: f64,
    /// 水平拡大率 `Tz`（パーセント）。
    h_scale: f64,
    /// 行送り `TL`。
    leading: f64,
    /// テキストライズ `Ts`。
    rise: f64,
}

/// コンテントストリーム 1 本分からスパンを集める。Form XObject は再帰する。
fn spans_from_content(
    doc: &Document,
    content: &[u8],
    resources: &Dictionary,
    base_ctm: Matrix,
    out: &mut Vec<TextSpan>,
    depth: usize,
) -> Result<()> {
    if depth > 8 {
        return Ok(()); // フォーム再帰の暴走防止
    }
    let ops = parse_content(content)?;

    let fonts_dict = doc
        .dict_get(resources, "Font")
        .and_then(|o| o.as_dict().ok())
        .cloned()
        .unwrap_or_default();
    let mut decoders: HashMap<String, FontDecoder> = HashMap::new();

    let mut gs = SpanState {
        ctm: base_ctm,
        font: None,
        font_size: 0.0,
        char_spacing: 0.0,
        word_spacing: 0.0,
        h_scale: 100.0,
        leading: 0.0,
        rise: 0.0,
    };
    let mut stack: Vec<SpanState> = Vec::new();
    let mut tm = Matrix::identity();
    let mut tlm = Matrix::identity();

    let num = |args: &[Object], i: usize| -> Option<f64> {
        args.get(i)
            .and_then(|o| o.as_number().ok())
            .filter(|v| v.is_finite())
    };

    for op in &ops {
        let args = &op.operands;
        match op.operator.as_str() {
            "q" => stack.push(gs.clone()),
            "Q" => {
                if let Some(s) = stack.pop() {
                    gs = s;
                }
            }
            "cm" => {
                if let Some(m) = operand_matrix(args) {
                    gs.ctm = m.then(&gs.ctm);
                }
            }
            "BT" => {
                tm = Matrix::identity();
                tlm = Matrix::identity();
            }
            "Tf" => {
                if let Some(Object::Name(n)) = args.first() {
                    if !decoders.contains_key(n) {
                        let decoder = doc
                            .dict_get(&fonts_dict, n)
                            .and_then(|o| o.as_dict().ok())
                            .map(|fd| FontDecoder::from_font_dict(doc, fd))
                            .unwrap_or_else(FontDecoder::fallback);
                        decoders.insert(n.clone(), decoder);
                    }
                    gs.font = Some(n.clone());
                }
                if let Some(s) = num(args, 1) {
                    gs.font_size = s;
                }
            }
            "Tc" => {
                if let Some(v) = num(args, 0) {
                    gs.char_spacing = v;
                }
            }
            "Tw" => {
                if let Some(v) = num(args, 0) {
                    gs.word_spacing = v;
                }
            }
            "Tz" => {
                if let Some(v) = num(args, 0) {
                    gs.h_scale = v;
                }
            }
            "TL" => {
                if let Some(v) = num(args, 0) {
                    gs.leading = v;
                }
            }
            "Ts" => {
                if let Some(v) = num(args, 0) {
                    gs.rise = v;
                }
            }
            "Td" | "TD" => {
                let tx = num(args, 0).unwrap_or(0.0);
                let ty = num(args, 1).unwrap_or(0.0);
                if op.operator == "TD" {
                    gs.leading = -ty;
                }
                tlm = Matrix::translate(tx, ty).then(&tlm);
                tm = tlm;
            }
            "Tm" => {
                if let Some(m) = operand_matrix(args) {
                    tlm = m;
                    tm = m;
                }
            }
            "T*" => {
                tlm = Matrix::translate(0.0, -gs.leading).then(&tlm);
                tm = tlm;
            }
            "Tj" | "'" | "\"" => {
                if op.operator != "Tj" {
                    // ' と " は次行へ移ってから表示。" は語間・字間も設定する。
                    if op.operator == "\"" {
                        if let Some(aw) = num(args, 0) {
                            gs.word_spacing = aw;
                        }
                        if let Some(ac) = num(args, 1) {
                            gs.char_spacing = ac;
                        }
                    }
                    tlm = Matrix::translate(0.0, -gs.leading).then(&tlm);
                    tm = tlm;
                }
                let bytes = args.iter().rev().find_map(|o| o.as_string().ok());
                if let Some(bytes) = bytes {
                    let decoder = span_decoder(&decoders, &gs.font);
                    let mut text = String::new();
                    let mut tx = 0.0;
                    let mut glyphs = Vec::new();
                    span_string_metrics(decoder, bytes, &gs, &mut text, &mut tx, &mut glyphs);
                    push_span(out, decoder, &gs, &tm, tx, text, glyphs);
                    tm = Matrix::translate(tx, 0.0).then(&tm);
                }
            }
            "TJ" => {
                let items = match args.first() {
                    Some(Object::Array(a)) => a,
                    _ => continue,
                };
                let decoder = span_decoder(&decoders, &gs.font);
                let mut text = String::new();
                let mut tx = 0.0;
                let mut glyphs = Vec::new();
                for item in items {
                    match item {
                        Object::String(bytes, _) => {
                            span_string_metrics(
                                decoder,
                                bytes,
                                &gs,
                                &mut text,
                                &mut tx,
                                &mut glyphs,
                            );
                        }
                        Object::Integer(_) | Object::Real(_) => {
                            let adj = item.as_number().unwrap_or(0.0);
                            tx += -adj / 1000.0 * gs.font_size * gs.h_scale / 100.0;
                        }
                        _ => {}
                    }
                }
                push_span(out, decoder, &gs, &tm, tx, text, glyphs);
                tm = Matrix::translate(tx, 0.0).then(&tm);
            }
            "Do" => {
                if let Some(Object::Name(n)) = args.first() {
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
                                // Form の /Matrix を CTM に合成して再帰。
                                let mut ctm = gs.ctm;
                                if let Some(Object::Array(m)) = doc.dict_get(&stream.dict, "Matrix")
                                {
                                    let nums: Vec<f64> = m
                                        .iter()
                                        .filter_map(|o| doc.resolve(o).as_number().ok())
                                        .collect();
                                    if let Some(fm) = matrix_from_nums(&nums) {
                                        ctm = fm.then(&ctm);
                                    }
                                }
                                spans_from_content(doc, &data, &sub_res, ctm, out, depth + 1)?;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// 現在フォントのデコーダを引く（未設定・解決失敗はフォールバック）。
fn span_decoder<'a>(
    decoders: &'a HashMap<String, FontDecoder>,
    current: &Option<String>,
) -> &'a FontDecoder {
    static FALLBACK: std::sync::OnceLock<FontDecoder> = std::sync::OnceLock::new();
    let fallback = FALLBACK.get_or_init(FontDecoder::fallback);
    current
        .as_ref()
        .and_then(|n| decoders.get(n))
        .unwrap_or(fallback)
}

/// 表示文字列 1 つ分のテキストと advance（テキスト空間）を累積する。
///
/// 幅不明のコードは 500/1000 em とみなす（bbox を返せることを優先）。
/// `glyphs` にはコードごとの `(デコード結果, 開始 tx, 終了 tx)` を積む
/// （グリフ単位境界箱の素材。tx はテキスト空間）。
fn span_string_metrics(
    decoder: &FontDecoder,
    bytes: &[u8],
    gs: &SpanState,
    text: &mut String,
    tx: &mut f64,
    glyphs: &mut Vec<(String, f64, f64)>,
) {
    for code in decoder.codes(bytes) {
        let mut gtext = String::new();
        decoder.decode_code(code, &mut gtext);
        let w0 = decoder.code_width(code).unwrap_or(500.0);
        let mut adv = w0 / 1000.0 * gs.font_size + gs.char_spacing;
        if !decoder.two_byte && code == 32 {
            adv += gs.word_spacing;
        }
        let tx0 = *tx;
        *tx += adv * gs.h_scale / 100.0;
        text.push_str(&gtext);
        glyphs.push((gtext, tx0, *tx));
    }
}

/// テキスト空間の矩形 `[x0, x1] × [y0, y1]` を `trm` で写した軸平行境界箱。
/// 非有限が混ざる場合は `None`。
fn text_rect_aabb(trm: &Matrix, x0: f64, x1: f64, y0: f64, y1: f64) -> Option<[f64; 4]> {
    let corners = [(x0, y0), (x1, y0), (x0, y1), (x1, y1)];
    let mut bx0 = f64::INFINITY;
    let mut by0 = f64::INFINITY;
    let mut bx1 = f64::NEG_INFINITY;
    let mut by1 = f64::NEG_INFINITY;
    for (cx, cy) in corners {
        let p = trm.apply(crate::render::path::Point::new(cx, cy));
        bx0 = bx0.min(p.x);
        by0 = by0.min(p.y);
        bx1 = bx1.max(p.x);
        by1 = by1.max(p.y);
    }
    if bx0.is_finite() && by0.is_finite() && bx1.is_finite() && by1.is_finite() {
        Some([bx0, by0, bx1, by1])
    } else {
        None
    }
}

/// スパン 1 つを構築して `out` へ追加する（テキストが空なら何もしない）。
///
/// `tm` はスパン開始時点のテキスト行列、`tx` は表示全体の advance
/// （テキスト空間。`TJ` の字送り調整込み）、`glyphs` はコードごとの
/// `(テキスト, 開始 tx, 終了 tx)`。
fn push_span(
    out: &mut Vec<TextSpan>,
    decoder: &FontDecoder,
    gs: &SpanState,
    tm: &Matrix,
    tx: f64,
    text: String,
    glyphs: Vec<(String, f64, f64)>,
) {
    if text.is_empty() {
        return;
    }
    let trm = tm.then(&gs.ctm); // テキスト空間 → ページ空間
    let y0 = decoder.descent * gs.font_size + gs.rise;
    let y1 = decoder.ascent * gs.font_size + gs.rise;
    let (x0, x1) = (0.0_f64.min(tx), 0.0_f64.max(tx));
    let bbox = match text_rect_aabb(&trm, x0, x1, y0, y1) {
        Some(b) => b,
        None => return, // 行列が壊れている（非有限）スパンは捨てる
    };
    // グリフ単位の境界箱（スパンと同じ高さで advance 区間を写す）。
    let span_glyphs: Vec<SpanGlyph> = glyphs
        .into_iter()
        .filter_map(|(gtext, gx0, gx1)| {
            let (gx0, gx1) = (gx0.min(gx1), gx0.max(gx1));
            text_rect_aabb(&trm, gx0, gx1, y0, y1).map(|bbox| SpanGlyph { text: gtext, bbox })
        })
        .collect();
    // 実効フォントサイズ: テキスト空間の単位縦ベクトル (0,1) の像の長さ。
    let v = (trm.c * trm.c + trm.d * trm.d).sqrt();
    let font_size = if v.is_finite() && v > 0.0 {
        gs.font_size * v
    } else {
        gs.font_size
    };
    out.push(TextSpan {
        text,
        bbox,
        font_size,
        glyphs: span_glyphs,
    });
}

/// `cm`/`Tm` のオペランド 6 要素から行列を作る（不足・非有限は `None`）。
fn operand_matrix(args: &[Object]) -> Option<Matrix> {
    let v: Vec<f64> = (0..6)
        .filter_map(|i| args.get(i).and_then(|o| o.as_number().ok()))
        .collect();
    matrix_from_nums(&v)
}

/// 6 要素のスライスから行列を作る。
fn matrix_from_nums(v: &[f64]) -> Option<Matrix> {
    if v.len() != 6 || !v.iter().all(|x| x.is_finite()) {
        return None;
    }
    Some(Matrix {
        a: v[0],
        b: v[1],
        c: v[2],
        d: v[3],
        e: v[4],
        f: v[5],
    })
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
        let before = lexer.pos;
        let token = match lexer.next_token() {
            Ok(Token::Eof) => break,
            Ok(t) => t,
            Err(_) => {
                // 位置が進まないエラー（未終端文字列など）での無限ループを防ぐ
                if lexer.pos <= before {
                    if before >= data.len() {
                        break;
                    }
                    lexer.pos = before + 1;
                }
                continue;
            }
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

    /// 未終端文字列を含む壊れた CMap で無限ループしない（回帰テスト）。
    #[test]
    fn broken_cmap_terminates() {
        parse_tounicode_cmap(b"1 beginbfchar <0041> (never closed");
        parse_tounicode_cmap(b"<48656");
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
