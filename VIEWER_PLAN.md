# PDF ビューワー実装計画

`pdf_rust` を土台に PDF ビューワーを構築するための不足機能の棚卸しとフェーズ計画。
（作成: 2026-06-10。現状調査に基づく。進捗に応じて各フェーズのチェックボックスを更新すること）

## 前提（委譲時も必ず守る制約）

- **依存クレートゼロ**（標準ライブラリのみ）。ユーザーの明示的な許可なしにクレートを追加しない。
- コメント・doc コメント・ドキュメントは日本語。
- パース系・描画系は **panic しない**（壊れた PDF は描けるだけ描く）。未対応の演算子・機能は無視して継続する。
- フォント・画像など外部由来データは信頼できない入力として checked 演算と `data.get(..)` のみ使用。
- テストベクタは実装から逆算せず外部ツールで生成する。
- 公開 API を変えたら `README.md` / `REFERENCE.md` を同期する。

## 現状の不足機能（調査結果サマリ）

ライブラリは「パース・テキスト抽出・編集・書き出し」までで、**ラスタライザが存在しない**。

- `content.rs` は演算子列への分解まで（解釈する状態機械はテキスト抽出用の部分実装のみ）
- `truetype.rs` の `glyph_data(gid)` は glyf 生バイトを返すだけ。アウトラインのデコードは未実装
- インライン画像（`BI...EI`）はデータを読み飛ばして破棄している
- `extract_text` は座標なしの `String`（テキスト選択・検索ハイライトに使えない）
- 画像デコーダ（DCT/JPX/CCITT/JBIG2）、暗号化、CFF/Type1、シェーディング、透明度、注釈は非対応

## モデル委譲の方針

各フェーズに「委譲可否」を付した。判断基準:

- **Sonnet 可**: 仕様が明確で、既存のテスト規約（往復テスト・外部ベクタ比較）で機械的に検証できる作業。
- **Opus 可**: アルゴリズム設計の裁量が大きい、または複数モジュールにまたがる統合・リファクタを含む作業。
- **委譲非推奨**: 自動検証が難しく目視確認が必須、もしくは設計判断がプロジェクト方針に直結する作業。

委譲する場合は、本ファイルの「前提」節と該当フェーズの記述、`CLAUDE.md` を必ずプロンプトに含めること。
大きなフェーズは下記の分割単位ごとに別タスクとして渡すのが安全。

---

## Phase 1: ラスタライザ基盤 — 規模: 大 ✅ 完了（2026-06-11）

新モジュール `render/`（`pixmap.rs` / `path.rs` / `raster.rs` / `state.rs`）。

- [x] `Pixmap`（RGBA バッファ）と PNG 書き出し（既存 stored-block zlib を流用）。BMP は PNG で十分なため取りやめ
- [x] パス構築（`m` `l` `c` `v` `y` `re` `h`）とベジェ平坦化
- [x] スキャンライン塗り（nonzero / even-odd）+ アンチエイリアス（縦 4x サブスキャン + 横解析的被覆）
- [x] ストローク生成（線幅・キャップ・ジョイン・マイター制限・ダッシュ → 塗りパス変換）
- [x] グラフィックス状態機械（`q`/`Q`/`cm`、色 `rg`/`g`/`k`/`sc(n)`、`gs` の線パラメータ、Form XObject `Do` 再帰）
- [x] クリッピングパス（`W`/`W*`、Mask 交差方式）
- [x] 公開 API: `doc.render_page(index, scale) -> Pixmap`（/Rotate・MediaBox 正規化・巨大サイズのガード込み）

検証済み: ユニット 28 + 統合 8 テスト、WinRT `Windows.Data.Pdf` との目視比較で
矩形・線・ベジェ・ダッシュ・クリップの描画一致を確認（2026-06-11）。
副産物: `parse_content`/`parse_tounicode_cmap` の未終端文字列での無限ループを修正
（lexer は EOF で文字列を閉じる + 呼び出し側に前進ガード）。

実績: Pixmap は Fable、path/raster と state/統合は Opus へ委譲（各 1 タスク、引き継ぎ事項を
プロンプトに明記する方式が有効だった）。

**委譲**: Pixmap/PNG 出力・ベジェ平坦化は **Sonnet 可**（仕様明確・単体検証容易）。
スキャンライン塗り・AA・ストロークのアルゴリズム本体は **Opus 推奨**
（品質判断と数値安定性の裁量が大きい）。状態機械と API 統合は **Opus 可**。

## Phase 2: テキスト描画 — 規模: 大 ✅ 完了（2026-06-11）

