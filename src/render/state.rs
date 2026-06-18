//! コンテント演算列を解釈してグラフィックスを描画する状態機械。
//!
//! [`crate::content::Operation`] の列を 1 つずつ実行し、PDF のグラフィックス
//! 状態（CTM・色・線属性・クリップ）を保ちながらパスを構築・描画する。
//!
//! ## 座標空間の扱い
//!
//! パスは**ユーザー空間のまま**構築し（[`Path`] にユーザー空間座標を蓄積）、
//! 描画時に現在の CTM でデバイス空間へ変換してから [`fill_path`] へ渡す。
//! ストロークは [`stroke_to_path`] に**ユーザー空間のパスとユーザー空間の
//! 線幅**を渡し、その戻り値（アウトライン）を CTM で変換してから
//! [`FillRule::NonZero`] で塗る。
//!
//! `re` の後に `cm` が来ても、塗り／線の実行演算子（`f`/`S` 等）が現れた
//! 時点の CTM が適用される。これは PDF の振る舞い（パスはカレント変換とは
//! 独立に座標で構築され、ペイント時の CTM で写像される）と一致する。
//!
//! ## 耐故障性
//!
//! 未対応の演算子・オペランド不足・型不一致は「その演算を読み飛ばす」だけで
//! panic しない。`unwrap`/`expect`/直接インデックスは使わない。

use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::colorspace::ColorSpace;
use super::path::{Matrix, Path};
use super::pixmap::Pixmap;
use super::raster::{fill_path_aa, stroke_to_path, FillRule, LineCap, LineJoin, Mask, StrokeStyle};
use super::text::{build_render_font, FontCache, RenderFont};
use super::RenderQuality;
use crate::content::{parse_content, Operation};
use crate::document::Document;
use crate::object::{Dictionary, Object};
use crate::truetype::OutlineSegment;

/// Form XObject の再帰展開の深さ上限（循環参照防止）。
const MAX_XOBJECT_DEPTH: u32 = 16;

/// 平坦化トレランス（デバイス空間ピクセル単位）。
const TOLERANCE: f64 = 0.25;

/// 演算ループでキャンセルフラグを確認する間隔（演算数）。
const CANCEL_CHECK_INTERVAL: u32 = 16;

/// 通常品質の縦サブスキャン本数（[`super::raster`] の既定と同じ）。
const SUBSAMPLES_NORMAL: u32 = 4;

/// 高速品質の縦サブスキャン本数。
const SUBSAMPLES_FAST: u32 = 1;

/// グラフィックス状態（`q`/`Q` で退避・復元される一式）。
#[derive(Debug, Clone)]
struct GraphicsState {
    /// 現在の変換行列（ユーザー空間 → デバイス空間）。
    ctm: Matrix,
    /// 塗り色（RGB）。
    fill_color: [u8; 3],
    /// 線色（RGB）。
    stroke_color: [u8; 3],
    /// 線幅（ユーザー空間）。
    line_width: f64,
    cap: LineCap,
    join: LineJoin,
    miter_limit: f64,
    /// ダッシュ配列（ユーザー空間。空＝実線）。
    dash: Vec<f64>,
    dash_phase: f64,
    /// 塗り側の色空間（`scn`/`sc` の解釈に使う）。
    fill_cs: ColorSpace,
    /// 線側の色空間。
    stroke_cs: ColorSpace,
    /// 塗り側の不透明度（ExtGState `/ca`。0–255）。画像合成にも使う。
    fill_alpha: u8,
    /// 現在のクリップマスク。`None` はクリップなし。
    clip: Option<Mask>,

    // --- テキスト状態（`q`/`Q` で退避・復元される側）---
    /// 現在フォント（解決済み描画情報）。`Tf` で設定。
    font: Option<Rc<RenderFont>>,
    /// フォントサイズ `Tfs`（`Tf` の第 2 オペランド）。
    font_size: f64,
    /// 字間 `Tc`（テキスト空間単位）。
    char_spacing: f64,
    /// 語間 `Tw`（テキスト空間単位。1 バイトコード 32 のみに作用）。
    word_spacing: f64,
    /// 水平拡大率 `Tz`（パーセント、既定 100）。
    h_scale: f64,
    /// 行送り `TL`（テキスト空間単位）。
    leading: f64,
    /// テキストライズ `Ts`（ベースラインからのずらし）。
    rise: f64,
    /// レンダリングモード `Tr`（0–7）。
    render_mode: i64,
}

impl GraphicsState {
    /// 基底 CTM から初期状態を作る。
    fn new(base_ctm: Matrix) -> GraphicsState {
        GraphicsState {
            ctm: base_ctm,
            fill_color: [0, 0, 0],
            stroke_color: [0, 0, 0],
            line_width: 1.0,
            cap: LineCap::Butt,
            join: LineJoin::Miter,
            miter_limit: 10.0,
            dash: Vec::new(),
            dash_phase: 0.0,
            fill_cs: ColorSpace::DeviceGray,
            stroke_cs: ColorSpace::DeviceGray,
            fill_alpha: 255,
            clip: None,
            font: None,
            font_size: 0.0,
            char_spacing: 0.0,
            word_spacing: 0.0,
            h_scale: 100.0,
            leading: 0.0,
            rise: 0.0,
            render_mode: 0,
        }
    }
}

/// 保留中クリップの種別（`W`/`W*` で立てるフラグ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingClip {
    None,
    NonZero,
    EvenOdd,
}

/// ページ 1 枚分の描画実行器。
pub struct Renderer<'a> {
    doc: &'a Document,
    pm: &'a mut Pixmap,
    /// 基底 CTM（ユーザー空間 → デバイス空間）。注釈外観の描画で使う。
    base_ctm: Matrix,
    /// グラフィックス状態スタック（先頭が現在状態）。
    stack: Vec<GraphicsState>,
    /// 構築中のパス（ユーザー空間座標）。
    path: Path,
    /// 現在点（ユーザー空間）。`c`/`v`/`y`/`h`・`re` のために保持する。
    current: Option<(f64, f64)>,
    /// 現在サブパスの開始点（`h` で戻る先）。
    start: Option<(f64, f64)>,
    /// `W`/`W*` で保留されたクリップ。
    pending_clip: PendingClip,
    /// Form XObject 展開の現在深さ。
    depth: u32,
    /// テキスト行列 `Tm`（`BT` で単位行列）。
    text_matrix: Matrix,
    /// テキスト行頭行列 `Tlm`（`BT`・`Td`・`Tm`・`T*` で更新）。
    line_matrix: Matrix,
    /// フォントのパース・名前解決のページ内キャッシュ。
    font_cache: FontCache,
    /// フォントリソース名 → 解決済み RenderFont のキャッシュ。
    font_by_name: std::collections::HashMap<String, Rc<RenderFont>>,
    /// 協調キャンセルフラグ（[`super::RenderOptions::cancel`]）。
    cancel: Option<Arc<AtomicBool>>,
    /// キャンセルを検知済みか（以後の演算は読み飛ばす）。
    cancelled: bool,
    /// 演算実行数のカウンタ（[`CANCEL_CHECK_INTERVAL`] ごとにフラグを確認）。
    op_counter: u32,
    /// 塗りの縦サブスキャン本数（Normal=4 / Fast=1）。
    subsamples: u32,
    /// 画像の双線形補間を許可するか（Fast では最近傍に落とす）。
    bilinear_allowed: bool,
}

