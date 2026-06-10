# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## プロジェクト概要

`pdf_rust` は**依存クレートゼロ**（標準ライブラリのみ）のフルスクラッチ PDF 閲覧・編集ライブラリ。zlib/DEFLATE の inflate（固定・動的ハフマン）や TrueType パーサ/サブセッタまで自前実装している。**依存クレートの追加はユーザーの明示的な要望なしに行わないこと**（フルスクラッチがプロジェクトの方針）。

コメント・doc コメント・ドキュメントは日本語で書く（既存スタイルに合わせる）。

## コマンド

```powershell
cargo test                        # 全テスト（ユニット + 統合 + doctest）
cargo test --lib                  # ユニットテストのみ
cargo test --lib truetype         # モジュール名でフィルタ（1 モジュール分）
cargo test --test ttf_integration # 統合テストファイル単位
cargo test japanese_embed_roundtrip  # テスト名でフィルタ（1 テスト）
cargo clippy --all-targets        # 警告ゼロを維持する
cargo fmt                         # コミット前に必ず実行
cargo doc --open                  # rustdoc（日本語）

# サンプル（動作確認に便利）
cargo run --example create_pdf                    # → hello.pdf
cargo run --example japanese_pdf                  # → japanese.pdf（要 Windows 日本語フォント）
cargo run --example inspect -- <file.pdf>         # 簡易 pdfinfo + pdftotext
cargo run --example edit_pdf -- <in.pdf> <out.pdf>
cargo run --example dump_content -- <file.pdf> [page]  # コンテントストリームの生ダンプ（デバッグ用）
```

## アーキテクチャ

データフロー: **読み込み** `lexer`（トークン）→ `parser`（Object）→ `xref`（startxref → 古典テーブル/xref ストリーム → /Prev チェーン）→ `Document`。**書き出し** `Document` → `writer`（全オブジェクト + 古典 xref で完全書き直し）。**テキスト抽出** ページ /Contents → `filters`（伸長）→ `content`（演算列）→ `text`（状態機械）。

- `document.rs` — 中心 API。全間接オブジェクトを `BTreeMap<(u32,u16), Object>` に即時読み込みし、編集はマップ操作に帰着する。ページツリー操作・テキスト/図形描画・メタデータ・フォント埋め込みの統合もここ。
- `object.rs` — `Object` 列挙型。`Dictionary` は挿入順保持（出力安定性のため）。`Stream.data` は**エンコード済みのまま**保持し、伸長は `Document::get_stream_data`（間接参照解決込み）か `Stream::decoded_data` で行う。
- `filters/` — FlateDecode（自前 inflate）、LZW、ASCII85、ASCIIHex、RunLength、PNG/TIFF predictor。**圧縮側は stored-block zlib のみ**（ハフマン符号化器は持たない。サイズより正しさとコード量を優先する設計判断）。
- `truetype.rs` / `subset.rs` — TTF/TTC パーサと **sparse-glyf サブセッタ**（グリフ ID を振り直さず未使用グリフを空にする方式。composite 参照の書き換え不要で `/CIDToGIDMap /Identity` が成立）。
- `text.rs` — 抽出。ToUnicode CMap、Form XObject 再帰、フォント幅（/Widths・/W・標準 14 メトリクス）から advance を計算して空白/改行を復元する（Td の移動量と表示済み advance の差分で判定。Chromium/Skia の「1 グリフ 1 Tj」パターン対応のため）。

## 重要な設計上の不変条件

- **保存は完全書き直し**。読み込んだ ObjStm/XRef ストリームはロード時に展開・破棄される。`to_bytes`/`save` は `&mut self`（保存時に埋め込みフォントのサブセットと FontFile2/W/ToUnicode を冪等に再生成するため）。
- **耐故障性が仕様**: 壊れた PDF は読めるだけ読む。/Length が嘘なら `endstream` を走査、xref が壊れていれば全走査で再構築（`xref::reconstruct`）、不正トークンは可能な限り読み飛ばす。パース系のエラーで panic しないこと。
- **フォントファイルは信頼できない入力**: `truetype.rs` では checked 演算と `data.get(..)` のみ使用。unwrap・直接インデックス禁止。
- 暗号化 PDF は `PdfError::EncryptionNotSupported`、CFF アウトライン（.otf）は `PdfError::Font` で明示的に拒否する。

## テストの約束事

- システムフォント（`C:\Windows\Fonts\arial.ttf`, `msgothic.ttc`, `YuGothM.ttc` 等）に依存するテストは、ファイルが無ければ `eprintln!` してスキップ（return）し、**パス扱い**にする。
- フィルタのテストベクタは実物由来（.NET `ZLibStream` で生成した zlib データ等）。新しいベクタを作るときも実装から逆算せず外部ツールで生成すること。
- 統合テストの柱は「生成 → `to_bytes` → `from_bytes` → `extract_text` の往復一致」。新機能も同じ形で検証する。

## 外部検証ツール（Windows 環境）

- Poppler: `& "C:\Program Files\Git\mingw64\bin\pdftotext.exe" -enc UTF-8 <pdf> <txt>` — **`-enc UTF-8` を付けないと CJK が黙って落ちる**。
- レンダリング確認: WinRT `Windows.Data.Pdf` を `powershell.exe`（5.1）から呼んで PNG 化できる（pwsh 7 では不可）。
- 実世界 PDF の生成: Chrome/Edge の `--headless --print-to-pdf`（xref ストリーム + ObjStm + 埋め込みフォント入りのテスト素材になる）。

## ドキュメントの同期

公開 API や対応機能/制限を変えたら `README.md` と `REFERENCE.md`（§2 API 表・§4 内部設計・§5 対応/制限）を必ず更新する。rustdoc コメントも日本語で。
