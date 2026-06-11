//! 統合テスト: 生成 → 保存 → 再読込 → 抽出 の往復、
//! 最新形式（xref ストリーム + オブジェクトストリーム）の読み込み、
//! 壊れた xref からの復元。

use pdf_rust::filters::flate;
use pdf_rust::{Document, DrawOptions, Object, StandardFont, TextOptions};

/// 生成 → 保存 → 再読込 → テキスト抽出の完全な往復。
#[test]
fn roundtrip_create_save_reload_extract() {
    let mut doc = Document::new();
    doc.add_page(595.0, 842.0).unwrap(); // A4
    doc.add_page(595.0, 842.0).unwrap();

    doc.add_text(
        0,
        "Hello, PDF!\nSecond line",
        &TextOptions {
            size: 24.0,
            x: 72.0,
            y: 770.0,
            ..Default::default()
        },
    )
    .unwrap();
    doc.add_text(
        1,
        "Page two",
        &TextOptions {
            font: StandardFont::TimesRoman,
            ..Default::default()
        },
    )
    .unwrap();
    doc.draw_rect(
        0,
        50.0,
        50.0,
        200.0,
        100.0,
        &DrawOptions {
            fill_color: Some((1.0, 0.9, 0.2)),
            ..Default::default()
        },
    )
    .unwrap();
    doc.draw_line(0, (50.0, 40.0), (250.0, 40.0), &DrawOptions::default())
        .unwrap();
    doc.set_title("統合テスト 📄").unwrap();
    doc.set_info_text("Author", "pdf_rust").unwrap();

    let bytes = doc.to_bytes().unwrap();
    assert!(bytes.starts_with(b"%PDF-1.7"));

    let reloaded = Document::from_bytes(&bytes).unwrap();
    assert_eq!(reloaded.page_count(), 2);
    assert_eq!(reloaded.title().unwrap(), "統合テスト 📄");
    assert_eq!(reloaded.info_text("Author").unwrap(), "pdf_rust");

    let text0 = reloaded.extract_text(0).unwrap();
    assert!(text0.contains("Hello, PDF!"), "page0 text: {text0:?}");
    assert!(text0.contains("Second line"));
    // 改行が保たれている
    assert!(
        text0.contains("Hello, PDF!\nSecond line"),
        "page0 text: {text0:?}"
    );
    let text1 = reloaded.extract_text(1).unwrap();
    assert!(text1.contains("Page two"));

    // ファイル経由でも同じ
    let path = std::env::temp_dir().join("pdf_rust_roundtrip_test.pdf");
    doc.save(&path).unwrap();
    let from_file = Document::load(&path).unwrap();
    assert_eq!(from_file.page_count(), 2);
    let _ = std::fs::remove_file(&path);
}

/// 再保存（ロード → 編集 → 保存 → 再ロード）の安定性。
#[test]
fn edit_loaded_document() {
    let mut doc = Document::new();
    doc.add_page(612.0, 792.0).unwrap();
    doc.add_text(0, "original", &TextOptions::default())
        .unwrap();
    let bytes = doc.to_bytes().unwrap();

    let mut doc2 = Document::from_bytes(&bytes).unwrap();
    doc2.add_page(595.0, 842.0).unwrap();
    doc2.add_text(1, "appended page", &TextOptions::default())
        .unwrap();
    doc2.add_text(
        0,
        "stamped",
        &TextOptions {
            y: 100.0,
            ..Default::default()
        },
    )
    .unwrap();
    doc2.rotate_page(0, 90).unwrap();
    let bytes2 = doc2.to_bytes().unwrap();

    let doc3 = Document::from_bytes(&bytes2).unwrap();
    assert_eq!(doc3.page_count(), 2);
    let t0 = doc3.extract_text(0).unwrap();
    assert!(t0.contains("original") && t0.contains("stamped"));
    assert!(doc3.extract_text(1).unwrap().contains("appended page"));

    // ページ削除
    let mut doc4 = Document::from_bytes(&bytes2).unwrap();
    doc4.remove_page(0).unwrap();
    let doc5 = Document::from_bytes(&doc4.to_bytes().unwrap()).unwrap();
    assert_eq!(doc5.page_count(), 1);
    assert!(doc5.extract_text(0).unwrap().contains("appended page"));
}

/// TJ 配列の字送り調整から空白を復元する。
#[test]
fn tj_array_spacing() {
    use pdf_rust::Operation;
    let mut doc = Document::new();
    doc.add_page(595.0, 842.0).unwrap();
    let ops = vec![
        Operation::new("BT", vec![]),
        Operation::new("Tf", vec![Object::name("F1"), 12.into()]),
        Operation::new("Td", vec![72.into(), 700.into()]),
        Operation::new(
            "TJ",
            vec![Object::Array(vec![
                Object::string_literal("Hel"),
                Object::Integer(-20), // 小さい調整: 空白にしない
                Object::string_literal("lo"),
                Object::Integer(-300), // 大きい調整: 空白
                Object::string_literal("world"),
            ])],
        ),
        Operation::new("ET", vec![]),
    ];
    doc.append_content(0, &ops).unwrap();
    let reloaded = Document::from_bytes(&doc.to_bytes().unwrap()).unwrap();
    assert_eq!(reloaded.extract_text(0).unwrap(), "Hello world");
}