- [x] `truetype.rs` に glyf アウトラインデコーダ（単純グリフ: 点列/フラグ/2次ベジェ、composite: 行列合成。
      点数/輪郭数/composite 展開後セグメント数に上限を設けた）
- [x] アウトライン → Phase 1 のパス塗りへの接続（`render/text.rs` 新設 + `state.rs` に全テキスト演算子。
      2次→3次ベジェ昇格。字送り計算は `text.rs` の `WidthSource`/`split_codes` を pub(crate) 共有）
- [x] 非埋め込みフォントのシステムフォント代替（標準 14 → arial/times/cour、CJK → msgothic/YuGothM 等。
      不明名は FontDescriptor Flags の Serif/FixedPitch で振り分け）
- [x] 描画用エンコーディング: `/Differences`、MacRoman、symbolic cmap (3,0)/(1,0)、`/CIDToGIDMap`
      （ストリーム形式含む）。`encoding.rs` 新設（Standard/MacRoman 表 + AGL サブセット）
- [x] `text.rs` の共通化リファクタ（状態機械の完全統合ではなく、コード分解と幅計算を共有する方式を採用。
      抽出側の改行/空白ヒューリスティックはレンダラと要件が異なるため分離のままが妥当と判断）

検証済み: 自前生成（hello.pdf / japanese.pdf）+ Edge `--headless --print-to-pdf` 素材を
WinRT `Windows.Data.Pdf` 出力と目視比較し、字形・位置・改行の一致を確認（2026-06-11、
比較スクリプト `winrt_render.ps1` をリポジトリに追加）。新規テスト: truetype ユニット 8 +
encoding 6 + render/text 6 + render 統合 5（標準代替・埋め込み往復・日本語・Tr3 不可視・耐故障）。

実績: 3 タスクに分割して Opus へ委譲（①glyf デコーダ + symbolic cmap、②encoding 表、
③レンダラ統合。①②は並列、③は ①② の API を引き継ぎ事項としてプロンプトに明記）。
既知の制限: CFF/Type1 はシステムフォント代替で近似（代替無しは字送りのみ）、Type3 未対応、
`Tr` 4–7 のクリップ成分は無視、Type0 の Encoding は Identity 扱い。

## Phase 3: 画像と色 — 規模: 大（JPEG が大半） ✅ 完了（2026-06-11）

- [x] JPEG (DCTDecode) baseline デコーダ → 後続で progressive（`filters/dct.rs`。
      SOF0/SOF1、サンプリング 4:4:4〜4:1:1、リスタートマーカー、YCbCr/YCCK・Adobe APP14。
      progressive/12bit は明示エラー）
- [x] 画像 XObject 描画（`render/image.rs`。BitsPerComponent 1/2/4/8/16、`/Decode`、
      ImageMask、SMask、ExtGState `/ca`。CTM 逆写像 + 双線形/最近傍サンプリング）
- [x] インライン画像のデータ保持化（BI 演算 = `[辞書, 生データ]` の 2 オペランド。
      フィルタなしは /W /H /BPC から長さ計算して偽 EI を回避）と描画
- [x] 色空間: DeviceCMYK→RGB、Indexed、ICCBased（近似）、Separation/DeviceN、CalRGB/Lab
      （`render/colorspace.rs`。`cs`/`scn` も色空間ベースの解釈に対応）
- [x] PDF 関数インタプリタ（`function.rs`。Type 0/2/3/4。Type 4 は PostScript 電卓のミニ評価器）

検証済み: JPEG は .NET System.Drawing で外部生成した 7 フィクスチャ（`tests/fixtures/`、
再生成スクリプト同梱）との誤差比較で max diff ≤ 3 / 平均 ≤ 0.21。切り詰め・ランダム破壊で
panic しないことを確認。Edge `--print-to-pdf` の JPEG+PNG 入り PDF を WinRT と目視比較し
一致を確認（2026-06-11）。新規テスト: function 24 + dct ユニット 12 + 統合 10 +
colorspace 21 + state 2 + image 14 + render 統合 8。

実績: 5 タスクに分割して委譲（第 1 波並列: ①関数=Sonnet、②JPEG=Opus、③インライン
画像=Sonnet → 第 2 波: ④色空間=Sonnet → 第 3 波: ⑤描画統合=Opus）。並列時に
`git stash` 誤実行と `filters/mod.rs` の上書き競合が発生（復旧済み）。**同一ファイルを
触り得るタスクの並列実行は避け、編集可能ファイルをプロンプトで明示する**こと。
既知の制限: /Mask（ステンシル・カラーキー）、progressive JPEG、JPX/CCITT/JBIG2、
画像境界の AA は未対応（読み飛ばし）。

