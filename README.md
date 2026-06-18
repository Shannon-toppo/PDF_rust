# pdf_rust

依存クレートゼロ・フルスクラッチの PDF 閲覧・編集ライブラリ（Rust）。

PDF のパース・シリアライズはもちろん、zlib/DEFLATE の伸長器（RFC 1950/1951 の
inflate、固定・動的ハフマン対応）まで標準ライブラリのみで実装している。

```rust
use pdf_rust::{Document, TextOptions, StandardFont};

// 読む
let doc = Document::load("input.pdf")?;
println!("{} ページ", doc.page_count());
println!("{}", doc.extract_text(0)?);

// 作る・編集する
let mut doc = Document::new();
doc.add_page(595.0, 842.0)?;  // A4
doc.add_text(0, "Hello, PDF!", &TextOptions {
    font: StandardFont::HelveticaBold,
    size: 24.0, x: 72.0, y: 770.0,
    ..Default::default()
})?;
doc.set_title("My Document")?;
doc.save("output.pdf")?;
```

## 主な機能

- **閲覧**: ページ列挙、テキスト抽出（ToUnicode CMap 対応）、メタデータ取得
- **ビューワー機能**: 位置付きテキスト抽出（`extract_text_spans`。グリフ単位
  ボックス込み）、テキスト検索（`search_page` / `search`。行跨ぎマッチ・
  ハイライト矩形）、しおり（`outlines`）、リンク注釈と宛先解決（`page_links`）、
  ページラベル（`page_label`）
- **編集**: テキスト・図形の描画、ページ追加/削除/回転、メタデータ編集
- **日本語描画**: TrueType/TTC フォント埋め込み・自動サブセット化・Identity-H（`add_text_with_font`）
- **レンダリング**: ページを PNG にラスタライズ（`render_page`）。ベクタ図形・
  TrueType テキスト・画像（baseline JPEG / FlateDecode 等）・各種色空間・
  注釈の外観ストリーム（/AP）に対応。`render_page_with`（`RenderOptions`）で
  領域（タイル）レンダリング・協調キャンセル・バッファ再利用・品質切替も可能
- **読み込み対応形式**: 古典 xref / クロスリファレンスストリーム /
  オブジェクトストリーム（PDF 1.5+）/ ハイブリッド / 破損 xref の自動再構築
- **暗号化 PDF**: 標準セキュリティハンドラ V1/V2/V4（RC4-40/128・AES-128）と
  V5 R6（AES-256）の復号に対応（`Document::from_bytes` は空ユーザーパスワード
  を自動試行、`from_bytes_with_password` で任意 PW 指定可能）。再保存は平文化
- **フィルタ**: FlateDecode（PNG/TIFF predictor 込み）, LZW, ASCII85,
  ASCIIHex, RunLength, baseline JPEG（DCTDecode）— すべて自前実装
- **暗号プリミティブ**: MD5/RC4/AES-128/AES-256/SHA-256/384/512 を自前実装
  （標準セキュリティハンドラ用）

## 使い方

```powershell
cargo test                                        # テスト一式
cargo run --example create_pdf                    # PDF を生成 → hello.pdf
cargo run --example japanese_pdf                  # 日本語 PDF を生成 → japanese.pdf
cargo run --example inspect -- hello.pdf          # 中身を表示
cargo run --example edit_pdf -- hello.pdf out.pdf # 編集して別名保存
cargo run --example render_page -- in.pdf out.png # ページを PNG に描画
cargo doc --open                                  # API ドキュメント
```

## ドキュメント

- [doc/REFERENCE.md](doc/REFERENCE.md) — 使い方・API 一覧・PDF 形式の解説・内部設計
- [doc/VIEWER_PLAN.md](doc/VIEWER_PLAN.md) — ビューワー実装フェーズ計画
- `cargo doc --open` — 全 API の rustdoc（日本語コメント付き）

## 制限事項（抜粋）

- 暗号化 PDF は読み込みのみ対応（再保存は平文として出力。再暗号化はしない）。
  非標準セキュリティハンドラと R5 暫定方式は未対応
- 保存は常に完全書き直し（増分更新ではない）
- フォント埋め込みは TrueType（glyf アウトライン）/TTC に対応
  （`add_text_with_font` で日本語を含む Unicode 文字を描画可能）
- CFF アウトライン（`.otf`）は未対応。`load_font_from_bytes` がエラーを返す
- 縦書き（Identity-V）は未対応
- レンダリングは progressive JPEG / JPX / CCITT / JBIG2 の画像、
  シェーディング、透明グループが未対応（読み飛ばして描画継続）

詳細は [doc/REFERENCE.md の §5](doc/REFERENCE.md#5-対応機能と制限事項) を参照。
