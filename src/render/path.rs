//! パス表現・アフィン変換・ベジェ平坦化。
//!
//! PDF のコンテントストリームが組み立てるパス（直線と 3 次ベジェ曲線の列）を
//! 保持し、デバイス空間への変換と折れ線への平坦化を担う。
//!
//! ## 設計方針
//!
//! - 座標は `f64`。NaN・無限大などの不正値は「そのセグメントを無視」して
//!   後段（ラスタライザ）が暴走しないようにする（panic しない）。
//! - 平坦化は再帰を使わず、ベジェの平坦さに応じて分割数を見積もる
//!   反復ループで行う。極端な座標でもスタックオーバーフロー・無限ループしない。

/// 2 次元の点（デバイス／ユーザー空間共通、単位は文脈依存）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    /// 座標を指定して点を作る。
    pub fn new(x: f64, y: f64) -> Point {
        Point { x, y }
    }

    /// x・y がともに有限（NaN・無限大でない）か。
    fn is_finite(&self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

/// アフィン変換行列。PDF の `[a b c d e f]`（`cm`/`Tm`）と同じ意味で、
///
/// ```text
/// x' = a·x + c·y + e
/// y' = b·x + d·y + f
/// ```
///
/// として点を写す。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Matrix {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub e: f64,
    pub f: f64,
}

impl Matrix {
    /// 恒等変換。
    pub fn identity() -> Matrix {
        Matrix {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: 0.0,
            f: 0.0,
        }
    }

    /// 拡大・縮小（原点中心）。
    pub fn scale(sx: f64, sy: f64) -> Matrix {
        Matrix {
            a: sx,
            b: 0.0,
            c: 0.0,
            d: sy,
            e: 0.0,
            f: 0.0,
        }
    }

    /// 平行移動。
    pub fn translate(tx: f64, ty: f64) -> Matrix {
        Matrix {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: tx,
            f: ty,
        }
    }

    /// 合成: **まず `self` を適用し、続けて `other` を適用**する変換を返す。
    ///
    /// すなわち `self.then(other).apply(p) == other.apply(self.apply(p))`。
    /// PDF の `cm` は新しい行列を CTM の **左** から掛ける（`CTM' = cm × CTM`）
    /// のと同じ重ね順で、`cm_matrix.then(&ctm)` のように使う。
    pub fn then(&self, other: &Matrix) -> Matrix {
        // other ∘ self（点には self を先に作用させる）。
        Matrix {
            a: self.a * other.a + self.b * other.c,
            b: self.a * other.b + self.b * other.d,
            c: self.c * other.a + self.d * other.c,
            d: self.c * other.b + self.d * other.d,
            e: self.e * other.a + self.f * other.c + other.e,
            f: self.e * other.b + self.f * other.d + other.f,
        }
    }

    /// 点を変換する。
    pub fn apply(&self, p: Point) -> Point {
        Point {
            x: self.a * p.x + self.c * p.y + self.e,
            y: self.b * p.x + self.d * p.y + self.f,
        }
    }

    /// 逆行列を返す。退化（行列式 0 や非有限）した場合は `None`。
    ///
    /// `image.rs` のサンプリングと同様、パターン・シェーディングが
    /// デバイス座標からパターン空間へ写像する逆変換用。
    pub fn inverse(&self) -> Option<Matrix> {
        let det = self.a * self.d - self.b * self.c;
        if !det.is_finite() || det == 0.0 {
            return None;
        }
        let inv_det = 1.0 / det;
        let a = self.d * inv_det;
        let b = -self.b * inv_det;
        let c = -self.c * inv_det;
        let d = self.a * inv_det;
        let e = (self.c * self.f - self.d * self.e) * inv_det;
        let f = (self.b * self.e - self.a * self.f) * inv_det;
        if !(a.is_finite()
            && b.is_finite()
            && c.is_finite()
            && d.is_finite()
            && e.is_finite()
            && f.is_finite())
        {
            return None;
        }
        Some(Matrix { a, b, c, d, e, f })
    }

    /// 平均的な拡大率の概算。線形部分の行列式の平方根
    /// （= 面積拡大率の平方根）で、平坦化トレランスをユーザー空間へ
    /// 換算する用途に使う。
    ///
    /// 退化（行列式 0 や非有限）した場合は 1.0 を返す（誤差ゼロ除算回避）。
    pub fn approx_scale(&self) -> f64 {
        let det = (self.a * self.d - self.b * self.c).abs();
        let s = det.sqrt();
        if s.is_finite() && s > 0.0 {
            s
        } else {
            1.0
        }
    }
}

