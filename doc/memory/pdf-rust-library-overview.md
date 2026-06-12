---
name: pdf-rust-library-overview
description: pdf_rust プロジェクトの概要 — 依存ゼロのフルスクラッチ PDF 閲覧・編集ライブラリ（2026-06-10 完成）
metadata: 
  node_type: memory
  type: project
---

D:\Program\VSC\Rust\PDF_rust は 2026-06-10 に作成した、依存クレートゼロの
フルスクラッチ PDF ライブラリ（ユーザーの要望: 「できるだけフルスクラッチで」）。

**Why:** ユーザーは外部クレートに頼らない自前実装を重視している。
zlib/DEFLATE の inflate（固定・動的ハフマン）まで自前。圧縮側は意図的に
stored-block zlib（RFC 1951 無圧縮ブロック）のみ — 正しさとコード量のトレードオフ。

**How to apply:** 機能追加時もこの方針を守る（クレート追加はユーザーに確認）。
検証には Git for Windows 付属の `C:\Program Files\Git\mingw64\bin\pdftotext.exe`
（Poppler）と、Chrome ヘッドレス `--print-to-pdf` で実世界 PDF を生成する手が使える。
日本語ドキュメント（README.md / doc/REFERENCE.md / rustdoc）を維持する。
既知の制限: 暗号化 PDF 非対応、CFF(.otf) 非対応、縦書き非対応、保存は常に完全書き直し。
2026-06-10 追記: TrueType/TTC 埋め込みによる日本語描画を実装済み
（truetype.rs パーサ=Opus 製, subset.rs sparse-glyf サブセッタ=メイン製,
document.rs 統合=Sonnet 製。save/to_bytes は &mut self に変更）。
レンダリング検証は WinRT Windows.Data.Pdf を powershell.exe (5.1) から呼ぶ手が使える。
pdftotext は `-enc UTF-8` を付けないと CJK を黙って落とすので注意。
