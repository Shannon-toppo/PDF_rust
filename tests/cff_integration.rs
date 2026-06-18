//! CFF（OpenType/Compact Font Format）の統合テスト。
//!
//! Windows 環境のシステム OTF（`C:\Windows\Fonts\SourceHanSansJP-*.otf` など）
//! が存在する場合に、OTTO sfnt の CFF テーブルを読み、Unicode → GID 解決と
//! グリフアウトラインのデコードまで通ることを確認する。
//!
//! テスト対象のフォントが無い環境では `eprintln!` してスキップする
//! （`return` でパス扱い）。

use pdf_rust::truetype::OutlineSegment;
use pdf_rust::TrueTypeFont;

/// 最低 1 件のセグメントが含まれる外形上の妥当性チェック。
fn assert_outline_has_curves(name: &str, segs: &[OutlineSegment]) {
    assert!(!segs.is_empty(), "{name}: outline は空でないはず");
    let mut moves = 0;
    let mut curves = 0;
    let mut closes = 0;
    for s in segs {
        match s {
            OutlineSegment::MoveTo(_, _) => moves += 1,
            OutlineSegment::LineTo(_, _) => {}
            OutlineSegment::QuadTo(_, _, _, _) => curves += 1,
            OutlineSegment::CurveTo(_, _, _, _, _, _) => curves += 1,
            OutlineSegment::Close => closes += 1,
        }
    }
    assert!(moves > 0, "{name}: MoveTo が無い");
    assert!(closes > 0, "{name}: Close が無い");
    // 'O' は曲線を必ず持つはず（CFF は 3 次ベジェ）。
    assert!(curves > 0, "{name}: 曲線が 1 つも無い");
}

#[test]
fn opentype_cff_parses_and_decodes_glyph_outline() {
    // Windows 既定の Source Han Sans JP（OTTO sfnt + CFF テーブル）。
    let path = "C:\\Windows\\Fonts\\SourceHanSansJP-Normal.otf";
    let Ok(data) = std::fs::read(path) else {
        eprintln!("skip: {path} が無い");
        return;
    };

    let font = TrueTypeFont::parse(data, 0).expect("OTF パース");
    assert!(font.is_cff(), "Source Han Sans JP は CFF アウトライン");
    assert!(font.cff().is_some(), "CFF テーブルが解析されている");
    assert!(font.units_per_em() > 0);

    // 'A' を引いて outline をデコード。
    let gid = font.glyph_id('A').expect("'A' の GID");
    assert!(gid > 0, "'A' の GID は 0 ではない");
    let segs = font.glyph_outline(gid).expect("'A' outline");
    assert_outline_has_curves("'A'", &segs);

    // advance_width が妥当範囲（0 < w < 5×upm）に収まる。
    let upm = font.units_per_em() as u32;
    let w = font.advance_width(gid) as u32;
    assert!(
        w > 0 && w < upm * 5,
        "advance_width が異常: {w} (upm={upm})"
    );
}

#[test]
fn opentype_cff_japanese_glyph() {
    let path = "C:\\Windows\\Fonts\\SourceHanSansJP-Normal.otf";
    let Ok(data) = std::fs::read(path) else {
        eprintln!("skip: {path} が無い");
        return;
    };
    let font = TrueTypeFont::parse(data, 0).expect("OTF パース");
    // 「あ」（U+3042）。CID キー付き CFF でも cmap 経由で GID が引ける。
    let Some(gid) = font.glyph_id('あ') else {
        eprintln!("skip: フォントに 'あ' が無い");
        return;
    };
    let segs = font.glyph_outline(gid).expect("'あ' outline");
    assert_outline_has_curves("'あ'", &segs);
}

/// 壊れたデータでも `parse_raw_cff` が panic せず Err を返す。
#[test]
fn raw_cff_garbage_no_panic() {
    let _ = TrueTypeFont::parse_raw_cff(Vec::new());
    let _ = TrueTypeFont::parse_raw_cff(vec![0u8; 4]);
    let _ = TrueTypeFont::parse_raw_cff(vec![0xFFu8; 64]);
}