/// パスを構成する 1 セグメント。座標はパスが属する空間（通常ユーザー空間）。
#[derive(Debug, Clone, Copy, PartialEq)]
enum Segment {
    /// 新しいサブパスを開始（現在点を移動）。
    MoveTo(Point),
    /// 現在点から直線を引く。
    LineTo(Point),
    /// 現在点から 3 次ベジェ曲線を引く（制御点 2 つ + 終点）。
    CurveTo(Point, Point, Point),
    /// 現在のサブパスを始点へ閉じる。
    Close,
}

/// 複数のサブパスから成るパス。
///
/// セグメント列をそのまま保持する。`MoveTo` が新しいサブパスの開始を表し、
/// `Close` がサブパスを閉じる。平坦化（[`Path::flatten`]）で折れ線に変換する。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Path {
    segments: Vec<Segment>,
}

impl Path {
    /// 空のパスを作る。
    pub fn new() -> Path {
        Path {
            segments: Vec::new(),
        }
    }

    /// 新しいサブパスを始める（現在点を `(x, y)` へ移す）。
    ///
    /// 非有限な座標は無視する。
    pub fn move_to(&mut self, x: f64, y: f64) {
        let p = Point::new(x, y);
        if p.is_finite() {
            self.segments.push(Segment::MoveTo(p));
        }
    }

    /// 現在点から `(x, y)` へ直線を引く。非有限な座標は無視する。
    pub fn line_to(&mut self, x: f64, y: f64) {
        let p = Point::new(x, y);
        if p.is_finite() {
            self.segments.push(Segment::LineTo(p));
        }
    }

    /// 現在点から制御点 `(x1,y1)`・`(x2,y2)`・終点 `(x3,y3)` の
    /// 3 次ベジェ曲線を引く。いずれかの座標が非有限なら無視する。
    #[allow(clippy::too_many_arguments)]
    pub fn curve_to(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, x3: f64, y3: f64) {
        let c1 = Point::new(x1, y1);
        let c2 = Point::new(x2, y2);
        let p = Point::new(x3, y3);
        if c1.is_finite() && c2.is_finite() && p.is_finite() {
            self.segments.push(Segment::CurveTo(c1, c2, p));
        }
    }

    /// 現在のサブパスを閉じる。
    pub fn close(&mut self) {
        self.segments.push(Segment::Close);
    }

    /// セグメントを 1 つも持たないか。
    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    /// 行列 `m` で全座標を変換した新しいパスを返す。
    ///
    /// 変換後に非有限になったセグメントは取り除く（後段の安全のため）。
    pub fn transform(&self, m: &Matrix) -> Path {
        let mut out = Path::new();
        for seg in &self.segments {
            match *seg {
                Segment::MoveTo(p) => {
                    let q = m.apply(p);
                    if q.is_finite() {
                        out.segments.push(Segment::MoveTo(q));
                    }
                }
                Segment::LineTo(p) => {
                    let q = m.apply(p);
                    if q.is_finite() {
                        out.segments.push(Segment::LineTo(q));
                    }
                }
                Segment::CurveTo(c1, c2, p) => {
                    let q1 = m.apply(c1);
                    let q2 = m.apply(c2);
                    let q = m.apply(p);
                    if q1.is_finite() && q2.is_finite() && q.is_finite() {
                        out.segments.push(Segment::CurveTo(q1, q2, q));
                    }
                }
                Segment::Close => out.segments.push(Segment::Close),
            }
        }
        out
    }

    /// 各サブパスを折れ線（点列）へ平坦化する。
    ///
    /// `tolerance` はこのパスが属する空間での許容誤差（ベジェを直線で
    /// 近似するときの最大ずれ）。`fill`/`stroke` では通常デバイス空間の
    /// ピクセル単位で 0.25 程度を渡す。
    ///
    /// 戻り値は閉じているかに関わらず実際に通過する頂点の列。`Close` は
    /// 始点をもう一度追加して表現する（ラスタライザ側で自動クローズもする）。
    /// 空のサブパス（点 1 つだけ等）も呼び出し側が判定できるよう保持する。
    pub fn flatten(&self, tolerance: f64) -> Vec<Vec<Point>> {
        // トレランスは正の有限値にクランプ（0 や非有限だと分割数が暴れる）。
        let tol = if tolerance.is_finite() && tolerance > 0.0 {
            tolerance
        } else {
            0.25
        };

        let mut polylines: Vec<Vec<Point>> = Vec::new();
        let mut current: Vec<Point> = Vec::new();
        // 各サブパスの始点（Close で戻る先）。
        let mut start = Point::new(0.0, 0.0);
        let mut have_current = false;

        for seg in &self.segments {
            match *seg {
                Segment::MoveTo(p) => {
                    if have_current && !current.is_empty() {
                        polylines.push(std::mem::take(&mut current));
                    } else {
                        current.clear();
                    }
                    current.push(p);
                    start = p;
                    have_current = true;
                }
                Segment::LineTo(p) => {
                    if !have_current {
                        // 暗黙の MoveTo として扱う。
                        current.push(p);
                        start = p;
                        have_current = true;
                    } else {
                        current.push(p);
                    }
                }
                Segment::CurveTo(c1, c2, p) => {
                    let from = current.last().copied().unwrap_or(start);
                    if !have_current {
                        current.push(from);
                        start = from;
                        have_current = true;
                    }
                    flatten_cubic(from, c1, c2, p, tol, &mut current);
                }
                Segment::Close => {
                    if have_current && !current.is_empty() {
                        // 始点へ戻る（重複頂点は許容、ラスタライザが吸収する）。
                        current.push(start);
                        polylines.push(std::mem::take(&mut current));
                        have_current = false;
                    }
                }
            }
        }
        if have_current && !current.is_empty() {
            polylines.push(current);
        }
        polylines
    }
}

