//! 統合テスト: ページのラスタライズ（レンダラ最終段）。
//!
//! 生成 → 描画、往復一致、ストローク、クリップ、回転、スケール、耐故障性、
//! PNG 保存を検証する。Phase 2 でテキスト描画（TrueType グリフ）を追加。
//! 画像は Phase 3 まで未対応。

use pdf_rust::{Document, DrawOptions, TextOptions};

/// 色がほぼ一致するか（各チャンネル ±tol）。
fn near(a: [u8; 3], b: [u8; 3], tol: i32) -> bool {
    (0..3).all(|i| (a[i] as i32 - b[i] as i32).abs() <= tol)
}

/// 矩形領域内に「暗い」（黒に近い）ピクセルが 1 つでもあるか。
fn has_dark_pixel(pm: &pdf_rust::render::Pixmap, x0: u32, y0: u32, x1: u32, y1: u32) -> bool {
    for y in y0..y1.min(pm.height()) {
        for x in x0..x1.min(pm.width()) {
            if let Some(p) = pm.pixel(x, y) {
                if p[0] < 128 && p[1] < 128 && p[2] < 128 {
                    return true;
                }
            }
        }
    }
    false
}

/// 矩形領域が全て白か。
fn all_white(pm: &pdf_rust::render::Pixmap, x0: u32, y0: u32, x1: u32, y1: u32) -> bool {
    for y in y0..y1.min(pm.height()) {
        for x in x0..x1.min(pm.width()) {
            if let Some(p) = pm.pixel(x, y) {
                if !near(p, [255, 255, 255], 8) {
                    return false;
                }
            }
        }
    }
    true
}

/// 生成 → 描画: 赤い塗り矩形の中心が赤、外が白、サイズが MediaBox と一致。
#[test]
fn render_filled_rect() {
    let mut doc = Document::new();
    doc.add_page(200.0, 100.0).unwrap();
    // 中央付近に 80x40 の赤塗り矩形（枠線は同色赤にして境界判定を単純化）。
    doc.draw_rect(
        0,
        60.0,
        30.0,
        80.0,
        40.0,
        &DrawOptions {
            stroke_color: (1.0, 0.0, 0.0),
            fill_color: Some((1.0, 0.0, 0.0)),
            line_width: 1.0,
        },
    )
    .unwrap();

    let pm = doc.render_page(0, 1.0).unwrap();
    assert_eq!(pm.width(), 200);
    assert_eq!(pm.height(), 100);

    // 矩形の中心（PDF 座標 (100, 50)）→ デバイス (100, 50)（y 反転で 100-50）。
    let center = pm.pixel(100, 50).unwrap();
    assert!(near(center, [255, 0, 0], 4), "中心が赤でない: {:?}", center);

    // 矩形外（左上の隅）は白。
    let outside = pm.pixel(5, 5).unwrap();
    assert!(
        near(outside, [255, 255, 255], 0),
        "外側が白でない: {:?}",
        outside
    );
}

/// 往復一致: to_bytes → from_bytes 後の描画が元と全ピクセル一致。
#[test]
fn render_roundtrip_identical() {
    let mut doc = Document::new();
    doc.add_page(200.0, 100.0).unwrap();
    doc.draw_rect(
        0,
        20.0,
        20.0,
        100.0,
        50.0,
        &DrawOptions {
            stroke_color: (0.0, 0.0, 1.0),
            fill_color: Some((0.2, 0.6, 0.9)),
            line_width: 2.0,
        },
    )
    .unwrap();

    let before = doc.render_page(0, 1.5).unwrap();
    let bytes = doc.to_bytes().unwrap();
    let doc2 = Document::from_bytes(&bytes).unwrap();
    let after = doc2.render_page(0, 1.5).unwrap();

    assert_eq!(before.width(), after.width());
    assert_eq!(before.height(), after.height());
    assert_eq!(before.data(), after.data(), "往復後にピクセルが変化した");
}

/// ストローク: 黒い水平線の上が黒、離れた場所が白。
#[test]
fn render_stroke_line() {
    let mut doc = Document::new();
    doc.add_page(100.0, 100.0).unwrap();
    doc.draw_line(
        0,
        (10.0, 50.0),
        (90.0, 50.0),
        &DrawOptions {
            stroke_color: (0.0, 0.0, 0.0),
            line_width: 4.0,
            ..Default::default()
        },
    )
    .unwrap();

    let pm = doc.render_page(0, 1.0).unwrap();
    // PDF y=50 → デバイス y=50。線上は黒。
    let on = pm.pixel(50, 50).unwrap();
    assert!(on[0] < 64, "線上が黒でない: {:?}", on);
    // 線から十分離れた場所は白。
    let off = pm.pixel(50, 10).unwrap();
    assert!(near(off, [255, 255, 255], 0), "線外が白でない: {:?}", off);
}

/// クリップ: 小さいクリップ + 大きい塗り → クリップ内のみ塗られる。
#[test]
fn render_clip_restricts_fill() {
    let mut doc = Document::new();
    doc.add_page(100.0, 100.0).unwrap();
    // クリップ矩形 (10,10)-(40,40) を設定し、全面黒で塗る。
    // re W n（クリップ確定）→ re f（全面塗り）。
    let content = b"10 10 30 30 re W n 0 0 100 100 re f".to_vec();
    doc.append_content_bytes(0, content).unwrap();

    let pm = doc.render_page(0, 1.0).unwrap();
    // クリップ内（PDF (25,25) → デバイス (25, 75)）は黒。
    let inside = pm.pixel(25, 75).unwrap();
    assert!(inside[0] < 16, "クリップ内が塗られていない: {:?}", inside);
    // クリップ外（PDF (70,70) → デバイス (70, 30)）は白。
    let outside = pm.pixel(70, 30).unwrap();
    assert!(
        near(outside, [255, 255, 255], 0),
        "クリップ外が塗られている: {:?}",
        outside
    );
}

