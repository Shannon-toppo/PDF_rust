//! CCITTFaxDecode 統合テスト。
//!
//! ハンドエンコードした T.6 / T.4 ストリームを画像 XObject や ImageMask に
//! 載せ、フィルタチェーンと描画パイプラインを通しで検証する。CCITT エンコーダ
//! を持たないため、`src/filters/ccitt.rs` のユニットテストで使ったビット列を
//! 流用し、PDF オブジェクトとして組み立てて読み込み・描画する。

use pdf_rust::filters::{self, ccitt};
use pdf_rust::object::{Dictionary, Object, Stream};
use pdf_rust::Document;

/// MSB ファーストの "0"/"1" 文字列をバイト列にする（末尾はビット 0 でパディング）。
fn bits(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut byte: u8 = 0;
    let mut n: u8 = 0;
    for c in s.chars() {
        if c == ' ' {
            continue;
        }
        byte <<= 1;
        if c == '1' {
            byte |= 1;
        }
        n += 1;
        if n == 8 {
            out.push(byte);
            byte = 0;
            n = 0;
        }
    }
    if n > 0 {
        byte <<= 8 - n;
        out.push(byte);
    }
    out
}

/// 8x1 全黒行（T.6, H モード）。
fn t6_row_all_black_8() -> Vec<u8> {
    // H + 白 0 (00110101) + 黒 8 (000101)
    bits("001 00110101 000101")
}

/// 8x1 行 5W + 3B（T.6, H モード）。
fn t6_row_5w_3b() -> Vec<u8> {
    // H + 白 5 (1100) + 黒 3 (10)
    bits("001 1100 10")
}

/// `decode_stream` 経由で CCITT が呼ばれることを確認（filters/mod.rs 配線テスト）。
#[test]
fn decode_stream_dispatches_to_ccitt() {
    let mut dict = Dictionary::new();
    dict.set("Filter", Object::name("CCITTFaxDecode"));
    let mut parms = Dictionary::new();
    parms.set("K", Object::Integer(-1));
    parms.set("Columns", Object::Integer(8));
    parms.set("Rows", Object::Integer(1));
    parms.set("EndOfBlock", Object::Boolean(false));
    parms.set("BlackIs1", Object::Boolean(true));
    dict.set("DecodeParms", Object::Dictionary(parms));

    let data = t6_row_5w_3b();
    let out = filters::decode_stream(&dict, &data, None).unwrap();
    // BlackIs1=true なので内部表現そのまま: 上位 5 ビット白 + 下位 3 ビット黒
    // → 0b00000111 = 0x07
    assert_eq!(out, vec![0x07]);
}

/// 省略フィルタ名 `/CCF` も同様にディスパッチされる。
#[test]
fn decode_stream_accepts_ccf_abbreviation() {
    let mut dict = Dictionary::new();
    dict.set("F", Object::name("CCF"));
    let mut parms = Dictionary::new();
    parms.set("K", Object::Integer(-1));
    parms.set("Columns", Object::Integer(8));
    parms.set("Rows", Object::Integer(1));
    parms.set("EndOfBlock", Object::Boolean(false));
    dict.set("DP", Object::Dictionary(parms));

    let data = t6_row_5w_3b();
    let out = filters::decode_stream(&dict, &data, None).unwrap();
    // BlackIs1=false（既定）→ 1=白に反転。0b11111000 = 0xF8
    assert_eq!(out, vec![0xF8]);
}

