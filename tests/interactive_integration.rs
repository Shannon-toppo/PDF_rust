//! 統合テスト: ビューワー機能（Phase 4）。
//!
//! - 位置付きテキスト抽出（`extract_text_spans`）の往復一致
//! - しおり・リンク・ページラベルの 生成 → 保存 → 再読込 → 読み取り
//! - 注釈の外観ストリーム（/AP /N）のレンダリング

use pdf_rust::{Dictionary, Document, LinkTarget, Object, Stream, StringFormat, TextOptions};

/// テスト用: カタログ辞書を可変で取得する。
fn catalog_mut(doc: &mut Document) -> &mut Dictionary {
    let root = doc
        .trailer
        .get("Root")
        .and_then(|o| o.as_reference().ok())
        .unwrap();
    doc.get_object_mut(root).unwrap().as_dict_mut().unwrap()
}

/// 生成 → to_bytes → from_bytes → extract_text_spans の往復で
/// テキストと位置が保たれる。
#[test]
fn text_spans_roundtrip() {
    let mut doc = Document::new();
    doc.add_page(612.0, 792.0).unwrap();
    doc.add_text(
        0,
        "Hello\nWorld",
        &TextOptions {
            size: 12.0,
            x: 72.0,
            y: 720.0,
            ..Default::default()
        },
    )
    .unwrap();

    let bytes = doc.to_bytes().unwrap();
    let reloaded = Document::from_bytes(&bytes).unwrap();
    let spans = reloaded.extract_text_spans(0).unwrap();

    assert_eq!(spans.len(), 2);
    assert_eq!(spans[0].text, "Hello");
    assert_eq!(spans[1].text, "World");

    // 1 行目: ベースライン (72, 720)、サイズ 12pt。bbox がベースラインを跨ぐ。
    let b = &spans[0].bbox;
    assert!((b[0] - 72.0).abs() < 0.5, "x0 = {}", b[0]);
    assert!(b[1] < 720.0 && 720.0 < b[3], "bbox = {b:?}");
    assert!(b[3] - b[1] < 12.5, "高さがフォントサイズ近傍: {b:?}");
    // Helvetica 12pt の "Hello" はおよそ 28pt 幅。
    let w = b[2] - b[0];
    assert!((20.0..40.0).contains(&w), "幅 = {w}");
    assert!((spans[0].font_size - 12.0).abs() < 0.01);

    // 2 行目は行送り（12 * 1.2 = 14.4pt）だけ下。
    let b2 = &spans[1].bbox;
    assert!(
        (b[1] - b2[1] - 14.4).abs() < 0.5,
        "行送り: {} vs {}",
        b[1],
        b2[1]
    );
    assert!((b2[0] - 72.0).abs() < 0.5);

    // 文字列抽出との整合（同じ内容が読めること）。
    assert_eq!(reloaded.extract_text(0).unwrap(), "Hello\nWorld");
}

/// cm による拡大がスパンの bbox と実効フォントサイズへ反映される。
#[test]
fn text_spans_respect_ctm() {
    let mut doc = Document::new();
    doc.add_page(612.0, 792.0).unwrap();
    doc.ensure_standard_font(0, pdf_rust::StandardFont::Helvetica)
        .unwrap();
    // 2 倍拡大の cm の下でテキストを表示する。
    let content = b"q 2 0 0 2 0 0 cm BT /F1 10 Tf 50 100 Td (Hi) Tj ET Q".to_vec();
    doc.append_content_bytes(0, content).unwrap();

    let spans = doc.extract_text_spans(0).unwrap();
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].text, "Hi");
    // ユーザー座標 (50,100) が cm で (100,200) になる。
    let b = &spans[0].bbox;
    assert!((b[0] - 100.0).abs() < 0.5, "bbox = {b:?}");
    assert!(b[1] < 200.0 && 200.0 < b[3], "bbox = {b:?}");
    // 実効サイズは 10 × 2 = 20pt。
    assert!((spans[0].font_size - 20.0).abs() < 0.01);
}

