//! テキスト検索 API（ビューワーの Ctrl+F 用）。
//!
//! [`crate::text::TextSpan`]（位置付きテキスト抽出）を素材に、ページ内の
//! 文字列マッチとハイライト矩形の計算を行う。データフローは:
//!
//! ```text
//! extract_text_spans ──▶ 文字列化（スパン連結 + 行判定）──▶ 照合 ──▶ SearchHit { rects }
//! ```
//!
//! ## スパン連結のヒューリスティック
//!
//! コンテント順に隣接するスパンを次の規則で 1 本のテキストに連結する:
//!
//! - **同一行判定**: 境界箱の縦中心の差が実効フォントサイズの一定割合
//!   （[`LINE_Y_TOLERANCE`]）以内なら同じ行とみなす。
//! - 同じ行で水平方向の隙間が一定割合（[`GAP_SPACE_RATIO`]）を超えるとき、
//!   空白 1 個を仮定する（Chromium 系の「1 グリフ 1 Tj」で空白が
//!   字送りのみで表現される PDF への対応）。
//! - 行を跨ぐ境界には空白 1 個を仮定する（クエリ側の改行・空白は
//!   どちらも空白 1 個に正規化して照合する）。
//!
//! ヒットの矩形は行ごとにマージし、行を跨ぐヒットは複数の矩形に分かれる。
//! 座標はページのユーザー空間（原点左下・ポイント単位）で、`/Rotate` は
//! 適用しない（[`crate::text::TextSpan`] と同じ規約）。

use crate::document::Document;
use crate::error::Result;
use crate::text::TextSpan;

/// 同一行とみなす縦中心差の上限（実効フォントサイズに対する割合）。
const LINE_Y_TOLERANCE: f64 = 0.5;

/// 空白 1 個を仮定する水平隙間の下限（実効フォントサイズに対する割合）。
const GAP_SPACE_RATIO: f64 = 0.3;

/// 検索オプション。
#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    /// 大文字小文字を区別しない（Unicode 単純小文字化で比較）。既定 `false`。
    pub case_insensitive: bool,
}

/// 検索ヒット 1 件。
#[derive(Debug, Clone)]
pub struct SearchHit {
    /// ハイライト矩形 `[x0, y0, x1, y1]` の列（ページのユーザー空間）。
    ///
    /// 行ごとにマージ済みで、ヒットが行を跨ぐ場合は行数分の矩形になる。
    pub rects: Vec<[f64; 4]>,
}

impl Document {
    /// ページ内をテキスト検索する（`index` は 0 始まり）。
    ///
    /// ヒットは出現順（コンテント順ベース）で返り、互いに重ならない
    /// （前のヒットの直後から次を探す）。空のクエリは空の結果を返す。
    /// 正規表現は対応しない（リテラル一致のみ）。
    pub fn search_page(
        &self,
        index: usize,
        query: &str,
        options: &SearchOptions,
    ) -> Result<Vec<SearchHit>> {
        let spans = self.extract_text_spans(index)?;
        Ok(search_spans(&spans, query, options))
    }

    /// 全ページをテキスト検索する。`(ページ番号, ヒット)` の列を
    /// ページ順で返す。
    pub fn search(&self, query: &str, options: &SearchOptions) -> Result<Vec<(usize, SearchHit)>> {
        let mut out = Vec::new();
        for index in 0..self.page_count() {
            for hit in self.search_page(index, query, options)? {
                out.push((index, hit));
            }
        }
        Ok(out)
    }
}

/// 照合用の 1 文字。スパンのグリフ由来（矩形あり）か、連結時に仮定した
/// 空白（矩形なし）のどちらか。
struct SearchChar {
    ch: char,
    /// この文字のハイライト矩形。仮定空白は `None`。
    rect: Option<[f64; 4]>,
    /// 所属する行の番号（矩形の行マージ用）。
    line: u32,
}

/// スパン列に対して検索を実行する（本体。スパンは抽出順のまま渡す）。
pub(crate) fn search_spans(
    spans: &[TextSpan],
    query: &str,
    options: &SearchOptions,
) -> Vec<SearchHit> {
    let needle: Vec<char> = query
        .chars()
        .map(|c| fold_char(c, options.case_insensitive))
        .collect();
    if needle.is_empty() {
        return Vec::new();
    }

    let chars = build_search_chars(spans);
    let haystack: Vec<char> = chars
        .iter()
        .map(|c| fold_char(c.ch, options.case_insensitive))
        .collect();

    let mut hits = Vec::new();
    let mut i = 0;
    while i + needle.len() <= haystack.len() {
        if haystack[i..i + needle.len()] == needle[..] {
            if let Some(hit) = build_hit(&chars[i..i + needle.len()]) {
                hits.push(hit);
            }
            i += needle.len(); // ヒット同士は重ねない（ビューワーの Ctrl+F 挙動）
        } else {
            i += 1;
        }
    }
    hits
}

