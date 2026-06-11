//! 描画用フォントローダ。
//!
//! テキスト描画（[`crate::render::state`] の状態機械）が 1 文字を塗るには、
//! 文字コードからグリフ ID（GID）を求め、そのアウトライン（[`OutlineSegment`]）
//! を引き、字送り幅（w0）を決める必要がある。本モジュールはフォント辞書 1 つ
//! 分の描画情報を [`RenderFont`] にまとめ、これらを提供する。
//!
//! ## フォントプログラムの取得（優先順）
//!
//! 1. 埋め込み: FontDescriptor の `/FontFile2`（TrueType）。Type0 は
//!    `/DescendantFonts[0]` の FontDescriptor を見る。`/FontFile3`（CFF）・
//!    `/FontFile`（Type1）は本フェーズでは描画不可 → 2 へフォールバック。
//! 2. 非埋め込み: BaseFont 名（サブセット接頭辞・スタイルサフィックス考慮）から
//!    `C:\Windows\Fonts` のシステムフォントへマッピングして代替する。
//! 3. 全く得られない場合もフォールバックを返し、字送り（PDF 辞書の幅情報）だけは
//!    機能させる（グリフは描かない）。
//!
//! ## 耐故障性
//!
//! フォントは信頼できない入力として扱う。パース失敗・グリフ欠落は「描かずに
//! 字送りだけ進める」で吸収し、panic しない。

use std::collections::HashMap;
use std::rc::Rc;

use crate::document::Document;
use crate::object::{Dictionary, Object};
use crate::text::{split_codes, WidthSource};
use crate::truetype::{OutlineSegment, TrueTypeFont};

/// フォント辞書 1 つ分の描画情報。
pub(crate) struct RenderFont {
    /// 実体のフォントプログラム（埋め込み or システム代替）。無ければ `None`。
    program: Option<Rc<TrueTypeFont>>,
    /// 2 バイトコード（Type0/CID）か。
    two_byte: bool,
    /// 字送り幅情報（`text.rs` と共通）。
    widths: WidthSource,
    /// 単純フォント（1 バイト）の code → GID 表（256 エントリ）。
    /// Type0 では使わず空のままにする。
    simple_gid: Vec<u16>,
    /// Type0 の `/CIDToGIDMap` ストリーム（2 バイト BE × CID で引く）。
    /// `Identity`・省略時は `None`（GID = CID）。
    cid_to_gid: Option<Vec<u8>>,
}

impl std::fmt::Debug for RenderFont {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RenderFont")
            .field("has_program", &self.program.is_some())
            .field("two_byte", &self.two_byte)
            .finish()
    }
}

impl RenderFont {
    /// フォントプログラムが得られなかった場合のフォールバック。
    /// 字送りだけ機能し、グリフは一切描かない。
    fn fallback(two_byte: bool, widths: WidthSource) -> RenderFont {
        RenderFont {
            program: None,
            two_byte,
            widths,
            simple_gid: Vec::new(),
            cid_to_gid: None,
        }
    }

    /// 表示文字列のバイト列をコード列へ分解する。
    pub(crate) fn codes(&self, bytes: &[u8]) -> Vec<u32> {
        split_codes(bytes, self.two_byte)
    }

    /// 1 バイトフォント（コード 32 を語間 `Tw` の対象にできる）か。
    pub(crate) fn is_single_byte(&self) -> bool {
        !self.two_byte
    }

    /// 文字コードの字送り幅（1000 分の 1 em）。
    ///
    /// PDF 辞書の幅情報を優先し、無ければフォントの advance から求める。
    /// どちらも不明なら既定 500 を返す（字送りを止めないため）。
    pub(crate) fn advance_w0(&self, code: u32) -> f64 {
        if let Some(w) = self.widths.width_of(code) {
            return w;
        }
        if let Some(font) = &self.program {
            if let Some(gid) = self.gid_for(code) {
                let upm = font.units_per_em() as f64;
                if upm > 0.0 {
                    return font.advance_width(gid) as f64 * 1000.0 / upm;
                }
            }
        }
        500.0
    }