/// しおり・リンク・ページラベルが保存 → 再読込後も読み取れる。
#[test]
fn outlines_links_labels_roundtrip() {
    let mut doc = Document::new();
    let p0 = doc.add_page(612.0, 792.0).unwrap();
    let p1 = doc.add_page(612.0, 792.0).unwrap();
    doc.add_text(0, "first", &TextOptions::default()).unwrap();

    // しおり: "Chapter 1" → page 1 /XYZ。
    let mut item = Dictionary::new();
    item.set("Title", Object::string_literal("Chapter 1"));
    item.set(
        "Dest",
        Object::Array(vec![
            Object::Reference(p1),
            Object::name("XYZ"),
            Object::Null,
            792.into(),
            Object::Null,
        ]),
    );
    let item_id = doc.add_object(item);
    let mut outlines = Dictionary::new();
    outlines.set("Type", Object::name("Outlines"));
    outlines.set("First", Object::Reference(item_id));
    outlines.set("Last", Object::Reference(item_id));
    let outlines_id = doc.add_object(outlines);
    catalog_mut(&mut doc).set("Outlines", Object::Reference(outlines_id));

    // ページラベル: 1 ページ目から "A-1", "A-2"。
    let mut style = Dictionary::new();
    style.set("S", Object::name("D"));
    style.set("P", Object::String(b"A-".to_vec(), StringFormat::Literal));
    let mut labels = Dictionary::new();
    labels.set(
        "Nums",
        Object::Array(vec![0.into(), Object::Dictionary(style)]),
    );
    catalog_mut(&mut doc).set("PageLabels", labels);

    // リンク: page 0 上の URI リンク。
    let mut action = Dictionary::new();
    action.set("S", Object::name("URI"));
    action.set("URI", Object::string_literal("https://example.com/"));
    let mut link = Dictionary::new();
    link.set("Subtype", Object::name("Link"));
    link.set(
        "Rect",
        Object::Array(vec![72.into(), 700.into(), 200.into(), 720.into()]),
    );
    link.set("A", Object::Dictionary(action));
    let link_id = doc.add_object(link);
    let page = doc.get_object_mut(p0).unwrap().as_dict_mut().unwrap();
    page.set("Annots", Object::Array(vec![Object::Reference(link_id)]));

    // 往復。
    let bytes = doc.to_bytes().unwrap();
    let reloaded = Document::from_bytes(&bytes).unwrap();

    let outlines = reloaded.outlines();
    assert_eq!(outlines.len(), 1);
    assert_eq!(outlines[0].title, "Chapter 1");
    match &outlines[0].target {
        Some(LinkTarget::Goto(d)) => {
            assert_eq!(d.page_index, Some(1));
            assert_eq!(d.y, Some(792.0));
        }
        other => panic!("unexpected target: {other:?}"),
    }

    assert_eq!(reloaded.page_labels(), vec!["A-1", "A-2"]);

    let links = reloaded.page_links(0).unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].rect, [72.0, 700.0, 200.0, 720.0]);
    assert_eq!(
        links[0].target,
        LinkTarget::Uri("https://example.com/".into())
    );
}

/// 注釈の外観ストリーム（/AP /N）が /Rect の位置に描画される。
#[test]
fn annotation_appearance_renders() {
    let mut doc = Document::new();
    let p0 = doc.add_page(612.0, 792.0).unwrap();

    // 外観: BBox [0 0 10 10] 全面を赤く塗る Form。
    let mut ap_dict = Dictionary::new();
    ap_dict.set("Type", Object::name("XObject"));
    ap_dict.set("Subtype", Object::name("Form"));
    ap_dict.set(
        "BBox",
        Object::Array(vec![0.into(), 0.into(), 10.into(), 10.into()]),
    );
    let ap_stream = Stream::new(ap_dict, b"1 0 0 rg 0 0 10 10 re f".to_vec());
    let ap_id = doc.add_object(Object::Stream(ap_stream));

    // 注釈: Rect [100 100 140 120]（BBox から 4x2 倍に引き伸ばされる）。
    let mut n = Dictionary::new();
    n.set("N", Object::Reference(ap_id));
    let mut annot = Dictionary::new();
    annot.set("Subtype", Object::name("Square"));
    annot.set(
        "Rect",
        Object::Array(vec![100.into(), 100.into(), 140.into(), 120.into()]),
    );
    annot.set("AP", n);
    let annot_id = doc.add_object(annot);
    let page = doc.get_object_mut(p0).unwrap().as_dict_mut().unwrap();
    page.set("Annots", Object::Array(vec![Object::Reference(annot_id)]));

    let pm = doc.render_page(0, 1.0).unwrap();
    // Rect 中心 (120, 110) → デバイス座標 (120, 792-110=682)。
    assert_eq!(pm.pixel(120, 682), Some([255, 0, 0]), "注釈の中心が赤");
    // Rect の外（左側）は白のまま。
    assert_eq!(pm.pixel(90, 682), Some([255, 255, 255]), "注釈の外は白");

    // 往復後も描画される。
    let bytes = doc.to_bytes().unwrap();
    let reloaded = Document::from_bytes(&bytes).unwrap();
    let pm2 = reloaded.render_page(0, 1.0).unwrap();
    assert_eq!(pm2.pixel(120, 682), Some([255, 0, 0]));
}