/// 照合用に 1 文字を正規化する。空白類は空白 1 個に、大文字小文字を
/// 区別しない場合は Unicode 単純小文字化（先頭 1 文字）に畳む。
fn fold_char(c: char, case_insensitive: bool) -> char {
    if c.is_whitespace() {
        return ' ';
    }
    if case_insensitive {
        c.to_lowercase().next().unwrap_or(c)
    } else {
        c
    }
}

/// スパン列を照合用の文字列（文字 + 矩形 + 行番号）へ展開する。
fn build_search_chars(spans: &[TextSpan]) -> Vec<SearchChar> {
    let mut out: Vec<SearchChar> = Vec::new();
    let mut line = 0u32;
    let mut prev: Option<&TextSpan> = None;

    for span in spans {
        if span.text.is_empty() {
            continue;
        }
        if let Some(p) = prev {
            let size = effective_size(p, span);
            if is_same_line(p, span, size) {
                // 同じ行: 水平の隙間が大きければ空白 1 個を仮定する。
                let gap = span.bbox[0] - p.bbox[2];
                if gap.is_finite() && gap > GAP_SPACE_RATIO * size {
                    out.push(SearchChar {
                        ch: ' ',
                        rect: None,
                        line,
                    });
                }
            } else {
                // 行を跨ぐ: 空白 1 個を仮定する。
                line += 1;
                out.push(SearchChar {
                    ch: ' ',
                    rect: None,
                    line,
                });
            }
        }
        for glyph in &span.glyphs {
            for ch in glyph.text.chars() {
                out.push(SearchChar {
                    ch,
                    rect: Some(glyph.bbox),
                    line,
                });
            }
        }
        prev = Some(span);
    }
    out
}

/// 2 スパンの行判定・隙間判定に使う実効サイズ。フォントサイズが取れない
/// （0 や非有限の）場合は境界箱の高さで代用する。
fn effective_size(a: &TextSpan, b: &TextSpan) -> f64 {
    let fs = a.font_size.max(b.font_size);
    if fs.is_finite() && fs > 0.0 {
        return fs;
    }
    let ha = a.bbox[3] - a.bbox[1];
    let hb = b.bbox[3] - b.bbox[1];
    ha.max(hb).max(1e-6)
}

/// 境界箱の縦中心の差が実効サイズの [`LINE_Y_TOLERANCE`] 以内なら同一行。
fn is_same_line(a: &TextSpan, b: &TextSpan, size: f64) -> bool {
    let cy_a = (a.bbox[1] + a.bbox[3]) / 2.0;
    let cy_b = (b.bbox[1] + b.bbox[3]) / 2.0;
    (cy_a - cy_b).abs() <= LINE_Y_TOLERANCE * size
}