/// 回転: 90 度回転で幅高さが交換され、内容の向きが変わる。
#[test]
fn render_rotate_90_swaps_dimensions() {
    let mut doc = Document::new();
    doc.add_page(200.0, 100.0).unwrap();
    // ページ左下隅に小さな黒矩形を置く（向きの確認用マーカー）。
    doc.draw_rect(
        0,
        0.0,
        0.0,
        20.0,
        20.0,
        &DrawOptions {
            stroke_color: (0.0, 0.0, 0.0),
            fill_color: Some((0.0, 0.0, 0.0)),
            line_width: 0.0,
        },
    )
    .unwrap();

    // 回転前: 幅 200 x 高さ 100。左下マーカー → デバイス左下。
    let normal = doc.render_page(0, 1.0).unwrap();
    assert_eq!((normal.width(), normal.height()), (200, 100));
    // PDF (10,10) → デバイス (10, 90) 付近が黒。
    assert!(normal.pixel(10, 90).unwrap()[0] < 64);

    doc.rotate_page(0, 90).unwrap();
    let rotated = doc.render_page(0, 1.0).unwrap();
    // 幅高さが交換される。
    assert_eq!((rotated.width(), rotated.height()), (100, 200));
    // 時計回り 90 度表示で左下マーカーは左上へ移る。
    // user(10,10) → 90 度 CW で device(v,u)=(10,10) 付近。
    let corner = rotated.pixel(10, 10).unwrap();
    assert!(corner[0] < 64, "回転後マーカー位置が黒でない: {:?}", corner);
    // 元の左下位置（デバイス左下）は今度は白。
    let was_corner = rotated.pixel(10, 190).unwrap();
    assert!(near(was_corner, [255, 255, 255], 4));
}

/// スケール: scale 2.0 で寸法が 2 倍になる。
#[test]
fn render_scale_doubles_dimensions() {
    let mut doc = Document::new();
    doc.add_page(150.0, 80.0).unwrap();
    let pm1 = doc.render_page(0, 1.0).unwrap();
    let pm2 = doc.render_page(0, 2.0).unwrap();
    assert_eq!((pm1.width(), pm1.height()), (150, 80));
    assert_eq!((pm2.width(), pm2.height()), (300, 160));
}

/// 耐故障性: 壊れたコンテントでも Err・panic にならず描画が返る。
#[test]
fn render_corrupt_content_no_panic() {
    let mut doc = Document::new();
    doc.add_page(100.0, 100.0).unwrap();
    // 不正バイト列・オペランド不足・未知演算子・過剰 Q を混ぜる。
    doc.append_content_bytes(0, vec![0xFF, 0x00, b'q', b' ', 0xDE, 0xAD])
        .unwrap();
    doc.append_content_bytes(0, b"1 2 cm 5 re garbage f Q Q Q 3 nonsense".to_vec())
        .unwrap();

    let pm = doc.render_page(0, 1.0).unwrap();
    assert_eq!((pm.width(), pm.height()), (100, 100));
}