/// Hidden フラグ付き注釈と Popup 注釈は描画されない。
#[test]
fn hidden_annotation_not_rendered() {
    let mut doc = Document::new();
    let p0 = doc.add_page(612.0, 792.0).unwrap();

    let mut ap_dict = Dictionary::new();
    ap_dict.set("Subtype", Object::name("Form"));
    ap_dict.set(
        "BBox",
        Object::Array(vec![0.into(), 0.into(), 10.into(), 10.into()]),
    );
    let ap_stream = Stream::new(ap_dict, b"1 0 0 rg 0 0 10 10 re f".to_vec());
    let ap_id = doc.add_object(Object::Stream(ap_stream));

    // Hidden(2) フラグ付き。
    let mut n = Dictionary::new();
    n.set("N", Object::Reference(ap_id));
    let mut annot = Dictionary::new();
    annot.set("Subtype", Object::name("Square"));
    annot.set("F", 2);
    annot.set(
        "Rect",
        Object::Array(vec![100.into(), 100.into(), 140.into(), 120.into()]),
    );
    annot.set("AP", n);
    let annot_id = doc.add_object(annot);
    let page = doc.get_object_mut(p0).unwrap().as_dict_mut().unwrap();
    page.set("Annots", Object::Array(vec![Object::Reference(annot_id)]));

    let pm = doc.render_page(0, 1.0).unwrap();
    assert_eq!(
        pm.pixel(120, 682),
        Some([255, 255, 255]),
        "Hidden は描かない"
    );
}

/// /AP /N が状態辞書（/AS で選択）でも描画できる。
#[test]
fn annotation_appearance_with_state_dict() {
    let mut doc = Document::new();
    let p0 = doc.add_page(612.0, 792.0).unwrap();

    // On: 緑 / Off: 赤 の 2 状態。/AS は On。
    let make_ap = |doc: &mut Document, ops: &[u8]| {
        let mut d = Dictionary::new();
        d.set("Subtype", Object::name("Form"));
        d.set(
            "BBox",
            Object::Array(vec![0.into(), 0.into(), 10.into(), 10.into()]),
        );
        doc.add_object(Object::Stream(Stream::new(d, ops.to_vec())))
    };
    let on_id = make_ap(&mut doc, b"0 1 0 rg 0 0 10 10 re f");
    let off_id = make_ap(&mut doc, b"1 0 0 rg 0 0 10 10 re f");

    let mut states = Dictionary::new();
    states.set("On", Object::Reference(on_id));
    states.set("Off", Object::Reference(off_id));
    let mut n = Dictionary::new();
    n.set("N", states);
    let mut annot = Dictionary::new();
    annot.set("Subtype", Object::name("Widget"));
    annot.set("AS", Object::name("On"));
    annot.set(
        "Rect",
        Object::Array(vec![100.into(), 100.into(), 110.into(), 110.into()]),
    );
    annot.set("AP", n);
    let annot_id = doc.add_object(annot);
    let page = doc.get_object_mut(p0).unwrap().as_dict_mut().unwrap();
    page.set("Annots", Object::Array(vec![Object::Reference(annot_id)]));

    let pm = doc.render_page(0, 1.0).unwrap();
    // (105, 105) → デバイス (105, 687)。On = 緑が選ばれる。
    assert_eq!(pm.pixel(105, 687), Some([0, 255, 0]));
}
