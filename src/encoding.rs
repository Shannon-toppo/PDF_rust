//! 単純フォントのエンコーディング解決テーブル群。
//!
//! PDF のテキスト描画（ビューワー）では、単純フォント（非 CID）の
//! 1 バイトコードを Unicode（およびグリフ名）へ解決する必要がある。
//! 本モジュールは PDF 32000-1:2008 Annex D.2 の StandardEncoding /
//! MacRomanEncoding 表と、Adobe Glyph List（AGL）のサブセットに基づく
//! グリフ名 → Unicode 変換を提供する。
//!
//! WinAnsiEncoding は既に [`crate::font::winansi_to_char`] が実装している
//! ため本モジュールでは再実装しない。呼び出し側が `/Encoding` の指定に
//! 応じて使い分けること（`/WinAnsiEncoding` は `font` 側、
//! `/StandardEncoding`・`/MacRomanEncoding` は本モジュール、
//! `/Differences` のグリフ名解決は [`glyph_name_to_unicode`]）。

/// StandardEncoding（PDF 仕様 Annex D.2）のコード → Unicode。
///
/// 未定義コード（0x00–0x1F と未割り当て位置）は `None`。
/// 表は「コード → グリフ名」を持ち、[`glyph_name_to_unicode`] を経由して
/// Unicode に変換する（検証しやすさと重複削減のため）。
pub fn standard_encoding(code: u8) -> Option<char> {
    let name = STANDARD_ENCODING[code as usize];
    if name.is_empty() {
        return None;
    }
    glyph_name_to_unicode(name)
}

/// MacRomanEncoding（PDF 仕様 Annex D.2）のコード → Unicode。
///
/// 未定義コード（0x00–0x1F と未割り当て位置）は `None`。
pub fn mac_roman_encoding(code: u8) -> Option<char> {
    let name = MAC_ROMAN_ENCODING[code as usize];
    if name.is_empty() {
        return None;
    }
    glyph_name_to_unicode(name)
}

/// グリフ名 → Unicode（Adobe Glyph List のサブセット + `uniXXXX`/`uXXXX` 形式）。
///
/// 解決順:
/// 1. AGL サブセット表（約 230 種のグリフ名）を引く。
/// 2. `uniXXXX`（16 進 4 桁）。サロゲート領域の値は `None`。
/// 3. `uXXXX`〜`uXXXXXX`（16 進 4–6 桁）。
/// 4. 上記に該当せず、名前が ASCII 1 文字ならその文字（AGL の慣行）。
/// 5. それ以外は `None`。
pub fn glyph_name_to_unicode(name: &str) -> Option<char> {
    // 1. AGL サブセット表（二分探索）。
    if let Ok(idx) = AGL.binary_search_by(|&(n, _)| n.cmp(name)) {
        return Some(AGL[idx].1);
    }

    // 2. uniXXXX 形式（16 進ちょうど 4 桁）。
    if let Some(hex) = name.strip_prefix("uni") {
        if hex.len() == 4 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            if let Ok(cp) = u32::from_str_radix(hex, 16) {
                // サロゲート領域（0xD800–0xDFFF）は単独で文字にならないため None。
                return char::from_u32(cp);
            }
        }
    }

    // 3. uXXXX〜uXXXXXX 形式（16 進 4–6 桁）。
    if let Some(hex) = name.strip_prefix('u') {
        if (4..=6).contains(&hex.len()) && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            if let Ok(cp) = u32::from_str_radix(hex, 16) {
                return char::from_u32(cp);
            }
        }
    }

    // 4. ASCII 1 文字ならその文字（AGL の慣行）。
    let bytes = name.as_bytes();
    if bytes.len() == 1 && bytes[0].is_ascii() {
        return Some(bytes[0] as char);
    }

    // 5. 不明。
    None
}

