---
name: viewer-plan
description: PDF ビューワー構築計画が doc/VIEWER_PLAN.md にある。フェーズ構成とモデル委譲可否を記載
metadata: 
  node_type: memory
  type: project
---

PDF ビューワー構築が進行中の目標（2026-06-10 開始）。計画はリポジトリの `doc/VIEWER_PLAN.md` に記録済み。**Phase 1–4（ラスタライザ・テキスト描画・画像と色・ビューワー機能）は 2026-06-11 完了。Phase 5（GUI）はユーザーが別リポジトリ `D:\Program\VSC\Rust\PDF_Viewer_rust`（Win32 生 FFI、pdf_rust に path 依存）として実装済み**。ビューワー実装で判明した不足は同リポジトリの `MISSING_FEATURES.md` に棚卸しされ、Phase 7（領域レンダリング・キャンセル・RenderOptions）・Phase 8（グリフ単位ボックス・検索 API）として計画化済み（2026-06-11、コミット 585140d）。残作業の優先順: Phase 7 → 8 → 6（CFF → 暗号化 → シェーディング → 透明度 → CCITT）。

Phase 1–4 は 2026-06-11 に main へマージし origin（github.com/Shannon-toppo/PDF_rust）へプッシュ済み。feature ブランチはローカル・リモートとも削除済みで、残るブランチは main のみ。注意: ユーザーは GitHub 上で PR マージを行うことがある（Phase 1–3 は PR #1 でマージされていた）。プッシュ前に fetch して origin/main との乖離を確認すること。

教訓: 並列委譲時は各タスクの編集可能ファイルをプロンプトで明示し、同一ファイルを触るタスクは直列にする（Phase 3 で `git stash` 誤実行と `filters/mod.rs` 上書き競合が発生、復旧済み）。Phase 4 は委譲せず直接実装で問題なく完了（規模: 中なら 1 セッションで可能）。

**Why:** ライブラリにはラスタライザが無く、ビューワー化には Phase 1（描画基盤）→ 2（テキスト描画）→ 5（Win32 FFI GUI）が最短経路。各フェーズに Opus/Sonnet への委譲可否を付してあり、委譲時は `doc/VIEWER_PLAN.md` の「前提」節と `CLAUDE.md` をプロンプトに含める約束。

**How to apply:** ビューワー関連の作業依頼が来たら、まず `doc/VIEWER_PLAN.md` を読んで該当フェーズを確認し、完了したらチェックボックスを更新する。Phase 5 に着手する場合はリンククリック（`page_links`）・テキスト選択（`extract_text_spans`）が Phase 4 で利用可能になっている。関連: [[pdf-rust-library-overview]]