/// マッチした文字列からヒット（行ごとにマージした矩形）を作る。
///
/// 矩形を持つ文字が 1 つもない（仮定空白だけの）マッチは `None`。
fn build_hit(matched: &[SearchChar]) -> Option<SearchHit> {
    let mut rects: Vec<[f64; 4]> = Vec::new();
    let mut current: Option<(u32, [f64; 4])> = None;
    for c in matched {
        let r = match c.rect {
            Some(r) => r,
            None => continue,
        };
        match &mut current {
            Some((line, acc)) if *line == c.line => {
                acc[0] = acc[0].min(r[0]);
                acc[1] = acc[1].min(r[1]);
                acc[2] = acc[2].max(r[2]);
                acc[3] = acc[3].max(r[3]);
            }
            Some((_, acc)) => {
                rects.push(*acc);
                current = Some((c.line, r));
            }
            None => current = Some((c.line, r)),
        }
    }
    if let Some((_, acc)) = current {
        rects.push(acc);
    }
    if rects.is_empty() {
        None
    } else {
        Some(SearchHit { rects })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::SpanGlyph;

    /// テスト用スパン: 原点 `(x, y)`（左下）、フォントサイズ `fs`、
    /// 1 文字 = fs/2 幅の等幅でグリフ箱を並べる。
    fn span(text: &str, x: f64, y: f64, fs: f64) -> TextSpan {
        let w = fs * 0.5;
        let glyphs: Vec<SpanGlyph> = text
            .chars()
            .enumerate()
            .map(|(i, ch)| SpanGlyph {
                text: ch.to_string(),
                bbox: [x + i as f64 * w, y, x + (i + 1) as f64 * w, y + fs],
            })
            .collect();
        let n = text.chars().count() as f64;
        TextSpan {
            text: text.into(),
            bbox: [x, y, x + n * w, y + fs],
            font_size: fs,
            glyphs,
        }
    }

    fn opts(ci: bool) -> SearchOptions {
        SearchOptions {
            case_insensitive: ci,
        }
    }

    /// 単一スパン内の一致: 矩形が該当文字のグリフ箱の合併になる。
    #[test]
    fn single_span_match_rect() {
        let spans = vec![span("Hello World", 10.0, 100.0, 10.0)];
        let hits = search_spans(&spans, "World", &opts(false));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].rects.len(), 1);
        // "World" は 7 文字目（index 6）から 5 文字。1 文字幅 5pt。
        let r = hits[0].rects[0];
        assert!((r[0] - (10.0 + 6.0 * 5.0)).abs() < 1e-9);
        assert!((r[2] - (10.0 + 11.0 * 5.0)).abs() < 1e-9);
        assert_eq!((r[1], r[3]), (100.0, 110.0));
    }

    /// 大文字小文字: 既定は区別、case_insensitive で一致。
    #[test]
    fn case_sensitivity() {
        let spans = vec![span("Hello", 0.0, 0.0, 10.0)];
        assert!(search_spans(&spans, "hello", &opts(false)).is_empty());
        assert_eq!(search_spans(&spans, "hello", &opts(true)).len(), 1);
        assert_eq!(search_spans(&spans, "HELLO", &opts(true)).len(), 1);
    }

    /// スパン跨ぎ（同一行・隙間なし）: 直接連結して一致する。
    #[test]
    fn cross_span_same_line() {
        // "He" の直後（x=10）から "llo"（Chromium の 1 グリフ 1 Tj 相当）。
        let spans = vec![span("He", 0.0, 0.0, 10.0), span("llo", 10.0, 0.0, 10.0)];
        let hits = search_spans(&spans, "Hello", &opts(false));
        assert_eq!(hits.len(), 1);
        // 同一行なので矩形は 1 つにマージされる。
        assert_eq!(hits[0].rects.len(), 1);
        let r = hits[0].rects[0];
        assert_eq!((r[0], r[2]), (0.0, 25.0));
    }

    /// 同一行の大きな隙間には空白 1 個を仮定する。
    #[test]
    fn gap_implies_space() {
        // "AB"（x=0..10）と "CD"（x=20..30）: 隙間 10pt > 0.3 × 10pt。
        let spans = vec![span("AB", 0.0, 0.0, 10.0), span("CD", 20.0, 0.0, 10.0)];
        assert_eq!(search_spans(&spans, "AB CD", &opts(false)).len(), 1);
        // 隙間があるので直接連結 "ABCD" では一致しない。
        assert!(search_spans(&spans, "ABCD", &opts(false)).is_empty());
    }

    /// 行を跨ぐ一致: 空白 1 個を仮定し、矩形は行ごとに分かれる。
    #[test]
    fn cross_line_match_splits_rects() {
        let spans = vec![span("one", 0.0, 100.0, 10.0), span("two", 0.0, 80.0, 10.0)];
        let hits = search_spans(&spans, "one two", &opts(false));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].rects.len(), 2);
        assert_eq!(hits[0].rects[0][1], 100.0); // 1 行目
        assert_eq!(hits[0].rects[1][1], 80.0); // 2 行目
                                               // クエリ側の改行も空白 1 個として照合できる。
        assert_eq!(search_spans(&spans, "one\ntwo", &opts(false)).len(), 1);
    }

    /// ヒット同士は重ならない（"aaa" から "aa" は 1 件）。
    #[test]
    fn hits_do_not_overlap() {
        let spans = vec![span("aaa", 0.0, 0.0, 10.0)];
        assert_eq!(search_spans(&spans, "aa", &opts(false)).len(), 1);
        // 複数ヒット: "ab ab" から "ab" は 2 件。
        let spans = vec![span("ab ab", 0.0, 0.0, 10.0)];
        assert_eq!(search_spans(&spans, "ab", &opts(false)).len(), 2);
    }

    /// 空クエリ・空白だけのクエリ・不一致は空の結果。
    #[test]
    fn empty_and_no_match() {
        let spans = vec![span("one", 0.0, 100.0, 10.0), span("two", 0.0, 80.0, 10.0)];
        assert!(search_spans(&spans, "", &opts(false)).is_empty());
        assert!(search_spans(&spans, "xyz", &opts(false)).is_empty());
        // 仮定空白（行間）だけにマッチするクエリは矩形が作れないので返さない。
        assert!(search_spans(&spans, " ", &opts(false)).is_empty());
        // スパンが空でも panic しない。
        assert!(search_spans(&[], "a", &opts(false)).is_empty());
    }

    /// 上付き（rise 相当の小さな y ずれ）は同一行として扱う。
    #[test]
    fn small_y_shift_same_line() {
        // 2 つ目のスパンが 3pt 上（< 0.5 × 10pt）。
        let spans = vec![span("x", 0.0, 0.0, 10.0), span("2", 5.0, 3.0, 10.0)];
        let hits = search_spans(&spans, "x2", &opts(false));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].rects.len(), 1);
    }
}