/// StandardEncoding 表（コード → グリフ名）。空文字列は未割り当て。
///
/// 出典: PDF 32000-1:2008 Annex D.2「Latin Character Set and Encodings」の
/// STD 列。0x00–0x1F は全て未割り当て。
#[rustfmt::skip]
static STANDARD_ENCODING: [&str; 256] = [
    // 0x00–0x1F: 未割り当て
    "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "",
    "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "",
    // 0x20–0x2F
    "space", "exclam", "quotedbl", "numbersign", "dollar", "percent", "ampersand", "quoteright",
    "parenleft", "parenright", "asterisk", "plus", "comma", "hyphen", "period", "slash",
    // 0x30–0x3F
    "zero", "one", "two", "three", "four", "five", "six", "seven",
    "eight", "nine", "colon", "semicolon", "less", "equal", "greater", "question",
    // 0x40–0x4F
    "at", "A", "B", "C", "D", "E", "F", "G",
    "H", "I", "J", "K", "L", "M", "N", "O",
    // 0x50–0x5F
    "P", "Q", "R", "S", "T", "U", "V", "W",
    "X", "Y", "Z", "bracketleft", "backslash", "bracketright", "asciicircum", "underscore",
    // 0x60–0x6F
    "quoteleft", "a", "b", "c", "d", "e", "f", "g",
    "h", "i", "j", "k", "l", "m", "n", "o",
    // 0x70–0x7F
    "p", "q", "r", "s", "t", "u", "v", "w",
    "x", "y", "z", "braceleft", "bar", "braceright", "asciitilde", "",
    // 0x80–0x8F: 未割り当て
    "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "",
    // 0x90–0x9F: 未割り当て
    "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "",
    // 0xA0–0xAF
    "", "exclamdown", "cent", "sterling", "fraction", "yen", "florin", "section",
    "currency", "quotesingle", "quotedblleft", "guillemotleft", "guilsinglleft", "guilsinglright", "fi", "fl",
    // 0xB0–0xBF
    "", "endash", "dagger", "daggerdbl", "periodcentered", "", "paragraph", "bullet",
    "quotesinglbase", "quotedblbase", "quotedblright", "guillemotright", "ellipsis", "perthousand", "", "questiondown",
    // 0xC0–0xCF
    "", "grave", "acute", "circumflex", "tilde", "macron", "breve", "dotaccent",
    "dieresis", "", "ring", "cedilla", "", "hungarumlaut", "ogonek", "caron",
    // 0xD0–0xDF
    "emdash", "", "", "", "", "", "", "",
    "", "", "", "", "", "", "", "",
    // 0xE0–0xEF
    "", "AE", "", "ordfeminine", "", "", "", "",
    "Lslash", "Oslash", "OE", "ordmasculine", "", "", "", "",
    // 0xF0–0xFF
    "", "ae", "", "", "", "dotlessi", "", "",
    "lslash", "oslash", "oe", "germandbls", "", "", "", "",
];