/// PNG 保存: 一時ディレクトリへ保存でき、シグネチャが正しい。
#[test]
fn render_save_png() {
    let mut doc = Document::new();
    doc.add_page(60.0, 40.0).unwrap();
    doc.draw_rect(
        0,
        10.0,
        10.0,
        40.0,
        20.0,
        &DrawOptions {
            fill_color: Some((0.1, 0.2, 0.3)),
            ..Default::default()
        },
    )
    .unwrap();
    let pm = doc.render_page(0, 1.0).unwrap();

    let mut path = std::env::temp_dir();
    path.push(format!("pdf_rust_render_test_{}.png", std::process::id()));
    pm.save_png(&path).unwrap();

    let bytes = std::fs::read(&path).unwrap();
    assert!(bytes.len() > 8);
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");

    // 後始末。
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// Phase 2: テキスト描画
// ---------------------------------------------------------------------------

/// 標準フォント代替: Helvetica の add_text がシステムフォント（arial.ttf）で
/// 描画され、テキスト行領域に暗いピクセルが出る。無ければスキップ（パス扱い）。
#[test]
fn render_standard_font_substitution() {
    if !std::path::Path::new("C:\\Windows\\Fonts\\arial.ttf").exists() {
        eprintln!("arial.ttf が無いためスキップ");
        return;
    }
    let mut doc = Document::new();
    doc.add_page(300.0, 100.0).unwrap();
    doc.add_text(
        0,
        "Hello",
        &TextOptions {
            font: pdf_rust::StandardFont::Helvetica,
            size: 40.0,
            x: 20.0,
            y: 40.0,
            color: (0.0, 0.0, 0.0),
            leading: None,
        },
    )
    .unwrap();

    let pm = doc.render_page(0, 1.0).unwrap();
    // テキストはベースライン y=40（PDF） → デバイス y≈30..60 の帯にある。
    assert!(
        has_dark_pixel(&pm, 20, 30, 200, 65),
        "テキスト領域に暗いピクセルが無い"
    );
    // テキストの無い右上は白。
    assert!(
        all_white(&pm, 250, 0, 300, 20),
        "テキストの無い領域が白でない"
    );
}

/// 埋め込み TrueType: load_font + add_text_with_font で描画でき、
/// to_bytes → from_bytes 往復後（CIDFontType2/Identity-H 経路）も描画される。
#[test]
fn render_embedded_truetype() {
    let path = "C:\\Windows\\Fonts\\arial.ttf";
    if !std::path::Path::new(path).exists() {
        eprintln!("arial.ttf が無いためスキップ");
        return;
    }
    let mut doc = Document::new();
    doc.add_page(300.0, 100.0).unwrap();
    let font = doc.load_font(path).unwrap();
    doc.add_text_with_font(
        0,
        "Hello",
        font,
        &TextOptions {
            size: 40.0,
            x: 20.0,
            y: 40.0,
            ..Default::default()
        },
    )
    .unwrap();

    let pm = doc.render_page(0, 1.0).unwrap();
    assert!(
        has_dark_pixel(&pm, 20, 30, 200, 65),
        "埋め込みフォントのグリフが描画されていない"
    );

    // 往復後も描画される（サブセット埋め込み → Identity-H 経路）。
    let bytes = doc.to_bytes().unwrap();
    let doc2 = Document::from_bytes(&bytes).unwrap();
    let pm2 = doc2.render_page(0, 1.0).unwrap();
    assert!(
        has_dark_pixel(&pm2, 20, 30, 200, 65),
        "往復後にグリフが描画されていない"
    );
}

/// 日本語埋め込み: msgothic.ttc または YuGothM.ttc で「あ」を描画。
/// どちらも無ければスキップ（パス扱い）。
#[test]
fn render_japanese_embedded() {
    let candidates = [
        "C:\\Windows\\Fonts\\msgothic.ttc",
        "C:\\Windows\\Fonts\\YuGothM.ttc",
    ];
    let path = candidates.iter().find(|p| std::path::Path::new(p).exists());
    let path = match path {
        Some(p) => *p,
        None => {
            eprintln!("日本語フォントが無いためスキップ");
            return;
        }
    };
    let mut doc = Document::new();
    doc.add_page(200.0, 100.0).unwrap();
    let font = doc.load_font(path).unwrap();
    doc.add_text_with_font(
        0,
        "あ",
        font,
        &TextOptions {
            size: 60.0,
            x: 20.0,
            y: 30.0,
            ..Default::default()
        },
    )
    .unwrap();

    let pm = doc.render_page(0, 1.0).unwrap();
    assert!(
        has_dark_pixel(&pm, 15, 5, 90, 80),
        "日本語グリフが描画されていない"
    );
}

/// Tr 3（不可視）: render_mode 3 のテキストは描画されず全面白のまま。
#[test]
fn render_text_render_mode_invisible() {
    if !std::path::Path::new("C:\\Windows\\Fonts\\arial.ttf").exists() {
        eprintln!("arial.ttf が無いためスキップ");
        return;
    }
    let mut doc = Document::new();
    doc.add_page(200.0, 60.0).unwrap();
    // フォントリソースを登録してから生コンテントで Tr 3 を仕込む。
    doc.add_text(
        0,
        " ",
        &TextOptions {
            size: 1.0,
            x: 0.0,
            y: 0.0,
            ..Default::default()
        },
    )
    .unwrap();
    // F1 が登録されている前提で Tr 3 のテキストを描く。
    let content = b"BT /F1 40 Tf 3 Tr 20 20 Td (Hello) Tj ET".to_vec();
    doc.append_content_bytes(0, content).unwrap();

    let pm = doc.render_page(0, 1.0).unwrap();
    // Tr 3 は不可視。スペース 1 文字（サイズ 1）は事実上見えないので全面白。
    assert!(all_white(&pm, 0, 0, 200, 60), "不可視テキストが描画された");
}

/// 耐故障性: 存在しないフォント名の Tf・壊れた FontFile2 でも
/// render_page が Ok で panic しない。
#[test]
fn render_text_fault_tolerant() {
    // 1. 存在しないフォント名を Tf で参照。
    let mut doc = Document::new();
    doc.add_page(100.0, 100.0).unwrap();
    doc.append_content_bytes(0, b"BT /NoSuchFont 12 Tf 10 10 Td (Hi) Tj ET".to_vec())
        .unwrap();
    let pm = doc.render_page(0, 1.0).unwrap();
    assert_eq!((pm.width(), pm.height()), (100, 100));

    // 2. ゴミバイトの FontFile2 を持つフォント辞書を手で組む。
    let mut doc2 = Document::new();
    doc2.add_page(100.0, 100.0).unwrap();
    build_doc_with_broken_truetype_font(&mut doc2).expect("壊れたフォント辞書の構築");
    let pm2 = doc2.render_page(0, 1.0).unwrap();
    assert_eq!((pm2.width(), pm2.height()), (100, 100));
}

/// ゴミバイトの /FontFile2 を持つ簡易な単純 TrueType フォント辞書を
/// ページ 0 のリソースに登録し、それを使うテキストを描く。
fn build_doc_with_broken_truetype_font(
    doc: &mut Document,
) -> Result<(), Box<dyn std::error::Error>> {
    use pdf_rust::object::{Dictionary, Object, Stream};

    // 壊れた FontFile2 ストリーム。
    let mut ff_dict = Dictionary::new();
    ff_dict.set("Length1", Object::Integer(8));
    let ff_id = doc.add_object(Object::Stream(Stream::new(
        ff_dict,
        vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01, 0x02, 0x03],
    )));

    // FontDescriptor。
    let mut desc = Dictionary::new();
    desc.set("Type", Object::name("FontDescriptor"));
    desc.set("FontName", Object::name("Broken"));
    desc.set("Flags", Object::Integer(4));
    desc.set("FontFile2", Object::Reference(ff_id));
    let desc_id = doc.add_object(Object::Dictionary(desc));

    // Font 辞書（単純 TrueType）。
    let mut font = Dictionary::new();
    font.set("Type", Object::name("Font"));
    font.set("Subtype", Object::name("TrueType"));
    font.set("BaseFont", Object::name("Broken"));
    font.set("FirstChar", Object::Integer(32));
    font.set("LastChar", Object::Integer(126));
    font.set("FontDescriptor", Object::Reference(desc_id));
    let font_id = doc.add_object(Object::Dictionary(font));

    // ページ 0 のリソースに /Font << /FB <font_id> >> を入れる。
    let page_id = doc.page_id(0)?;
    let mut resources = doc.page_resources(page_id);
    let mut fonts = Dictionary::new();
    fonts.set("FB", Object::Reference(font_id));
    resources.set("Font", fonts);
    doc.get_object_mut(page_id)?
        .as_dict_mut()?
        .set("Resources", resources);

    doc.append_content_bytes(0, b"BT /FB 20 Tf 10 50 Td (Test) Tj ET".to_vec())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Phase 3: 画像描画
// ---------------------------------------------------------------------------

use pdf_rust::object::{Dictionary, Object, Stream};

/// ページ 0 のリソースに名前 `name` の画像 XObject を登録する。
fn add_image_xobject(doc: &mut Document, name: &str, stream: Stream) {
    let img_id = doc.add_object(Object::Stream(stream));
    let page_id = doc.page_id(0).unwrap();
    let mut resources = doc.page_resources(page_id);
    // 既存の /XObject 辞書を取り出すか新規作成。
    let mut xobjects = match resources.get("XObject").and_then(|o| o.as_dict().ok()) {
        Some(d) => d.clone(),
        None => Dictionary::new(),
    };
    xobjects.set(name, Object::Reference(img_id));
    resources.set("XObject", Object::Dictionary(xobjects));
    doc.get_object_mut(page_id)
        .unwrap()
        .as_dict_mut()
        .unwrap()
        .set("Resources", resources);
}

/// 2x2 RGB 画像辞書（FlateDecode）を作る。
fn rgb_image_2x2(pixels: &[u8]) -> Stream {
    let mut d = Dictionary::new();
    d.set("Type", Object::name("XObject"));
    d.set("Subtype", Object::name("Image"));
    d.set("Width", Object::Integer(2));
    d.set("Height", Object::Integer(2));
    d.set("BitsPerComponent", Object::Integer(8));
    d.set("ColorSpace", Object::name("DeviceRGB"));
    Stream::new_compressed(d, pixels)
}

/// 画像 XObject 描画: 2x2 RGB を 100x100 に拡大し、四隅の色を検証する。
#[test]
fn render_image_xobject_corners() {
    let mut doc = Document::new();
    doc.add_page(100.0, 100.0).unwrap();
    // 行優先・左上原点: row0 = 赤,緑 / row1 = 青,白。
    let pixels = vec![
        255, 0, 0, 0, 255, 0, // 赤, 緑
        0, 0, 255, 255, 255, 255, // 青, 白
    ];
    add_image_xobject(&mut doc, "Im0", rgb_image_2x2(&pixels));
    // 画像を全面（100x100）に配置: cm で単位正方形をスケール。
    doc.append_content_bytes(0, b"q 100 0 0 100 0 0 cm /Im0 Do Q".to_vec())
        .unwrap();

    let pm = doc.render_page(0, 1.0).unwrap();
    // PDF 画像の row0 は単位正方形の上端（v 小）。CTM は y 反転を含むため
    // デバイス上端（y 小）= 画像 row0。
    // 左上デバイス (10,10) ≈ 画像左上 = 赤。
    let tl = pm.pixel(10, 10).unwrap();
    assert!(near(tl, [255, 0, 0], 8), "左上が赤でない: {:?}", tl);
    // 右上 (90,10) ≈ 緑。
    let tr = pm.pixel(90, 10).unwrap();
    assert!(near(tr, [0, 255, 0], 8), "右上が緑でない: {:?}", tr);
    // 左下 (10,90) ≈ 青。
    let bl = pm.pixel(10, 90).unwrap();
    assert!(near(bl, [0, 0, 255], 8), "左下が青でない: {:?}", bl);
    // 右下 (90,90) ≈ 白。
    let br = pm.pixel(90, 90).unwrap();
    assert!(near(br, [255, 255, 255], 8), "右下が白でない: {:?}", br);
}

/// 90 度回転 CTM での向き検証。
///
/// 画像を反時計回り 90 度回転して配置し、元の左上（赤）が回転後の位置へ
/// 移ることを確認する。
#[test]
fn render_image_rotated_90() {
    let mut doc = Document::new();
    doc.add_page(100.0, 100.0).unwrap();
    // row0 = 赤,赤 / row1 = 青,青（上半分赤・下半分青）。
    let pixels = vec![
        255, 0, 0, 255, 0, 0, // 赤, 赤
        0, 0, 255, 0, 0, 255, // 青, 青
    ];
    add_image_xobject(&mut doc, "Im0", rgb_image_2x2(&pixels));
    // 画像座標系（単位正方形）を 90 度回転 + 平行移動でページ中央 80x80 へ。
    // cm = [0 1 -1 0 90 10]: 単位正方形を 90 度回し (10..90, 10..90) へ。
    // ここではスケールも掛ける: [0 80 -80 0 90 10]。
    doc.append_content_bytes(0, b"q 0 80 -80 0 90 10 cm /Im0 Do Q".to_vec())
        .unwrap();

    let pm = doc.render_page(0, 1.0).unwrap();
    // 回転により上半分赤・下半分青が「左右に分かれる」向きになるはず。
    // 中央付近で左右の色が異なる（赤系と青系）ことを確認する。
    let left = pm.pixel(20, 50).unwrap();
    let right = pm.pixel(80, 50).unwrap();
    // 一方が赤寄り、他方が青寄り。
    let left_red = left[0] as i32 - left[2] as i32;
    let right_red = right[0] as i32 - right[2] as i32;
    assert!(
        (left_red > 100 && right_red < -100) || (left_red < -100 && right_red > 100),
        "回転後に左右で色が分かれていない: left={:?} right={:?}",
        left,
        right
    );
}

/// ImageMask: ステンシルが現在の塗り色（赤）で塗られる。
#[test]
fn render_image_mask_uses_fill_color() {
    let mut doc = Document::new();
    doc.add_page(100.0, 100.0).unwrap();
    // 2x1 ImageMask: ビット列 0b01 → px0=塗る, px1=透明。
    let mut d = Dictionary::new();
    d.set("Type", Object::name("XObject"));
    d.set("Subtype", Object::name("Image"));
    d.set("Width", Object::Integer(2));
    d.set("Height", Object::Integer(1));
    d.set("ImageMask", Object::Boolean(true));
    let stream = Stream::new(d, vec![0b0100_0000u8]);
    add_image_xobject(&mut doc, "Im0", stream);
    // 赤の塗り色を設定し、画像を全面に配置。
    doc.append_content_bytes(0, b"q 1 0 0 rg 100 0 0 100 0 0 cm /Im0 Do Q".to_vec())
        .unwrap();

    let pm = doc.render_page(0, 1.0).unwrap();
    // 左半分（px0=塗る）は赤、右半分（px1=透明）は白。
    let left = pm.pixel(25, 50).unwrap();
    assert!(near(left, [255, 0, 0], 8), "左半分が赤でない: {:?}", left);
    let right = pm.pixel(75, 50).unwrap();
    assert!(
        near(right, [255, 255, 255], 8),
        "右半分が透明でない: {:?}",
        right
    );
}

/// SMask: 半透明（アルファ 128）の青画像を白地に合成 → 中間色になる。
#[test]
fn render_image_smask_alpha() {
    let mut doc = Document::new();
    doc.add_page(100.0, 100.0).unwrap();
    // SMask: 1x1 DeviceGray 値 128（半透明）。
    let mut sm = Dictionary::new();
    sm.set("Type", Object::name("XObject"));
    sm.set("Subtype", Object::name("Image"));
    sm.set("Width", Object::Integer(1));
    sm.set("Height", Object::Integer(1));
    sm.set("BitsPerComponent", Object::Integer(8));
    sm.set("ColorSpace", Object::name("DeviceGray"));
    let smask = Stream::new(sm, vec![128u8]);

    // 本体: 1x1 青。
    let mut d = Dictionary::new();
    d.set("Type", Object::name("XObject"));
    d.set("Subtype", Object::name("Image"));
    d.set("Width", Object::Integer(1));
    d.set("Height", Object::Integer(1));
    d.set("BitsPerComponent", Object::Integer(8));
    d.set("ColorSpace", Object::name("DeviceRGB"));
    d.set("SMask", Object::Stream(smask));
    let stream = Stream::new(d, vec![0u8, 0, 255]);
    add_image_xobject(&mut doc, "Im0", stream);

    doc.append_content_bytes(0, b"q 100 0 0 100 0 0 cm /Im0 Do Q".to_vec())
        .unwrap();

    let pm = doc.render_page(0, 1.0).unwrap();
    // 青(0,0,255) を白(255,255,255) に α≈128/255 で合成 →
    // R=G≈(255*127)/255≈127, B≈255。
    let c = pm.pixel(50, 50).unwrap();
    assert!(
        (c[0] as i32 - 127).abs() <= 16 && (c[1] as i32 - 127).abs() <= 16 && c[2] > 220,
        "半透明合成が想定外: {:?}",
        c
    );
}

/// インライン画像（BI、フィルタなし）が描画される。
#[test]
fn render_inline_image() {
    let mut doc = Document::new();
    doc.add_page(100.0, 100.0).unwrap();
    // 1x1 RGB 緑のインライン画像を全面に。
    // BI /W 1 /H 1 /BPC 8 /CS /RGB ID <00 FF 00> EI
    let mut content = Vec::new();
    content.extend_from_slice(b"q 100 0 0 100 0 0 cm BI /W 1 /H 1 /BPC 8 /CS /RGB ID ");
    content.extend_from_slice(&[0x00, 0xFF, 0x00]);
    content.extend_from_slice(b" EI Q");
    doc.append_content_bytes(0, content).unwrap();

    let pm = doc.render_page(0, 1.0).unwrap();
    let c = pm.pixel(50, 50).unwrap();
    assert!(near(c, [0, 255, 0], 8), "インライン画像が緑でない: {:?}", c);
}

/// 壊れた画像データ（長さ不足・不正 bpc）で panic せず継続する。
#[test]
fn render_corrupt_image_no_panic() {
    let mut doc = Document::new();
    doc.add_page(100.0, 100.0).unwrap();
    // bpc=8 RGB だがデータが極端に短い。
    let mut d = Dictionary::new();
    d.set("Type", Object::name("XObject"));
    d.set("Subtype", Object::name("Image"));
    d.set("Width", Object::Integer(50));
    d.set("Height", Object::Integer(50));
    d.set("BitsPerComponent", Object::Integer(8));
    d.set("ColorSpace", Object::name("DeviceRGB"));
    let stream = Stream::new(d, vec![1, 2, 3]); // 全然足りない
    add_image_xobject(&mut doc, "Im0", stream);
    doc.append_content_bytes(0, b"q 50 0 0 50 0 0 cm /Im0 Do Q".to_vec())
        .unwrap();
    // panic せず描画が返る。
    let pm = doc.render_page(0, 1.0).unwrap();
    assert_eq!((pm.width(), pm.height()), (100, 100));

    // 不正 bpc（3）の画像は読み飛ばされる。
    let mut doc2 = Document::new();
    doc2.add_page(100.0, 100.0).unwrap();
    let mut d2 = Dictionary::new();
    d2.set("Type", Object::name("XObject"));
    d2.set("Subtype", Object::name("Image"));
    d2.set("Width", Object::Integer(4));
    d2.set("Height", Object::Integer(4));
    d2.set("BitsPerComponent", Object::Integer(3));
    d2.set("ColorSpace", Object::name("DeviceGray"));
    let stream2 = Stream::new(d2, vec![0u8; 16]);
    add_image_xobject(&mut doc2, "Im0", stream2);
    doc2.append_content_bytes(0, b"q 50 0 0 50 0 0 cm /Im0 Do Q".to_vec())
        .unwrap();
    let pm2 = doc2.render_page(0, 1.0).unwrap();
    assert_eq!((pm2.width(), pm2.height()), (100, 100));
}

/// JPEG 画像 XObject を描画し、中心数点の色が期待 RGB と誤差 ≤12 で一致する。
#[test]
fn render_jpeg_image_xobject() {
    use std::path::PathBuf;
    let mut jpg_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    jpg_path.push("tests");
    jpg_path.push("fixtures");
    let mut rgb_path = jpg_path.clone();
    jpg_path.push("solid_16_q90.jpg");
    rgb_path.push("solid_16_q90.rgb");
    if !jpg_path.exists() || !rgb_path.exists() {
        eprintln!("JPEG フィクスチャが無いためスキップ");
        return;
    }
    let jpg = std::fs::read(&jpg_path).unwrap();
    let expected = std::fs::read(&rgb_path).unwrap();
    // 16x16 RGB 期待値。中心ピクセル (8,8) の色。
    let w = 16usize;
    let cx = 8usize;
    let cy = 8usize;
    let exp = [
        expected[(cy * w + cx) * 3],
        expected[(cy * w + cx) * 3 + 1],
        expected[(cy * w + cx) * 3 + 2],
    ];

    let mut doc = Document::new();
    doc.add_page(64.0, 64.0).unwrap();
    let mut d = Dictionary::new();
    d.set("Type", Object::name("XObject"));
    d.set("Subtype", Object::name("Image"));
    d.set("Width", Object::Integer(16));
    d.set("Height", Object::Integer(16));
    d.set("BitsPerComponent", Object::Integer(8));
    d.set("ColorSpace", Object::name("DeviceRGB"));
    d.set("Filter", Object::name("DCTDecode"));
    // DCTDecode は new() ではフィルタを消されるため手で辞書を組む。
    let stream = Stream {
        dict: {
            d.set("Length", Object::Integer(jpg.len() as i64));
            d
        },
        data: jpg,
    };
    add_image_xobject(&mut doc, "Im0", stream);
    // 64x64 ページ全面に配置（4 倍拡大）。
    doc.append_content_bytes(0, b"q 64 0 0 64 0 0 cm /Im0 Do Q".to_vec())
        .unwrap();

    let pm = doc.render_page(0, 1.0).unwrap();
    // ページ中心 (32,32) ≈ 画像中心。
    let c = pm.pixel(32, 32).unwrap();
    assert!(
        near(c, exp, 12),
        "JPEG 中心色が期待値と一致しない: got={:?} exp={:?}",
        c,
        exp
    );
}

// ---------------------------------------------------------------------------
// Phase 7: レンダリング性能・制御（RenderOptions）
// ---------------------------------------------------------------------------

use pdf_rust::{PdfError, RenderOptions, RenderQuality};

/// テスト用: 矩形・斜め線・ベジェ・テキストを含むページを作る。
fn build_mixed_page() -> Document {
    let mut doc = Document::new();
    doc.add_page(200.0, 150.0).unwrap();
    doc.draw_rect(
        0,
        30.0,
        40.0,
        80.0,
        50.0,
        &DrawOptions {
            stroke_color: (0.0, 0.0, 1.0),
            fill_color: Some((0.9, 0.3, 0.1)),
            line_width: 2.0,
        },
    )
    .unwrap();
    doc.draw_line(
        0,
        (10.0, 10.0),
        (190.0, 140.0),
        &DrawOptions {
            stroke_color: (0.0, 0.5, 0.0),
            line_width: 3.0,
            ..Default::default()
        },
    )
    .unwrap();
    // ベジェ曲線（生コンテント）。
    doc.append_content_bytes(
        0,
        b"q 0 0 0 RG 2 w 20 100 m 60 140 140 60 180 100 c S Q".to_vec(),
    )
    .unwrap();
    doc.add_text(
        0,
        "Tile",
        &TextOptions {
            size: 24.0,
            x: 60.0,
            y: 110.0,
            ..Default::default()
        },
    )
    .unwrap();
    doc
}

/// 全面レンダ結果から領域を切り出した結果と、region 指定のタイルレンダが
/// ピクセル一致する（浮動小数の丸め揺れとして各チャンネル ±1 まで許容）。
#[test]
fn render_region_matches_full_crop() {
    let doc = build_mixed_page();
    let opts_full = RenderOptions {
        scale: 2.0,
        ..Default::default()
    };
    let full = doc.render_page_with(0, &opts_full).unwrap();

    // ページ中央のタイル [x=100, y=80, w=120, h=90]。
    let (rx, ry, rw, rh) = (100u32, 80u32, 120u32, 90u32);
    let opts_tile = RenderOptions {
        scale: 2.0,
        region: Some([rx as f64, ry as f64, rw as f64, rh as f64]),
        ..Default::default()
    };
    let tile = doc.render_page_with(0, &opts_tile).unwrap();
    assert_eq!((tile.width(), tile.height()), (rw, rh));

    let mut max_diff = 0i32;
    for y in 0..rh {
        for x in 0..rw {
            let a = tile.pixel(x, y).unwrap();
            let b = full.pixel(rx + x, ry + y).unwrap();
            for i in 0..3 {
                max_diff = max_diff.max((a[i] as i32 - b[i] as i32).abs());
            }
        }
    }
    assert!(
        max_diff <= 1,
        "タイルと全面切り出しの最大差 {} が 1 を超えた",
        max_diff
    );
}

/// ページ外にはみ出した領域は白のまま、ページ内部分は一致する。
#[test]
fn render_region_outside_page_is_white() {
    let doc = build_mixed_page();
    // ページは 200x150 @ scale 1.0。右下へはみ出すタイル。
    let opts = RenderOptions {
        region: Some([180.0, 130.0, 40.0, 40.0]),
        ..Default::default()
    };
    let tile = doc.render_page_with(0, &opts).unwrap();
    assert_eq!((tile.width(), tile.height()), (40, 40));
    // ページ外（タイル内座標 (30, 30) = デバイス (210, 160)）は白。
    assert_eq!(tile.pixel(30, 30), Some([255, 255, 255]));
}

/// 不正な領域（負サイズ・非有限）は Err。
#[test]
fn render_region_invalid_rejected() {
    let doc = build_mixed_page();
    for region in [
        [0.0, 0.0, -10.0, 10.0],
        [0.0, 0.0, 10.0, 0.0],
        [f64::NAN, 0.0, 10.0, 10.0],
    ] {
        let opts = RenderOptions {
            region: Some(region),
            ..Default::default()
        };
        assert!(
            doc.render_page_with(0, &opts).is_err(),
            "region {:?} が拒否されない",
            region
        );
    }
}

/// 協調キャンセル: フラグを事前に立てると PdfError::Cancelled が返る。
#[test]
fn render_cancel_returns_cancelled() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let doc = build_mixed_page();
    let flag = Arc::new(AtomicBool::new(true));
    let opts = RenderOptions {
        cancel: Some(flag.clone()),
        ..Default::default()
    };
    match doc.render_page_with(0, &opts) {
        Err(PdfError::Cancelled) => {}
        other => panic!("Cancelled が返らない: {other:?}"),
    }

    // フラグを下ろせば普通に描ける。
    flag.store(false, Ordering::Relaxed);
    assert!(doc.render_page_with(0, &opts).is_ok());
}