/// xref ストリーム + オブジェクトストリームを使う「最新形式」の PDF を
/// 手組みで構築して読めることを確認する。
#[test]
fn loads_xref_stream_and_object_stream_pdf() {
    let pdf = build_modern_pdf();
    let doc = Document::from_bytes(&pdf).unwrap();
    assert_eq!(doc.version, "1.5");
    assert_eq!(doc.page_count(), 1);
    let text = doc.extract_text(0).unwrap();
    assert!(text.contains("Modern"), "extracted: {text:?}");
    // ObjStm 内にあったカタログが読めている
    let cat = doc.catalog().unwrap();
    assert_eq!(cat.get("Type").unwrap().as_name().unwrap(), "Catalog");
}

/// startxref を破壊しても全走査による再構築で読める。
#[test]
fn recovers_from_broken_startxref() {
    let mut doc = Document::new();
    doc.add_page(595.0, 842.0).unwrap();
    doc.add_text(0, "survivor", &TextOptions::default())
        .unwrap();
    let mut bytes = doc.to_bytes().unwrap();

    // startxref のオフセット数値を壊す
    let pos = find_last(&bytes, b"startxref").unwrap();
    for b in &mut bytes[pos + 10..] {
        if b.is_ascii_digit() {
            *b = b'9';
        }
    }
    let recovered = Document::from_bytes(&bytes).unwrap();
    assert_eq!(recovered.page_count(), 1);
    assert!(recovered.extract_text(0).unwrap().contains("survivor"));
}

/// 暗号化 PDF は明示的なエラーになる。
#[test]
fn rejects_encrypted_pdf() {
    let mut doc = Document::new();
    doc.add_page(595.0, 842.0).unwrap();
    let mut bytes = doc.to_bytes().unwrap();
    // trailer に /Encrypt を注入した亜種を作る
    let pos = find_last(&bytes, b"/Size").unwrap();
    let inject = b"/Encrypt 99 0 R ".to_vec();
    bytes.splice(pos..pos, inject);
    match Document::from_bytes(&bytes) {
        Err(pdf_rust::PdfError::EncryptionNotSupported) => {}
        other => panic!("expected EncryptionNotSupported, got {other:?}"),
    }
}

fn find_last(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    (0..=haystack.len().saturating_sub(needle.len()))
        .rev()
        .find(|&i| &haystack[i..i + needle.len()] == needle)
}

/// PDF 1.5 形式（xref ストリーム + ObjStm）のファイルをバイト単位で構築する。
fn build_modern_pdf() -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(b"%PDF-1.5\n%\xE2\xE3\xCF\xD3\n");

    // --- obj 4: ページのコンテントストリーム（Flate 圧縮） ---
    let content = b"BT /F1 24 Tf 72 700 Td (Modern) Tj ET";
    let comp = flate::compress(content);
    let off4 = out.len();
    out.extend_from_slice(
        format!(
            "4 0 obj\n<< /Length {} /Filter /FlateDecode >>\nstream\n",
            comp.len()
        )
        .as_bytes(),
    );
    out.extend_from_slice(&comp);
    out.extend_from_slice(b"\nendstream\nendobj\n");

    // --- obj 5: ObjStm（カタログ・ページツリー・ページを圧縮格納） ---
    let inner: [(u32, &[u8]); 3] = [
        (1, b"<< /Type /Catalog /Pages 2 0 R >>"),
        (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
        (
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R >>",
        ),
    ];
    let mut header = String::new();
    let mut payload: Vec<u8> = Vec::new();
    for (num, body) in inner {
        header.push_str(&format!("{num} {} ", payload.len()));
        payload.extend_from_slice(body);
        payload.push(b'\n');
    }
    let first = header.len();
    let mut objstm_data = header.into_bytes();
    objstm_data.extend_from_slice(&payload);
    let comp = flate::compress(&objstm_data);
    let off5 = out.len();
    out.extend_from_slice(
        format!(
            "5 0 obj\n<< /Type /ObjStm /N 3 /First {first} /Length {} /Filter /FlateDecode >>\nstream\n",
            comp.len()
        )
        .as_bytes(),
    );
    out.extend_from_slice(&comp);
    out.extend_from_slice(b"\nendstream\nendobj\n");

    // --- obj 6: クロスリファレンスストリーム ---
    let off6 = out.len();
    // W = [1 4 2]: type(1) / field2(4) / field3(2)
    let mut rows: Vec<u8> = Vec::new();
    let push_row = |t: u8, f2: u32, f3: u16, rows: &mut Vec<u8>| {
        rows.push(t);
        rows.extend_from_slice(&f2.to_be_bytes());
        rows.extend_from_slice(&f3.to_be_bytes());
    };
    push_row(0, 0, 0xFFFF, &mut rows); // obj 0: free
    push_row(2, 5, 0, &mut rows); // obj 1: ObjStm 5 内 index 0
    push_row(2, 5, 1, &mut rows); // obj 2
    push_row(2, 5, 2, &mut rows); // obj 3
    push_row(1, off4 as u32, 0, &mut rows); // obj 4
    push_row(1, off5 as u32, 0, &mut rows); // obj 5
    push_row(1, off6 as u32, 0, &mut rows); // obj 6 (自分自身)
    let comp = flate::compress(&rows);
    out.extend_from_slice(
        format!(
            "6 0 obj\n<< /Type /XRef /Size 7 /W [1 4 2] /Root 1 0 R /Length {} /Filter /FlateDecode >>\nstream\n",
            comp.len()
        )
        .as_bytes(),
    );
    out.extend_from_slice(&comp);
    out.extend_from_slice(b"\nendstream\nendobj\n");

    out.extend_from_slice(format!("startxref\n{off6}\n%%EOF\n").as_bytes());
    out
}