/// MacRomanEncoding 表（コード → グリフ名）。空文字列は未割り当て。
///
/// 出典: PDF 32000-1:2008 Annex D.2 の MAC 列（Mac OS Roman 相当）。
/// 0x00–0x1F は全て未割り当て。0x20–0x7E は ASCII と一致。
#[rustfmt::skip]
static MAC_ROMAN_ENCODING: [&str; 256] = [
    // 0x00–0x1F: 未割り当て
    "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "",
    "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "",
    // 0x20–0x2F
    "space", "exclam", "quotedbl", "numbersign", "dollar", "percent", "ampersand", "quotesingle",
    "parenleft", "parenright", "asterisk", "plus", "comma", "hyphen", "period", "slash",
    // 0x30–0x3F
    "zero", "one", "two", "three", "four", "five", "six", "seven",
    "eight", "nine", "colon", "semicolon", "less", "equal", "greater", "question",
    // 0x40–0x4F
    "at", "A", "B", "C", "D", "E", "F", "G",
    "H", "I", "J", "K", "L", "M", "N", "O",
    // 0x50–0x5F
    "P", "Q", "R", "S", "T", "U", "V", "W",
    "X", "Y", "Z", "bracketleft", "backslash", "bracketright", "asciicircum", "underscore",
    // 0x60–0x6F
    "grave", "a", "b", "c", "d", "e", "f", "g",
    "h", "i", "j", "k", "l", "m", "n", "o",
    // 0x70–0x7F
    "p", "q", "r", "s", "t", "u", "v", "w",
    "x", "y", "z", "braceleft", "bar", "braceright", "asciitilde", "",
    // 0x80–0x8F
    "Adieresis", "Aring", "Ccedilla", "Eacute", "Ntilde", "Odieresis", "Udieresis", "aacute",
    "agrave", "acircumflex", "adieresis", "atilde", "aring", "ccedilla", "eacute", "egrave",
    // 0x90–0x9F
    "ecircumflex", "edieresis", "iacute", "igrave", "icircumflex", "idieresis", "ntilde", "oacute",
    "ograve", "ocircumflex", "odieresis", "otilde", "uacute", "ugrave", "ucircumflex", "udieresis",
    // 0xA0–0xAF
    "dagger", "degree", "cent", "sterling", "section", "bullet", "paragraph", "germandbls",
    "registered", "copyright", "trademark", "acute", "dieresis", "notequal", "AE", "Oslash",
    // 0xB0–0xBF
    "infinity", "plusminus", "lessequal", "greaterequal", "yen", "mu", "partialdiff", "summation",
    "product", "pi", "integral", "ordfeminine", "ordmasculine", "Omega", "ae", "oslash",
    // 0xC0–0xCF
    "questiondown", "exclamdown", "logicalnot", "radical", "florin", "approxequal", "Delta", "guillemotleft",
    "guillemotright", "ellipsis", "space", "Agrave", "Atilde", "Otilde", "OE", "oe",
    // 0xD0–0xDF
    "endash", "emdash", "quotedblleft", "quotedblright", "quoteleft", "quoteright", "divide", "lozenge",
    "ydieresis", "Ydieresis", "fraction", "currency", "guilsinglleft", "guilsinglright", "fi", "fl",
    // 0xE0–0xEF
    "daggerdbl", "periodcentered", "quotesinglbase", "quotedblbase", "perthousand", "Acircumflex", "Ecircumflex", "Aacute",
    "Edieresis", "Egrave", "Iacute", "Icircumflex", "Idieresis", "Igrave", "Oacute", "Ocircumflex",
    // 0xF0–0xFF
    "apple", "Ograve", "Uacute", "Ucircumflex", "Ugrave", "dotlessi", "circumflex", "tilde",
    "macron", "breve", "dotaccent", "ring", "cedilla", "hungarumlaut", "ogonek", "caron",
];