/// `Document::to_bytes`→`from_bytes` 往復で CCITT ストリームが保持され、
/// 再読み込み後に正しくデコードできる。
#[test]
fn ccitt_stream_roundtrips_through_document() {
    let mut doc = Document::new();
    doc.add_page(100.0, 100.0).unwrap();

    // CCITT エンコード済みのストリームを画像 XObject として追加する。
    let mut d = Dictionary::new();
    d.set("Type", Object::name("XObject"));
    d.set("Subtype", Object::name("Image"));
    d.set("Width", Object::Integer(8));
    d.set("Height", Object::Integer(1));
    d.set("BitsPerComponent", Object::Integer(1));
    d.set("ColorSpace", Object::name("DeviceGray"));
    let mut stream = Stream::new(d, t6_row_5w_3b());
    // Stream::new は Filter/DecodeParms を削除するため、構築後に設定する。
    stream.dict.set("Filter", Object::name("CCITTFaxDecode"));
    let mut parms = Dictionary::new();
    parms.set("K", Object::Integer(-1));
    parms.set("Columns", Object::Integer(8));
    parms.set("Rows", Object::Integer(1));
    parms.set("EndOfBlock", Object::Boolean(false));
    stream.dict.set("DecodeParms", Object::Dictionary(parms));
    let img_id = doc.add_object(Object::Stream(stream));

    // ページのリソースに登録。
    let page_id = doc.page_id(0).unwrap();
    let mut resources = doc.page_resources(page_id);
    let mut xobjects = match resources.get("XObject").and_then(|o| o.as_dict().ok()) {
        Some(d) => d.clone(),
        None => Dictionary::new(),
    };
    xobjects.set("Im0", Object::Reference(img_id));
    resources.set("XObject", Object::Dictionary(xobjects));
    doc.get_object_mut(page_id)
        .unwrap()
        .as_dict_mut()
        .unwrap()
        .set("Resources", resources);
    doc.append_content_bytes(0, b"q 100 0 0 100 0 0 cm /Im0 Do Q".to_vec())
        .unwrap();

    // 直接描画（CCITT 統合パスの検証）+ 保存→再読込で同じ描画が得られる。
    let pm = doc.render_page(0, 1.0).unwrap();
    let bytes = doc.to_bytes().unwrap();
    let reloaded = Document::from_bytes(&bytes).unwrap();
    let pm2 = reloaded.render_page(0, 1.0).unwrap();
    // 直接描画と再読み込み後で同じピクセル
    assert_eq!(pm.pixel(10, 50), pm2.pixel(10, 50));
    assert_eq!(pm.pixel(90, 50), pm2.pixel(90, 50));
    // 左から ~5/8 が白、右 ~3/8 が黒。x=10 は白、x=90 は黒に近い。
    let left = pm.pixel(10, 50).unwrap();
    let right = pm.pixel(90, 50).unwrap();
    assert!(left[0] > 200, "左側が白でない: {:?}", left);
    assert!(right[0] < 80, "右側が黒でない: {:?}", right);
}

/// CCITT で符号化された ImageMask が塗り色でステンシル描画される。
#[test]
fn ccitt_image_mask_renders_with_fill_color() {
    let mut doc = Document::new();
    doc.add_page(100.0, 100.0).unwrap();

    // 8x1 ImageMask: 左 5px は「塗る」、右 3px は「透明」。
    // PDF 既定では ImageMask の bit 0 が塗り、bit 1 が透明。
    // CCITTFaxDecode の BlackIs1=false 既定で 1=白、0=黒の通常ビット出力になる。
    // 5W + 3B → 内部 0b00000111 → 反転 0b11111000 = 0xF8 が出力される。
    // ImageMask は /Decode 既定 [0, 1] で 0=塗る、1=透明とする。
    // つまり最終的に: 左 5px (bit=1) → 透明、右 3px (bit=0) → 塗り。
    // テスト意図と合わせるため Decode [1, 0] を指定して反転させ、
    // 「左 5px (bit=1) → 塗り、右 3px (bit=0) → 透明」にする。
    let mut d = Dictionary::new();
    d.set("Type", Object::name("XObject"));
    d.set("Subtype", Object::name("Image"));
    d.set("Width", Object::Integer(8));
    d.set("Height", Object::Integer(1));
    d.set("ImageMask", Object::Boolean(true));
    d.set(
        "Decode",
        Object::Array(vec![Object::Real(1.0), Object::Real(0.0)]),
    );
    let mut stream = Stream::new(d, t6_row_5w_3b());
    stream.dict.set("Filter", Object::name("CCITTFaxDecode"));
    let mut parms = Dictionary::new();
    parms.set("K", Object::Integer(-1));
    parms.set("Columns", Object::Integer(8));
    parms.set("Rows", Object::Integer(1));
    parms.set("EndOfBlock", Object::Boolean(false));
    stream.dict.set("DecodeParms", Object::Dictionary(parms));
    let img_id = doc.add_object(Object::Stream(stream));

    let page_id = doc.page_id(0).unwrap();
    let mut resources = doc.page_resources(page_id);
    let mut xobjects = match resources.get("XObject").and_then(|o| o.as_dict().ok()) {
        Some(d) => d.clone(),
        None => Dictionary::new(),
    };
    xobjects.set("Im0", Object::Reference(img_id));
    resources.set("XObject", Object::Dictionary(xobjects));
    doc.get_object_mut(page_id)
        .unwrap()
        .as_dict_mut()
        .unwrap()
        .set("Resources", resources);

    // 赤の塗り色でフルページ配置。
    doc.append_content_bytes(0, b"q 1 0 0 rg 100 0 0 100 0 0 cm /Im0 Do Q".to_vec())
        .unwrap();

    let pm = doc.render_page(0, 1.0).unwrap();
    // 左 5/8 領域 (~x<62.5) は赤、右 3/8 は塗らないので白（背景）。
    let left = pm.pixel(20, 50).unwrap();
    let right = pm.pixel(90, 50).unwrap();
    assert!(left[0] > 200 && left[1] < 50, "左が赤でない: {:?}", left);
    assert!(
        right[0] > 200 && right[1] > 200 && right[2] > 200,
        "右が白でない: {:?}",
        right
    );
}

