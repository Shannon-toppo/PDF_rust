//! フォント関連: 標準 14 フォントのメトリクスと WinAnsi エンコーディング。
//!
//! PDF ビューアが必ず内蔵する「標準 14 フォント」（§9.6.2.2）は、
//! フォントファイルを埋め込まずに利用できる。本モジュールはそのうち
//! 主要書体の文字幅テーブル（AFM 由来、1000 分の 1 em 単位）を持ち、
//! テキスト幅の計算（レイアウト補助）に使う。

/// 標準 14 フォント。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StandardFont {
    Helvetica,
    HelveticaBold,
    HelveticaOblique,
    HelveticaBoldOblique,
    TimesRoman,
    TimesBold,
    TimesItalic,
    TimesBoldItalic,
    Courier,
    CourierBold,
    CourierOblique,
    CourierBoldOblique,
    Symbol,
    ZapfDingbats,
}

impl StandardFont {
    /// PDF の `/BaseFont` 名。
    pub fn base_font(&self) -> &'static str {
        match self {
            StandardFont::Helvetica => "Helvetica",
            StandardFont::HelveticaBold => "Helvetica-Bold",
            StandardFont::HelveticaOblique => "Helvetica-Oblique",
            StandardFont::HelveticaBoldOblique => "Helvetica-BoldOblique",
            StandardFont::TimesRoman => "Times-Roman",
            StandardFont::TimesBold => "Times-Bold",
            StandardFont::TimesItalic => "Times-Italic",
            StandardFont::TimesBoldItalic => "Times-BoldItalic",
            StandardFont::Courier => "Courier",
            StandardFont::CourierBold => "Courier-Bold",
            StandardFont::CourierOblique => "Courier-Oblique",
            StandardFont::CourierBoldOblique => "Courier-BoldOblique",
            StandardFont::Symbol => "Symbol",
            StandardFont::ZapfDingbats => "ZapfDingbats",
        }
    }

    /// ASCII 文字の幅（1000 分の 1 em 単位）。表にない文字は 500 を返す。
    pub fn char_width(&self, c: char) -> u16 {
        let idx = c as usize;
        if !(0x20..=0x7E).contains(&idx) {
            return 500;
        }
        let i = idx - 0x20;
        match self {
            StandardFont::Courier
            | StandardFont::CourierBold
            | StandardFont::CourierOblique
            | StandardFont::CourierBoldOblique => 600,
            StandardFont::Helvetica | StandardFont::HelveticaOblique => HELVETICA_WIDTHS[i],
            StandardFont::HelveticaBold | StandardFont::HelveticaBoldOblique => {
                HELVETICA_BOLD_WIDTHS[i]
            }
            StandardFont::TimesRoman => TIMES_ROMAN_WIDTHS[i],
            // 簡易版: イタリック・ボールド系は Roman の幅で近似
            StandardFont::TimesBold | StandardFont::TimesItalic | StandardFont::TimesBoldItalic => {
                TIMES_ROMAN_WIDTHS[i]
            }
            StandardFont::Symbol | StandardFont::ZapfDingbats => 500,
        }
    }

    /// 文字列をフォントサイズ `size` で描画したときの幅（ポイント単位）。
    pub fn measure_text(&self, text: &str, size: f64) -> f64 {
        let units: u64 = text.chars().map(|c| self.char_width(c) as u64).sum();
        units as f64 * size / 1000.0
    }
}

/// Helvetica の幅テーブル（U+0020..U+007E、AFM 由来）。
const HELVETICA_WIDTHS: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278, // sp..'/'
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, // 0..9
    278, 278, 584, 584, 584, 556, 1015, // :..@
    667, 667, 722, 722, 667, 611, 778, 722, 278, 500, 667, 556, 833, 722, 778, 667, 778, 722, 667,
    611, 722, 667, 944, 667, 667, 611, // A..Z
    278, 278, 278, 469, 556, 333, // [..`
    556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500, 222, 833, 556, 556, 556, 556, 333, 500,
    278, 556, 500, 722, 500, 500, 500, // a..z
    334, 260, 334, 584, // {..~
];

/// Helvetica-Bold の幅テーブル。
const HELVETICA_BOLD_WIDTHS: [u16; 95] = [
    278, 333, 474, 556, 556, 889, 722, 238, 333, 333, 389, 584, 278, 333, 278, 278, 556, 556, 556,
    556, 556, 556, 556, 556, 556, 556, 333, 333, 584, 584, 584, 611, 975, 722, 722, 722, 722, 667,
    611, 778, 722, 278, 556, 722, 611, 833, 722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667,
    667, 611, 333, 278, 333, 584, 556, 333, 556, 611, 556, 611, 556, 333, 611, 611, 278, 278, 556,
    278, 889, 611, 611, 611, 611, 389, 556, 333, 611, 556, 778, 556, 556, 500, 389, 280, 389, 584,
];

