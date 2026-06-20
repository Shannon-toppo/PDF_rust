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

## Phase 5: GUI シェル — 規模: 中 ✅ 完了（2026-06-11、別リポジトリ）

**別リポジトリ `../PDF_Viewer_rust` として実装済み**（Win32 生 FFI、依存は
`pdf_rust`（path 指定）のみ。`main.rs` + `win32.rs`）。本ライブラリの責務は
画像出力（`render_page`）までとし、GUI はビューワー側が持つ分担で確定。

- [x] ウィンドウ生成・メッセージループ・ページ描画表示
- [x] スクロール・ズーム・ページ送り
- [x] （Phase 4 連携）リンク・しおり・ページラベルの利用
- [ ] テキスト選択は未完（グリフ単位ヒットテストが必要 → Phase 8）

ビューワー実装で判明したライブラリ側の不足は
`../PDF_Viewer_rust/MISSING_FEATURES.md` に棚卸し済み。その内容を
Phase 7（性能・制御）・Phase 8（検索・選択）として本計画に取り込んだ。

**委譲**: **委譲非推奨**（unsafe FFI が大量で自動検証が困難。動作確認が目視必須）。

## Phase 6: 実世界カバレッジ拡大 — 規模: 特大・継続的

優先順: CFF → 暗号化（空パスワード優先）→ シェーディング → 透明度 → CCITT。
「手元の実 PDF が表示できない」事例ドリブンで進める。
現状: CFF・暗号化・シェーディング・透明度・CCITT・JBIG2 まで完了（2026-06-20）。

- [x] CFF/Type1 チャーストリング解釈（実世界 PDF の多数派。3,000–5,000 行級）
      — `src/cff.rs`（CFF パーサ + Type 2 解釈器）と `src/cff_strings.rs`
      （SID 標準文字列表 391 件）。OTTO sfnt の `CFF ` テーブル経由と
      `/FontFile3`（`OpenType` / `Type1C` / `CIDFontType0C`）経由の両方を
      `TrueTypeFont` 経由でレンダラから扱える。Type 2 演算子は hstem/vstem・
      hintmask・rrcurveto・hvcurveto/vhcurveto/vvcurveto/hhcurveto・rcurveline/
      rlinecurve・flex/hflex/hflex1/flex1・callsubr/callgsubr・seac 互換の
      endchar まで対応。CharstringType 2 のみ受け付け、Type1 eexec（旧式）と
      CFF2 は明示的に拒否する。検証: cff::tests（10）+ tests/cff_integration.rs
      で OTTO sfnt（Source Han Sans JP）の `'A'` / `'あ'` の outline デコードを
      確認
- [ ] Type3 フォント（コンテントストリームの再帰描画）
- [x] 暗号化 PDF: RC4/AES-128/AES-256 + MD5/SHA-2 自前実装（2026-06-19 完了。
      V1/R2・V2/R3・V4/R4（RC4-128 / AESV2）・V5/R6（AESV3）に対応。
      ユーザーパスワード認証で復号、再保存は平文化。`tests/fixtures/gen_encrypted_pdfs.py`
      で PyMuPDF から 4 種のフィクスチャを生成し、`extract_text` の一致で検証。
      R6 ハードンドハッシュの終了条件は仕様の文言と Acrobat/mupdf 実装で
      ラウンド番号の indexing が違うため、1-indexed 解釈を採用）
- [x] シェーディング（`sh`、axial/radial）・タイリングパターン（2026-06-19 完了。
      Type 2 (Axial)・Type 3 (Radial) の評価器を `render/shading.rs` に追加し、
      `sh` 演算子と `scn /PatternName`（Pattern 色空間）を `render/state.rs` で
      統合。Tiling (PatternType 1) は BBox を 1:1 でラスタライズして
      `xstep/ystep` で繰り返しサンプリング、Shading (PatternType 2) は
      パターン Matrix × CTM の逆写像で点ごとに shade。Type 1 関数ベースと
      Type 4–7 メッシュは読み飛ばし、uncolored Tiling の色注入は近似のみ。
      検証: 軸/放射の両端色、scn パターン経由の矩形塗り、タイリング繰り返し、
      不正な sh 名の no-op、`to_bytes`→`from_bytes` 往復のピクセル一致を確認）