/// Adobe Glyph List のサブセット（グリフ名 → Unicode）。
///
/// Standard / WinAnsi / MacRoman / PDFDoc の各エンコーディング表に登場する
/// グリフ名を網羅する。**名前順にソート済み**（[`glyph_name_to_unicode`] が
/// 二分探索するため）。値は AGL（Adobe Glyph List 2.0）由来。
#[rustfmt::skip]
static AGL: &[(&str, char)] = &[
    ("A", 'A'),
    ("AE", '\u{00C6}'),
    ("Aacute", '\u{00C1}'),
    ("Acircumflex", '\u{00C2}'),
    ("Adieresis", '\u{00C4}'),
    ("Agrave", '\u{00C0}'),
    ("Aring", '\u{00C5}'),
    ("Atilde", '\u{00C3}'),
    ("B", 'B'),
    ("C", 'C'),
    ("Ccedilla", '\u{00C7}'),
    ("D", 'D'),
    ("Delta", '\u{2206}'),
    ("E", 'E'),
    ("Eacute", '\u{00C9}'),
    ("Ecircumflex", '\u{00CA}'),
    ("Edieresis", '\u{00CB}'),
    ("Egrave", '\u{00C8}'),
    ("Eth", '\u{00D0}'),
    ("Euro", '\u{20AC}'),
    ("F", 'F'),
    ("G", 'G'),
    ("H", 'H'),
    ("I", 'I'),
    ("Iacute", '\u{00CD}'),
    ("Icircumflex", '\u{00CE}'),
    ("Idieresis", '\u{00CF}'),
    ("Igrave", '\u{00CC}'),
    ("J", 'J'),
    ("K", 'K'),
    ("L", 'L'),
    ("Lslash", '\u{0141}'),
    ("M", 'M'),
    ("N", 'N'),
    ("Ntilde", '\u{00D1}'),
    ("O", 'O'),
    ("OE", '\u{0152}'),
    ("Oacute", '\u{00D3}'),
    ("Ocircumflex", '\u{00D4}'),
    ("Odieresis", '\u{00D6}'),
    ("Ograve", '\u{00D2}'),
    ("Omega", '\u{2126}'),
    ("Oslash", '\u{00D8}'),
    ("Otilde", '\u{00D5}'),
    ("P", 'P'),
    ("Q", 'Q'),
    ("R", 'R'),
    ("S", 'S'),
    ("Scaron", '\u{0160}'),
    ("T", 'T'),
    ("Thorn", '\u{00DE}'),
    ("U", 'U'),
    ("Uacute", '\u{00DA}'),
    ("Ucircumflex", '\u{00DB}'),
    ("Udieresis", '\u{00DC}'),
    ("Ugrave", '\u{00D9}'),
    ("V", 'V'),
    ("W", 'W'),
    ("X", 'X'),
    ("Y", 'Y'),
    ("Yacute", '\u{00DD}'),
    ("Ydieresis", '\u{0178}'),
    ("Z", 'Z'),
    ("Zcaron", '\u{017D}'),
    ("a", 'a'),
    ("aacute", '\u{00E1}'),
    ("acircumflex", '\u{00E2}'),
    ("acute", '\u{00B4}'),
    ("adieresis", '\u{00E4}'),
    ("ae", '\u{00E6}'),
    ("agrave", '\u{00E0}'),
    ("ampersand", '&'),
    ("apple", '\u{F8FF}'),
    ("approxequal", '\u{2248}'),
    ("aring", '\u{00E5}'),
    ("asciicircum", '^'),
    ("asciitilde", '~'),
    ("asterisk", '*'),
    ("at", '@'),
    ("atilde", '\u{00E3}'),
    ("b", 'b'),
    ("backslash", '\\'),
    ("bar", '|'),
    ("braceleft", '{'),
    ("braceright", '}'),
    ("bracketleft", '['),
    ("bracketright", ']'),
    ("breve", '\u{02D8}'),
    ("brokenbar", '\u{00A6}'),
    ("bullet", '\u{2022}'),
    ("c", 'c'),
    ("caron", '\u{02C7}'),
    ("ccedilla", '\u{00E7}'),
    ("cedilla", '\u{00B8}'),
    ("cent", '\u{00A2}'),
    ("circumflex", '\u{02C6}'),
    ("colon", ':'),
    ("comma", ','),
    ("copyright", '\u{00A9}'),
    ("currency", '\u{00A4}'),
    ("d", 'd'),
    ("dagger", '\u{2020}'),
    ("daggerdbl", '\u{2021}'),
    ("degree", '\u{00B0}'),
    ("dieresis", '\u{00A8}'),
    ("divide", '\u{00F7}'),
    ("dollar", '$'),
    ("dotaccent", '\u{02D9}'),
    ("dotlessi", '\u{0131}'),
    ("e", 'e'),
    ("eacute", '\u{00E9}'),
    ("ecircumflex", '\u{00EA}'),
    ("edieresis", '\u{00EB}'),
    ("egrave", '\u{00E8}'),
    ("eight", '8'),
    ("ellipsis", '\u{2026}'),
    ("emdash", '\u{2014}'),
    ("endash", '\u{2013}'),
    ("equal", '='),
    ("eth", '\u{00F0}'),
    ("exclam", '!'),
    ("exclamdown", '\u{00A1}'),
    ("f", 'f'),
    ("fi", '\u{FB01}'),
    ("five", '5'),
    ("fl", '\u{FB02}'),
    ("florin", '\u{0192}'),
    ("four", '4'),
    ("fraction", '\u{2044}'),
    ("g", 'g'),
    ("germandbls", '\u{00DF}'),
    ("grave", '`'),
    ("greater", '>'),
    ("greaterequal", '\u{2265}'),
    ("guillemotleft", '\u{00AB}'),
    ("guillemotright", '\u{00BB}'),
    ("guilsinglleft", '\u{2039}'),
    ("guilsinglright", '\u{203A}'),
    ("h", 'h'),
    ("hungarumlaut", '\u{02DD}'),
    ("hyphen", '-'),
    ("i", 'i'),
    ("iacute", '\u{00ED}'),
    ("icircumflex", '\u{00EE}'),
    ("idieresis", '\u{00EF}'),
    ("igrave", '\u{00EC}'),
    ("infinity", '\u{221E}'),
    ("integral", '\u{222B}'),
    ("j", 'j'),
    ("k", 'k'),
    ("l", 'l'),
    ("less", '<'),
    ("lessequal", '\u{2264}'),
    ("logicalnot", '\u{00AC}'),
    ("lozenge", '\u{25CA}'),
    ("lslash", '\u{0142}'),
    ("m", 'm'),
    ("macron", '\u{00AF}'),
    ("minus", '\u{2212}'),
    ("mu", '\u{00B5}'),
    ("multiply", '\u{00D7}'),
    ("n", 'n'),
    ("nine", '9'),
    ("notequal", '\u{2260}'),
    ("ntilde", '\u{00F1}'),
    ("numbersign", '#'),
    ("o", 'o'),
    ("oacute", '\u{00F3}'),
    ("ocircumflex", '\u{00F4}'),
    ("odieresis", '\u{00F6}'),
    ("oe", '\u{0153}'),
    ("ogonek", '\u{02DB}'),
    ("ograve", '\u{00F2}'),
    ("one", '1'),
    ("ordfeminine", '\u{00AA}'),
    ("ordmasculine", '\u{00BA}'),
    ("oslash", '\u{00F8}'),
    ("otilde", '\u{00F5}'),
    ("p", 'p'),
    ("paragraph", '\u{00B6}'),
    ("partialdiff", '\u{2202}'),
    ("percent", '%'),
    ("period", '.'),
    ("periodcentered", '\u{00B7}'),
    ("perthousand", '\u{2030}'),
    ("pi", '\u{03C0}'),
    ("plus", '+'),
    ("plusminus", '\u{00B1}'),
    ("product", '\u{220F}'),
    ("q", 'q'),
    ("question", '?'),
    ("questiondown", '\u{00BF}'),
    ("quotedbl", '"'),
    ("quotedblbase", '\u{201E}'),
    ("quotedblleft", '\u{201C}'),
    ("quotedblright", '\u{201D}'),
    ("quoteleft", '\u{2018}'),
    ("quoteright", '\u{2019}'),
    ("quotesinglbase", '\u{201A}'),
    ("quotesingle", '\''),
    ("r", 'r'),
    ("radical", '\u{221A}'),
    ("registered", '\u{00AE}'),
    ("ring", '\u{02DA}'),
    ("s", 's'),
    ("scaron", '\u{0161}'),
    ("section", '\u{00A7}'),
    ("semicolon", ';'),
    ("seven", '7'),
    ("six", '6'),
    ("slash", '/'),
    ("space", ' '),
    ("sterling", '\u{00A3}'),
    ("summation", '\u{2211}'),
    ("t", 't'),
    ("thorn", '\u{00FE}'),
    ("three", '3'),
    ("tilde", '\u{02DC}'),
    ("trademark", '\u{2122}'),
    ("two", '2'),
    ("u", 'u'),
    ("uacute", '\u{00FA}'),
    ("ucircumflex", '\u{00FB}'),
    ("udieresis", '\u{00FC}'),
    ("ugrave", '\u{00F9}'),
    ("underscore", '_'),
    ("v", 'v'),
    ("w", 'w'),
    ("x", 'x'),
    ("y", 'y'),
    ("yacute", '\u{00FD}'),
    ("ydieresis", '\u{00FF}'),
    ("yen", '\u{00A5}'),
    ("z", 'z'),
    ("zcaron", '\u{017E}'),
    ("zero", '0'),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// AGL 表が名前順にソート済みであること（二分探索の前提）を検証。
    #[test]
    fn agl_is_sorted() {
        for w in AGL.windows(2) {
            assert!(w[0].0 < w[1].0, "未ソート: {} >= {}", w[0].0, w[1].0);
        }
    }

    #[test]
    fn standard_encoding_spots() {
        assert_eq!(standard_encoding(0x41), Some('A'));
        assert_eq!(standard_encoding(0x20), Some(' '));
        // StandardEncoding 固有: quoteright / quoteleft
        assert_eq!(standard_encoding(0x27), Some('\u{2019}'));
        assert_eq!(standard_encoding(0x60), Some('\u{2018}'));
        // 記号類
        assert_eq!(standard_encoding(0xA4), Some('\u{2044}')); // fraction
        assert_eq!(standard_encoding(0xB7), Some('\u{2022}')); // bullet
        assert_eq!(standard_encoding(0xAE), Some('\u{FB01}')); // fi
        assert_eq!(standard_encoding(0xAF), Some('\u{FB02}')); // fl
        assert_eq!(standard_encoding(0xD0), Some('\u{2014}')); // emdash
        assert_eq!(standard_encoding(0xE1), Some('\u{00C6}')); // AE
        assert_eq!(standard_encoding(0xF1), Some('\u{00E6}')); // ae
        assert_eq!(standard_encoding(0xFB), Some('\u{00DF}')); // germandbls
                                                               // 未定義
        assert_eq!(standard_encoding(0x00), None);
        assert_eq!(standard_encoding(0x1F), None);
        assert_eq!(standard_encoding(0x7F), None);
        assert_eq!(standard_encoding(0x80), None);
        assert_eq!(standard_encoding(0xA0), None);
        assert_eq!(standard_encoding(0xB0), None);
    }

    #[test]
    fn mac_roman_encoding_spots() {
        assert_eq!(mac_roman_encoding(0x41), Some('A'));
        assert_eq!(mac_roman_encoding(0x20), Some(' '));
        assert_eq!(mac_roman_encoding(0x27), Some('\'')); // quotesingle
        assert_eq!(mac_roman_encoding(0x80), Some('\u{00C4}')); // Adieresis
        assert_eq!(mac_roman_encoding(0x8E), Some('\u{00E9}')); // eacute
        assert_eq!(mac_roman_encoding(0xA5), Some('\u{2022}')); // bullet
        assert_eq!(mac_roman_encoding(0xA8), Some('\u{00AE}')); // registered
        assert_eq!(mac_roman_encoding(0xD0), Some('\u{2013}')); // endash
        assert_eq!(mac_roman_encoding(0xD1), Some('\u{2014}')); // emdash
        assert_eq!(mac_roman_encoding(0xDB), Some('\u{00A4}')); // currency
        assert_eq!(mac_roman_encoding(0xDE), Some('\u{FB01}')); // fi
        assert_eq!(mac_roman_encoding(0xAE), Some('\u{00C6}')); // AE
        assert_eq!(mac_roman_encoding(0xCA), Some(' ')); // space (nbsp 扱いも space グリフ)
                                                         // 未定義
        assert_eq!(mac_roman_encoding(0x00), None);
        assert_eq!(mac_roman_encoding(0x1F), None);
        assert_eq!(mac_roman_encoding(0x7F), None);
    }

    #[test]
    fn glyph_name_spots() {
        assert_eq!(glyph_name_to_unicode("A"), Some('A'));
        assert_eq!(glyph_name_to_unicode("eacute"), Some('\u{00E9}'));
        assert_eq!(glyph_name_to_unicode("bullet"), Some('\u{2022}'));
        assert_eq!(glyph_name_to_unicode("fi"), Some('\u{FB01}'));
        assert_eq!(glyph_name_to_unicode("space"), Some(' '));
        assert_eq!(glyph_name_to_unicode("germandbls"), Some('\u{00DF}'));
    }

    #[test]
    fn glyph_name_uni_forms() {
        // uniXXXX（4 桁固定）
        assert_eq!(glyph_name_to_unicode("uni3042"), Some('あ'));
        assert_eq!(glyph_name_to_unicode("uni0041"), Some('A'));
        // サロゲートは None
        assert_eq!(glyph_name_to_unicode("uniD800"), None);
        // 桁数違いは uniXXXX としては不成立
        assert_eq!(glyph_name_to_unicode("uni304"), None);
        // uXXXX〜uXXXXXX
        assert_eq!(glyph_name_to_unicode("u1F600"), char::from_u32(0x1F600));
        assert_eq!(glyph_name_to_unicode("u0041"), Some('A'));
        // 桁数外
        assert_eq!(glyph_name_to_unicode("u041"), None);
        assert_eq!(glyph_name_to_unicode("u1234567"), None);
    }

    #[test]
    fn glyph_name_unknown_and_ascii() {
        // 不明名は None
        assert_eq!(glyph_name_to_unicode("glyph12345"), None);
        assert_eq!(glyph_name_to_unicode("g42"), None);
        // ASCII 1 文字はその文字（AGL 慣行、ただし表にある "A" 等は表優先）
        assert_eq!(glyph_name_to_unicode("$"), Some('$'));
        assert_eq!(glyph_name_to_unicode("+"), Some('+'));
        // 空文字列は None
        assert_eq!(glyph_name_to_unicode(""), None);
        // 非 ASCII 1 文字は None
        assert_eq!(glyph_name_to_unicode("あ"), None);
    }
}
