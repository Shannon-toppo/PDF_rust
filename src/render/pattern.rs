//! タイリングおよびシェーディングパターン（PDF 32000-1:2008 §8.7）。
//!
//! `/Pattern` 色空間の `scn`/`SCN` で参照されるパターンリソースを実体化する。
//! 対応:
//!
//! | パターン種別 | 内容 |
//! |---|---|
//! | Tiling (PatternType=1) | 矩形タイルの繰り返し。PaintType 1=colored / 2=uncolored |
//! | Shading (PatternType=2) | [`Shading`](super::shading::Shading) をパターン化 |
//!
//! ## 座標系
//!
//! `pattern.Matrix` はパターン座標系→ユーザー空間（ペイント時の CTM が
//! 掛かる前）を表す。実描画では「pattern → ユーザー空間 → デバイス空間」と
//! なるよう **`pattern_matrix.then(&ctm_at_paint)` を全体行列**とし、デバイス
//! 座標からパターン座標への逆写像で点ごとの色を求める。
//!
//! ## 耐故障性
//!
//! 不正な辞書・未対応 PatternType は [`Pattern::Unsupported`] に縮退し、
//! 描画時には何もしない（呼び出し側で fallthrough）。

use std::sync::Arc;

use super::path::Matrix;
use super::pixmap::Pixmap;
use super::shading::Shading;
use crate::content::{parse_content, Operation};
use crate::document::Document;
use crate::object::{Dictionary, Object};

/// 解決済みのパターン。
///
/// [`Pattern::parse`] でリソース名から構築する。タイルの内容は
/// [`Tiling::content`] にあらかじめ `parse_content` した演算列を保持し、
/// 描画器側で必要に応じてタイル 1 枚分を [`Pixmap`] にラスタライズする。
#[derive(Debug, Clone)]
pub(crate) enum Pattern {
    /// PatternType 1（タイリング）。
    Tiling(Box<Tiling>),
    /// PatternType 2（シェーディング）。Shading が PdfFunction を抱えるため
    /// バリアント間サイズ差を抑える目的で Box 化する（clippy::large_enum_variant）。
    Shading(Box<ShadingPattern>),
    /// 未対応または不正な辞書（描画時に何もしない）。
    Unsupported,
}

/// タイリングパターンの解決済みデータ。
#[derive(Debug, Clone)]
pub(crate) struct Tiling {
    /// パターン座標系 → ユーザー空間（ペイント時 CTM 適用前）の行列。
    pub(crate) matrix: Matrix,
    /// `/BBox`（パターン座標系の矩形 `[x0, y0, x1, y1]`、x0<x1・y0<y1 に正規化済み）。
    pub(crate) bbox: [f64; 4],
    /// `/XStep` / `/YStep`（タイル間隔。0/非有限は描画時に弾く）。
    pub(crate) xstep: f64,
    pub(crate) ystep: f64,
    /// `/PaintType`（1=colored / 2=uncolored。本実装は両方とも colored と同じ処理。
    /// uncolored は呼び出し側が色をフィードするのが本来だが、簡略化のため
    /// パターン内部の色だけで塗る。Acrobat と若干違うが、外観の大幅な崩れは無い）。
    pub(crate) _paint_type: i64,
    /// パターン自身のリソース辞書（埋め込み演算列の `Tf`/`Do` 解決用）。
    pub(crate) resources: Dictionary,
    /// パース済みのコンテント演算列。
    pub(crate) content: Arc<Vec<Operation>>,
}

/// シェーディングパターンの解決済みデータ。
#[derive(Debug, Clone)]
pub(crate) struct ShadingPattern {
    /// パターン座標系 → ユーザー空間（CTM 適用前）の行列。
    pub(crate) matrix: Matrix,
    /// 中身のシェーディング。
    pub(crate) shading: Shading,
}

impl Pattern {
    /// `/Resources /Pattern` 辞書から名前で引いてパースする。
    ///
    /// 見つからない・型不一致は [`Pattern::Unsupported`]。
    pub(crate) fn parse(doc: &Document, name: &str, resources: &Dictionary) -> Pattern {
        let patterns = match doc.dict_get(resources, "Pattern") {
            Some(Object::Dictionary(d)) => d.clone(),
            _ => return Pattern::Unsupported,
        };
        let obj = match doc.dict_get(&patterns, name) {
            Some(o) => o.clone(),
            None => return Pattern::Unsupported,
        };
        let resolved = doc.resolve(&obj);
        // Tiling は Stream、Shading は Dictionary。
        match resolved {
            Object::Stream(s) => {
                Self::parse_tiling(doc, &s.dict, &doc.get_stream_data(s).unwrap_or_default())
            }
            Object::Dictionary(d) => Self::parse_shading_pattern(doc, d, resources),
            _ => Pattern::Unsupported,
        }
    }

