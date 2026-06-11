//! 統合テスト: テキスト検索・グリフ単位ボックス（Phase 8）。
//!
//! 「生成 → to_bytes → from_bytes → 検索/スパン抽出」の往復で、
//! 検索ヒットの位置とグリフ箱の整合性を検証する。

use pdf_rust::{Document, SearchOptions, TextOptions};

/// 2 行のテキストを持つ文書を作って往復したものを返す。
fn build_two_line_doc() -> Document {
    let mut doc = Document::new();
    doc.add_page(612.0, 792.0).unwrap();
    doc.add_text(
        0,
        "Hello World\nSecond line",
        &TextOptions {
            size: 12.0,
            x: 72.0,
            y: 720.0,
            ..Default::default()
        },
    )
    .unwrap();
    let bytes = doc.to_bytes().unwrap();
    Document::from_bytes(&bytes).unwrap()
}

/// 基本検索: 往復後の文書から 1 件ヒットし、矩形がスパンの範囲内にある。
#[test]
fn search_basic_roundtrip() {
    let doc = build_two_line_doc();
    let hits = doc
        .search_page(0, "World", &SearchOptions::default())
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].rects.len(), 1);

    // 矩形は 1 行目のスパン境界箱の内側（x は部分区間、y は同一）。
    let spans = doc.extract_text_spans(0).unwrap();
    let line1 = &spans[0];
    assert_eq!(line1.text, "Hello World");
    let r = hits[0].rects[0];
    let eps = 1e-6;
    assert!(r[0] >= line1.bbox[0] - eps && r[2] <= line1.bbox[2] + eps);
    assert!((r[1] - line1.bbox[1]).abs() < eps && (r[3] - line1.bbox[3]).abs() < eps);
    // "World" は行の後半にある。
    assert!(r[0] > line1.bbox[0] + (line1.bbox[2] - line1.bbox[0]) * 0.3);

    // 一致しないクエリ・大文字小文字違いは 0 件。
    assert!(doc
        .search_page(0, "world", &SearchOptions::default())
        .unwrap()
        .is_empty());
    assert!(doc
        .search_page(0, "xyzzy", &SearchOptions::default())
        .unwrap()
        .is_empty());
}

/// case_insensitive: 大文字小文字を無視して一致する。
#[test]
fn search_case_insensitive() {
    let doc = build_two_line_doc();
    let opts = SearchOptions {
        case_insensitive: true,
    };
    assert_eq!(doc.search_page(0, "world", &opts).unwrap().len(), 1);
    assert_eq!(doc.search_page(0, "SECOND", &opts).unwrap().len(), 1);
}

/// 行を跨ぐ検索: 空白 1 個を仮定して一致し、矩形は行ごとに分かれる。
#[test]
fn search_across_lines() {
    let doc = build_two_line_doc();
    let hits = doc
        .search_page(0, "World Second", &SearchOptions::default())
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].rects.len(), 2, "行ごとに 2 矩形になるはず");
    // 1 行目の矩形は 2 行目より上（y が大きい）。
    assert!(hits[0].rects[0][1] > hits[0].rects[1][3]);

    // クエリ側が改行でも一致する。
    let hits2 = doc
        .search_page(0, "World\nSecond", &SearchOptions::default())
        .unwrap();
    assert_eq!(hits2.len(), 1);
}

/// 全ページ検索: ページ番号付きでページ順に返る。
#[test]
fn search_all_pages() {
    let mut doc = Document::new();
    for i in 0..3 {
        doc.add_page(612.0, 792.0).unwrap();
        let text = if i == 1 { "needle here" } else { "hay only" };
        doc.add_text(
            i,
            text,
            &TextOptions {
                size: 12.0,
                x: 72.0,
                y: 720.0,
                ..Default::default()
            },
        )
        .unwrap();
    }
    let bytes = doc.to_bytes().unwrap();
    let doc = Document::from_bytes(&bytes).unwrap();

    let hits = doc.search("needle", &SearchOptions::default()).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0, 1, "2 ページ目（index 1）でヒットするはず");

    // 全ページにあるクエリは各ページから返る（"h": hay / here / hay）。
    let hits = doc.search("h", &SearchOptions::default()).unwrap();
    assert!(hits.iter().any(|(p, _)| *p == 0));
    assert!(hits.iter().any(|(p, _)| *p == 1));
    assert!(hits.iter().any(|(p, _)| *p == 2));
}

/// グリフ単位ボックス: テキストの連結一致・左から右への単調性・
/// スパン境界箱との整合。
#[test]
fn span_glyphs_consistent() {
    let doc = build_two_line_doc();
    let spans = doc.extract_text_spans(0).unwrap();
    assert!(!spans.is_empty());
    let eps = 1e-6;
    for span in &spans {
        // 連結するとスパンのテキストに一致する。
        let joined: String = span.glyphs.iter().map(|g| g.text.as_str()).collect();
        assert_eq!(joined, span.text);
        // グリフ箱は左から右へ並び、スパン境界箱に含まれる。
        let mut prev_x = f64::NEG_INFINITY;
        for g in &span.glyphs {
            assert!(g.bbox[0] >= prev_x - eps, "グリフ箱が左へ戻った");
            prev_x = g.bbox[0];
            assert!(g.bbox[0] >= span.bbox[0] - eps && g.bbox[2] <= span.bbox[2] + eps);
            assert!(g.bbox[1] >= span.bbox[1] - eps && g.bbox[3] <= span.bbox[3] + eps);
        }
        // グリフ箱の合併の x 範囲はスパンの x 範囲と一致する。
        let first = span.glyphs.first().unwrap();
        let last = span.glyphs.last().unwrap();
        assert!((first.bbox[0] - span.bbox[0]).abs() < eps);
        assert!((last.bbox[2] - span.bbox[2]).abs() < eps);
    }
}

/// cm によるスケール下でも検索矩形が変換後の位置に追従する。
#[test]
fn search_respects_ctm() {
    let mut doc = Document::new();
    doc.add_page(612.0, 792.0).unwrap();
    // 2 倍スケールの cm の下でテキストを置く（Td は 100, 300 → 実際は 200, 600）。
    // フォント名 /FS1 はリソース未登録 → フォールバックデコーダ（WinAnsi・
    // 幅 500/1000 em 近似）で抽出される経路の検証を兼ねる。
    doc.append_content_bytes(
        0,
        b"q 2 0 0 2 0 0 cm BT /FS1 12 Tf 100 300 Td (scaled) Tj ET Q".to_vec(),
    )
    .unwrap();

    let hits = doc
        .search_page(0, "scaled", &SearchOptions::default())
        .unwrap();
    assert_eq!(hits.len(), 1);
    let r = hits[0].rects[0];
    // ベースラインは y=600（300 × 2）付近、x は 200 から。
    assert!((r[0] - 200.0).abs() < 1.0, "x0 = {}", r[0]);
    assert!(r[1] > 590.0 && r[1] < 600.0, "y0 = {}", r[1]);
    // 実効フォントサイズも 2 倍（24pt）になっている。
    let spans = doc.extract_text_spans(0).unwrap();
    let scaled = spans.iter().find(|s| s.text == "scaled").unwrap();
    assert!((scaled.font_size - 24.0).abs() < 1e-6);
}