/// render_page_into: バッファ再利用でも render_page_with と全ピクセル一致。
/// サイズ不一致のバッファを渡しても内部で作り直される。
#[test]
fn render_into_reuses_buffer() {
    let doc = build_mixed_page();
    let opts = RenderOptions::default();
    let expected = doc.render_page_with(0, &opts).unwrap();

    // わざと違うサイズ + 汚れた内容のバッファを使い回す。
    let mut pm = pdf_rust::Pixmap::new(10, 10);
    pm.fill([0, 0, 0]);
    doc.render_page_into(0, &opts, &mut pm).unwrap();
    assert_eq!(
        (pm.width(), pm.height()),
        (expected.width(), expected.height())
    );
    assert_eq!(
        pm.data(),
        expected.data(),
        "into の結果が with と一致しない"
    );

    // 同じバッファで 2 ページ目相当（同ページ再描画）も一致する。
    doc.render_page_into(0, &opts, &mut pm).unwrap();
    assert_eq!(pm.data(), expected.data());
}

/// annotations: false で注釈外観（/AP /N）が描かれない。
#[test]
fn render_annotations_toggle() {
    let mut doc = Document::new();
    let p0 = doc.add_page(200.0, 200.0).unwrap();

    // 外観: BBox 全面を赤く塗る Form。
    let mut ap_dict = Dictionary::new();
    ap_dict.set("Type", Object::name("XObject"));
    ap_dict.set("Subtype", Object::name("Form"));
    ap_dict.set(
        "BBox",
        Object::Array(vec![0.into(), 0.into(), 10.into(), 10.into()]),
    );
    let ap_stream = Stream::new(ap_dict, b"1 0 0 rg 0 0 10 10 re f".to_vec());
    let ap_id = doc.add_object(Object::Stream(ap_stream));
    let mut n = Dictionary::new();
    n.set("N", Object::Reference(ap_id));
    let mut annot = Dictionary::new();
    annot.set("Subtype", Object::name("Square"));
    annot.set(
        "Rect",
        Object::Array(vec![80.into(), 80.into(), 120.into(), 120.into()]),
    );
    annot.set("AP", n);
    let annot_id = doc.add_object(annot);
    let page = doc.get_object_mut(p0).unwrap().as_dict_mut().unwrap();
    page.set("Annots", Object::Array(vec![Object::Reference(annot_id)]));

    // 既定（annotations: true）は描かれる。Rect 中心 (100,100) → デバイス (100, 100)。
    let on = doc.render_page_with(0, &RenderOptions::default()).unwrap();
    assert_eq!(
        on.pixel(100, 100),
        Some([255, 0, 0]),
        "注釈が描かれていない"
    );

    // annotations: false で白のまま。
    let off = doc
        .render_page_with(
            0,
            &RenderOptions {
                annotations: false,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(off.pixel(100, 100), Some([255, 255, 255]), "注釈が描かれた");
}

/// quality: Fast でも図形の内部（AA に依らない部分）は Normal と一致し、
/// 全体としても描画が成立する。
#[test]
fn render_quality_fast() {
    let doc = build_mixed_page();
    let normal = doc.render_page_with(0, &RenderOptions::default()).unwrap();
    let fast = doc
        .render_page_with(
            0,
            &RenderOptions {
                quality: RenderQuality::Fast,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        (fast.width(), fast.height()),
        (normal.width(), normal.height())
    );
    // 矩形内部（PDF (70, 65) → デバイス (70, 150-65=85)）は両方とも同じ塗り色。
    assert_eq!(fast.pixel(70, 85), normal.pixel(70, 85));
    // 外側の白も一致。
    assert_eq!(fast.pixel(5, 70), Some([255, 255, 255]));
}

/// page_size: /Rotate 反映済みの論理サイズ（pt）を返す。
#[test]
fn page_size_reflects_rotation() {
    let mut doc = Document::new();
    doc.add_page(200.0, 150.0).unwrap();
    assert_eq!(doc.page_size(0).unwrap(), (200.0, 150.0));

    doc.rotate_page(0, 90).unwrap();
    assert_eq!(doc.page_size(0).unwrap(), (150.0, 200.0));

    doc.rotate_page(0, 90).unwrap(); // 計 180 度
    assert_eq!(doc.page_size(0).unwrap(), (200.0, 150.0));

    // render_page のデバイスサイズと整合する。
    let pm = doc.render_page(0, 2.0).unwrap();
    assert_eq!((pm.width(), pm.height()), (400, 300));

    // 範囲外は Err。
    assert!(doc.page_size(9).is_err());
}

// ---------------------------------------------------------------------------
// Phase 6: シェーディング・パターン
// ---------------------------------------------------------------------------

/// 軸方向シェーディング (`sh`) を Document 経由で描画する。
///
/// ページに `/Resources /Shading /Sh1` を直接登録し、コンテントから `sh` を
/// 呼び出して、左端が黒・右端が赤になるグラデーションを確認する。
#[test]
fn render_axial_shading_via_document() {
    use pdf_rust::object::{Dictionary, Object};

    let mut doc = Document::new();
    let page_id = doc.add_page(100.0, 50.0).unwrap();

    // Type 2 関数（C0=黒、C1=赤）。
    let mut func = Dictionary::new();
    func.set("FunctionType", Object::Integer(2));
    func.set(
        "Domain",
        Object::Array(vec![Object::Real(0.0), Object::Real(1.0)]),
    );
    func.set("N", Object::Integer(1));
    func.set(
        "C0",
        Object::Array(vec![
            Object::Real(0.0),
            Object::Real(0.0),
            Object::Real(0.0),
        ]),
    );
    func.set(
        "C1",
        Object::Array(vec![
            Object::Real(1.0),
            Object::Real(0.0),
            Object::Real(0.0),
        ]),
    );

    // Shading 辞書（Axial、ユーザー座標 0..100 を補間範囲とする）。
    let mut shading = Dictionary::new();
    shading.set("ShadingType", Object::Integer(2));
    shading.set("ColorSpace", Object::name("DeviceRGB"));
    shading.set(
        "Coords",
        Object::Array(vec![
            Object::Real(0.0),
            Object::Real(0.0),
            Object::Real(100.0),
            Object::Real(0.0),
        ]),
    );
    shading.set("Function", Object::Dictionary(func));

    // /Resources /Shading /Sh1 をページに登録する。
    let mut shadings = Dictionary::new();
    shadings.set("Sh1", Object::Dictionary(shading));
    let mut new_res = Dictionary::new();
    new_res.set("Shading", Object::Dictionary(shadings));
    {
        let page = doc.get_object_mut(page_id).unwrap().as_dict_mut().unwrap();
        page.set("Resources", Object::Dictionary(new_res));
    }

    // 全画面に sh で塗る。
    doc.append_content_bytes(0, b"/Sh1 sh".to_vec()).unwrap();
    let pm = doc.render_page(0, 1.0).unwrap();

    // 左端（user x=0 ≈ 黒）、右端（user x=100 ≈ 赤）。
    let left = pm.pixel(2, 25).unwrap();
    let right = pm.pixel(97, 25).unwrap();
    assert!(left[0] < 30, "左端が黒くない: {:?}", left);
    assert!(right[0] > 220, "右端が赤くない: {:?}", right);
    assert_eq!(left[1], 0);
    assert_eq!(right[1], 0);

    // to_bytes → from_bytes 後も同じ描画になる（書き出し前後で /Resources /Shading が保たれる）。
    let bytes = doc.to_bytes().unwrap();
    let doc2 = Document::from_bytes(&bytes).unwrap();
    let pm2 = doc2.render_page(0, 1.0).unwrap();
    assert_eq!(pm.data(), pm2.data(), "往復後にピクセルが変化した");
}
