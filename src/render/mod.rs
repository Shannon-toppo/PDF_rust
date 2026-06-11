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
pub mod pixmap;
pub mod raster;
pub mod state;
pub(crate) mod text;

pub use path::{Matrix, Path, Point};
pub use pixmap::Pixmap;
pub use raster::{fill_path, stroke_to_path, FillRule, LineCap, LineJoin, Mask, StrokeStyle};
pub use state::Renderer;
