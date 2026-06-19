//! シェーディング（PDF 32000-1:2008 §8.7.4）。
//!
//! `sh` 演算子およびシェーディングパターン（`PatternType 2`）の実体。
//! 本実装は Type 2（軸方向＝Axial）と Type 3（放射方向＝Radial）の 2 種類のみに
//! 対応する。Type 1（関数ベース）と Type 4–7（メッシュ）は実 PDF での出現頻度が
//! 低く実装量も多いため未対応として読み飛ばす。
//!
//! ## データフロー
//!
//! ```text
//! /Shading 辞書 (or ストリーム) ──Shading::parse──▶ Shading
//!     ──shade_at(x, y)──▶ Option<[u8; 3]>
//! ```
//!
//! [`Shading::shade_at`] はシェーディング自身の座標系（pattern space）の点
//! `(x, y)` を受け取り、その点の sRGB を返す。`Extend` 指定により範囲外でも
//! 端の色で塗り続けるか、`None` を返してその点を塗らないかを切り替える。
//!
//! ## 耐故障性
//!
//! 不正な辞書・非有限な座標は [`Shading::Unsupported`] へ縮退し
//! [`shade_at`](Shading::shade_at) は常に `None` を返す。`unwrap` は使わない。

use super::colorspace::ColorSpace;
use crate::document::Document;
use crate::function::PdfFunction;
use crate::object::{Dictionary, Object};

/// シェーディング 1 枚分の解決済みデータ。
#[derive(Debug, Clone)]
pub(crate) struct Shading {
    /// 形状ごとのデータ（軸方向／放射方向／未対応）。
    pub(crate) kind: ShadingKind,
    /// 出力色空間（関数の出力成分を RGB へ）。Pattern は不可。
    pub(crate) color_space: ColorSpace,
    /// `/Background`（指定された場合、シェーディング範囲外の塗りに使う色）。
    /// `sh` 演算子で使う想定（パターンとしては無視される——§8.7.4.5.1）。
    pub(crate) background: Option<[u8; 3]>,
    /// `/BBox`（シェーディング座標系での描画範囲。`sh` 時のクリップに使う）。
    pub(crate) bbox: Option<[f64; 4]>,
}

/// シェーディング種別ごとのパラメータ。
#[derive(Debug, Clone)]
pub(crate) enum ShadingKind {
    /// Axial（Type 2）: 2 点間の線形補間。
    Axial {
        /// 始点 `(x0, y0)` と終点 `(x1, y1)`。
        coords: [f64; 4],
        /// `/Domain` の `[t0, t1]`（既定 `[0, 1]`）。
        domain: [f64; 2],
        /// tint 関数（domain → 色成分）。
        function: PdfFunction,
        /// `/Extend` の `[start_extend, end_extend]`。
        extend: [bool; 2],
    },
    /// Radial（Type 3）: 2 円間の補間。
    Radial {
        /// `[x0, y0, r0, x1, y1, r1]`。
        coords: [f64; 6],
        /// `/Domain`。
        domain: [f64; 2],
        /// tint 関数。
        function: PdfFunction,
        /// `/Extend`。
        extend: [bool; 2],
    },
    /// 未対応形式（読み飛ばし用の縮退）。
    Unsupported,
}