    /// 文字コード → GID。解決できなければ `None`。
    pub(crate) fn gid_for(&self, code: u32) -> Option<u16> {
        if self.two_byte {
            // CID = コード（Identity 前提）。
            match &self.cid_to_gid {
                None => Some(code as u16),
                Some(map) => {
                    let pos = (code as usize).checked_mul(2)?;
                    let hi = *map.get(pos)?;
                    let lo = *map.get(pos + 1)?;
                    Some(u16::from_be_bytes([hi, lo]))
                }
            }
        } else {
            let gid = *self.simple_gid.get(code as usize & 0xFF)?;
            if gid == 0 {
                None
            } else {
                Some(gid)
            }
        }
    }

    /// 文字コードのアウトライン（フォント単位・y 上向き）と units_per_em を返す。
    /// プログラムが無い／グリフが引けない場合は `None`（描画スキップ）。
    pub(crate) fn glyph_outline(&self, code: u32) -> Option<(Vec<OutlineSegment>, f64)> {
        let font = self.program.as_ref()?;
        let gid = self.gid_for(code)?;
        let outline = font.glyph_outline(gid)?;
        let upm = font.units_per_em() as f64;
        if upm <= 0.0 {
            return None;
        }
        Some((outline, upm))
    }
}

/// フォントのパース結果・名前解決をページ内で再利用するキャッシュ。
pub(crate) struct FontCache {
    /// システムフォントのファイルパス → パース済みフォント。
    files: HashMap<String, Option<Rc<TrueTypeFont>>>,
    /// 埋め込みフォントの (世代込み) 識別キー → パース済みフォント。
    /// キーには FontFile2 ストリームのバイト長と先頭バイトを使う簡易ハッシュ。
    embedded: HashMap<u64, Option<Rc<TrueTypeFont>>>,
}

impl FontCache {
    pub(crate) fn new() -> FontCache {
        FontCache {
            files: HashMap::new(),
            embedded: HashMap::new(),
        }
    }

    /// システムフォントファイルを読み込み・パースして（キャッシュ経由で）返す。
    fn load_system(&mut self, path: &str) -> Option<Rc<TrueTypeFont>> {
        if let Some(cached) = self.files.get(path) {
            return cached.clone();
        }
        let parsed = std::fs::read(path)
            .ok()
            .and_then(|data| TrueTypeFont::parse(data, 0).ok())
            .filter(|f| !f.is_cff())
            .map(Rc::new);
        self.files.insert(path.to_string(), parsed.clone());
        parsed
    }

    /// 埋め込み FontFile2 をパースして（キャッシュ経由で）返す。
    fn load_embedded(&mut self, data: Vec<u8>) -> Option<Rc<TrueTypeFont>> {
        // 簡易キー: 長さ + 先頭 8 バイト + 末尾 8 バイト。
        let key = embed_key(&data);
        if let Some(cached) = self.embedded.get(&key) {
            return cached.clone();
        }
        let parsed = TrueTypeFont::parse(data, 0)
            .ok()
            .filter(|f| !f.is_cff())
            .map(Rc::new);
        self.embedded.insert(key, parsed.clone());
        parsed
    }
}

/// 埋め込みフォントデータの簡易識別キー。
fn embed_key(data: &[u8]) -> u64 {
    let mut h = data.len() as u64;
    for &b in data.iter().take(8) {
        h = h.wrapping_mul(131).wrapping_add(b as u64);
    }
    for &b in data.iter().rev().take(8) {
        h = h.wrapping_mul(131).wrapping_add(b as u64);
    }
    h
}

/// フォント辞書から [`RenderFont`] を構築する。
///
/// `cache` でフォントプログラムのパースを再利用する。`ref_id` はこの
/// フォントリソースの間接参照 ID で、`to_bytes` 前の埋め込みフォント
/// （`/FontFile2` 未生成）をメモリ上の [`TrueTypeFont`] から直接引くために使う。
pub(crate) fn build_render_font(
    doc: &Document,
    cache: &mut FontCache,
    font: &Dictionary,
    ref_id: Option<crate::object::ObjectId>,
) -> RenderFont {
    let subtype = font
        .get("Subtype")
        .and_then(|o| o.as_name().ok())
        .unwrap_or("");
    let two_byte = subtype == "Type0";
    let widths = WidthSource::from_font_dict(doc, font, two_byte);

    if two_byte {
        build_type0(doc, cache, font, widths, ref_id)
    } else {
        build_simple(doc, cache, font, widths)
    }
}