- [x] 透明度（ExtGState `CA`/`ca`、ブレンドモード、透明グループ）（2026-06-19 完了。
      `/ca`（既存）と `/CA` を `GraphicsState` に拡張し、塗り・線・パターン・
      シェーディング・画像・グリフ描画のすべてに不透明度を反映。`/BM` の
      ブレンドモードを `BlendMode` 列挙体（分離可能 12 種 + 非分離可能
      Hue/Saturation/Color/Luminosity）として `render/blend.rs` に実装し、
      `Pixmap::blend_pixel_with` でピクセル単位に B(Cb,Cs) を適用する。
      透明グループ（Form XObject の `/Group <</S /Transparency>>`）は
      `Pixmap::new_transparent` で同サイズのオフスクリーンを `Renderer` の
      `offscreens` スタックに積み、内部の描画をそこへ蓄積してから離脱時に
      `composite_from` で Do 呼び出し時点の `/ca`・`/BM`・クリップで親へ
      合成する（PDF §11.3.4 の合成式）。検証: blend ユニット 7 + pixmap
      ユニット 4（不透明/透明バッファの合成、composite_from）+ state
      ユニット 4（`/ca`・`/CA`・`/BM Multiply`・透明グループの 1 単位合成）。
      `/SMask`・isolated/knockout の細部は SMask は無視、isolated 扱い相当のみ）
- [x] CCITTFaxDecode（スキャン文書）— `src/filters/ccitt.rs`。T.4 1D (MH)・
      T.4 2D (MR)・T.6 (MMR) の全方式を 1 つのデコーダで処理する。`/K`
      `Columns` `Rows` `EndOfBlock` `EndOfLine` `EncodedByteAlign` `BlackIs1`
      の DecodeParms に対応。1bpp パックビットを出力し、画像 XObject /
      ImageMask の双方で `decode_image` パスから利用可能。拡張 1D/2D
      （uncommon）と JBIG2Decode は明示的に拒否。検証: ccitt ユニット 22
      + 統合 9（フィルタ単体・配列フィルタ・略号 `/CCF`・XObject 描画・
      ImageMask 描画・全黒行・保存→再読込で同一ピクセル）。JBIG2 は
      仕様が極めて大きく（MQ コーダ・generic / text / halftone region・
      symbol dictionary）、本プロジェクトの「依存ゼロ + フルスクラッチ」
      方針では別 Phase に切り出すのが妥当としてここではスコープ外
- [x] JBIG2Decode（スキャン文書、`/Filter /JBIG2Decode`）— ITU-T T.88 /
      ISO 14492。算術経路の主要セグメント（Generic / Symbol / Text /
      Refinement / Pattern / Halftone）に対応（2026-06-20 完了）。
      - セッション 1（2026-06-20、完了）: 基盤
        （`bitmap`/`reader`/`mq`/`huffman`/`segment`/`page`/`mod`）。
        セグメントヘッダのパース + ページ情報セグメント + MQ 算術復号器
        （T.88 Annex E）+ Huffman 復号エンジン（標準テーブル B.1 のみ）+
        `/JBIG2Globals` の resolver 経由解決。最小ストリーム（ページ情報だけ）
        を背景一様画像として返す
      - セッション 2（2026-06-20、完了）: Generic region
        （`generic_region.rs`）。MMR (T.6) 経路は `ccitt::decode` を流用、
        算術経路は GBTEMPLATE 0/1/2/3（コンテキスト 16/13/10/10 ビット）+
        AT pixels（template=0 で 4 ペア、それ以外で 1 ペア）+ TPGDON
        （SLTP_CX による行スキップ）を実装。Driver は immediate generic
        region をパースして領域結合演算子（OR/AND/XOR/XNOR/REPLACE）で
        ページに合成する。検証: ユニット 11 増（generic_region 9 +
        mod 2: MMR 経路完走・算術経路完走の往復）
      - セッション 3（2026-06-20、完了）: Symbol dictionary
        （`symbol_dict.rs`）／ Text region（`text_region.rs`）／ Generic
        refinement（`refinement.rs`）／ Huffman B.2–B.15。SDHUFF=0 の
        arithmetic 経路（SDTEMPLATE 0–3、SDREFAGG=1 の単一インスタンス
        refinement 含む）と SBHUFF=0 の text region（refinement・transposed・
        参照コーナー対応）を実装。Generic refinement は GRTEMPLATE 0/1 +
        AT pixels（既定 (-1,-1)）+ TPGRON。Huffman は標準テーブル B.1–B.15
        を pdf.js の Annex B データから移植 + カスタムテーブル（type 53）
        パーサ。Driver は `BTreeMap<u32, SegmentArtifact>` でシンボル列・
        中間ビットマップ・カスタムテーブルを保持し、参照番号で解決する。
        Huffman 経路の symbol/text region は明示的に未対応エラーで返す
        （実 PDF では arithmetic がほぼ全て）。検証: ユニット 49 増
        （huffman 7 + refinement 5 + symbol_dict 5 + text_region 4 +
        mod 3: 空シンボル辞書 / 不正テーブル素通り / 参照無し refinement）
      - セッション 4（2026-06-20、完了）: Pattern dictionary
        （`pattern_dict.rs`）／ Halftone region（`halftone_region.rs`）。
        Pattern dictionary は HDMMR/HDTEMPLATE/HDPW/HDPH/GRAYMAX をパースし、
        全パターンを 1 つの大きな generic region として復号して
        `pattern_width` ごとに分割（AT pixels = T.88 §6.7.5 の固定値）。
        Halftone region は HMMR/HTEMPLATE/HENABLESKIP/HCOMBOP/HDEFPIXEL +
        HGW/HGH/HGX/HGY/HRX/HRY をパースし、算術経路で MSB → LSB の順に
        `HNUMBITPLANES` 個の generic region を 1 本の MQ ストリームから
        復号、Gray code でパターンインデックスを再構成して `>> 8` の
        固定小数点座標で配置する。`SegmentArtifact::Patterns` を追加し、
        Halftone セグメントが referred_segments で参照する。MMR 経路と
        HENABLESKIP は未対応エラー（実 PDF の出現が稀）。検証: ユニット
        17 増（pattern_dict 5 + halftone_region 11 + mod 1: pattern→
        halftone セグメント連鎖の `decode` 完走）