impl Shading {
    /// シェーディング辞書または `/Shading` キーで参照される値からパースする。
    ///
    /// 入力は辞書か、`/Shading` がストリームを許す Type 4–7 用のストリーム。
    /// 解決不能な場合は [`ShadingKind::Unsupported`] へ縮退する。
    pub(crate) fn parse(doc: &Document, obj: &Object, resources: &Dictionary) -> Shading {
        let obj = doc.resolve(obj);
        let dict = match obj {
            Object::Dictionary(d) => d,
            Object::Stream(s) => &s.dict,
            _ => return Self::unsupported(),
        };

        // /ShadingType: 1=関数, 2=Axial, 3=Radial, 4–7=メッシュ。
        let shading_type = doc
            .dict_get(dict, "ShadingType")
            .and_then(|o| o.as_int().ok())
            .unwrap_or(0);

        // /ColorSpace（Pattern は不可——§8.7.4.5.1。Pattern なら Unsupported）。
        let cs = match doc.dict_get(dict, "ColorSpace") {
            Some(cs_obj) => {
                let cs_obj = cs_obj.clone();
                let cs = ColorSpace::parse(doc, &cs_obj, resources);
                if matches!(cs, ColorSpace::Pattern { .. }) {
                    return Self::unsupported();
                }
                cs
            }
            None => return Self::unsupported(),
        };

        // /Background（出力色空間の成分数で読む。失敗時 None）。
        let background = match doc.dict_get(dict, "Background") {
            Some(Object::Array(a)) => {
                let nums: Vec<f64> = a
                    .iter()
                    .filter_map(|o| doc.resolve(o).as_number().ok())
                    .collect();
                Some(cs.to_rgb(&nums))
            }
            _ => None,
        };

        // /BBox（任意）。
        let bbox = numbers4(doc, dict, "BBox");

        match shading_type {
            2 => Self::parse_axial(doc, dict, cs, background, bbox),
            3 => Self::parse_radial(doc, dict, cs, background, bbox),
            _ => Self::unsupported(),
        }
    }

    fn parse_axial(
        doc: &Document,
        dict: &Dictionary,
        cs: ColorSpace,
        background: Option<[u8; 3]>,
        bbox: Option<[f64; 4]>,
    ) -> Shading {
        let coords = match numbers4(doc, dict, "Coords") {
            Some(c) => c,
            None => return Self::unsupported(),
        };
        let domain = numbers2(doc, dict, "Domain").unwrap_or([0.0, 1.0]);
        let function = match parse_function(doc, dict) {
            Some(f) => f,
            None => return Self::unsupported(),
        };
        let extend = parse_extend(doc, dict);
        Shading {
            kind: ShadingKind::Axial {
                coords,
                domain,
                function,
                extend,
            },
            color_space: cs,
            background,
            bbox,
        }
    }

    fn parse_radial(
        doc: &Document,
        dict: &Dictionary,
        cs: ColorSpace,
        background: Option<[u8; 3]>,
        bbox: Option<[f64; 4]>,
    ) -> Shading {
        let coords = match doc.dict_get(dict, "Coords") {
            Some(Object::Array(a)) if a.len() == 6 => {
                let mut v = [0.0_f64; 6];
                for (i, o) in a.iter().enumerate() {
                    match doc.resolve(o).as_number().ok().filter(|x| x.is_finite()) {
                        Some(n) => v[i] = n,
                        None => return Self::unsupported(),
                    }
                }
                v
            }
            _ => return Self::unsupported(),
        };
        let domain = numbers2(doc, dict, "Domain").unwrap_or([0.0, 1.0]);
        let function = match parse_function(doc, dict) {
            Some(f) => f,
            None => return Self::unsupported(),
        };
        let extend = parse_extend(doc, dict);
        Shading {
            kind: ShadingKind::Radial {
                coords,
                domain,
                function,
                extend,
            },
            color_space: cs,
            background,
            bbox,
        }
    }

    fn unsupported() -> Shading {
        Shading {
            kind: ShadingKind::Unsupported,
            color_space: ColorSpace::DeviceGray,
            background: None,
            bbox: None,
        }
    }

    /// シェーディング座標系の点 `(x, y)` における色を返す。
    ///
    /// `/Extend` で許可されない範囲は `None`（呼び出し側は背景色または無描画にする）。
    pub(crate) fn shade_at(&self, x: f64, y: f64) -> Option<[u8; 3]> {
        if !(x.is_finite() && y.is_finite()) {
            return None;
        }
        // /BBox 範囲外は対象外（sh 演算子のクリップとして機能）。
        if let Some(b) = self.bbox {
            if x < b[0] || x > b[2] || y < b[1] || y > b[3] {
                return None;
            }
        }
        match &self.kind {
            ShadingKind::Axial {
                coords,
                domain,
                function,
                extend,
            } => self.shade_axial(x, y, coords, domain, function, extend),
            ShadingKind::Radial {
                coords,
                domain,
                function,
                extend,
            } => self.shade_radial(x, y, coords, domain, function, extend),
            ShadingKind::Unsupported => None,
        }
    }