impl<'a> Renderer<'a> {
    /// 描画器を作る。
    ///
    /// `base_ctm` は PDF ユーザー空間 → デバイス空間の基底行列で、呼び出し側
    /// （[`Document::render_page`]）が MediaBox 原点・y 軸反転・回転・スケールを
    /// 畳み込んで用意する。
    pub fn new(doc: &'a Document, pm: &'a mut Pixmap, base_ctm: Matrix) -> Renderer<'a> {
        Renderer {
            doc,
            pm,
            base_ctm,
            stack: vec![GraphicsState::new(base_ctm)],
            path: Path::new(),
            current: None,
            start: None,
            pending_clip: PendingClip::None,
            depth: 0,
            text_matrix: Matrix::identity(),
            line_matrix: Matrix::identity(),
            font_cache: FontCache::new(),
            font_by_name: std::collections::HashMap::new(),
            cancel: None,
            cancelled: false,
            op_counter: 0,
            subsamples: SUBSAMPLES_NORMAL,
            bilinear_allowed: true,
        }
    }

    /// 協調キャンセルフラグを設定する。
    ///
    /// フラグが `true` になると演算ループ・グリフ描画・画像デコードの内周で
    /// 検知して以後の描画を打ち切る。中断されたかは
    /// [`is_cancelled`](Self::is_cancelled) で確認できる。
    pub fn set_cancel_flag(&mut self, cancel: Option<Arc<AtomicBool>>) {
        self.cancel = cancel;
    }

    /// 描画品質を設定する（[`RenderQuality::Fast`] は AA 縦サブスキャン 1x +
    /// 画像最近傍サンプリング）。
    pub fn set_quality(&mut self, quality: RenderQuality) {
        match quality {
            RenderQuality::Normal => {
                self.subsamples = SUBSAMPLES_NORMAL;
                self.bilinear_allowed = true;
            }
            RenderQuality::Fast => {
                self.subsamples = SUBSAMPLES_FAST;
                self.bilinear_allowed = false;
            }
        }
    }

    /// キャンセルを検知して描画を打ち切ったか。
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    /// キャンセルフラグを確認する（検知したら `cancelled` を立てて true）。
    fn check_cancel(&mut self) -> bool {
        if self.cancelled {
            return true;
        }
        if let Some(c) = &self.cancel {
            if c.load(Ordering::Relaxed) {
                self.cancelled = true;
                return true;
            }
        }
        false
    }

    /// 演算ループ用のキャンセル確認（[`CANCEL_CHECK_INTERVAL`] 件ごとに
    /// フラグを読む。アトミック読みのコストを抑えるための間引き）。
    fn check_cancel_throttled(&mut self) -> bool {
        if self.cancelled {
            return true;
        }
        if self.cancel.is_none() {
            return false;
        }
        self.op_counter = self.op_counter.wrapping_add(1);
        if !self.op_counter.is_multiple_of(CANCEL_CHECK_INTERVAL) {
            return false;
        }
        self.check_cancel()
    }

    /// 演算列を解釈して描画する。
    ///
    /// `resources` はこの演算列の実効リソース辞書（ページまたは Form の
    /// `/Resources`）。`/XObject` の解決に使う。キャンセルを検知した場合は
    /// 残りの演算を実行せずに戻る。
    pub fn run(&mut self, ops: &[Operation], resources: &Dictionary) {
        for op in ops {
            if self.check_cancel_throttled() {
                return;
            }
            self.exec(op, resources);
        }
    }

    /// 現在状態への可変参照。スタックが空にならないよう常に 1 つは保つ。
    fn gs_mut(&mut self) -> &mut GraphicsState {
        if self.stack.is_empty() {
            self.stack.push(GraphicsState::new(Matrix::identity()));
        }
        // 末尾は常に存在する（直前で保証）。
        let last = self.stack.len() - 1;
        &mut self.stack[last]
    }

    /// 現在状態への不変参照（クローン）。
    fn gs(&self) -> GraphicsState {
        match self.stack.last() {
            Some(g) => g.clone(),
            None => GraphicsState::new(Matrix::identity()),
        }
    }

    /// 演算 1 つを実行する。
    fn exec(&mut self, op: &Operation, resources: &Dictionary) {
        let args = &op.operands;
        match op.operator.as_str() {
            // --- グラフィックス状態 ---
            "q" => {
                let cur = self.gs();
                self.stack.push(cur);
            }
            "Q" => {
                // 過剰な Q は無視（最低 1 つは残す）。
                if self.stack.len() > 1 {
                    self.stack.pop();
                }
            }
            "cm" => {
                if let Some(m) = matrix_from(args) {
                    let ctm = self.gs().ctm;
                    self.gs_mut().ctm = m.then(&ctm);
                }
            }
            "w" => {
                if let Some(v) = num(args, 0) {
                    self.gs_mut().line_width = v;
                }
            }
            "J" => {
                if let Some(v) = int(args, 0) {
                    self.gs_mut().cap = line_cap(v);
                }
            }
            "j" => {
                if let Some(v) = int(args, 0) {
                    self.gs_mut().join = line_join(v);
                }
            }
            "M" => {
                if let Some(v) = num(args, 0) {
                    self.gs_mut().miter_limit = v;
                }
            }
            "d" => {
                self.apply_dash(args);
            }
            "gs" => {
                self.apply_ext_gstate(args, resources);
            }
            "ri" | "i" => {} // レンダリングインテント・平滑度: 無視

            // --- パス構築 ---
            "m" => {
                if let (Some(x), Some(y)) = (num(args, 0), num(args, 1)) {
                    self.path.move_to(x, y);
                    self.current = Some((x, y));
                    self.start = Some((x, y));
                }
            }
            "l" => {
                if let (Some(x), Some(y)) = (num(args, 0), num(args, 1)) {
                    self.path.line_to(x, y);
                    self.current = Some((x, y));
                }
            }
            "c" => {
                if let (Some(x1), Some(y1), Some(x2), Some(y2), Some(x3), Some(y3)) = (
                    num(args, 0),
                    num(args, 1),
                    num(args, 2),
                    num(args, 3),
                    num(args, 4),
                    num(args, 5),
                ) {
                    self.path.curve_to(x1, y1, x2, y2, x3, y3);
                    self.current = Some((x3, y3));
                }
            }
            "v" => {
                // 第 1 制御点 = 現在点。
                if let (Some(x2), Some(y2), Some(x3), Some(y3)) =
                    (num(args, 0), num(args, 1), num(args, 2), num(args, 3))
                {
                    let (x1, y1) = self.current.unwrap_or((x2, y2));
                    self.path.curve_to(x1, y1, x2, y2, x3, y3);
                    self.current = Some((x3, y3));
                }
            }
            "y" => {
                // 第 2 制御点 = 終点。
                if let (Some(x1), Some(y1), Some(x3), Some(y3)) =
                    (num(args, 0), num(args, 1), num(args, 2), num(args, 3))
                {
                    self.path.curve_to(x1, y1, x3, y3, x3, y3);
                    self.current = Some((x3, y3));
                }
            }
            "re" => {
                if let (Some(x), Some(y), Some(w), Some(h)) =
                    (num(args, 0), num(args, 1), num(args, 2), num(args, 3))
                {
                    self.path.move_to(x, y);
                    self.path.line_to(x + w, y);
                    self.path.line_to(x + w, y + h);
                    self.path.line_to(x, y + h);
                    self.path.close();
                    self.current = Some((x, y));
                    self.start = Some((x, y));
                }
            }
            "h" => {
                self.path.close();
                if let Some(s) = self.start {
                    self.current = Some(s);
                }
            }

            // --- パス描画 ---
            "S" => self.paint(false, true, FillRule::NonZero),
            "s" => {
                self.path.close();
                self.paint(false, true, FillRule::NonZero);
            }
            "f" | "F" => self.paint(true, false, FillRule::NonZero),
            "f*" => self.paint(true, false, FillRule::EvenOdd),
            "B" => self.paint(true, true, FillRule::NonZero),
            "B*" => self.paint(true, true, FillRule::EvenOdd),
            "b" => {
                self.path.close();
                self.paint(true, true, FillRule::NonZero);
            }
            "b*" => {
                self.path.close();
                self.paint(true, true, FillRule::EvenOdd);
            }
            "n" => self.paint(false, false, FillRule::NonZero),

            // --- クリップ ---
            "W" => self.pending_clip = PendingClip::NonZero,
            "W*" => self.pending_clip = PendingClip::EvenOdd,

            // --- 色 ---
            "g" => {
                if let Some(v) = num(args, 0) {
                    self.gs_mut().fill_cs = ColorSpace::DeviceGray;
                    self.gs_mut().fill_color = gray_rgb(v);
                }
            }
            "G" => {
                if let Some(v) = num(args, 0) {
                    self.gs_mut().stroke_cs = ColorSpace::DeviceGray;
                    self.gs_mut().stroke_color = gray_rgb(v);
                }
            }
            "rg" => {
                if let (Some(r), Some(g), Some(b)) = (num(args, 0), num(args, 1), num(args, 2)) {
                    self.gs_mut().fill_cs = ColorSpace::DeviceRGB;
                    self.gs_mut().fill_color = rgb(r, g, b);
                }
            }
            "RG" => {
                if let (Some(r), Some(g), Some(b)) = (num(args, 0), num(args, 1), num(args, 2)) {
                    self.gs_mut().stroke_cs = ColorSpace::DeviceRGB;
                    self.gs_mut().stroke_color = rgb(r, g, b);
                }
            }
            "k" => {
                if let (Some(c), Some(m), Some(y), Some(kk)) =
                    (num(args, 0), num(args, 1), num(args, 2), num(args, 3))
                {
                    self.gs_mut().fill_cs = ColorSpace::DeviceCMYK;
                    self.gs_mut().fill_color = cmyk_rgb(c, m, y, kk);
                }
            }
            "K" => {
                if let (Some(c), Some(m), Some(y), Some(kk)) =
                    (num(args, 0), num(args, 1), num(args, 2), num(args, 3))
                {
                    self.gs_mut().stroke_cs = ColorSpace::DeviceCMYK;
                    self.gs_mut().stroke_color = cmyk_rgb(c, m, y, kk);
                }
            }
            "cs" => {
                // cs: 塗り色空間を設定し、色を既定値（黒）にリセット。
                let cs = self.resolve_color_space(args, resources);
                self.gs_mut().fill_cs = cs;
                self.gs_mut().fill_color = [0, 0, 0];
            }
            "CS" => {
                // CS: 線色空間を設定し、色を既定値（黒）にリセット。
                let cs = self.resolve_color_space(args, resources);
                self.gs_mut().stroke_cs = cs;
                self.gs_mut().stroke_color = [0, 0, 0];
            }
            "sc" | "scn" => {
                // sc/scn: 保存された色空間で解釈。Unsupported/不明はオペランド数ベースにフォールバック。
                let cs = self.gs().fill_cs.clone();
                let color = color_from_cs(args, &cs);
                self.gs_mut().fill_color = color;
            }
            "SC" | "SCN" => {
                // SC/SCN: 線色空間版。
                let cs = self.gs().stroke_cs.clone();
                let color = color_from_cs(args, &cs);
                self.gs_mut().stroke_color = color;
            }

            // --- テキストオブジェクト ---
            "BT" => {
                self.text_matrix = Matrix::identity();
                self.line_matrix = Matrix::identity();
            }
            "ET" => {}

            // --- テキスト状態 ---
            "Tc" => {
                if let Some(v) = num(args, 0) {
                    self.gs_mut().char_spacing = v;
                }
            }
            "Tw" => {
                if let Some(v) = num(args, 0) {
                    self.gs_mut().word_spacing = v;
                }
            }
            "Tz" => {
                if let Some(v) = num(args, 0) {
                    self.gs_mut().h_scale = v;
                }
            }
            "TL" => {
                if let Some(v) = num(args, 0) {
                    self.gs_mut().leading = v;
                }
            }
            "Ts" => {
                if let Some(v) = num(args, 0) {
                    self.gs_mut().rise = v;
                }
            }
            "Tr" => {
                if let Some(v) = int(args, 0) {
                    self.gs_mut().render_mode = v;
                }
            }
            "Tf" => self.set_font(args, resources),

            // --- テキスト位置 ---
            "Td" => self.op_td(args, false),
            "TD" => self.op_td(args, true),
            "Tm" => {
                if let Some(m) = matrix_from(args) {
                    self.line_matrix = m;
                    self.text_matrix = m;
                }
            }
            "T*" => self.op_tstar(),

            // --- テキスト表示 ---
            "Tj" => {
                if let Some(bytes) = args.first().and_then(|o| o.as_string().ok()) {
                    self.show_text(bytes);
                }
            }
            "'" => {
                self.op_tstar();
                if let Some(bytes) = args.first().and_then(|o| o.as_string().ok()) {
                    self.show_text(bytes);
                }
            }
            "\"" => {
                // aw ac string '
                if let Some(aw) = num(args, 0) {
                    self.gs_mut().word_spacing = aw;
                }
                if let Some(ac) = num(args, 1) {
                    self.gs_mut().char_spacing = ac;
                }
                self.op_tstar();
                if let Some(bytes) = args.get(2).and_then(|o| o.as_string().ok()) {
                    self.show_text(bytes);
                }
            }
            "TJ" => self.op_tj(args),

            // --- XObject ---
            "Do" => self.do_xobject(args, resources),

            // --- インライン画像 ---
            // オペランドは [Dictionary(画像辞書), String(生データ)] の 2 要素
            // （content.rs の parse_inline_image が生成）。同じデコード・描画
            // パスへ流す。
            "BI" => self.do_inline_image(args, resources),

            // --- 無視する演算子（シェーディング・マーク内容など）---
            // sh・マーク内容・互換は描画に影響しないので何もしない（耐故障性方針）。
            _ => {}
        }
    }

    /// 現在パスを塗り／線で描画し、保留クリップがあれば適用する。
    ///
    /// `do_fill`・`do_stroke` の組み合わせで `f`/`S`/`B`/`n` を表現する。
    /// 描画後はパスをリセットする（PDF のパス描画演算子の規定）。
    fn paint(&mut self, do_fill: bool, do_stroke: bool, fill_rule: FillRule) {
        let gs = self.gs();

        // 塗り（デバイス空間へ変換してから）。
        if do_fill && !self.path.is_empty() {
            let dev = self.path.transform(&gs.ctm);
            fill_path_aa(
                self.pm,
                &dev,
                fill_rule,
                gs.fill_color,
                255,
                gs.clip.as_ref(),
                self.subsamples,
            );
        }

        // 線（ユーザー空間でアウトライン化 → CTM で変換 → NonZero で塗り）。
        if do_stroke && !self.path.is_empty() {
            let style = StrokeStyle {
                width: gs.line_width,
                cap: gs.cap,
                join: gs.join,
                miter_limit: gs.miter_limit,
                dash: gs.dash.clone(),
                dash_phase: gs.dash_phase,
            };
            let scale = gs.ctm.approx_scale();
            let tol = TOLERANCE / scale;
            let outline = stroke_to_path(&self.path, &style, tol);
            let dev = outline.transform(&gs.ctm);
            fill_path_aa(
                self.pm,
                &dev,
                FillRule::NonZero,
                gs.stroke_color,
                255,
                gs.clip.as_ref(),
                self.subsamples,
            );
        }

        // 保留クリップの適用（描画演算子の後に現在パスでクリップ）。
        if self.pending_clip != PendingClip::None && !self.path.is_empty() {
            let rule = match self.pending_clip {
                PendingClip::EvenOdd => FillRule::EvenOdd,
                _ => FillRule::NonZero,
            };
            let dev = self.path.transform(&gs.ctm);
            let mask = Mask::from_path(&dev, rule, self.pm.width(), self.pm.height());
            let new_clip = match self.gs().clip {
                Some(mut existing) => {
                    existing.intersect(&mask);
                    existing
                }
                None => mask,
            };
            self.gs_mut().clip = Some(new_clip);
        }
        self.pending_clip = PendingClip::None;

        // パスをリセット。
        self.path = Path::new();
        self.current = None;
        self.start = None;
    }

    /// `d`（ダッシュ）演算子を適用する。
    fn apply_dash(&mut self, args: &[Object]) {
        // 形式: [配列] phase d
        let arr = match args.first().map(|o| self.doc.resolve(o)) {
            Some(Object::Array(a)) => a,
            _ => return,
        };
        let dash: Vec<f64> = arr
            .iter()
            .filter_map(|o| self.doc.resolve(o).as_number().ok())
            .collect();
        let phase = num(args, 1).unwrap_or(0.0);
        let gs = self.gs_mut();
        gs.dash = dash;
        gs.dash_phase = phase;
    }

    /// `gs`（ExtGState）演算子: `/LW /LC /LJ /ML /D` のみ反映する。
    fn apply_ext_gstate(&mut self, args: &[Object], resources: &Dictionary) {
        let name = match args.first().and_then(|o| o.as_name().ok()) {
            Some(n) => n,
            None => return,
        };
        let egs = match self.doc.dict_get(resources, "ExtGState") {
            Some(Object::Dictionary(d)) => d,
            _ => return,
        };
        let dict = match self.doc.dict_get(egs, name) {
            Some(Object::Dictionary(d)) => d.clone(),
            _ => return,
        };
        if let Some(v) = self
            .doc
            .dict_get(&dict, "LW")
            .and_then(|o| o.as_number().ok())
        {
            self.gs_mut().line_width = v;
        }
        if let Some(v) = self.doc.dict_get(&dict, "LC").and_then(|o| o.as_int().ok()) {
            self.gs_mut().cap = line_cap(v);
        }
        if let Some(v) = self.doc.dict_get(&dict, "LJ").and_then(|o| o.as_int().ok()) {
            self.gs_mut().join = line_join(v);
        }
        if let Some(v) = self
            .doc
            .dict_get(&dict, "ML")
            .and_then(|o| o.as_number().ok())
        {
            self.gs_mut().miter_limit = v;
        }
        // /ca: 塗り（と画像）の不透明度（0.0–1.0）。
        if let Some(v) = self
            .doc
            .dict_get(&dict, "ca")
            .and_then(|o| o.as_number().ok())
        {
            let a = (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            self.gs_mut().fill_alpha = a;
        }
        // /D は [配列 phase] の入れ子。
        if let Some(Object::Array(d)) = self.doc.dict_get(&dict, "D") {
            let d = d.clone();
            if let Some(Object::Array(arr)) = d.first().map(|o| self.doc.resolve(o)) {
                let dash: Vec<f64> = arr
                    .iter()
                    .filter_map(|o| self.doc.resolve(o).as_number().ok())
                    .collect();
                let phase = d
                    .get(1)
                    .and_then(|o| self.doc.resolve(o).as_number().ok())
                    .unwrap_or(0.0);
                let gs = self.gs_mut();
                gs.dash = dash;
                gs.dash_phase = phase;
            }
        }
    }

    /// `cs`/`CS` のオペランドから [`ColorSpace`] を解決する。
    ///
    /// 名前が "/ColorSpace 辞書" に登録されていれば `ColorSpace::parse` で解決する。
    /// 解決できない場合や名前省略の場合はオペランド名で直接解決を試みる。
    fn resolve_color_space(&self, args: &[Object], resources: &Dictionary) -> ColorSpace {
        let name_obj = match args.first() {
            Some(o) => o.clone(),
            None => return ColorSpace::DeviceGray,
        };
        ColorSpace::parse(self.doc, &name_obj, resources)
    }

    /// `Do`（XObject 描画）演算子。Form のみ再帰展開し、Image は無視する。
    fn do_xobject(&mut self, args: &[Object], resources: &Dictionary) {
        if self.depth >= MAX_XOBJECT_DEPTH {
            return; // 循環・過深防止。
        }
        let name = match args.first().and_then(|o| o.as_name().ok()) {
            Some(n) => n,
            None => return,
        };
        let xobjects = match self.doc.dict_get(resources, "XObject") {
            Some(Object::Dictionary(d)) => d,
            _ => return,
        };
        // ストリームを取得（XObject は常にストリーム）。
        let stream = match self.doc.dict_get(xobjects, name) {
            Some(Object::Stream(s)) => s.clone(),
            _ => return,
        };
        let subtype = stream.dict.get("Subtype").and_then(|o| o.as_name().ok());
        if subtype == Some("Image") {
            // 画像 XObject はデコードして CTM で描画する。
            self.draw_image_xobject(&stream.dict, &stream.data, resources);
            return;
        }
        if subtype != Some("Form") {
            return; // それ以外（PS XObject 等）は未対応。
        }

        // q 相当の退避。
        let saved = self.gs();
        self.stack.push(saved);

        // /Matrix を CTM に合成。
        if let Some(Object::Array(m)) = self.doc.dict_get(&stream.dict, "Matrix") {
            let nums: Vec<f64> = m
                .iter()
                .filter_map(|o| self.doc.resolve(o).as_number().ok())
                .collect();
            if let Some(fm) = matrix_from_slice(&nums) {
                let ctm = self.gs().ctm;
                self.gs_mut().ctm = fm.then(&ctm);
            }
        }

        // /BBox でクリップ（CTM 適用後のデバイス空間で）。
        if let Some(Object::Array(bb)) = self.doc.dict_get(&stream.dict, "BBox") {
            let nums: Vec<f64> = bb
                .iter()
                .filter_map(|o| self.doc.resolve(o).as_number().ok())
                .collect();
            if nums.len() == 4 {
                let (x0, y0, x1, y1) = (nums[0], nums[1], nums[2], nums[3]);
                let mut bbox = Path::new();
                bbox.move_to(x0, y0);
                bbox.line_to(x1, y0);
                bbox.line_to(x1, y1);
                bbox.line_to(x0, y1);
                bbox.close();
                let ctm = self.gs().ctm;
                let dev = bbox.transform(&ctm);
                let mask =
                    Mask::from_path(&dev, FillRule::NonZero, self.pm.width(), self.pm.height());
                let new_clip = match self.gs().clip {
                    Some(mut existing) => {
                        existing.intersect(&mask);
                        existing
                    }
                    None => mask,
                };
                self.gs_mut().clip = Some(new_clip);
            }
        }

        // Form 自身の /Resources、なければ親のを使う。
        let form_res = match self.doc.dict_get(&stream.dict, "Resources") {
            Some(Object::Dictionary(d)) => d.clone(),
            _ => resources.clone(),
        };

        // ストリームを伸長して再帰実行。
        if let Ok(data) = self.doc.get_stream_data(&stream) {
            if let Ok(ops) = parse_content(&data) {
                // 構築中パスは Form 内では独立扱いにする（退避）。
                let saved_path = std::mem::take(&mut self.path);
                let saved_cur = self.current.take();
                let saved_start = self.start.take();
                let saved_pending = std::mem::replace(&mut self.pending_clip, PendingClip::None);

                self.depth += 1;
                for op in &ops {
                    if self.check_cancel_throttled() {
                        break;
                    }
                    self.exec(op, &form_res);
                }
                self.depth -= 1;

                self.path = saved_path;
                self.current = saved_cur;
                self.start = saved_start;
                self.pending_clip = saved_pending;
            }
        }

        // 復元。
        if self.stack.len() > 1 {
            self.stack.pop();
        }
    }

    // --- 注釈の外観 ----------------------------------------------------------

    /// 注釈 1 件の外観ストリーム（`/AP` `/N`）を描画する（§12.5.5）。
    ///
    /// 外観の `/BBox` を `/Matrix` で変換した境界箱を注釈の `/Rect` へ写す
    /// 行列を合成し、Form XObject と同様に内容を実行する。Popup 注釈と
    /// Hidden / NoView フラグ付きは描かない。`page_resources` は外観に
    /// `/Resources` が無い場合のフォールバック。
    pub(crate) fn draw_annotation(&mut self, annot: &Dictionary, page_resources: &Dictionary) {
        if annot.get("Subtype").and_then(|o| o.as_name().ok()) == Some("Popup") {
            return;
        }
        // /F フラグ: bit2 = Hidden(2)、bit6 = NoView(32)。
        let flags = self
            .doc
            .dict_get(annot, "F")
            .and_then(|o| o.as_int().ok())
            .unwrap_or(0);
        if flags & 2 != 0 || flags & 32 != 0 {
            return;
        }

        // /Rect を正規化（デバイスではなくユーザー空間）。
        let rect = match self.annot_rect(annot) {
            Some(r) => r,
            None => return,
        };
        let (rw, rh) = (rect[2] - rect[0], rect[3] - rect[1]);
        if rw <= 0.0 || rh <= 0.0 {
            return; // 大きさのない注釈は描かない。
        }

        // /AP /N: ストリーム直置きか、状態名（/AS）で引く辞書。
        let ap = match self.doc.dict_get(annot, "AP") {
            Some(Object::Dictionary(d)) => d.clone(),
            _ => return,
        };
        let stream = match self.doc.dict_get(&ap, "N") {
            Some(Object::Stream(s)) => s.clone(),
            Some(Object::Dictionary(states)) => {
                let by_as = annot
                    .get("AS")
                    .and_then(|o| o.as_name().ok())
                    .and_then(|n| self.doc.dict_get(states, n));
                let chosen = match by_as {
                    Some(Object::Stream(s)) => Some(s.clone()),
                    // /AS が無い・引けない場合は最初のストリームで代用（耐故障性）。
                    _ => states.iter().find_map(|(_, v)| match self.doc.resolve(v) {
                        Object::Stream(s) => Some(s.clone()),
                        _ => None,
                    }),
                };
                match chosen {
                    Some(s) => s,
                    None => return,
                }
            }
            _ => return,
        };

        // /BBox（必須）と /Matrix（既定は単位行列）。
        let bbox = match self.numbers4(&stream.dict, "BBox") {
            Some(b) => b,
            None => return,
        };
        let form_matrix = match self.doc.dict_get(&stream.dict, "Matrix") {
            Some(Object::Array(m)) => {
                let nums: Vec<f64> = m
                    .iter()
                    .filter_map(|o| self.doc.resolve(o).as_number().ok())
                    .collect();
                matrix_from_slice(&nums).unwrap_or_else(Matrix::identity)
            }
            _ => Matrix::identity(),
        };

        // BBox の四隅を /Matrix で変換した軸平行境界箱を求める。
        let (bx0, by0, bx1, by1) = {
            let corners = [
                (bbox[0], bbox[1]),
                (bbox[2], bbox[1]),
                (bbox[2], bbox[3]),
                (bbox[0], bbox[3]),
            ];
            let mut x0 = f64::INFINITY;
            let mut y0 = f64::INFINITY;
            let mut x1 = f64::NEG_INFINITY;
            let mut y1 = f64::NEG_INFINITY;
            for (cx, cy) in corners {
                let p = form_matrix.apply(super::path::Point::new(cx, cy));
                x0 = x0.min(p.x);
                y0 = y0.min(p.y);
                x1 = x1.max(p.x);
                y1 = y1.max(p.y);
            }
            (x0, y0, x1, y1)
        };
        if !(bx0.is_finite() && by0.is_finite() && bx1.is_finite() && by1.is_finite()) {
            return;
        }

        // 変換後 BBox → /Rect の写像 A（退化時はスケール 1）。
        let sx = if bx1 - bx0 > 1e-9 {
            rw / (bx1 - bx0)
        } else {
            1.0
        };
        let sy = if by1 - by0 > 1e-9 {
            rh / (by1 - by0)
        } else {
            1.0
        };
        let a = Matrix::translate(-bx0, -by0)
            .then(&Matrix::scale(sx, sy))
            .then(&Matrix::translate(rect[0], rect[1]));

        // 外観の CTM = /Matrix → A → 基底 CTM。ページ内容の状態とは独立に、
        // 既定状態から開始する。
        let saved_len = self.stack.len();
        let mut gs = GraphicsState::new(self.base_ctm);
        gs.ctm = form_matrix.then(&a).then(&self.base_ctm);
        self.stack.push(gs);

        // /BBox でクリップ（フォーム空間 → デバイス空間）。
        let mut clip_path = Path::new();
        clip_path.move_to(bbox[0], bbox[1]);
        clip_path.line_to(bbox[2], bbox[1]);
        clip_path.line_to(bbox[2], bbox[3]);
        clip_path.line_to(bbox[0], bbox[3]);
        clip_path.close();
        let ctm = self.gs().ctm;
        let dev = clip_path.transform(&ctm);
        let mask = Mask::from_path(&dev, FillRule::NonZero, self.pm.width(), self.pm.height());
        self.gs_mut().clip = Some(mask);

        // 外観自身の /Resources、無ければページのものを使う。
        let form_res = match self.doc.dict_get(&stream.dict, "Resources") {
            Some(Object::Dictionary(d)) => d.clone(),
            _ => page_resources.clone(),
        };

        if let Ok(data) = self.doc.get_stream_data(&stream) {
            if let Ok(ops) = parse_content(&data) {
                let saved_path = std::mem::take(&mut self.path);
                let saved_cur = self.current.take();
                let saved_start = self.start.take();
                let saved_pending = std::mem::replace(&mut self.pending_clip, PendingClip::None);

                self.depth += 1;
                for op in &ops {
                    if self.check_cancel_throttled() {
                        break;
                    }
                    self.exec(op, &form_res);
                }
                self.depth -= 1;

                self.path = saved_path;
                self.current = saved_cur;
                self.start = saved_start;
                self.pending_clip = saved_pending;
            }
        }

        // 外観実行中の q 過剰にも耐えるよう、スタック長を呼び出し前に戻す。
        self.stack.truncate(saved_len.max(1));
    }

    /// 注釈の `/Rect` を正規化して返す。
    fn annot_rect(&self, annot: &Dictionary) -> Option<[f64; 4]> {
        let v = self.numbers4(annot, "Rect")?;
        Some([
            v[0].min(v[2]),
            v[1].min(v[3]),
            v[0].max(v[2]),
            v[1].max(v[3]),
        ])
    }

    /// 辞書から数値 4 つの配列を取り出す（間接参照解決込み。非有限は `None`）。
    fn numbers4(&self, dict: &Dictionary, key: &str) -> Option<[f64; 4]> {
        let arr = match self.doc.dict_get(dict, key) {
            Some(Object::Array(a)) if a.len() == 4 => a,
            _ => return None,
        };
        let mut v = [0.0f64; 4];
        for (i, o) in arr.iter().enumerate() {
            v[i] = self
                .doc
                .resolve(o)
                .as_number()
                .ok()
                .filter(|x| x.is_finite())?;
        }
        Some(v)
    }

    // --- 画像 --------------------------------------------------------------

    /// 画像 XObject（`Do` の Subtype=Image）を描画する。
    fn draw_image_xobject(&mut self, dict: &Dictionary, raw: &[u8], resources: &Dictionary) {
        let gs = self.gs();
        let cancel = self.cancel.clone();
        let cancel_ref = cancel.as_deref();
        let img = match super::image::decode_image(self.doc, dict, raw, resources, cancel_ref) {
            Some(i) => i,
            None => return, // 未対応形式・壊れた画像（またはキャンセル）は読み飛ばす。
        };
        super::image::draw_image(
            self.pm,
            &img,
            &gs.ctm,
            gs.clip.as_ref(),
            gs.fill_alpha,
            gs.fill_color,
            self.bilinear_allowed,
            cancel_ref,
        );
    }

    /// インライン画像（`BI`）を描画する。
    ///
    /// オペランドは `[Dictionary(画像辞書), String(生データ)]` の 2 要素。
    fn do_inline_image(&mut self, args: &[Object], resources: &Dictionary) {
        let dict = match args.first().and_then(|o| o.as_dict().ok()) {
            Some(d) => d.clone(),
            None => return,
        };
        let raw = match args.get(1).and_then(|o| o.as_string().ok()) {
            Some(s) => s.to_vec(),
            None => return,
        };
        self.draw_image_xobject(&dict, &raw, resources);
    }

    // --- テキスト ----------------------------------------------------------

    /// `Tf`（フォント + サイズ）。リソースからフォントを解決してキャッシュする。
    fn set_font(&mut self, args: &[Object], resources: &Dictionary) {
        let name = match args.first().and_then(|o| o.as_name().ok()) {
            Some(n) => n.to_string(),
            None => return,
        };
        if let Some(size) = num(args, 1) {
            self.gs_mut().font_size = size;
        }

        // 名前 → RenderFont をキャッシュ（ページ内で再解決しない）。
        let font = if let Some(f) = self.font_by_name.get(&name) {
            Some(f.clone())
        } else {
            // フォントリソースの参照先 ID（メモリ上の埋め込みフォント照合用）。
            let fonts = self
                .doc
                .dict_get(resources, "Font")
                .and_then(|o| o.as_dict().ok());
            let ref_id = fonts
                .and_then(|fd| fd.get(&name))
                .and_then(|o| o.as_reference().ok());
            let dict = fonts
                .and_then(|fd| self.doc.dict_get(fd, &name))
                .and_then(|o| o.as_dict().ok())
                .cloned();
            // フォント辞書が解決できる場合はそれで構築。
            // `to_bytes` 前の埋め込みフォントは辞書がまだ Null（プレースホルダ）
            // のため、ref_id がメモリ上の埋め込みフォントを指すなら空辞書 +
            // ref_id で Type0 として構築する。
            let built = match dict {
                Some(d) => Some(build_render_font(
                    self.doc,
                    &mut self.font_cache,
                    &d,
                    ref_id,
                )),
                None => {
                    if ref_id
                        .map(|id| self.doc.embedded_program_by_type0_id(id).is_some())
                        .unwrap_or(false)
                    {
                        // 空の Type0 辞書として構築（DW=1000・Identity 既定）。
                        let mut d = Dictionary::new();
                        d.set("Subtype", Object::name("Type0"));
                        Some(build_render_font(
                            self.doc,
                            &mut self.font_cache,
                            &d,
                            ref_id,
                        ))
                    } else {
                        None
                    }
                }
            };
            built.map(|rf| {
                let rf = Rc::new(rf);
                self.font_by_name.insert(name.clone(), rf.clone());
                rf
            })
        };
        self.gs_mut().font = font;
    }

    /// `Td`（行頭を移動）。`set_leading` が真なら `TL = -ty` も設定する（`TD`）。
    fn op_td(&mut self, args: &[Object], set_leading: bool) {
        let tx = num(args, 0).unwrap_or(0.0);
        let ty = num(args, 1).unwrap_or(0.0);
        if set_leading {
            self.gs_mut().leading = -ty;
        }
        // Tlm' = translate(tx, ty) × Tlm、Tm = Tlm'。
        let m = Matrix::translate(tx, ty).then(&self.line_matrix);
        self.line_matrix = m;
        self.text_matrix = m;
    }

    /// `T*`（0, -TL の Td 相当）。
    fn op_tstar(&mut self) {
        let tl = self.gs().leading;
        let m = Matrix::translate(0.0, -tl).then(&self.line_matrix);
        self.line_matrix = m;
        self.text_matrix = m;
    }

    /// `TJ`（配列表示）。数値は字送り調整、文字列は表示。
    fn op_tj(&mut self, args: &[Object]) {
        let items = match args.first() {
            Some(Object::Array(a)) => a.clone(),
            _ => return,
        };
        for item in &items {
            match item {
                Object::String(bytes, _) => self.show_text(bytes),
                Object::Integer(_) | Object::Real(_) => {
                    let adj = item.as_number().unwrap_or(0.0);
                    // tx = -adj/1000 · Tfs · Tz/100 を Tm に前置合成。
                    let gs = self.gs();
                    let tx = -adj / 1000.0 * gs.font_size * gs.h_scale / 100.0;
                    self.text_matrix = Matrix::translate(tx, 0.0).then(&self.text_matrix);
                }
                _ => {}
            }
        }
    }

    /// 文字列 1 つ分を表示する（コードごとにグリフ描画 + 字送り）。
    fn show_text(&mut self, bytes: &[u8]) {
        let gs = self.gs();
        let font = match &gs.font {
            Some(f) => f.clone(),
            None => return, // フォント未設定: 何も描かず字送りもしない。
        };
        let single_byte = font.is_single_byte();
        let codes = font.codes(bytes);
        for code in codes {
            // グリフ単位のキャンセル確認（長大なテキストの内周）。
            if self.check_cancel() {
                return;
            }
            // グリフ描画（render_mode 3 = 不可視は描かない）。
            if gs.render_mode != 3 && gs.render_mode != 7 {
                self.draw_glyph(&font, code, &gs);
            }

            // 字送り: tx = (w0/1000·Tfs + Tc + (code==32 かつ 1 バイトなら Tw)) · Tz/100。
            let w0 = font.advance_w0(code);
            let mut tx = w0 / 1000.0 * gs.font_size + gs.char_spacing;
            if single_byte && code == 32 {
                tx += gs.word_spacing;
            }
            tx *= gs.h_scale / 100.0;
            self.text_matrix = Matrix::translate(tx, 0.0).then(&self.text_matrix);
        }
    }

    /// 1 グリフを描画する。アウトラインが引けなければ何もしない（字送りは別途進む）。
    fn draw_glyph(&mut self, font: &RenderFont, code: u32, gs: &GraphicsState) {
        let (outline, upm) = match font.glyph_outline(code) {
            Some(v) => v,
            None => return,
        };
        if outline.is_empty() {
            return; // 空グリフ（スペース等）。
        }

        // グリフ空間 → テキスト空間の行列。
        // s = 1/upm、x は Tz/100 を掛ける。ライズ Ts は y 平行移動。
        let s = 1.0 / upm;
        let glyph_to_text = Matrix {
            a: gs.font_size * gs.h_scale / 100.0 * s,
            b: 0.0,
            c: 0.0,
            d: gs.font_size * s,
            e: 0.0,
            f: gs.rise,
        };
        // 合成: glyph → text → Tm → CTM。
        let to_device = glyph_to_text.then(&self.text_matrix).then(&gs.ctm);

        // アウトライン → Path（QuadTo は 3 次へ昇格）。
        let path = outline_to_path(&outline);
        let dev = path.transform(&to_device);

        let mode = gs.render_mode;
        let do_fill = matches!(mode, 0 | 2 | 4 | 6);
        let do_stroke = matches!(mode, 1 | 2 | 5 | 6);

        if do_fill {
            fill_path_aa(
                self.pm,
                &dev,
                FillRule::NonZero,
                gs.fill_color,
                255,
                gs.clip.as_ref(),
                self.subsamples,
            );
        }
        if do_stroke {
            let style = StrokeStyle {
                width: gs.line_width,
                cap: gs.cap,
                join: gs.join,
                miter_limit: gs.miter_limit,
                dash: gs.dash.clone(),
                dash_phase: gs.dash_phase,
            };
            // ストロークはテキスト空間でアウトライン化してからデバイス変換。
            let scale = to_device.approx_scale();
            let tol = TOLERANCE / scale.max(1e-6);
            let text_to_device = self.text_matrix.then(&gs.ctm);
            let glyph_path = path.transform(&glyph_to_text);
            let outline_path = stroke_to_path(&glyph_path, &style, tol);
            let stroked = outline_path.transform(&text_to_device);
            fill_path_aa(
                self.pm,
                &stroked,
                FillRule::NonZero,
                gs.stroke_color,
                255,
                gs.clip.as_ref(),
                self.subsamples,
            );
        }
    }
}