/// Type0（CID）フォントの描画情報を作る。
fn build_type0(
    doc: &Document,
    cache: &mut FontCache,
    font: &Dictionary,
    widths: WidthSource,
    ref_id: Option<crate::object::ObjectId>,
) -> RenderFont {
    // `to_bytes` 前のメモリ上埋め込みフォント（/FontFile2 未生成）を優先。
    // 該当すれば in-memory の TrueTypeFont をそのまま使う。
    let in_memory = ref_id
        .and_then(|id| doc.embedded_program_by_type0_id(id))
        .filter(|f| !f.is_cff())
        .map(|f| Rc::new(f.clone()));

    // /DescendantFonts[0] を取得。
    let desc_font = doc
        .dict_get(font, "DescendantFonts")
        .and_then(|o| o.as_array().ok())
        .and_then(|a| a.first())
        .map(|o| doc.resolve(o))
        .and_then(|o| o.as_dict().ok())
        .cloned();
    let desc_font = match desc_font {
        Some(d) => d,
        None => {
            // 子フォントが無くても in-memory プログラムがあれば描画可能。
            return RenderFont {
                program: in_memory,
                two_byte: true,
                widths,
                simple_gid: Vec::new(),
                cid_to_gid: None,
            };
        }
    };

    // FontDescriptor → FontFile2（無ければ in-memory、さらに無ければシステム代替）。
    let program = in_memory.or_else(|| load_program_from_descriptor(doc, cache, &desc_font, font));

    // /CIDToGIDMap（省略 or /Identity → None、ストリーム → 伸長して保持）。
    let cid_to_gid = match doc.dict_get(&desc_font, "CIDToGIDMap") {
        Some(Object::Stream(s)) => doc.get_stream_data(s).ok(),
        _ => None,
    };

    RenderFont {
        program,
        two_byte: true,
        widths,
        simple_gid: Vec::new(),
        cid_to_gid,
    }
}

/// 単純フォント（1 バイト）の描画情報を作る。
fn build_simple(
    doc: &Document,
    cache: &mut FontCache,
    font: &Dictionary,
    widths: WidthSource,
) -> RenderFont {
    let program = load_program_from_descriptor(doc, cache, font, font);

    let program = match program {
        Some(p) => p,
        None => return RenderFont::fallback(false, widths),
    };

    // symbolic 判定（FontDescriptor /Flags の bit 3 = 値 4）。
    let flags = font_descriptor(doc, font)
        .and_then(|d| doc.dict_get(&d, "Flags").and_then(|o| o.as_int().ok()))
        .unwrap_or(0);
    let symbolic = flags & 4 != 0;

    // /Encoding の解析（名前 or 辞書）。
    let enc = doc.dict_get(font, "Encoding").cloned();
    let (base_enc_name, differences) = parse_encoding(doc, &enc);

    let is_truetype = font.get("Subtype").and_then(|o| o.as_name().ok()) == Some("TrueType");

    let has_encoding = enc.is_some();
    let base_enc = base_enc_name.as_deref();
    let mut simple_gid = vec![0u16; 256];
    for code in 0u32..256 {
        let gid = resolve_simple_gid(
            &program,
            code as u8,
            symbolic,
            has_encoding,
            base_enc,
            is_truetype,
        );
        simple_gid[code as usize] = gid;
    }
    // /Differences の上書き。
    for (code, name) in &differences {
        let gid = resolve_difference_gid(&program, *code, name);
        if let Some(slot) = simple_gid.get_mut(*code as usize) {
            *slot = gid;
        }
    }

    RenderFont {
        program: Some(program),
        two_byte: false,
        widths,
        simple_gid,
        cid_to_gid: None,
    }
}

/// FontDescriptor 辞書を取得する（単純フォントは自身、Type0 は子フォント）。
fn font_descriptor(doc: &Document, font: &Dictionary) -> Option<Dictionary> {
    doc.dict_get(font, "FontDescriptor")
        .and_then(|o| o.as_dict().ok())
        .cloned()
}