/// 3 次ベジェを `tol` 以内の折れ線に分割し `out` へ追加する（始点は追加済み前提、
/// 終点を含む中間・終端の頂点だけを足す）。
///
/// 分割数を曲線の「平坦さ」から見積もって固定ステップで評価する。再帰を
/// 使わないのでスタックオーバーフローせず、ステップ数も上限を設けて
/// 極端な座標でも有限時間で終わる。
fn flatten_cubic(p0: Point, p1: Point, p2: Point, p3: Point, tol: f64, out: &mut Vec<Point>) {
    // 制御点が始点・終点から外れる量（=曲がり具合）でステップ数を決める。
    // 制御多角形と弦の距離の上界に基づく古典的な見積もり。
    let dx1 = p1.x - p0.x;
    let dy1 = p1.y - p0.y;
    let dx2 = p2.x - p3.x;
    let dy2 = p2.y - p3.y;
    // 制御点の始点/終点からのずれの最大成分（曲率の代用指標）。
    let dev = (dx1 * dx1 + dy1 * dy1)
        .max(dx2 * dx2 + dy2 * dy2)
        .max(0.0)
        .sqrt();

    if !dev.is_finite() || dev <= tol {
        // ほぼ直線、または異常値 → 弦 1 本で近似。
        if p3.is_finite() {
            out.push(p3);
        }
        return;
    }

    // ステップ数 n を dev/tol の平方根から見積もる（誤差 ~ dev/n²）。
    let est = (dev / tol).sqrt().ceil();
    // 上限を設けて暴走を防ぐ（巨大座標でも安全）。
    let n = est.clamp(1.0, 256.0) as u32;

    for i in 1..=n {
        let t = i as f64 / n as f64;
        let q = cubic_at(p0, p1, p2, p3, t);
        if q.is_finite() {
            out.push(q);
        }
    }
}