/// TrueType のアウトライン（2 次ベジェ）を [`Path`]（3 次ベジェ）へ変換する。
///
/// 2 次→3 次の昇格: c1 = start + 2/3·(ctrl−start)、c2 = end + 2/3·(ctrl−end)。
fn outline_to_path(outline: &[OutlineSegment]) -> Path {
    let mut path = Path::new();
    let mut cur = (0.0_f64, 0.0_f64);
    for seg in outline {
        match *seg {
            OutlineSegment::MoveTo(x, y) => {
                path.move_to(x, y);
                cur = (x, y);
            }
            OutlineSegment::LineTo(x, y) => {
                path.line_to(x, y);
                cur = (x, y);
            }
            OutlineSegment::QuadTo(cx, cy, ex, ey) => {
                let (sx, sy) = cur;
                let c1x = sx + 2.0 / 3.0 * (cx - sx);
                let c1y = sy + 2.0 / 3.0 * (cy - sy);
                let c2x = ex + 2.0 / 3.0 * (cx - ex);
                let c2y = ey + 2.0 / 3.0 * (cy - ey);
                path.curve_to(c1x, c1y, c2x, c2y, ex, ey);
                cur = (ex, ey);
            }
            OutlineSegment::CurveTo(c1x, c1y, c2x, c2y, ex, ey) => {
                path.curve_to(c1x, c1y, c2x, c2y, ex, ey);
                cur = (ex, ey);
            }
            OutlineSegment::Close => path.close(),
        }
    }
    path
}