/// FontDescriptor の `/FontFile2` をパースして得る。無ければシステム代替へ。
///
/// `descriptor_owner` は FontDescriptor を持つ辞書（単純フォントは font 自身、
/// Type0 は子 CIDFont）。`name_owner` は BaseFont 名を持つ辞書（システム代替の
/// 名前解決に使う。通常は上位の font 辞書）。
fn load_program_from_descriptor(
    doc: &Document,
    cache: &mut FontCache,
    descriptor_owner: &Dictionary,
    name_owner: &Dictionary,
) -> Option<Rc<TrueTypeFont>> {
    let descriptor = font_descriptor(doc, descriptor_owner);

    // 1. 埋め込み FontFile2。
    if let Some(desc) = &descriptor {
        if let Some(Object::Stream(s)) = doc.dict_get(desc, "FontFile2") {
            if let Ok(data) = doc.get_stream_data(s) {
                if let Some(font) = cache.load_embedded(data) {
                    return Some(font);
                }
            }
        }
        // /FontFile3（CFF）・/FontFile（Type1）は描画不可 → 代替へ。
    }

    // 2. システムフォント代替。
    let base = name_owner
        .get("BaseFont")
        .and_then(|o| o.as_name().ok())
        .or_else(|| {
            descriptor
                .as_ref()
                .and_then(|d| d.get("FontName").and_then(|o| o.as_name().ok()))
        })
        .unwrap_or("");
    let flags = descriptor
        .as_ref()
        .and_then(|d| doc.dict_get(d, "Flags").and_then(|o| o.as_int().ok()))
        .unwrap_or(0);
    if let Some((path, ttc_index)) = system_font_path(base, flags) {
        let _ = ttc_index; // 代替は index 0 固定（TTC も先頭書体）。
        return cache.load_system(path);
    }
    None
}

/// `/Encoding` を (基底エンコーディング名, Differences) へ分解する。
///
/// 名前なら基底名のみ。辞書なら `/BaseEncoding` と `/Differences` を読む。
fn parse_encoding(doc: &Document, enc: &Option<Object>) -> (Option<String>, Vec<(u8, String)>) {
    match enc {
        Some(Object::Name(n)) => (Some(n.clone()), Vec::new()),
        Some(Object::Dictionary(d)) => {
            let base = d
                .get("BaseEncoding")
                .and_then(|o| o.as_name().ok())
                .map(String::from);
            let mut diffs = Vec::new();
            if let Some(Object::Array(arr)) = doc.dict_get(d, "Differences") {
                let mut cur: u32 = 0;
                for o in arr {
                    match doc.resolve(o) {
                        Object::Integer(v) if *v >= 0 && *v < 256 => cur = *v as u32,
                        Object::Real(r) if *r >= 0.0 && *r < 256.0 => cur = *r as u32,
                        Object::Name(name) => {
                            if cur < 256 {
                                diffs.push((cur as u8, name.clone()));
                            }
                            cur += 1;
                        }
                        _ => {}
                    }
                }
            }
            (base, diffs)
        }
        _ => (None, Vec::new()),
    }
}

/// 単純フォントの 1 コード分の GID を解決する。
///
/// symbolic かつ `/Encoding` 無し → `glyph_id_by_code`。それ以外は基底
/// エンコーディングで Unicode へ変換 → `glyph_id`。TrueType で指定なしの
/// 場合は standard → winansi → `glyph_id_by_code` の順にフォールバックする。
fn resolve_simple_gid(
    font: &TrueTypeFont,
    code: u8,
    symbolic: bool,
    has_encoding: bool,
    base_enc_name: Option<&str>,
    is_truetype: bool,
) -> u16 {
    if symbolic && !has_encoding {
        return font.glyph_id_by_code(code as u32).unwrap_or(0);
    }

    // 基底エンコーディングから Unicode を引く。
    let unicode = encode_to_unicode(code, base_enc_name);
    if let Some(c) = unicode {
        if let Some(gid) = font.glyph_id(c) {
            return gid;
        }
    }

    // 指定なしの TrueType: standard で引けなければ winansi → code 直引き。
    if base_enc_name.is_none() && is_truetype {
        let wc = crate::font::winansi_to_char(code);
        if let Some(gid) = font.glyph_id(wc) {
            return gid;
        }
        if let Some(gid) = font.glyph_id_by_code(code as u32) {
            return gid;
        }
    }

    // 最後の手段: コード直引き。
    font.glyph_id_by_code(code as u32).unwrap_or(0)
}