/// T.4 1D 方式 (K=0) でも decode_stream 経由で動作する。
#[test]
fn t4_1d_via_decode_stream() {
    let mut dict = Dictionary::new();
    dict.set("Filter", Object::name("CCITTFaxDecode"));
    let mut parms = Dictionary::new();
    parms.set("K", Object::Integer(0));
    parms.set("Columns", Object::Integer(8));
    parms.set("Rows", Object::Integer(1));
    parms.set("EndOfBlock", Object::Boolean(false));
    parms.set("EndOfLine", Object::Boolean(false));
    dict.set("DecodeParms", Object::Dictionary(parms));

    // 白 5 (1100) + 黒 3 (10) を 1D MH で
    let data = bits("1100 10");
    let out = filters::decode_stream(&dict, &data, None).unwrap();
    assert_eq!(out, vec![0xF8]);
}

/// 全黒 1 行を T.6 で配置し、描画結果が真っ黒に近いことを確認する。
#[test]
fn ccitt_all_black_row_renders_black() {
    let mut doc = Document::new();
    doc.add_page(100.0, 100.0).unwrap();

    let mut d = Dictionary::new();
    d.set("Type", Object::name("XObject"));
    d.set("Subtype", Object::name("Image"));
    d.set("Width", Object::Integer(8));
    d.set("Height", Object::Integer(1));
    d.set("BitsPerComponent", Object::Integer(1));
    d.set("ColorSpace", Object::name("DeviceGray"));
    let mut stream = Stream::new(d, t6_row_all_black_8());
    stream.dict.set("Filter", Object::name("CCITTFaxDecode"));
    let mut parms = Dictionary::new();
    parms.set("K", Object::Integer(-1));
    parms.set("Columns", Object::Integer(8));
    parms.set("Rows", Object::Integer(1));
    parms.set("EndOfBlock", Object::Boolean(false));
    stream.dict.set("DecodeParms", Object::Dictionary(parms));
    let img_id = doc.add_object(Object::Stream(stream));

    let page_id = doc.page_id(0).unwrap();
    let mut resources = doc.page_resources(page_id);
    let mut xobjects = match resources.get("XObject").and_then(|o| o.as_dict().ok()) {
        Some(d) => d.clone(),
        None => Dictionary::new(),
    };
    xobjects.set("Im0", Object::Reference(img_id));
    resources.set("XObject", Object::Dictionary(xobjects));
    doc.get_object_mut(page_id)
        .unwrap()
        .as_dict_mut()
        .unwrap()
        .set("Resources", resources);
    doc.append_content_bytes(0, b"q 100 0 0 100 0 0 cm /Im0 Do Q".to_vec())
        .unwrap();

    let pm = doc.render_page(0, 1.0).unwrap();
    let center = pm.pixel(50, 50).unwrap();
    assert!(
        center[0] < 40 && center[1] < 40 && center[2] < 40,
        "中央が真っ黒でない: {:?}",
        center
    );
}