// --- オペランド取り出しヘルパ ---------------------------------------------

/// `args[i]` を数値として取り出す。型不一致・不足は `None`。
fn num(args: &[Object], i: usize) -> Option<f64> {
    args.get(i).and_then(|o| o.as_number().ok())
}

/// `args[i]` を整数として取り出す。実数も丸めて受け付ける。
fn int(args: &[Object], i: usize) -> Option<i64> {
    args.get(i).and_then(|o| match o {
        Object::Integer(v) => Some(*v),
        Object::Real(r) if r.is_finite() => Some(*r as i64),
        _ => None,
    })
}

/// `cm`/`Tm` 形式のオペランド `[a b c d e f]` から行列を作る。
fn matrix_from(args: &[Object]) -> Option<Matrix> {
    let v: Vec<f64> = (0..6).filter_map(|i| num(args, i)).collect();
    matrix_from_slice(&v)
}

/// 6 要素のスライスから行列を作る（非有限を含む・不足は `None`）。
fn matrix_from_slice(v: &[f64]) -> Option<Matrix> {
    if v.len() != 6 || !v.iter().all(|x| x.is_finite()) {
        return None;
    }
    Some(Matrix {
        a: v[0],
        b: v[1],
        c: v[2],
        d: v[3],
        e: v[4],
        f: v[5],
    })
}