/// 3 次ベジェの媒介変数 `t`（0..=1）での点。
fn cubic_at(p0: Point, p1: Point, p2: Point, p3: Point, t: f64) -> Point {
    let u = 1.0 - t;
    let b0 = u * u * u;
    let b1 = 3.0 * u * u * t;
    let b2 = 3.0 * u * t * t;
    let b3 = t * t * t;
    Point {
        x: b0 * p0.x + b1 * p1.x + b2 * p2.x + b3 * p3.x,
        y: b0 * p0.y + b1 * p1.y + b2 * p2.y + b3 * p3.y,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() <= eps
    }

    #[test]
    fn matrix_identity_apply() {
        let m = Matrix::identity();
        let p = m.apply(Point::new(3.0, -7.0));
        assert_eq!(p, Point::new(3.0, -7.0));
    }

    #[test]
    fn matrix_scale_translate_apply() {
        let m = Matrix::scale(2.0, 3.0);
        assert_eq!(m.apply(Point::new(4.0, 5.0)), Point::new(8.0, 15.0));
        let t = Matrix::translate(10.0, -2.0);
        assert_eq!(t.apply(Point::new(4.0, 5.0)), Point::new(14.0, 3.0));
    }

    #[test]
    fn matrix_then_order() {
        // まず 2 倍に拡大し、その後 (10, 0) 平行移動。
        let scale = Matrix::scale(2.0, 2.0);
        let trans = Matrix::translate(10.0, 0.0);
        let m = scale.then(&trans);
        // (1, 1) → 拡大で (2, 2) → 移動で (12, 2)
        assert_eq!(m.apply(Point::new(1.0, 1.0)), Point::new(12.0, 2.0));
        // then の定義: other.apply(self.apply(p)) と一致
        let p = Point::new(3.0, 4.0);
        let expected = trans.apply(scale.apply(p));
        assert_eq!(m.apply(p), expected);

        // 逆順は別の結果（移動してから拡大）。
        let m2 = trans.then(&scale);
        // (1, 1) → 移動で (11, 1) → 拡大で (22, 2)
        assert_eq!(m2.apply(Point::new(1.0, 1.0)), Point::new(22.0, 2.0));
    }

    #[test]
    fn matrix_approx_scale() {
        assert!(approx(Matrix::scale(4.0, 9.0).approx_scale(), 6.0, 1e-9));
        // 退化行列は 1.0 にフォールバック
        let degen = Matrix {
            a: 0.0,
            b: 0.0,
            c: 0.0,
            d: 0.0,
            e: 0.0,
            f: 0.0,
        };
        assert_eq!(degen.approx_scale(), 1.0);
    }

    #[test]
    fn flatten_straight_line_keeps_points() {
        let mut path = Path::new();
        path.move_to(0.0, 0.0);
        path.line_to(10.0, 0.0);
        path.line_to(10.0, 5.0);
        let polys = path.flatten(0.25);
        assert_eq!(polys.len(), 1);
        assert_eq!(
            polys[0],
            vec![
                Point::new(0.0, 0.0),
                Point::new(10.0, 0.0),
                Point::new(10.0, 5.0),
            ]
        );
    }

    #[test]
    fn flatten_close_returns_to_start() {
        let mut path = Path::new();
        path.move_to(0.0, 0.0);
        path.line_to(10.0, 0.0);
        path.line_to(10.0, 10.0);
        path.close();
        let polys = path.flatten(0.25);
        assert_eq!(polys.len(), 1);
        let pl = &polys[0];
        assert_eq!(pl.first(), Some(&Point::new(0.0, 0.0)));
        assert_eq!(pl.last(), Some(&Point::new(0.0, 0.0)));
    }

    #[test]
    fn flatten_cubic_midpoint_matches_analytic() {
        // 始点 (0,0)、制御点 (0,1)(1,1)、終点 (1,0) の対称な弧。
        // t=0.5 の解析値: x = 0.5, y = 0.75。
        let mut path = Path::new();
        path.move_to(0.0, 0.0);
        path.curve_to(0.0, 1.0, 1.0, 1.0, 1.0, 0.0);
        let polys = path.flatten(0.001);
        let pl = &polys[0];
        // 折れ線上で x≈0.5 に最も近い点を探し、y が 0.75 付近か確認。
        let mut best = (f64::INFINITY, 0.0);
        for p in pl {
            let d = (p.x - 0.5).abs();
            if d < best.0 {
                best = (d, p.y);
            }
        }
        assert!(approx(best.1, 0.75, 0.01), "y={} ではない", best.1);
        // 始点と終点が含まれる。
        assert_eq!(pl.first(), Some(&Point::new(0.0, 0.0)));
        assert_eq!(pl.last(), Some(&Point::new(1.0, 0.0)));
    }

    #[test]
    fn flatten_tolerance_controls_subdivision() {
        let mut path = Path::new();
        path.move_to(0.0, 0.0);
        path.curve_to(0.0, 100.0, 100.0, 100.0, 100.0, 0.0);
        let coarse = path.flatten(10.0)[0].len();
        let fine = path.flatten(0.1)[0].len();
        assert!(fine > coarse, "fine={fine} coarse={coarse}");
    }

    #[test]
    fn nan_segments_ignored_no_panic() {
        let mut path = Path::new();
        path.move_to(0.0, 0.0);
        path.line_to(f64::NAN, 1.0);
        path.line_to(1.0, f64::INFINITY);
        path.curve_to(0.0, 0.0, f64::NAN, 0.0, 1.0, 1.0);
        path.line_to(5.0, 5.0);
        // 不正セグメントは捨てられ、有効な点だけ残る。
        let polys = path.flatten(0.25);
        assert_eq!(polys.len(), 1);
        assert_eq!(polys[0], vec![Point::new(0.0, 0.0), Point::new(5.0, 5.0)]);
    }

    #[test]
    fn empty_path_flatten_no_panic() {
        let path = Path::new();
        assert!(path.is_empty());
        assert!(path.flatten(0.25).is_empty());
    }

    #[test]
    fn transform_applies_matrix() {
        let mut path = Path::new();
        path.move_to(1.0, 1.0);
        path.line_to(2.0, 2.0);
        let m = Matrix::scale(10.0, 10.0);
        let t = path.transform(&m);
        let polys = t.flatten(0.25);
        assert_eq!(
            polys[0],
            vec![Point::new(10.0, 10.0), Point::new(20.0, 20.0)]
        );
    }

    #[test]
    fn huge_coordinates_terminate() {
        // 巨大座標でもステップ上限により有限時間で終わる。
        let mut path = Path::new();
        path.move_to(0.0, 0.0);
        path.curve_to(1e18, 1e18, -1e18, 1e18, 1e9, 0.0);
        let polys = path.flatten(0.25);
        assert!(!polys[0].is_empty());
        assert!(polys[0].len() <= 258);
    }
}