/// デバッグ用: Filter が配列で渡された場合（画像 XObject 経由のパス）の挙動。
#[test]
fn decode_stream_with_filter_array() {
    let mut dict = Dictionary::new();
    dict.set(
        "Filter",
        Object::Array(vec![Object::name("CCITTFaxDecode")]),
    );
    let mut parms = Dictionary::new();
    parms.set("K", Object::Integer(-1));
    parms.set("Columns", Object::Integer(8));
    parms.set("Rows", Object::Integer(1));
    parms.set("EndOfBlock", Object::Boolean(false));
    dict.set("DecodeParms", Object::Dictionary(parms));

    let data = t6_row_5w_3b();
    let out = filters::decode_stream(&dict, &data, None).unwrap();
    eprintln!("array-filter CCITT output: {:02X?}", out);
    assert_eq!(out, vec![0xF8]);
}

/// 非 CCITT・素の 8x1 グレー画像で「左白・右黒」が期待通り描けることを先に確認する
/// （CCITT を絡めない比較対象。これが正しく描けないなら CCITT 統合以前の問題）。
#[test]
fn baseline_gray_image_renders_left_white_right_black() {
    let mut doc = Document::new();
    doc.add_page(100.0, 100.0).unwrap();

    let mut d = Dictionary::new();
    d.set("Type", Object::name("XObject"));
    d.set("Subtype", Object::name("Image"));
    d.set("Width", Object::Integer(8));
    d.set("Height", Object::Integer(1));
    d.set("BitsPerComponent", Object::Integer(1));
    d.set("ColorSpace", Object::name("DeviceGray"));
    // 5 白 (=1) + 3 黒 (=0) を 1 バイトに詰める → 0xF8
    let stream = Stream::new(d, vec![0xF8]);
    let img_id = doc.add_object(Object::Stream(stream));

    let page_id = doc.page_id(0).unwrap();
    let mut resources = doc.page_resources(page_id);
    let mut xobjects = match resources.get("XObject").and_then(|o| o.as_dict().ok()) {
        Some(d) => d.clone(),
        None => Dictionary::new(),
    };
    xobjects.set("Im0", Object::Reference(img_id));
    resources.set("XObject", Object::Dictionary(xobjects));
    doc.get_object_mut(page_id)
        .unwrap()
        .as_dict_mut()
        .unwrap()
        .set("Resources", resources);
    doc.append_content_bytes(0, b"q 100 0 0 100 0 0 cm /Im0 Do Q".to_vec())
        .unwrap();

    let pm = doc.render_page(0, 1.0).unwrap();
    let left = pm.pixel(10, 50).unwrap();
    let right = pm.pixel(90, 50).unwrap();
    eprintln!("baseline left={:?} right={:?}", left, right);
    assert!(left[0] > 200, "baseline 左が白でない: {:?}", left);
    assert!(right[0] < 80, "baseline 右が黒でない: {:?}", right);
}

/// 公開 API: `ccitt::decode` と `ccitt::params_from_dict` がライブラリ外から使える。
#[test]
fn public_api_decode_and_params() {
    let mut d = Dictionary::new();
    d.set("K", Object::Integer(-1));
    d.set("Columns", Object::Integer(8));
    d.set("Rows", Object::Integer(1));
    d.set("EndOfBlock", Object::Boolean(false));
    let p = ccitt::params_from_dict(&d);
    assert_eq!(p.k, -1);
    assert_eq!(p.columns, 8);
    let out = ccitt::decode(&t6_row_5w_3b(), &p).unwrap();
    // BlackIs1=false 既定 → 1=白の通常 PDF 表現
    assert_eq!(out, vec![0xF8]);
}