/// `J` 値から線端形状へ。未知は Butt。
fn line_cap(v: i64) -> LineCap {
    match v {
        1 => LineCap::Round,
        2 => LineCap::Square,
        _ => LineCap::Butt,
    }
}

/// `j` 値から接合形状へ。未知は Miter。
fn line_join(v: i64) -> LineJoin {
    match v {
        1 => LineJoin::Round,
        2 => LineJoin::Bevel,
        _ => LineJoin::Miter,
    }
}

/// `sc`/`scn`/`SC`/`SCN` の数値オペランド列を色空間に応じて RGB へ変換する。
///
/// 保存された色空間（[`ColorSpace`]）で成分数を確定し `to_rgb` を呼ぶ。
/// Pattern 名などの非数値オペランドは無視する。
/// 色空間が `Unsupported` の場合はオペランド数（1/3/4）でフォールバック解釈する
/// （後方互換）。
fn color_from_cs(args: &[Object], cs: &ColorSpace) -> [u8; 3] {
    // 末尾に Pattern 名が付く場合に備え、数値だけ取り出す。
    let nums: Vec<f64> = args.iter().filter_map(|o| o.as_number().ok()).collect();

    // Unsupported または成分数 0 の場合はオペランド数ベースのフォールバック。
    let n = cs.n_components();
    if matches!(cs, ColorSpace::Unsupported) || n == 0 {
        return match nums.len() {
            1 => gray_rgb(nums[0]),
            3 => rgb(nums[0], nums[1], nums[2]),
            4 => cmyk_rgb(nums[0], nums[1], nums[2], nums[3]),
            _ => [0, 0, 0],
        };
    }

    // 成分が足りない場合は 0 で補う。
    let comps: Vec<f64> = (0..n)
        .map(|i| nums.get(i).copied().unwrap_or(0.0))
        .collect();
    cs.to_rgb(&comps)
}