/// 基底エンコーディング名とコードから Unicode 文字を引く。
fn encode_to_unicode(code: u8, base_enc_name: Option<&str>) -> Option<char> {
    match base_enc_name {
        Some("WinAnsiEncoding") => Some(crate::font::winansi_to_char(code)),
        Some("MacRomanEncoding") => crate::encoding::mac_roman_encoding(code),
        Some("StandardEncoding") => crate::encoding::standard_encoding(code),
        // 指定なし: StandardEncoding を既定とする（ASCII 域はこれで足りる）。
        _ => crate::encoding::standard_encoding(code),
    }
}

/// `/Differences` のグリフ名から GID を解決する。
fn resolve_difference_gid(font: &TrueTypeFont, code: u8, name: &str) -> u16 {
    if let Some(c) = crate::encoding::glyph_name_to_unicode(name) {
        if let Some(gid) = font.glyph_id(c) {
            return gid;
        }
    }
    font.glyph_id_by_code(code as u32).unwrap_or(0)
}

/// BaseFont 名・Flags からシステムフォントのファイルパスを決める。
///
/// 返り値は (絶対パス, TTC index)。該当が無ければ `None`。
/// サブセット接頭辞 `ABCDEF+` とスタイルサフィックス（`,Bold` `-Bold` 等）を
/// 考慮する。`C:\Windows\Fonts` 配下を前提とする（Windows 環境）。
fn system_font_path(base_font: &str, flags: i64) -> Option<(&'static str, u32)> {
    // サブセット接頭辞 "ABCDEF+" を除去。
    let name = base_font.split('+').next_back().unwrap_or(base_font);
    // スタイル判定（サフィックス・部分一致）。
    let lower = name.to_ascii_lowercase();
    let bold = lower.contains("bold");
    let italic = lower.contains("italic") || lower.contains("oblique");

    // 正規化名: 小文字化し、区切り文字（空白・ハイフン・カンマ）を除去。
    // "MS-Gothic"・"MS Gothic"・"MSGothic" を同一に扱うため。
    let norm: String = lower
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | ',' | '_'))
        .collect();

    // 日本語フォント（TTC は index 0 で代替）。
    if norm.contains("msgothic") || norm.contains("mspgothic") {
        return Some(("C:\\Windows\\Fonts\\msgothic.ttc", 0));
    }
    if norm.contains("msmincho") || norm.contains("mspmincho") {
        return Some(("C:\\Windows\\Fonts\\msmincho.ttc", 0));
    }
    if norm.contains("yugoth") {
        return Some(("C:\\Windows\\Fonts\\YuGothM.ttc", 0));
    }
    if norm.contains("meiryo") {
        return Some(("C:\\Windows\\Fonts\\meiryo.ttc", 0));
    }

    // Symbol（厳密一致に近い: 正規化名が "symbol" で始まる）。
    if norm == "symbol" {
        return Some(("C:\\Windows\\Fonts\\symbol.ttf", 0));
    }

    // Helvetica/Arial 系。
    let is_helv = norm.contains("helvetica") || norm.contains("arial");
    let is_times = norm.contains("times");
    let is_courier = norm.contains("courier") || norm.contains("mono");

    if is_helv {
        return Some((arial_variant(bold, italic), 0));
    }
    if is_times {
        return Some((times_variant(bold, italic), 0));
    }
    if is_courier {
        return Some((courier_variant(bold, italic), 0));
    }

    // 不明な名前: Flags の Serif(bit 2=値 2)・FixedPitch(bit 1=値 1) で判定。
    let serif = flags & 2 != 0;
    let fixed = flags & 1 != 0;
    if fixed {
        return Some((courier_variant(bold, italic), 0));
    }
    if serif {
        return Some((times_variant(bold, italic), 0));
    }
    Some((arial_variant(bold, italic), 0))
}

fn arial_variant(bold: bool, italic: bool) -> &'static str {
    match (bold, italic) {
        (true, true) => "C:\\Windows\\Fonts\\arialbi.ttf",
        (true, false) => "C:\\Windows\\Fonts\\arialbd.ttf",
        (false, true) => "C:\\Windows\\Fonts\\ariali.ttf",
        (false, false) => "C:\\Windows\\Fonts\\arial.ttf",
    }
}

