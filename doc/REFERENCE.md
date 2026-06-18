# pdf_rust リファレンス

依存クレートゼロ（標準ライブラリのみ）のフルスクラッチ PDF 閲覧・編集ライブラリ。
zlib/DEFLATE の伸長器（inflate）も自前実装している。

API の詳細（全関数のシグネチャと doc コメント）は `cargo doc --open` で
生成される rustdoc を参照。本書は「使い方」と「設計・内部構造」をまとめる。

---

## 目次

1. [クイックスタート](#1-クイックスタート)
2. [API リファレンス](#2-api-リファレンス)
3. [PDF ファイル形式の基礎知識](#3-pdf-ファイル形式の基礎知識)
4. [内部設計](#4-内部設計)
5. [対応機能と制限事項](#5-対応機能と制限事項)

---

## 1. クイックスタート

### 読む（閲覧）

```rust
use pdf_rust::Document;

let doc = Document::load("input.pdf")?;
println!("バージョン: PDF {}", doc.version);
println!("ページ数  : {}", doc.page_count());
println!("タイトル  : {:?}", doc.title());

// ページごとのテキスト抽出
for i in 0..doc.page_count() {
    println!("{}", doc.extract_text(i)?);
}
```

### 作る

```rust
use pdf_rust::{Document, TextOptions, DrawOptions, StandardFont};

let mut doc = Document::new();
doc.add_page(595.0, 842.0)?;            // A4（単位はポイント = 1/72 インチ）

doc.add_text(0, "Hello, PDF!", &TextOptions {
    font: StandardFont::HelveticaBold,
    size: 24.0,
    x: 72.0, y: 770.0,                   // 原点は左下・y 軸は上向き
    ..Default::default()
})?;

doc.draw_rect(0, 60.0, 700.0, 200.0, 80.0, &DrawOptions {
    fill_color: Some((1.0, 0.9, 0.2)),
    ..Default::default()
})?;

doc.set_title("My Document")?;
doc.save("output.pdf")?;
```

### 編集する

```rust
use pdf_rust::{Document, TextOptions};

let mut doc = Document::load("input.pdf")?;
doc.add_text(0, "CONFIDENTIAL", &TextOptions { y: 30.0, ..Default::default() })?; // スタンプ
doc.rotate_page(0, 90)?;     // 1 ページ目を時計回りに 90°
doc.remove_page(2)?;         // 3 ページ目を削除
doc.save("edited.pdf")?;     // 完全書き直しで保存
```

---

## 2. API リファレンス

### 2.1 `Document` — 中心 API

| メソッド | 説明 |
|---|---|
| `Document::new()` | 空ドキュメント（カタログ + 空ページツリー）を作成 |
| `Document::load(path)` | ファイルから読み込み |
| `Document::from_bytes(&[u8])` | バイト列から読み込み（xref 破損時は自動再構築） |
| `doc.save(path)` | ファイルへ保存（完全書き直し）。**`&mut self`**（保存時にフォントサブセット化） |
| `doc.to_bytes()` | PDF バイト列へシリアライズ。**`&mut self`**（保存時にフォントサブセット化） |

#### ページ閲覧

| メソッド | 説明 |
|---|---|
| `doc.page_count()` | ページ数 |
| `doc.pages()` | 全ページの `ObjectId` を文書順で返す |
| `doc.page_id(index)` | index（0 始まり）番目のページ ID |
| `doc.page_media_box(id)` | `[x0, y0, x1, y1]`（継承解決込み、既定 Letter） |
| `doc.page_attr(id, key)` | 継承属性（`Resources` `Rotate` など）を祖先まで遡って取得 |
| `doc.page_content_bytes(id)` | 全コンテントストリームを伸長して連結 |
| `doc.page_resources(id)` | 実効 `/Resources` 辞書 |
| `doc.extract_text(index)` | テキスト抽出 |
| `doc.extract_text_spans(index)` | 位置付きテキスト抽出。`Vec<TextSpan { text, bbox, font_size, glyphs }>`（テキスト選択・検索ハイライト用。bbox はユーザー空間）。`glyphs` はグリフ（コード）単位の `SpanGlyph { text, bbox }` 列で、連結するとスパンの `text` に一致する |
| `doc.search_page(index, query, &SearchOptions)` | ページ内テキスト検索。`Vec<SearchHit { rects }>`（矩形は行ごとにマージ、行を跨ぐヒットは複数矩形）。`SearchOptions { case_insensitive }`。スパン跨ぎ・行跨ぎ（空白 1 個を仮定）に対応。正規表現は非対応 |
| `doc.search(query, &SearchOptions)` | 全ページ検索。`Vec<(ページ番号, SearchHit)>` をページ順で返す |
| `doc.render_page(index, scale)` | ページを `Pixmap`（RGBA）にラスタライズ（注釈の外観 `/AP` 込み）。`pixmap.save_png(path)` / `pixmap.to_png()` で PNG 化。`render_page_with` の薄いラッパ |
| `doc.render_page_with(index, &RenderOptions)` | オプション付きラスタライズ。`RenderOptions { scale, region, cancel, annotations, quality }` で領域（タイル）レンダリング・協調キャンセル（`PdfError::Cancelled`）・注釈の ON/OFF・品質切替（`Fast` = AA 1x + 最近傍補間）を制御 |
| `doc.render_page_into(index, &RenderOptions, &mut Pixmap)` | 出力先 `Pixmap` を再利用する変種（連続レンダリングでの再確保回避。サイズ不一致は内部で作り直す） |
| `doc.page_size(index)` | `/Rotate` 反映済みの論理ページサイズ `(幅, 高さ)`（pt）。DPI 換算は `scale = dpi / 72.0` |

#### ビューワー機能（しおり・リンク・ページラベル）

| メソッド | 説明 |
|---|---|
| `doc.outlines()` | しおり（`/Outlines` ツリー）を `Vec<OutlineItem { title, target, children }>` で返す |
| `doc.page_links(index)` | ページのリンク注釈を `Vec<Link { rect, target }>` で返す（GoTo / URI） |
| `doc.page_label(index)` | ページラベル（`/PageLabels`。D/R/r/A/a スタイル + 接頭辞）。無ければ `"1"` 始まりの 10 進 |
| `doc.page_labels()` | 全ページのラベルを一括取得 |

移動先は [`LinkTarget`]（`Goto(Destination)` / `Uri(String)`）で表現され、
`Destination` はページ番号（0 始まり）と `/XYZ` 等の表示座標を持つ。
名前付き宛先（古典 `/Dests` 辞書・`/Names /Dests` 名前ツリー）も解決される。

#### ページ編集

| メソッド | 説明 |
|---|---|
| `doc.add_page(w, h)` | 空ページを末尾に追加（A4 = 595×842, Letter = 612×792） |
| `doc.remove_page(index)` | ページ削除（ページツリーの `/Count` も更新） |
| `doc.rotate_page(index, deg)` | 90 の倍数で回転（既存値に加算） |
| `doc.add_text(index, text, &TextOptions)` | テキスト描画（`\n` で複数行） |
| `doc.draw_line(index, from, to, &DrawOptions)` | 直線 |
| `doc.draw_rect(index, x, y, w, h, &DrawOptions)` | 矩形（塗り対応） |
| `doc.append_content(index, &[Operation])` | 任意の演算列を追記（低レベル） |
| `doc.append_content_bytes(index, bytes)` | 生バイトのコンテントを追記（最低レベル） |
| `doc.ensure_standard_font(index, font)` | 標準フォントをリソース登録し名前を返す |
| `doc.load_font(path)` | TrueType/TTC ファイルを読み込み、埋め込みフォントを登録。[`EmbeddedFontId`] を返す |
| `doc.load_font_from_bytes(data, ttc_index)` | バイト列からフォントを登録（埋め込み目的。CFF は `PdfError::Font` エラー。読み込み・描画は対応） |
| `doc.add_text_with_font(index, text, font_id, &TextOptions)` | 埋め込みフォントでテキスト描画（`\n` で複数行）。`opts.font` は無視される |
| `doc.text_width(font_id, text, size)` | 埋め込みフォントで描画幅（ポイント）を概算。`\n` はスキップ |

#### メタデータ

| メソッド | 説明 |
|---|---|
| `doc.title()` / `doc.set_title(s)` | タイトル |
| `doc.info_text(key)` / `doc.set_info_text(key, s)` | `/Info` の任意キー（`Author` `Subject` `Creator` `Producer` …） |
| `doc.info()` | `/Info` 辞書そのもの |

日本語などの非 ASCII 文字列は自動的に BOM 付き UTF-16BE で格納される。

#### 低レベルアクセス

| メソッド | 説明 |
|---|---|
| `doc.objects` | 全間接オブジェクトの `BTreeMap<(u32,u16), Object>`（直接操作可） |
| `doc.trailer` | トレーラ辞書（`/Root` `/Info`） |
| `doc.get_object(id)` / `get_object_mut(id)` | ID 指定アクセス |
| `doc.resolve(&obj)` | 間接参照を実体まで辿る |
| `doc.dict_get(&dict, key)` | 辞書キー取得 + 参照解決 |
| `doc.add_object(obj)` | 新しい間接オブジェクトを登録 |
| `doc.catalog()` | ルート（カタログ）辞書 |
| `doc.get_stream_data(&stream)` | フィルタを適用してストリームを伸長 |

### 2.2 `Object` — PDF オブジェクトモデル

```rust
pub enum Object {
    Null,
    Boolean(bool),
    Integer(i64),
    Real(f64),
    String(Vec<u8>, StringFormat), // PDF 文字列はバイト列
    Name(String),                  // /Name（先頭スラッシュは含まない）
    Array(Vec<Object>),
    Dictionary(Dictionary),
    Stream(Stream),
    Reference(ObjectId),           // (番号, 世代)
}
```

- 変換: `as_int` `as_number` `as_name` `as_string` `as_array` `as_dict` `as_stream` `as_reference`（型が違えば `PdfError::TypeMismatch`）
- 生成: `From` 実装（`5.into()`, `3.14.into()`, `true.into()`）、`Object::name("X")`、`Object::string_literal("text")`
- `Dictionary` は挿入順保持の連想配列: `get` / `set` / `remove` / `iter` / `require`
- `Stream` はエンコード済みデータを保持。`Stream::new`（無圧縮）/ `Stream::new_compressed`（Flate 圧縮）/ `decoded_data()`

### 2.3 `TextOptions` / `DrawOptions`

```rust
TextOptions {
    font: StandardFont,        // 既定 Helvetica
    size: f64,                 // 既定 12.0
    x: f64, y: f64,            // 1 行目ベースライン左端（既定 72, 720）
    color: (f64, f64, f64),    // RGB 0.0–1.0（既定 黒）
    leading: Option<f64>,      // 行送り（既定 size × 1.2）
}
DrawOptions {
    stroke_color: (f64, f64, f64), // 既定 黒
    fill_color: Option<(f64, f64, f64)>, // 既定 None（塗らない）
    line_width: f64,           // 既定 1.0
}
```

`add_text` のテキストは WinAnsiEncoding（≒ CP1252）で符号化できる文字のみ
描画可能。表せない文字は `?` になる（標準 14 フォントは欧文フォントのため）。

日本語など Unicode 文字の描画には `add_text_with_font` を使う。
このメソッドは `opts.font`（StandardFont）を**無視し**、`font` 引数の
`EmbeddedFontId` で指定した埋め込みフォントを使う。
文字は GID 大端 u16 ペア（Identity-H / CIDFontType2）でエンコードされる。

### 2.4 `StandardFont` — 標準 14 フォント

`Helvetica` `HelveticaBold` `HelveticaOblique` `HelveticaBoldOblique`
`TimesRoman` `TimesBold` `TimesItalic` `TimesBoldItalic`
`Courier` `CourierBold` `CourierOblique` `CourierBoldOblique`
`Symbol` `ZapfDingbats`

- `font.measure_text("text", 12.0)` — 描画幅をポイントで概算（右寄せ・中央寄せの計算に）
- `font.char_width(c)` — 1000 分の 1 em 単位の文字幅

### 2.5 `Operation` — コンテントストリーム演算

```rust
use pdf_rust::{Operation, Object};
let ops = vec![
    Operation::new("BT", vec![]),
    Operation::new("Tf", vec![Object::name("F1"), 12.into()]),
    Operation::new("Td", vec![72.into(), 700.into()]),
    Operation::new("Tj", vec![Object::string_literal("Hello")]),
    Operation::new("ET", vec![]),
];
doc.append_content(0, &ops)?;
```

`content::parse_content(&bytes)` で既存ページの演算列を解析、
`content::write_content(&ops)` でバイト列へ戻せる。

主な演算子（PDF 32000-1:2008 §8–9）:

| 系統 | 演算子 |
|---|---|
| グラフィックス状態 | `q`（保存） `Q`（復元） `cm`（行列） `w`（線幅） |
| パス構築 | `m`（移動） `l`（直線） `c`（ベジェ） `re`(矩形) `h`（閉路） |
| パス描画 | `S`（線） `f`（塗り） `B`（塗り+線） `n`（破棄） |
| 色 | `rg`/`RG`（塗り/線の RGB） `g`/`G`（グレー） `k`/`K`（CMYK） |
| テキスト | `BT`/`ET`（開始/終了） `Tf`（フォント） `Td`/`TD`/`Tm`/`T*`（位置） `Tj`/`TJ`/`'`/`"`（表示） `TL`（行送り） |
| その他 | `Do`（XObject 描画） `BI...EI`（インライン画像） |

### 2.6 `PdfError`

| バリアント | 意味 |
|---|---|
| `Io` | 入出力エラー |
| `Syntax { offset, message }` | 構文エラー（バイト位置付き） |
| `NotAPdf` | `%PDF-` ヘッダがない |
| `BrokenXref` | xref が壊れており再構築も失敗 |
| `MissingObject(num, gen)` | 参照切れ |
| `TypeMismatch` | 期待した型と違う |
| `MissingKey` | 辞書の必須キー欠落 |
| `Filter` | ストリーム伸長失敗 |
| `EncryptionNotSupported` | 暗号化 PDF |
| `PageOutOfRange` | ページ番号範囲外 |
| `Invalid` | その他不正 |

---

## 3. PDF ファイル形式の基礎知識

本ライブラリの実装対象である PDF 32000-1:2008（ISO 標準）の要点。

### 3.1 ファイルの物理構造

```
%PDF-1.7              ← ヘッダ
%âãÏÓ                 ← バイナリであることを示す慣習的コメント
1 0 obj               ← 本体: 間接オブジェクトの並び
  << /Type /Catalog /Pages 2 0 R >>
endobj
2 0 obj ... endobj
...
xref                  ← 相互参照テーブル（各オブジェクトのバイト位置）
0 6
0000000000 65535 f
0000000015 00000 n
...
trailer               ← トレーラ（ルートへの入口）
<< /Size 6 /Root 1 0 R /Info 5 0 R >>
startxref
12345                 ← xref テーブルの位置
%%EOF
```

読み込みは**末尾から**始まる: `startxref` → xref テーブル → トレーラの
`/Root` → カタログ → ページツリー、と辿る。

### 3.2 オブジェクトの 8 つの型

| 型 | 表記例 |
|---|---|
| ブール | `true` `false` |
| 数値 | `42` `-3.14` `.5` |
| 文字列 | `(literal)` または `<48656C6C6F>`（16 進） |
| 名前 | `/Name`（`#20` で特殊文字をエスケープ） |
| 配列 | `[1 2 (three) /Four]` |
| 辞書 | `<< /Key value ... >>` |
| ストリーム | `辞書 + stream ... endstream`（バイナリデータ） |
| null | `null` |

`12 0 obj ... endobj` で定義したオブジェクトは `12 0 R` で参照できる
（**間接参照**）。番号と世代番号の組で識別される。

### 3.3 文書の論理構造

```
trailer /Root
  └─ カタログ << /Type /Catalog >>
       └─ /Pages ページツリー << /Type /Pages /Kids [...] /Count n >>
            ├─ ページ << /Type /Page /Contents ... /Resources ... >>
            └─ （中間ノードを挟んで木構造にできる）
trailer /Info
  └─ 文書情報 << /Title (...) /Author (...) >>
```

- ページの `/Resources` `/MediaBox` `/Rotate` は**親ノードから継承**できる。
- `/Contents` はコンテントストリーム（1 本またはストリームの配列）。
  ページの見た目はすべてここに演算子列として記述される。

### 3.4 ストリームとフィルタ

ストリームのデータは `/Filter` で圧縮・符号化される。複数フィルタの
チェーンも可能。最も一般的なのは `FlateDecode`（zlib/DEFLATE）。

xref ストリームや画像では、Flate の前段に **PNG predictor**
（`/DecodeParms << /Predictor 12 /Columns n >>`）がかかることが多い。
行ごとの差分符号化で圧縮率を上げる仕組み。

### 3.5 PDF 1.5+ の新しい相互参照形式

- **クロスリファレンスストリーム**: xref テーブル自体を Flate 圧縮した
  ストリームオブジェクト（`/Type /XRef`）にしたもの。各エントリは
  `/W [a b c]` で指定された固定幅バイナリレコード。
- **オブジェクトストリーム**（`/Type /ObjStm`）: 複数の小さなオブジェクトを
  1 本のストリームにまとめて圧縮する仕組み。xref エントリの type 2 が
  「ObjStm 番号 + ストリーム内 index」を指す。

本ライブラリは読み込み時に両方を展開し、保存時は古典形式で書き出す。

### 3.6 増分更新

PDF は追記による更新（incremental update）が可能で、その場合 xref が
複数世代チェーン（`/Prev`）になる。読み込みは新しい世代から順にマージする
（同じオブジェクト番号は新しい方が勝つ）。本ライブラリの保存は常に
1 世代の完全書き直し。

### 3.7 座標系

- 単位はポイント（1/72 インチ）。A4 = 595×842、Letter = 612×792。
- **原点はページ左下、y 軸は上向き**（スクリーン座標と逆）。
- `cm` 演算子で座標変換行列（CTM）を操作できる。

---

## 4. 内部設計

### 4.1 モジュールとデータフロー

```
読み込み:
  バイト列 ──lexer──▶ トークン ──parser──▶ Object
       ▲                                      │
       └── xref（startxref → テーブル/ストリーム → /Prev チェーン） 
                                              ▼
                              Document { objects: BTreeMap<(u32,u16), Object> }
書き出し:
  Document ──writer──▶ ヘッダ + 全オブジェクト + xref テーブル + トレーラ
テキスト抽出:
  page /Contents ──filters(伸長)──▶ content(演算列) ──text(状態機械)──▶ String
レンダリング:
  page /Contents ──content(演算列)──▶ render::state(解釈) ──path/raster──▶ Pixmap ──▶ PNG
```

| モジュール | 内容 |
|---|---|
| `lexer` | 字句解析。空白/区切り/コメント、文字列エスケープ、`#xx` 名前 |
| `parser` | 構文解析。`n g R` 先読み、ストリーム読み（`/Length` 間接参照解決、`endstream` スキャン復元） |
| `xref` | 古典テーブル / xref ストリーム / ハイブリッド / 全走査再構築 |
| `filters` | Flate（自前 inflate）、LZW、ASCII85、ASCIIHex、RunLength、PNG/TIFF predictor |
| `filters::flate` | RFC 1950/1951 実装。inflate は固定・動的ハフマン両対応。deflate は stored ブロック |
| `object` | `Object` / `Dictionary`（挿入順保持）/ `Stream` |
| `document` | 中心 API。オブジェクト管理、ページツリー操作、編集、メタデータ |
| `content` | コンテントストリーム ⇔ `Operation` 列の相互変換。インライン画像は辞書 + 生データとして保持 |
| `text` | テキスト抽出。ToUnicode CMap（bfchar/bfrange）、Form XObject 再帰、改行/空白ヒューリスティック、位置付きスパン抽出（`TextSpan`） |
| `interactive` | ビューワー機能。しおり（/Outlines）、リンク注釈、宛先解決（明示配列・名前付き）、ページラベル（/PageLabels 数値ツリー） |
| `search` | テキスト検索。スパン連結（境界箱中心による同一行判定 + 隙間/行境界への空白仮定）→ リテラル照合 → 行ごとのハイライト矩形マージ |
| `font` | 標準 14 フォントの幅テーブル（AFM 由来）、WinAnsi ⇔ Unicode |
| `truetype` | TTF/TTC パーサ。cmap format 4/12、hmtx/head/hhea/maxp/OS∕ 2/post/name/loca/glyf。OTTO sfnt は `CFF ` テーブルを `cff` 経由でデコード |
| `cff` | CFF（Compact Font Format）パーサと Type 2 チャーストリング解釈器。CFF / OpenType-CFF / `/FontFile3`（OpenType・Type1C・CIDFontType0C）対応 |
| `subset` | TrueType サブセッタ。グリフ閉包（composite 対応）、sparse glyf、チェックサム再計算 |
| `writer` | シリアライザ。実数の正規化、文字列/名前のエスケープ、xref 生成、`/ID` 生成（FNV-1a） |
| `filters::dct` | baseline JPEG（DCTDecode）デコーダ。ハフマン復号 + 浮動小数 IDCT + 双線形クロマアップサンプリング、YCbCr/YCCK 色変換 |
| `function` | PDF 関数（§7.10）インタプリタ。Type 0（サンプル）/ 2（指数）/ 3（継ぎ接ぎ）/ 4（PostScript 電卓） |
| `encoding` | 単純フォントのエンコーディング解決（Standard/MacRoman/`/Differences`/グリフ名） |
| `render` | ラスタライザ。`pixmap`（RGBA + PNG 出力）/ `path`（行列・ベジェ平坦化）/ `raster`（AA スキャンライン塗り・ストローク・クリップ）/ `state`（演算解釈 + 注釈外観 `/AP` の描画）/ `text`（描画用フォント解決）/ `colorspace`（色空間 → sRGB）/ `image`（画像 XObject・インライン画像の描画）。`RenderOptions` で領域（タイル）レンダリング（基底 CTM へ `translate(-x,-y)` 合成）・協調キャンセル（`AtomicBool`、演算ループ/グリフ/画像行単位で確認）・品質切替を制御 |

### 4.2 設計上の選択

- **全オブジェクトを即時読み込み**（遅延なし）。実装が単純になり、編集 API
  が `BTreeMap` への操作に帰着する。巨大ファイルでのメモリ効率より単純さを優先。
- **保存は完全書き直し**。読み込んだ ObjStm / XRef ストリームは展開後に破棄し、
  古典 xref で書き出す。どのビューアでも読める最も保守的な形式。
- **圧縮は stored-block zlib**。自前 deflate 圧縮器（ハフマン符号化）は持たず、
  RFC 1951 の無圧縮ブロックで包む。サイズは縮まないが常に正しい zlib
  ストリームになり、コード量とバグのリスクを大幅に減らせる。
  伸長（inflate）側は固定・動的ハフマンを完全実装している。
- **耐故障性**: 実在の PDF は壊れていることが多い。
  - `/Length` が間違っていれば `endstream` を走査して復元
  - xref が壊れていればファイル全走査で `n g obj` を拾って再構築
  - 数値の桁あふれ・不正エスケープなどは可能な限り読み飛ばす
- **フォントサブセットは sparse glyf 方式**。グリフ ID を振り直さず、
  未使用グリフのアウトラインを長さ 0 にする。composite グリフの参照先
  ID 書き換えが不要になり、`/CIDToGIDMap /Identity` がそのまま成立する。
  保存（`to_bytes`/`save`）のたびにその時点の使用グリフ集合から
  サブセットと関連オブジェクト（FontFile2 / W / ToUnicode）を再生成する
  （冪等）。フォントファイルの解析は信頼できない入力として全境界検査する。

### 4.3 テスト

```
cargo test          # ユニット 206 + 統合 61 + doctest 3
```

- フィルタは既知ベクタ（.NET `ZLibStream` で生成した zlib データ、
  Adler-32 既知値、ASCII85 の手計算ベクタ）で検証
- JPEG デコーダは .NET `System.Drawing` で外部生成したフィクスチャ
  （`tests/fixtures/*.jpg` と参照デコード結果 `*.rgb`）との誤差比較で検証
- レンダリングは「コンテント生成 → `render_page` → ピクセル検証」の統合テスト +
  WinRT `Windows.Data.Pdf` 出力との目視比較（`winrt_render.ps1`）で検証
- 統合テストは「生成 → 保存 → 再読込 → 抽出」の往復、
  xref ストリーム + ObjStm を使う PDF 1.5 形式ファイルの手組みバイト列、
  startxref 破壊からの復元、暗号化 PDF の拒否を含む

---

## 5. 対応機能と制限事項

### 対応

- ✅ 読み込み: PDF 1.0–1.7（古典 xref / xref ストリーム / ObjStm / ハイブリッド / 増分更新済みファイル / 破損 xref の再構築）
- ✅ フィルタ: FlateDecode（+PNG/TIFF predictor）, LZWDecode, ASCII85Decode, ASCIIHexDecode, RunLengthDecode
- ✅ テキスト抽出: 単純フォント（WinAnsi 相当）+ ToUnicode CMap（CID フォント含む）+ Form XObject 再帰
- ✅ 編集: ページ追加/削除/回転、テキスト・直線・矩形の描画、任意コンテント追記、メタデータ
- ✅ 生成: ゼロからの文書作成、標準 14 フォント、文書情報、`/ID` 生成
- ✅ 日本語描画: TrueType（glyf）/TTC フォントの埋め込みによる Unicode テキスト描画 + 自動サブセット化
- ✅ 日本語: 抽出・メタデータは UTF-16BE で完全対応
- ✅ レンダリング: `render_page` でページを RGBA ラスタライズ（PNG 書き出し）。
  パス（塗り/ストローク/ダッシュ/クリップ、アンチエイリアス）、テキスト
  （埋め込み TrueType + システムフォント代替）、Form XObject 再帰
- ✅ レンダリング制御: `render_page_with` / `render_page_into`（`RenderOptions`）。
  領域（タイル）レンダリング（全面結果の切り出しとピクセル一致）、協調
  キャンセル（`PdfError::Cancelled`）、出力バッファ再利用、注釈の ON/OFF、
  高速品質モード（AA 1x + 最近傍補間）、`page_size`（/Rotate 反映済み pt サイズ）
- ✅ 画像描画: 画像 XObject とインライン画像（BI）。BitsPerComponent 1/2/4/8/16、
  `/Decode`、ImageMask（ステンシル）、SMask（アルファ）、baseline JPEG（DCTDecode）、
  ExtGState `/ca`、回転・せん断 CTM（双線形/最近傍サンプリング）
- ✅ 色空間: DeviceGray/RGB/CMYK、Indexed、ICCBased（/N・/Alternate 近似）、
  Separation/DeviceN（tint 変換 = PDF 関数 Type 0/2/3/4 インタプリタ）、
  CalGray/CalRGB（Device 同一視）、Lab（近似変換）
- ✅ ビューワー機能: 位置付きテキスト抽出（`extract_text_spans`。グリフ単位
  ボックス込み）、しおり（`outlines`）、リンク注釈と宛先解決（`page_links`。
  明示配列・名前付き宛先）、ページラベル（`page_label`）、注釈の外観描画
  （`/AP` `/N`。`/AS` 状態選択、Hidden/NoView フラグ対応）
- ✅ テキスト検索: `search_page` / `search`（大文字小文字無視オプション、
  スパン跨ぎ・行跨ぎマッチ、行ごとのハイライト矩形）

### 制限

- ❌ 暗号化 PDF（RC4/AES）— 読み込み時に `EncryptionNotSupported`
- ⚠️ CFF アウトライン（`.otf` / OpenType-CFF）の**埋め込み（サブセット化）**は
  非対応 — `load_font_from_bytes` が `PdfError::Font` を返す。
  **読み込み・レンダリングは対応**: OTTO sfnt の `CFF ` テーブルと PDF
  `/FontFile3`（`OpenType` / `Type1C` / `CIDFontType0C`）を
  Type 2 チャーストリング解釈器（`src/cff.rs`）で描画する
- ❌ 縦書き（Identity-V）— 横書き（Identity-H）のみ対応
- ❌ 画像: progressive JPEG / JPXDecode / CCITTFaxDecode / JBIG2Decode は
  デコード不可（レンダリングでは読み飛ばし。生データ取得は可能）
- ❌ レンダリング: `/Mask`（ステンシル・カラーキー）、シェーディング（`sh`）、
  透明グループ・ブレンドモード、Type3 フォント、画像境界のアンチエイリアス
- ⚠️ CFF（`.otf` / FontFile3 の OpenType・Type1C・CIDFontType0C）は
  自前の Type 2 解釈器で描画可能。Type1（旧式 eexec）はシステムフォント代替の近似
- ❌ 増分更新での保存（電子署名は保存すると無効になる）
- ❌ レイアウト解析 — テキスト抽出は読み上げ順のヒューリスティック
- ❌ ToUnicode の無い CID フォント、`/Differences` エンコーディングは近似
- ❌ タグ付き PDF、フォーム（AcroForm）の高レベル API、注釈の作成・編集 API
  （読み取り = `page_links` / 外観描画は対応。`doc.objects` への
  低レベルアクセスでは操作可能）