    fn shade_axial(
        &self,
        x: f64,
        y: f64,
        coords: &[f64; 4],
        domain: &[f64; 2],
        function: &PdfFunction,
        extend: &[bool; 2],
    ) -> Option<[u8; 3]> {
        let (x0, y0, x1, y1) = (coords[0], coords[1], coords[2], coords[3]);
        let dx = x1 - x0;
        let dy = y1 - y0;
        let denom = dx * dx + dy * dy;
        if !denom.is_finite() || denom <= 0.0 {
            return None;
        }
        let s = ((x - x0) * dx + (y - y0) * dy) / denom;
        let t = if s < 0.0 {
            if !extend[0] {
                return None;
            }
            domain[0]
        } else if s > 1.0 {
            if !extend[1] {
                return None;
            }
            domain[1]
        } else {
            domain[0] + s * (domain[1] - domain[0])
        };
        Some(self.eval_color(function, t))
    }

    fn shade_radial(
        &self,
        x: f64,
        y: f64,
        coords: &[f64; 6],
        domain: &[f64; 2],
        function: &PdfFunction,
        extend: &[bool; 2],
    ) -> Option<[u8; 3]> {
        // 中心と半径が `s` の関数として直線補間: c(s) = c0 + s·(c1-c0)、r(s) = r0 + s·(r1-r0)。
        // 点 (x,y) が円上に乗る s を解く: |(x,y) - c(s)|² = r(s)²。
        // → A s² + B s + C = 0、A = |c1-c0|² - dr²、B = -2[(x-c0)·(c1-c0) + r0·dr]、
        //   C = |x-c0|² - r0²。
        let (x0, y0, r0, x1, y1, r1) = (
            coords[0], coords[1], coords[2], coords[3], coords[4], coords[5],
        );
        let dx = x1 - x0;
        let dy = y1 - y0;
        let dr = r1 - r0;
        let fx = x - x0;
        let fy = y - y0;
        let a = dx * dx + dy * dy - dr * dr;
        let b = -2.0 * (fx * dx + fy * dy + r0 * dr);
        let c = fx * fx + fy * fy - r0 * r0;

        // 解候補。s_pick(true) は s が範囲内かつ r(s)≥0 のうち**最大**の s（前景優先）、
        // それが無ければ extend を許す範囲で範囲外の解（最大）を取る。
        let s_pick = |s: f64| -> bool {
            let r = r0 + s * dr;
            if !r.is_finite() || r < 0.0 {
                return false;
            }
            if (0.0..=1.0).contains(&s) {
                return true;
            }
            if s < 0.0 {
                extend[0]
            } else {
                extend[1]
            }
        };

        let s = if a.abs() < 1e-12 {
            // 線形: B s + C = 0。
            if b.abs() < 1e-12 {
                return None;
            }
            let s = -c / b;
            if !s.is_finite() || !s_pick(s) {
                return None;
            }
            s
        } else {
            let disc = b * b - 4.0 * a * c;
            if !disc.is_finite() || disc < 0.0 {
                return None;
            }
            let sq = disc.sqrt();
            let s1 = (-b + sq) / (2.0 * a);
            let s2 = (-b - sq) / (2.0 * a);
            // 大きい方を優先（手前の円が前景になる慣習）。
            let (lo, hi) = if s1 < s2 { (s1, s2) } else { (s2, s1) };
            if s_pick(hi) {
                hi
            } else if s_pick(lo) {
                lo
            } else {
                return None;
            }
        };

        let s_clamped = s.clamp(0.0, 1.0);
        let t = domain[0] + s_clamped * (domain[1] - domain[0]);
        Some(self.eval_color(function, t))
    }

    /// 関数を 1 入力で評価して色空間で RGB へ。
    fn eval_color(&self, function: &PdfFunction, t: f64) -> [u8; 3] {
        let comps = function.eval(&[t]);
        self.color_space.to_rgb(&comps)
    }
}

// ============================================================
// パースヘルパ
// ============================================================

fn numbers2(doc: &Document, dict: &Dictionary, key: &str) -> Option<[f64; 2]> {
    match doc.dict_get(dict, key) {
        Some(Object::Array(a)) if a.len() == 2 => {
            let v0 = doc.resolve(&a[0]).as_number().ok()?;
            let v1 = doc.resolve(&a[1]).as_number().ok()?;
            if v0.is_finite() && v1.is_finite() {
                Some([v0, v1])
            } else {
                None
            }
        }
        _ => None,
    }
}