- [ ] progressive JPEG（`filters/dct.rs` の拡張。スキャン文書・写真系で遭遇）
- [ ] `/Mask`（ステンシル・カラーキー）
- [ ] 縦書き（Identity-V。和文ビューワーとしては将来必要）

**委譲**: 暗号プリミティブ（MD5/SHA/RC4/AES 単体）は **Sonnet 可**
（公式テストベクタで完全検証できる）。セキュリティハンドラ統合・CFF・
透明度は **Opus 推奨**。シェーディング・CCITT は **Opus 可**。

## Phase 7: レンダリング性能・制御 — 規模: 中 ✅ 完了（2026-06-12）

出典: `../PDF_Viewer_rust/MISSING_FEATURES.md` §1・§4・§5。ズーム/パンの全面
再描画と GUI スレッドのブロックが現状の最大の体感問題。Phase 6/8 と独立。

中心は `RenderOptions` の導入（`render_page(index, scale)` は薄いラッパとして維持）:

```rust
pub struct RenderOptions {
    pub scale: f64,                      // 72dpi = 1.0（既定 1.0）
    pub region: Option<[f64; 4]>,        // デバイス px [x, y, w, h]。None = 全面
    pub cancel: Option<Arc<AtomicBool>>, // true で協調キャンセル
    pub annotations: bool,               // 注釈外観の描画（既定 true）
    pub quality: RenderQuality,          // Normal / Fast（サムネイル用）
}
doc.render_page_with(index, &RenderOptions) -> Result<Pixmap>
doc.render_page_into(index, &RenderOptions, &mut Pixmap) -> Result<()>  // バッファ再利用
doc.page_size(index) -> Result<(f64, f64)>  // /Rotate 反映済み論理サイズ（pt）
```

- [x] 領域（タイル）レンダリング: 基底 CTM に `translate(-x, -y)` を合成して
      region サイズの Pixmap に描く。**全面レンダ結果の切り出しとのピクセル一致**で検証
      （矩形・斜め線・ベジェ・テキスト混在ページで最大差 ±1/チャンネル以内を確認。
      整数平行移動でも浮動小数の丸めが変わるため完全一致ではなく ±1 とした）。
      演算列は全件解釈し、範囲外はラスタライザのクリップで落ちる設計。
      演算単位のカリングは効果測定後の追加最適化とする。
      region 指定時はスケールの自動縮小ガードを適用しない（深いズームのタイルが
      目的のため）。タイル自体の大きさには全面と同じ上限ガードをかける
- [x] 協調キャンセル: `Renderer` の演算ループ 16 件ごと + グリフ単位 +
      画像デコード・描画の行単位にチェックを挿入。`PdfError::Cancelled` を追加し、
      部分結果は返さず `Err`