    fn parse_tiling(doc: &Document, dict: &Dictionary, content: &[u8]) -> Pattern {
        // PatternType 1 期待（既定値も 1 で扱う）。
        let pt = doc
            .dict_get(dict, "PatternType")
            .and_then(|o| o.as_int().ok())
            .unwrap_or(1);
        if pt != 1 {
            return Pattern::Unsupported;
        }
        let matrix = parse_matrix(doc, dict, "Matrix").unwrap_or_else(Matrix::identity);
        let bbox = match parse_numbers4(doc, dict, "BBox") {
            Some(b) => {
                let (x0, y0, x1, y1) = (
                    b[0].min(b[2]),
                    b[1].min(b[3]),
                    b[0].max(b[2]),
                    b[1].max(b[3]),
                );
                [x0, y0, x1, y1]
            }
            None => return Pattern::Unsupported,
        };
        let xstep = doc
            .dict_get(dict, "XStep")
            .and_then(|o| o.as_number().ok())
            .unwrap_or(0.0);
        let ystep = doc
            .dict_get(dict, "YStep")
            .and_then(|o| o.as_number().ok())
            .unwrap_or(0.0);
        if !(xstep.is_finite() && ystep.is_finite()) || xstep == 0.0 || ystep == 0.0 {
            return Pattern::Unsupported;
        }
        let paint_type = doc
            .dict_get(dict, "PaintType")
            .and_then(|o| o.as_int().ok())
            .unwrap_or(1);
        let resources = match doc.dict_get(dict, "Resources") {
            Some(Object::Dictionary(d)) => d.clone(),
            _ => Dictionary::new(),
        };
        let ops = parse_content(content).unwrap_or_default();
        Pattern::Tiling(Box::new(Tiling {
            matrix,
            bbox,
            xstep,
            ystep,
            _paint_type: paint_type,
            resources,
            content: Arc::new(ops),
        }))
    }

    fn parse_shading_pattern(doc: &Document, dict: &Dictionary, resources: &Dictionary) -> Pattern {
        // PatternType 2 期待。
        let pt = doc
            .dict_get(dict, "PatternType")
            .and_then(|o| o.as_int().ok())
            .unwrap_or(2);
        if pt != 2 {
            return Pattern::Unsupported;
        }
        let matrix = parse_matrix(doc, dict, "Matrix").unwrap_or_else(Matrix::identity);
        let shading_obj = match doc.dict_get(dict, "Shading") {
            Some(o) => o.clone(),
            None => return Pattern::Unsupported,
        };
        let shading = Shading::parse(doc, &shading_obj, resources);
        Pattern::Shading(Box::new(ShadingPattern { matrix, shading }))
    }
}

/// パターンタイルを単一の [`Pixmap`] にラスタライズする。
///
/// 1 タイル分（BBox 範囲）をパターン座標系のまま描画した「タイル画像」を返す。
/// 戻り値の `tile_w` × `tile_h` ピクセルは BBox の幅・高さに対応し、
/// 呼び出し側がパターン座標 (px, py) から `(px - bbox.x0) / step * tile_w`
/// 等のサンプリングで色を取得する想定。
///
/// `tile_pixel_size` はタイル 1 ピクセルあたりの大きさ（pattern unit）。
/// 既定で 1.0 を渡すと「1 パターン単位 = 1 ピクセル」のタイル画像を作る。
pub(crate) fn rasterize_tile(
    doc: &Document,
    tiling: &Tiling,
    tile_pixel_size: f64,
) -> Option<Pixmap> {
    let [x0, y0, x1, y1] = tiling.bbox;
    let w = (x1 - x0).abs();
    let h = (y1 - y0).abs();
    if !(w.is_finite() && h.is_finite()) || w <= 0.0 || h <= 0.0 {
        return None;
    }
    // タイルの大きさは過剰に大きくならないように上限を設ける。
    const MAX_TILE_SIDE: u32 = 2048;
    let scale = if tile_pixel_size.is_finite() && tile_pixel_size > 0.0 {
        1.0 / tile_pixel_size
    } else {
        1.0
    };
    let tw = (w * scale).ceil().max(1.0) as u32;
    let th = (h * scale).ceil().max(1.0) as u32;
    if tw > MAX_TILE_SIDE || th > MAX_TILE_SIDE {
        return None;
    }
    let mut pm = Pixmap::new(tw, th);
    // パターン座標 (x, y) → タイル画像座標 (px, py):
    //   px = (x - x0) * scale
    //   py = (y1 - y) * scale   （タイル画像は左上原点、PDF は左下原点）
    let base_ctm = Matrix {
        a: scale,
        b: 0.0,
        c: 0.0,
        d: -scale,
        e: -x0 * scale,
        f: y1 * scale,
    };
    let mut r = super::state::Renderer::new(doc, &mut pm, base_ctm);
    r.run(&tiling.content, &tiling.resources);
    Some(pm)
}

// --- パースヘルパ ---------------------------------------------------------

fn parse_matrix(doc: &Document, dict: &Dictionary, key: &str) -> Option<Matrix> {
    match doc.dict_get(dict, key) {
        Some(Object::Array(a)) if a.len() == 6 => {
            let mut v = [0.0_f64; 6];
            for (i, o) in a.iter().enumerate() {
                let n = doc.resolve(o).as_number().ok()?;
                if !n.is_finite() {
                    return None;
                }
                v[i] = n;
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
        _ => None,
    }
}

fn parse_numbers4(doc: &Document, dict: &Dictionary, key: &str) -> Option<[f64; 4]> {
    match doc.dict_get(dict, key) {
        Some(Object::Array(a)) if a.len() == 4 => {
            let mut v = [0.0_f64; 4];
            for (i, o) in a.iter().enumerate() {
                let n = doc.resolve(o).as_number().ok()?;
                if !n.is_finite() {
                    return None;
                }
                v[i] = n;
            }
            Some(v)
        }
        _ => None,
    }
}