## Phase 4: ビューワー機能（描画以外）— 規模: 中 ✅ 完了（2026-06-11）

Phase 1–3 と独立。並行作業可能。

- [x] 位置付きテキスト抽出: `extract_text_spans(index) -> Vec<TextSpan { text, bbox, font_size }>`
      （`text.rs` に独立した状態機械を追加。抽出側の改行/空白ヒューリスティックとは
      分離し、1 表示演算 = 1 スパン。bbox は FontDescriptor の Ascent/Descent
      （無ければ 0.8/-0.2 em 近似）から推定。cm・Form /Matrix の CTM 合成済み）
- [x] しおり（/Outlines ツリー）読み取り API: `doc.outlines() -> Vec<OutlineItem>`
- [x] リンク注釈と移動先: `doc.page_links(index) -> Vec<Link>`（GoTo/URI、明示宛先
      配列 + 古典 /Dests 辞書 + /Names /Dests 名前ツリーの解決。新モジュール
      `interactive.rs`）
- [x] ページラベル（/PageLabels）: `doc.page_label(index)`（D/R/r/A/a・/P・/St、
      数値ツリーの /Kids 再帰対応）
- [x] 注釈の外観描画（`/AP` `/N` の BBox×Matrix → /Rect 写像で Form XObject 同様に
      実行。/AS 状態辞書・Hidden/NoView フラグ・Popup 除外に対応）

検証済み: `interactive.rs` ユニット 8 + 統合 6（スパン往復・CTM 反映・しおり/リンク/
ラベル往復・外観描画のピクセル検証・Hidden 非描画・/AS 選択）。しおり自己参照・
名前ツリー循環で無限ループしないことをテストで確認（2026-06-11）。

実績: 委譲なしで直接実装（1 セッション、全 5 項目 + ドキュメント同期）。
既知の制限: 外観は /N のみ（/D・/R は無視）、GoToR・Launch・JavaScript アクションは
未対応（無視）、スパンの bbox はグリフ実測ではなくメトリクス推定。

**委譲**: しおり・リンク・ページラベルは **Sonnet 可**（辞書の走査が主体）。
位置付き抽出は **Opus 推奨**（`text.rs` の改行/空白ヒューリスティックとの整合が必要）。

## Phase 5: GUI シェル — 規模: 中

依存ゼロを貫く場合は **Win32 API を `extern "system"` の生 FFI で直接呼ぶ**
（`CreateWindowExW` + `StretchDIBits` + メッセージループ。クレート不要）。
代替案: 画像出力まででライブラリの責務を切る / ユーザー許可の上で windowing クレート導入。

- [ ] ウィンドウ生成・メッセージループ・ページ描画表示
- [ ] スクロール・ズーム・ページ送り
- [ ] （Phase 4 連携）リンククリック・テキスト選択

**委譲**: **委譲非推奨**（unsafe FFI が大量で自動検証が困難。動作確認が目視必須。
やるなら骨組みの生成のみ委譲し、統合と確認は手元で行う）。

## Phase 6: 実世界カバレッジ拡大 — 規模: 特大・継続的

優先順: CFF → 暗号化（空パスワード優先）→ シェーディング → 透明度 → CCITT。
「手元の実 PDF が表示できない」事例ドリブンで進める。

- [ ] CFF/Type1 チャーストリング解釈（実世界 PDF の多数派。3,000–5,000 行級）
- [ ] Type3 フォント（コンテントストリームの再帰描画）
- [ ] 暗号化 PDF: RC4/AES-128/AES-256 + MD5/SHA-2 自前実装（空パスワード PDF は多い）
- [ ] シェーディング（`sh`、axial/radial）・タイリングパターン
- [ ] 透明度（ExtGState `CA`/`ca`、ブレンドモード、透明グループ）
- [ ] CCITTFaxDecode / JBIG2（スキャン文書）。JPXDecode はスコープ外も妥当

**委譲**: 暗号プリミティブ（MD5/SHA/RC4/AES 単体）は **Sonnet 可**
（公式テストベクタで完全検証できる）。セキュリティハンドラ統合・CFF・
透明度は **Opus 推奨**。シェーディング・CCITT は **Opus 可**。

---

## 推奨経路

最短で「動くビューワー」: **Phase 1 → 2 → 5**（テキスト中心 PDF で実用化）。
画像（Phase 3）とビューワー機能（Phase 4）は後付け・並行可能。
最大のコスト要因は JPEG デコーダと CFF インタプリタの自前実装。