fn numbers4(doc: &Document, dict: &Dictionary, key: &str) -> Option<[f64; 4]> {
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

fn parse_extend(doc: &Document, dict: &Dictionary) -> [bool; 2] {
    let as_bool = |o: &Object| -> bool {
        match doc.resolve(o) {
            Object::Boolean(b) => *b,
            _ => false,
        }
    };
    match doc.dict_get(dict, "Extend") {
        Some(Object::Array(a)) if a.len() == 2 => [as_bool(&a[0]), as_bool(&a[1])],
        _ => [false, false],
    }
}

/// `/Function` を解決する。配列形式（各成分ごと）は Type3 風に評価したいが、
/// ここでは「最初の関数」だけを使うフォールバックとする（多くの実 PDF は単一関数）。
///
/// 単一関数の場合、関数の出力次元 = 色成分数で評価される（既存実装）。
/// 配列の場合は各要素を呼び出して結合するが、まれなので未対応とする。
fn parse_function(doc: &Document, dict: &Dictionary) -> Option<PdfFunction> {
    let f_obj = doc.dict_get(dict, "Function")?;
    // 配列なら先頭を取る（成分ごとの関数は未対応——縮退）。
    let f_obj = match f_obj {
        Object::Array(a) => a.first()?.clone(),
        other => other.clone(),
    };
    PdfFunction::from_object(doc, &f_obj).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::Dictionary;

    fn type2_function(c0: [f64; 3], c1: [f64; 3]) -> Dictionary {
        let mut d = Dictionary::new();
        d.set("FunctionType", Object::Integer(2));
        d.set(
            "Domain",
            Object::Array(vec![Object::Real(0.0), Object::Real(1.0)]),
        );
        d.set("N", Object::Integer(1));
        d.set(
            "C0",
            Object::Array(vec![
                Object::Real(c0[0]),
                Object::Real(c0[1]),
                Object::Real(c0[2]),
            ]),
        );
        d.set(
            "C1",
            Object::Array(vec![
                Object::Real(c1[0]),
                Object::Real(c1[1]),
                Object::Real(c1[2]),
            ]),
        );
        d
    }

    fn build_axial(extend: [bool; 2]) -> Shading {
        // 黒 (0,0) → 赤 (10,0)。
        let doc = Document::new();
        let mut sd = Dictionary::new();
        sd.set("ShadingType", Object::Integer(2));
        sd.set("ColorSpace", Object::name("DeviceRGB"));
        sd.set(
            "Coords",
            Object::Array(vec![
                Object::Real(0.0),
                Object::Real(0.0),
                Object::Real(10.0),
                Object::Real(0.0),
            ]),
        );
        sd.set(
            "Function",
            Object::Dictionary(type2_function([0.0, 0.0, 0.0], [1.0, 0.0, 0.0])),
        );
        sd.set(
            "Extend",
            Object::Array(vec![Object::Boolean(extend[0]), Object::Boolean(extend[1])]),
        );
        Shading::parse(&doc, &Object::Dictionary(sd), &Dictionary::new())
    }

    #[test]
    fn axial_endpoints_and_middle() {
        let s = build_axial([false, false]);
        // 始点で黒、終点で赤、中間でその間。
        assert_eq!(s.shade_at(0.0, 0.0), Some([0, 0, 0]));
        assert_eq!(s.shade_at(10.0, 0.0), Some([255, 0, 0]));
        let mid = s.shade_at(5.0, 0.0).unwrap();
        assert!(mid[0] > 100 && mid[0] < 180);
        assert_eq!(mid[1], 0);
        assert_eq!(mid[2], 0);
    }

    #[test]
    fn axial_extend_false_returns_none_outside() {
        let s = build_axial([false, false]);
        assert!(s.shade_at(-1.0, 0.0).is_none());
        assert!(s.shade_at(11.0, 0.0).is_none());
    }

    #[test]
    fn axial_extend_true_clamps_to_endpoint_color() {
        let s = build_axial([true, true]);
        assert_eq!(s.shade_at(-5.0, 0.0), Some([0, 0, 0]));
        assert_eq!(s.shade_at(15.0, 0.0), Some([255, 0, 0]));
    }

    #[test]
    fn axial_perpendicular_projection() {
        // 線上ではない点でも、線への射影で評価される。
        let s = build_axial([false, false]);
        // (5, 3) は x=0..10 の線分上 t=0.5 に射影。色は中間。
        let c = s.shade_at(5.0, 3.0).unwrap();
        assert!(c[0] > 100 && c[0] < 180);
    }

    fn build_radial(extend: [bool; 2]) -> Shading {
        // 中心 (0,0) 半径 0 → 中心 (0,0) 半径 10、黒→白。
        let doc = Document::new();
        let mut sd = Dictionary::new();
        sd.set("ShadingType", Object::Integer(3));
        sd.set("ColorSpace", Object::name("DeviceRGB"));
        sd.set(
            "Coords",
            Object::Array(vec![
                Object::Real(0.0),
                Object::Real(0.0),
                Object::Real(0.0),
                Object::Real(0.0),
                Object::Real(0.0),
                Object::Real(10.0),
            ]),
        );
        sd.set(
            "Function",
            Object::Dictionary(type2_function([0.0, 0.0, 0.0], [1.0, 1.0, 1.0])),
        );
        sd.set(
            "Extend",
            Object::Array(vec![Object::Boolean(extend[0]), Object::Boolean(extend[1])]),
        );
        Shading::parse(&doc, &Object::Dictionary(sd), &Dictionary::new())
    }

    #[test]
    fn radial_center_is_inner_color() {
        let s = build_radial([false, false]);
        // 中心は半径 0 の点に乗るので s=0 で黒。
        assert_eq!(s.shade_at(0.0, 0.0), Some([0, 0, 0]));
    }

    #[test]
    fn radial_edge_is_outer_color() {
        let s = build_radial([false, false]);
        // 半径 10 の点は s=1 で白。
        let c = s.shade_at(10.0, 0.0).unwrap();
        assert_eq!(c, [255, 255, 255]);
    }

    #[test]
    fn radial_outside_no_extend_is_none() {
        let s = build_radial([false, false]);
        assert!(s.shade_at(20.0, 0.0).is_none());
    }

    #[test]
    fn radial_outside_extend_clamps() {
        let s = build_radial([false, true]);
        // 半径 20 → 外周色に固定。
        assert_eq!(s.shade_at(20.0, 0.0), Some([255, 255, 255]));
    }

    #[test]
    fn unsupported_type_returns_none() {
        let doc = Document::new();
        let mut sd = Dictionary::new();
        sd.set("ShadingType", Object::Integer(4)); // メッシュ未対応
        sd.set("ColorSpace", Object::name("DeviceRGB"));
        let s = Shading::parse(&doc, &Object::Dictionary(sd), &Dictionary::new());
        assert!(matches!(s.kind, ShadingKind::Unsupported));
        assert!(s.shade_at(0.0, 0.0).is_none());
    }

    #[test]
    fn bbox_restricts_paintable_area() {
        let doc = Document::new();
        let mut sd = Dictionary::new();
        sd.set("ShadingType", Object::Integer(2));
        sd.set("ColorSpace", Object::name("DeviceRGB"));
        sd.set(
            "Coords",
            Object::Array(vec![
                Object::Real(0.0),
                Object::Real(0.0),
                Object::Real(10.0),
                Object::Real(0.0),
            ]),
        );
        sd.set(
            "Function",
            Object::Dictionary(type2_function([0.0, 0.0, 0.0], [1.0, 0.0, 0.0])),
        );
        sd.set(
            "BBox",
            Object::Array(vec![
                Object::Real(0.0),
                Object::Real(0.0),
                Object::Real(10.0),
                Object::Real(5.0),
            ]),
        );
        sd.set(
            "Extend",
            Object::Array(vec![Object::Boolean(true), Object::Boolean(true)]),
        );
        let s = Shading::parse(&doc, &Object::Dictionary(sd), &Dictionary::new());
        // BBox 内: 描かれる。
        assert!(s.shade_at(5.0, 2.0).is_some());
        // BBox の y 外: None。
        assert!(s.shade_at(5.0, 10.0).is_none());
    }
}
