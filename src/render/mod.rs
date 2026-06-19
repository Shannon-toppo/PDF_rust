//! ページのラスタライズ（描画）。
//!
//! PDF のコンテントストリームを解釈してピクセルバッファ（[`Pixmap`]）へ
//! 描画するレンダリングエンジン。データフローは次の通り:
//!
//! ```text
//! page /Contents ──content(演算列)──▶ state(グラフィックス状態機械)
//!     ──path(パス構築・平坦化)──▶ raster(スキャンライン塗り)──▶ Pixmap
//! ```
//!
//! | サブモジュール | 役割 |
//! |---|---|
//! | [`pixmap`] | RGBA ピクセルバッファと PNG 書き出し |
//! | `path` | パス表現・アフィン変換・ベジェ平坦化（Phase 1 で追加） |
//! | `raster` | スキャンライン塗り（AA）・ストローク生成（Phase 1 で追加） |
//! | `state` | コンテント演算の解釈・グラフィックス状態（Phase 1 で追加） |
//! | `text` | 描画用フォントローダ（GID 解決・アウトライン取得。Phase 2 で追加） |
//! | `colorspace` | PDF 色空間の解決と RGB 変換（Phase 3 で追加） |
//! | `image` | 画像 XObject・インライン画像のデコードと描画（Phase 3 で追加） |
//! | `shading` | シェーディング Type 2/3（Phase 6 で追加） |
//! | `pattern` | タイリング／シェーディングパターン（Phase 6 で追加） |
//!
//! ## 座標系
//!
//! PDF はページ左下原点・y 軸上向きだが、[`Pixmap`] は画像慣習に従い
//! **左上原点・y 軸下向き**。変換はレンダラの基底行列（CTM）が吸収する。
//!
//! ## 耐故障性
//!
//! 壊れた PDF・未対応の演算子に遭遇しても panic せず、解釈できる範囲で
//! 描画を継続する（ライブラリ全体の方針と同じ）。

pub(crate) mod colorspace;
pub(crate) mod image;
pub mod path;
pub(crate) mod pattern;
pub mod pixmap;
pub mod raster;
pub(crate) mod shading;
pub mod state;
pub(crate) mod text;

pub use path::{Matrix, Path, Point};
pub use pixmap::Pixmap;
pub use raster::{fill_path, stroke_to_path, FillRule, LineCap, LineJoin, Mask, StrokeStyle};
pub use state::Renderer;

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// レンダリング品質。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RenderQuality {
    /// 通常品質（既定）。アンチエイリアスは縦 4x サブスキャン、画像は双線形補間。
    #[default]
    Normal,
    /// 高速・低品質。AA の縦サブスキャンを 1x に、画像補間を最近傍に切り替える。
    /// サムネイルや先読みなど画質劣化を許容できる用途向け。
    Fast,
}

/// [`crate::Document::render_page_with`] /
/// [`render_page_into`](crate::Document::render_page_into) の描画オプション。
///
/// `render_page(index, scale)` は
/// `RenderOptions { scale, ..Default::default() }` の薄いラッパ。
///
/// ```no_run
/// use pdf_rust::{Document, render::RenderOptions};
///
/// let doc = Document::load("input.pdf")?;
/// // 144dpi（scale = dpi / 72.0）で全面レンダリング。
/// let pm = doc.render_page_with(0, &RenderOptions { scale: 144.0 / 72.0, ..Default::default() })?;
/// # Ok::<(), pdf_rust::PdfError>(())
/// ```
#[derive(Debug, Clone)]
pub struct RenderOptions {
    /// 拡大率。72dpi を 1.0 とする（`dpi / 72.0` で換算。例: 144dpi → 2.0）。
    /// 非有限・0 以下は 1.0 として扱う。
    pub scale: f64,
    /// 描画する領域（タイル）。**デバイスピクセル座標**（スケール・回転適用後、
    /// 左上原点）の `[x, y, w, h]`。`None` は全面。
    ///
    /// 結果の [`Pixmap`] は `w × h`（切り上げ）になり、全面レンダリング結果から
    /// 同領域を切り出したものとピクセル一致する。ページ外にはみ出した部分は
    /// 白のまま残る。全面レンダリングと違いスケールの自動縮小は行わない
    /// （深いズームのタイル描画が目的のため）。タイル自体の大きさには
    /// 全面と同じ上限ガード（長辺 10000・総面積 1 億 px）がかかる。
    pub region: Option<[f64; 4]>,
    /// 協調キャンセル。`true` にすると描画ループが速やかに中断し、
    /// [`crate::PdfError::Cancelled`] が返る（部分結果は返さない）。
    pub cancel: Option<Arc<AtomicBool>>,
    /// 注釈外観（`/AP` `/N`）を描画するか（既定 `true`）。
    pub annotations: bool,
    /// 描画品質（既定 [`RenderQuality::Normal`]）。
    pub quality: RenderQuality,
}

impl Default for RenderOptions {
    fn default() -> RenderOptions {
        RenderOptions {
            scale: 1.0,
            region: None,
            cancel: None,
            annotations: true,
            quality: RenderQuality::Normal,
        }
    }
}