/// Times-Roman の幅テーブル。
const TIMES_ROMAN_WIDTHS: [u16; 95] = [
    250, 333, 408, 500, 500, 833, 778, 180, 333, 333, 500, 564, 250, 333, 250, 278, 500, 500, 500,
    500, 500, 500, 500, 500, 500, 500, 278, 278, 564, 564, 564, 444, 921, 722, 667, 667, 722, 611,
    556, 722, 722, 333, 389, 722, 611, 889, 722, 722, 556, 722, 667, 556, 611, 722, 722, 944, 722,
    722, 611, 333, 278, 333, 469, 500, 333, 444, 500, 444, 500, 444, 333, 500, 500, 278, 278, 500,
    278, 778, 500, 500, 500, 500, 333, 389, 278, 500, 500, 722, 500, 500, 444, 480, 200, 480, 541,
];

/// WinAnsiEncoding（CP1252 相当, §Annex D.2）のバイト → Unicode 変換。
///
/// 0x80..0x9F の領域は CP1252 固有のマッピングを持つ。
pub fn winansi_to_char(b: u8) -> char {
    match b {
        0x80 => '€',
        0x82 => '‚',
        0x83 => 'ƒ',
        0x84 => '„',
        0x85 => '…',
        0x86 => '†',
        0x87 => '‡',
        0x88 => 'ˆ',
        0x89 => '‰',
        0x8A => 'Š',
        0x8B => '‹',
        0x8C => 'Œ',
        0x8E => 'Ž',
        0x91 => '\u{2018}', // '
        0x92 => '\u{2019}', // '
        0x93 => '\u{201C}', // "
        0x94 => '\u{201D}', // "
        0x95 => '•',
        0x96 => '–',
        0x97 => '—',
        0x98 => '˜',
        0x99 => '™',
        0x9A => 'š',
        0x9B => '›',
        0x9C => 'œ',
        0x9E => 'ž',
        0x9F => 'Ÿ',
        // 未定義コード（0x81 など）は U+FFFD ではなく空白に近い扱い
        0x81 | 0x8D | 0x8F | 0x90 | 0x9D => '\u{FFFD}',
        // それ以外は Latin-1 と同一
        b => b as char,
    }
}

/// Unicode 文字 → WinAnsi バイト（書き込み用）。表せない文字は `None`。
pub fn char_to_winansi(c: char) -> Option<u8> {
    let cp = c as u32;
    match c {
        '€' => Some(0x80),
        '‚' => Some(0x82),
        'ƒ' => Some(0x83),
        '„' => Some(0x84),
        '…' => Some(0x85),
        '†' => Some(0x86),
        '‡' => Some(0x87),
        'ˆ' => Some(0x88),
        '‰' => Some(0x89),
        'Š' => Some(0x8A),
        '‹' => Some(0x8B),
        'Œ' => Some(0x8C),
        'Ž' => Some(0x8E),
        '\u{2018}' => Some(0x91),
        '\u{2019}' => Some(0x92),
        '\u{201C}' => Some(0x93),
        '\u{201D}' => Some(0x94),
        '•' => Some(0x95),
        '–' => Some(0x96),
        '—' => Some(0x97),
        '˜' => Some(0x98),
        '™' => Some(0x99),
        'š' => Some(0x9A),
        '›' => Some(0x9B),
        'œ' => Some(0x9C),
        'ž' => Some(0x9E),
        'Ÿ' => Some(0x9F),
        _ if cp < 0x80 => Some(cp as u8),
        _ if (0xA0..=0xFF).contains(&cp) => Some(cp as u8),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths() {
        assert_eq!(StandardFont::Helvetica.char_width('A'), 667);
        assert_eq!(StandardFont::Helvetica.char_width(' '), 278);
        assert_eq!(StandardFont::Courier.char_width('W'), 600);
        let w = StandardFont::Helvetica.measure_text("AA", 10.0);
        assert!((w - 13.34).abs() < 1e-9);
    }

    #[test]
    fn winansi_roundtrip() {
        for c in ['A', 'é', '€', '\u{201C}', '~'] {
            let b = char_to_winansi(c).unwrap();
            assert_eq!(winansi_to_char(b), c);
        }
        assert_eq!(char_to_winansi('あ'), None);
    }
}