- [x] `render_page_into`: 既存 `Pixmap` の再利用（`Pixmap::reset` を追加。
      サイズ不一致は内部で作り直し）
- [x] 注釈描画の ON/OFF（`annotations: false` で /AP をスキップ）
- [x] `quality: Fast`: AA の縦サブスキャン 4x→1x（`fill_path_aa` を追加）・
      画像補間を最近傍に切替（サムネイル・先読み用。画質劣化は許容）
- [x] `page_size(index)`（/Rotate 反映済み論理サイズ）。DPI 換算は
      `scale = dpi / 72.0` の指針を rustdoc に明記（換算ヘルパ関数は増やさない）

検証済み: 統合テスト 7（タイル=全面切り出し一致・ページ外白・不正 region 拒否・
キャンセル・into 再利用一致・注釈 OFF・Fast 品質・page_size 回転整合）。
既存テストは全て無変更でパス（既定動作の互換を維持）（2026-06-12）。

実績: 委譲なしで直接実装（1 セッション、全 6 項目 + ドキュメント同期）。

## Phase 8: テキスト検索・選択 API — 規模: 中 ✅ 完了（2026-06-12。選択支援の検証のみ残）

出典: `../PDF_Viewer_rust/MISSING_FEATURES.md` §2。ビューワーの Ctrl+F と
テキスト選択（キャレット・ドラッグ）に必要。Phase 7 と独立・並行可能。

- [x] グリフ単位ボックス: `TextSpan` に `glyphs: Vec<SpanGlyph { text, bbox }>` を追加
      （スパン状態機械のコードごとの advance 区間を記録して bbox 化。
      `glyphs[].text` の連結 = `span.text` の不変条件をテストで保証。
      既存フィールドは不変 = ビューワー側の読み取り互換を維持）
- [x] 検索 API:
      `doc.search_page(index, query, &SearchOptions) -> Vec<SearchHit { rects }>` と
      全ページ版 `doc.search(query, ..) -> Vec<(usize, SearchHit)>`。
      `SearchOptions { case_insensitive: bool }`（正規表現はスコープ外）。
      新モジュール `search.rs`。ヒット同士は重ねない（Ctrl+F 挙動）
- [x] スパン跨ぎマッチ: 同一行判定（bbox 縦中心差が実効サイズの 0.5 倍以内。
      ベースラインはスパンに保持していないため中心で代用）で隣接スパンを連結し、
      行を跨ぐ境界と同一行の大きな隙間（0.3 em 超。Chromium 系の
      「1 グリフ 1 Tj」対応）に空白 1 個を仮定して照合。クエリ側の空白類も
      空白 1 個に正規化。ヒットの矩形は行ごとにマージして `rects` に分割する
- [ ] （選択支援）`SearchHit`/グリフボックスだけで足りるかビューワー側で検証し、
      必要ならヒットテストヘルパ `span/glyph_at(point)` を追加
      （→ `../PDF_Viewer_rust` 側でテキスト選択を実装するときに判断）

検証済み: search ユニット 8（矩形位置・大小文字・スパン/行跨ぎ・隙間空白・
非重複・空クエリ・上付きの同一行扱い）+ 統合 6（往復検索・case_insensitive・
行跨ぎ 2 矩形・全ページ・グリフ連結/単調性/境界箱整合・cm スケール追従）
（2026-06-12）。

実績: 委譲なしで直接実装（Phase 7 と同一セッション、ドキュメント同期込み）。

---

## 推奨経路

~~最短で「動くビューワー」: Phase 1 → 2 → 5~~ → **達成済み**（Phase 1–5 完了。
ビューワーは `../PDF_Viewer_rust` で稼働中）。

次に効く順（`MISSING_FEATURES.md` の提案どおり）:

1. ~~**Phase 7**（領域レンダリング・キャンセル）~~ → **完了**（2026-06-12。
   ビューワー側のタイル描画・ワーカースレッド化は `../PDF_Viewer_rust` 側の作業）
2. ~~**Phase 8**（検索・選択）~~ → **完了**（2026-06-12。ビューワー側の
   Ctrl+F UI・テキスト選択実装と、その過程でのヒットテストヘルパ要否判断が残）
3. **Phase 6**（実 PDF カバレッジ）— CFF → 暗号化（空パスワード優先）→
   シェーディング → 透明度 → CCITT の順。表示できない実例ドリブンで進める

Phase 6 は粒度が大きいので、着手時に「CFF だけ」「暗号化だけ」を
1 単位として切り出す。