/// 0–1 の階調値（クランプ）を 0–255 へ。
fn comp(v: f64) -> u8 {
    let c = if v.is_finite() {
        v.clamp(0.0, 1.0)
    } else {
        0.0
    };
    (c * 255.0 + 0.5) as u8
}

/// グレースケール値を RGB へ。
fn gray_rgb(v: f64) -> [u8; 3] {
    let g = comp(v);
    [g, g, g]
}

/// RGB 値（各 0–1）を 8bit へ。
fn rgb(r: f64, g: f64, b: f64) -> [u8; 3] {
    [comp(r), comp(g), comp(b)]
}

/// CMYK → RGB の単純変換（r = 255·(1 - min(1, c + k)) 方式）。
fn cmyk_rgb(c: f64, m: f64, y: f64, k: f64) -> [u8; 3] {
    let conv = |v: f64, k: f64| -> u8 {
        let vv = if v.is_finite() { v.max(0.0) } else { 0.0 };
        let kk = if k.is_finite() { k.max(0.0) } else { 0.0 };
        let val = (1.0 - (vv + kk).min(1.0)).clamp(0.0, 1.0);
        (val * 255.0 + 0.5) as u8
    };
    [conv(c, k), conv(m, k), conv(y, k)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::Object;

    fn op(operator: &str, operands: Vec<Object>) -> Operation {
        Operation::new(operator, operands)
    }

    /// 単純な赤い矩形塗り → 中心が赤、外が白。
    #[test]
    fn fills_rectangle_red() {
        let doc = Document::new();
        let mut pm = Pixmap::new(20, 20);
        // y 反転なしの恒等基底（左下原点のまま）でテスト。
        let mut r = Renderer::new(&doc, &mut pm, Matrix::identity());
        let ops = vec![
            op("rg", vec![1.0.into(), 0.0.into(), 0.0.into()]),
            op("re", vec![5.into(), 5.into(), 10.into(), 10.into()]),
            op("f", vec![]),
        ];
        r.run(&ops, &Dictionary::new());
        assert_eq!(pm.pixel(10, 10), Some([255, 0, 0]));
        assert_eq!(pm.pixel(0, 0), Some([255, 255, 255]));
    }

    /// q/Q が状態を退避・復元する。Q 過多は無視。
    #[test]
    fn q_qstate_balance() {
        let doc = Document::new();
        let mut pm = Pixmap::new(10, 10);
        let mut r = Renderer::new(&doc, &mut pm, Matrix::identity());
        let ops = vec![
            op("q", vec![]),
            op("rg", vec![1.0.into(), 0.0.into(), 0.0.into()]),
            op("Q", vec![]),
            op("Q", vec![]), // 過剰 Q → 無視
            op("Q", vec![]),
            // 復元後は塗り色が黒に戻っているはず。
            op("re", vec![1.into(), 1.into(), 5.into(), 5.into()]),
            op("f", vec![]),
        ];
        r.run(&ops, &Dictionary::new());
        // スタックは空になっていない。
        assert!(!r.stack.is_empty());
        drop(r);
        assert_eq!(pm.pixel(3, 3), Some([0, 0, 0]));
    }

    /// CMYK の純シアンが妥当な色（青緑寄り）になる。
    #[test]
    fn cmyk_cyan() {
        let c = cmyk_rgb(1.0, 0.0, 0.0, 0.0);
        assert_eq!(c, [0, 255, 255]);
        let k = cmyk_rgb(0.0, 0.0, 0.0, 1.0);
        assert_eq!(k, [0, 0, 0]);
    }

    /// 未対応・不正演算子で panic しない。
    #[test]
    fn unknown_and_malformed_ops_no_panic() {
        let doc = Document::new();
        let mut pm = Pixmap::new(10, 10);
        let mut r = Renderer::new(&doc, &mut pm, Matrix::identity());
        let ops = vec![
            op("BT", vec![]),
            op("Tj", vec![Object::string_literal("x")]),
            op("ET", vec![]),
            op("cm", vec![1.into()]), // オペランド不足
            op("rg", vec![]),         // オペランド不足
            op("WeirdOp", vec![42.into()]),
            op("re", vec![]), // 不足 → パス追加されない
            op("f", vec![]),
        ];
        r.run(&ops, &Dictionary::new());
        // 何も描かれず、全面白のまま。
        assert_eq!(pm.pixel(5, 5), Some([255, 255, 255]));
    }

    /// color_from_cs の成分数・フォールバック動作を確認する。
    #[test]
    fn scn_component_count() {
        // DeviceGray: 1 成分 → グレー。
        assert_eq!(
            color_from_cs(&[0.5.into()], &ColorSpace::DeviceGray),
            [128, 128, 128]
        );
        // DeviceRGB: 3 成分 → 赤。
        assert_eq!(
            color_from_cs(
                &[1.0.into(), 0.0.into(), 0.0.into()],
                &ColorSpace::DeviceRGB
            ),
            [255, 0, 0]
        );
        // Pattern 名が混ざっても数値だけ拾う（Unsupported → オペランド数ベース）。
        assert_eq!(
            color_from_cs(
                &[
                    0.0.into(),
                    0.0.into(),
                    0.0.into(),
                    1.0.into(),
                    Object::name("P1")
                ],
                &ColorSpace::DeviceCMYK
            ),
            [0, 0, 0]
        );
        // Unsupported: オペランド数 1 → グレーとして解釈（後方互換）。
        assert_eq!(
            color_from_cs(&[0.5.into()], &ColorSpace::Unsupported),
            [128, 128, 128]
        );
    }

    /// cs + sc で Separation 色空間の塗りが反映されることを確認する。
    #[test]
    fn cs_sc_separation_color() {
        // Separation [/MyInk /DeviceRGB tint] を /Resources /ColorSpace に登録する。
        // tint: Type2, N=1, C0=[1,1,1], C1=[1,0,0] (白→赤)。
        let mut func_dict = crate::object::Dictionary::new();
        func_dict.set("FunctionType", Object::Integer(2));
        func_dict.set(
            "Domain",
            Object::Array(vec![Object::Real(0.0), Object::Real(1.0)]),
        );
        func_dict.set("N", Object::Integer(1));
        func_dict.set(
            "C0",
            Object::Array(vec![
                Object::Real(1.0),
                Object::Real(1.0),
                Object::Real(1.0),
            ]),
        );
        func_dict.set(
            "C1",
            Object::Array(vec![
                Object::Real(1.0),
                Object::Real(0.0),
                Object::Real(0.0),
            ]),
        );
        let sep_arr = Object::Array(vec![
            Object::name("Separation"),
            Object::name("MyInk"),
            Object::name("DeviceRGB"),
            Object::Dictionary(func_dict),
        ]);

        let mut cs_dict = crate::object::Dictionary::new();
        cs_dict.set("MyCS", sep_arr);
        let mut resources = crate::object::Dictionary::new();
        resources.set("ColorSpace", Object::Dictionary(cs_dict));

        let doc = Document::new();
        let mut pm = crate::render::pixmap::Pixmap::new(20, 20);
        let mut r = Renderer::new(&doc, &mut pm, Matrix::identity());

        let ops = vec![
            op("cs", vec![Object::name("MyCS")]), // 塗り色空間を Separation に設定
            op("sc", vec![Object::Real(1.0)]),    // tint=1.0 → 赤
            op("re", vec![5.into(), 5.into(), 10.into(), 10.into()]),
            op("f", vec![]),
        ];
        r.run(&ops, &resources);
        // 中心ピクセルが赤になっているはず。
        assert_eq!(pm.pixel(10, 10), Some([255, 0, 0]));
    }

    /// cs + sc で Indexed 色空間の塗りが反映されることを確認する。
    #[test]
    fn cs_sc_indexed_color() {
        // [/Indexed /DeviceRGB 1 lookup] を /ColorSpace に登録。
        // lookup: index=0→赤, index=1→青。
        let lookup: Vec<u8> = vec![255, 0, 0, 0, 0, 255];
        let lookup_obj = Object::String(lookup, crate::object::StringFormat::Literal);
        let indexed_arr = Object::Array(vec![
            Object::name("Indexed"),
            Object::name("DeviceRGB"),
            Object::Integer(1),
            lookup_obj,
        ]);

        let mut cs_dict = crate::object::Dictionary::new();
        cs_dict.set("IdxCS", indexed_arr);
        let mut resources = crate::object::Dictionary::new();
        resources.set("ColorSpace", Object::Dictionary(cs_dict));

        let doc = Document::new();
        let mut pm = crate::render::pixmap::Pixmap::new(20, 20);
        let mut r = Renderer::new(&doc, &mut pm, Matrix::identity());

        let ops = vec![
            op("cs", vec![Object::name("IdxCS")]),
            op("sc", vec![Object::Real(0.0)]), // index=0 → 赤
            op("re", vec![5.into(), 5.into(), 10.into(), 10.into()]),
            op("f", vec![]),
        ];
        r.run(&ops, &resources);
        assert_eq!(pm.pixel(10, 10), Some([255, 0, 0]));
    }
}