fn times_variant(bold: bool, italic: bool) -> &'static str {
    match (bold, italic) {
        (true, true) => "C:\\Windows\\Fonts\\timesbi.ttf",
        (true, false) => "C:\\Windows\\Fonts\\timesbd.ttf",
        (false, true) => "C:\\Windows\\Fonts\\timesi.ttf",
        (false, false) => "C:\\Windows\\Fonts\\times.ttf",
    }
}

fn courier_variant(bold: bool, italic: bool) -> &'static str {
    match (bold, italic) {
        (true, true) => "C:\\Windows\\Fonts\\courbi.ttf",
        (true, false) => "C:\\Windows\\Fonts\\courbd.ttf",
        (false, true) => "C:\\Windows\\Fonts\\couri.ttf",
        (false, false) => "C:\\Windows\\Fonts\\cour.ttf",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::Object;

    #[test]
    fn system_font_mapping_styles() {
        assert_eq!(
            system_font_path("Helvetica", 0),
            Some(("C:\\Windows\\Fonts\\arial.ttf", 0))
        );
        assert_eq!(
            system_font_path("Helvetica-BoldOblique", 0),
            Some(("C:\\Windows\\Fonts\\arialbi.ttf", 0))
        );
        assert_eq!(
            system_font_path("ABCDEF+Times-Bold", 0),
            Some(("C:\\Windows\\Fonts\\timesbd.ttf", 0))
        );
        assert_eq!(
            system_font_path("Courier", 0),
            Some(("C:\\Windows\\Fonts\\cour.ttf", 0))
        );
        assert_eq!(
            system_font_path("Symbol", 0),
            Some(("C:\\Windows\\Fonts\\symbol.ttf", 0))
        );
        // MS Gothic 系は TTC。
        assert_eq!(
            system_font_path("MS-Gothic", 0),
            Some(("C:\\Windows\\Fonts\\msgothic.ttc", 0))
        );
    }

    #[test]
    fn unknown_name_uses_flags() {
        // FixedPitch(1) → courier。
        assert_eq!(
            system_font_path("WeirdFont", 1),
            Some(("C:\\Windows\\Fonts\\cour.ttf", 0))
        );
        // Serif(2) → times。
        assert_eq!(
            system_font_path("WeirdFont", 2),
            Some(("C:\\Windows\\Fonts\\times.ttf", 0))
        );
        // それ以外 → arial。
        assert_eq!(
            system_font_path("WeirdFont", 0),
            Some(("C:\\Windows\\Fonts\\arial.ttf", 0))
        );
    }

    #[test]
    fn parse_encoding_differences() {
        let doc = Document::new();
        let mut enc = Dictionary::new();
        enc.set("BaseEncoding", Object::name("WinAnsiEncoding"));
        enc.set(
            "Differences",
            Object::Array(vec![
                Object::Integer(65),
                Object::name("A"),
                Object::name("B"),
                Object::Integer(200),
                Object::name("bullet"),
            ]),
        );
        let (base, diffs) = parse_encoding(&doc, &Some(Object::Dictionary(enc)));
        assert_eq!(base.as_deref(), Some("WinAnsiEncoding"));
        assert_eq!(diffs.len(), 3);
        assert_eq!(diffs[0], (65, "A".to_string()));
        assert_eq!(diffs[1], (66, "B".to_string()));
        assert_eq!(diffs[2], (200, "bullet".to_string()));
    }

    #[test]
    fn parse_encoding_name_only() {
        let doc = Document::new();
        let (base, diffs) = parse_encoding(&doc, &Some(Object::name("MacRomanEncoding")));
        assert_eq!(base.as_deref(), Some("MacRomanEncoding"));
        assert!(diffs.is_empty());
    }

    /// 埋め込みデータの簡易キーは内容で変わる。
    #[test]
    fn embed_key_differs() {
        assert_ne!(embed_key(b"hello world"), embed_key(b"hello WORLD"));
        assert_eq!(embed_key(b"abc"), embed_key(b"abc"));
    }

    /// フォールバック RenderFont は字送りだけ機能し、グリフは描かない。
    #[test]
    fn fallback_renders_nothing_but_advances() {
        let rf = RenderFont::fallback(false, WidthSource::Unknown);
        assert!(rf.glyph_outline(65).is_none());
        // 幅情報も無ければ既定 500。
        assert_eq!(rf.advance_w0(65), 500.0);
        assert!(rf.is_single_byte());
    }
}
