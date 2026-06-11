//! スキャンライン塗り（アンチエイリアス）・クリップマスク・ストローク生成。
//!
//! [`Path`] を平坦化した折れ線をスキャンラインで走査し、巻き数規則
//! （[`FillRule`]）に従って各ピクセルの被覆率を求めて [`Pixmap`] に合成する。
//!
//! ## ラスタライズ方式
//!
//! 縦方向に複数のサブスキャンライン（既定 4 本）を引き、各サブスキャンと
//! エッジの交点を求めて巻き数を計算。区間内のピクセルへは**水平方向の解析的な
//! 被覆率**（交点がピクセルを部分的に覆う割合を厳密に計算）を、縦方向は
//! サブスキャン本数で平均してアンチエイリアスする。スーパーサンプリングの
//! 一様性（縦）と解析積分（横）を組み合わせ、メモリは 1 行分のカバレッジ
//! バッファのみで済む。
//!
//! ## 耐故障性
//!
//! 巨大・負・非有限な座標はキャンバス範囲へクランプしてから処理し、
//! 確保するバッファはキャンバス幅に限定する。空パスや範囲外パスでも
//! panic しない。

use super::path::{Path, Point};
use super::pixmap::Pixmap;

/// 塗り規則（巻き数の解釈）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillRule {
    /// 非ゼロ巻き数規則（巻き数 ≠ 0 を内側とする）。
    NonZero,
    /// 偶奇規則（巻き数の偶奇で内外を決める）。
    EvenOdd,
}

/// 線端の形状（PDF の `J` 演算子。0/1/2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineCap {
    /// 端点で切り落とす（0）。
    Butt,
    /// 半円を付ける（1）。
    Round,
    /// 線幅の半分だけ延長した矩形（2）。
    Square,
}

/// 線の接合形状（PDF の `j` 演算子。0/1/2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineJoin {
    /// 尖り接合（マイターリミット超過時は Bevel にフォールバック）（0）。
    Miter,
    /// 円弧で接合（1）。
    Round,
    /// 面取り接合（2）。
    Bevel,
}

/// ストロークのスタイル（PDF のグラフィックス状態に対応）。
#[derive(Debug, Clone)]
pub struct StrokeStyle {
    /// 線幅（ユーザー空間）。0 は最細としてごく細い幅にクランプする。
    pub width: f64,
    /// 端の形状。
    pub cap: LineCap,
    /// 接合の形状。
    pub join: LineJoin,
    /// マイターリミット（既定 10.0）。
    pub miter_limit: f64,
    /// ダッシュ配列（空 = 実線）。
    pub dash: Vec<f64>,
    /// ダッシュの開始位相。
    pub dash_phase: f64,
}

impl Default for StrokeStyle {
    fn default() -> StrokeStyle {
        StrokeStyle {
            width: 1.0,
            cap: LineCap::Butt,
            join: LineJoin::Miter,
            miter_limit: 10.0,
            dash: Vec::new(),
            dash_phase: 0.0,
        }
    }
}

/// クリップ用のカバレッジマスク（ピクセルごとの被覆率 0–255）。
#[derive(Debug, Clone)]
pub struct Mask {
    width: u32,
    height: u32,
    /// 行優先の被覆率。長さは `width * height`。
    data: Vec<u8>,
}

impl Mask {
    /// 全面 255（クリップなし相当）のマスク。
    pub fn full(width: u32, height: u32) -> Mask {
        let width = width.max(1);
        let height = height.max(1);
        Mask {
            width,
            height,
            data: vec![255u8; (width as usize) * (height as usize)],
        }
    }

    /// パス（デバイス空間）をラスタライズして被覆率マスクを作る。
    pub fn from_path(path: &Path, rule: FillRule, width: u32, height: u32) -> Mask {
        let width = width.max(1);
        let height = height.max(1);
        let mut data = vec![0u8; (width as usize) * (height as usize)];
        rasterize(path, rule, width, height, |x, y, cov| {
            let idx = (y as usize) * (width as usize) + (x as usize);
            if let Some(slot) = data.get_mut(idx) {
                // 同一ピクセルへの最大被覆を採用（重複区間の飽和）。
                *slot = (*slot).max(cov);
            }
        });
        Mask {
            width,
            height,
            data,
        }
    }

    /// 別マスクとの交差（255 正規化の乗算）で自身を更新する（入れ子クリップ用）。
    ///
    /// サイズが異なる場合は両者の共通範囲のみ掛け合わせ、自身の範囲外は 0 にする。
    pub fn intersect(&mut self, other: &Mask) {
        for y in 0..self.height {
            for x in 0..self.width {
                let idx = (y as usize) * (self.width as usize) + (x as usize);
                let o = other.coverage(x, y) as u32;
                if let Some(slot) = self.data.get_mut(idx) {
                    let s = *slot as u32;
                    *slot = ((s * o + 127) / 255) as u8;
                }
            }
        }
    }

    /// ピクセルの被覆率。範囲外は 0。
    pub fn coverage(&self, x: u32, y: u32) -> u8 {
        if x < self.width && y < self.height {
            self.data
                .get((y as usize) * (self.width as usize) + (x as usize))
                .copied()
                .unwrap_or(0)
        } else {
            0
        }
    }
}

/// パス（**デバイス空間**）を塗る。
///
/// `rule` は巻き数規則、`rgb` は塗り色、`alpha` は塗り全体の不透明度
/// （ピクセル被覆率に乗算）。`clip` が `Some` ならそのマスクの被覆率も乗算する。
pub fn fill_path(
    pm: &mut Pixmap,
    path: &Path,
    rule: FillRule,
    rgb: [u8; 3],
    alpha: u8,
    clip: Option<&Mask>,
) {
    if alpha == 0 {
        return;
    }
    let width = pm.width();
    let height = pm.height();
    let base = alpha as u32;
    rasterize(path, rule, width, height, |x, y, cov| {
        let mut a = (cov as u32 * base + 127) / 255;
        if let Some(mask) = clip {
            a = (a * mask.coverage(x, y) as u32 + 127) / 255;
        }
        if a > 0 {
            pm.blend_pixel(x, y, rgb, a as u8);
        }
    });
}

/// 縦方向のサブスキャンライン本数（スーパーサンプリング係数）。
const SUBSAMPLES: u32 = 4;

/// パスをラスタライズし、被覆率が 0 でないピクセルごとに `emit(x, y, cov)` を呼ぶ。
///
/// `cov` は 0–255 の被覆率。スキャンラインを縦に [`SUBSAMPLES`] 分割し、各サブ
/// スキャンで巻き数規則に従う区間を求め、水平方向の解析的被覆を 1 行分の
/// バッファに加算してから出力する。
fn rasterize<F: FnMut(u32, u32, u8)>(
    path: &Path,
    rule: FillRule,
    width: u32,
    height: u32,
    mut emit: F,
) {
    // 平坦化（デバイス空間なのでトレランスはピクセル単位）。
    let polylines = path.flatten(0.25);
    if polylines.is_empty() {
        return;
    }

    // エッジ列を作る。開いたサブパスは自動クローズ（PDF §8.5.3.3）。
    let mut edges: Vec<Edge> = Vec::new();
    let mut min_y = f64::INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for poly in &polylines {
        if poly.len() < 2 {
            continue;
        }
        let n = poly.len();
        for i in 0..n {
            let a = poly[i];
            // 最後の頂点は始点へ繋いで閉じる。
            let b = if i + 1 < n { poly[i + 1] } else { poly[0] };
            if let Some(e) = Edge::new(a, b) {
                min_y = min_y.min(e.y_top);
                max_y = max_y.max(e.y_bottom);
                edges.push(e);
            }
        }
    }
    if edges.is_empty() {
        return;
    }

    // 走査する行範囲をキャンバスへクランプ。
    let y_start = (min_y.floor().max(0.0)) as i64;
    let y_end = (max_y.ceil().min(height as f64)) as i64;
    if y_start >= y_end {
        return;
    }

    let w = width as usize;
    // 1 行分のカバレッジ累積（サブスキャン合計、最大 255*SUBSAMPLES）。
    let mut cover = vec![0u32; w];
    // サブスキャンの交点 (x, winding_dir) を貯めるバッファ。
    let mut xs: Vec<(f64, i32)> = Vec::new();

    let sub_step = 1.0 / SUBSAMPLES as f64;
    let max_per_pixel = 255u32; // 各サブスキャンの水平被覆の最大寄与（正規化前）

    for py in y_start..y_end {
        for c in cover.iter_mut() {
            *c = 0;
        }
        for s in 0..SUBSAMPLES {
            let sy = py as f64 + (s as f64 + 0.5) * sub_step;
            xs.clear();
            for e in &edges {
                if sy >= e.y_top && sy < e.y_bottom {
                    let x = e.x_at(sy);
                    xs.push((x, e.winding));
                }
            }
            if xs.len() < 2 {
                continue;
            }
            xs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

            // 交点間の区間で巻き数規則を判定し、内側区間に水平被覆を加算。
            let mut winding = 0i32;
            for pair in 0..xs.len().saturating_sub(1) {
                let (x0, dir) = xs[pair];
                winding += dir;
                let inside = match rule {
                    FillRule::NonZero => winding != 0,
                    FillRule::EvenOdd => (winding & 1) != 0,
                };
                if !inside {
                    continue;
                }
                let x1 = xs[pair + 1].0;
                add_span(&mut cover, x0, x1, w, max_per_pixel);
            }
        }
        // 行を出力（サブスキャン平均）。
        for (x, &c) in cover.iter().enumerate() {
            if c == 0 {
                continue;
            }
            // c は最大 255*SUBSAMPLES。SUBSAMPLES で割って 0–255 へ。
            let cov = (c / SUBSAMPLES).min(255) as u8;
            if cov > 0 {
                emit(x as u32, py as u32, cov);
            }
        }
    }
}

/// 区間 `[x0, x1)`（同一サブスキャン上）の水平被覆を `cover` に加算する。
///
/// 端のピクセルは交点が内部にある分だけ部分被覆（解析的）、内部は満被覆。
/// `unit` は満被覆 1 ピクセル分の寄与（= 255）。
fn add_span(cover: &mut [u32], x0: f64, x1: f64, w: usize, unit: u32) {
    if !(x0.is_finite() && x1.is_finite()) || x1 <= x0 {
        return;
    }
    // キャンバス幅へクランプ。
    let xa = x0.max(0.0);
    let xb = x1.min(w as f64);
    if xb <= xa {
        return;
    }
    let ixa = xa.floor() as usize;
    let ixb = xb.floor() as usize;

    if ixa == ixb {
        // 単一ピクセル内の区間。
        if let Some(slot) = cover.get_mut(ixa) {
            let frac = xb - xa; // 0..1
            *slot += (frac * unit as f64) as u32;
        }
        return;
    }

    // 左端ピクセルの部分被覆。
    if let Some(slot) = cover.get_mut(ixa) {
        let frac = (ixa as f64 + 1.0) - xa; // xa からピクセル右端まで
        *slot += (frac * unit as f64) as u32;
    }
    // 中間ピクセルは満被覆。
    let mid_end = ixb.min(w);
    for slot in cover.iter_mut().take(mid_end).skip(ixa + 1) {
        *slot += unit;
    }
    // 右端ピクセルの部分被覆（ixb がバッファ内なら）。
    if ixb < w {
        if let Some(slot) = cover.get_mut(ixb) {
            let frac = xb - ixb as f64;
            *slot += (frac * unit as f64) as u32;
        }
    }
}

/// スキャンライン用のエッジ（単調な線分。水平線は除外）。
struct Edge {
    y_top: f64,
    y_bottom: f64,
    /// y_top での x。
    x_at_top: f64,
    /// dx/dy（y を 1 進めたときの x 増分）。
    dxdy: f64,
    /// 巻き方向（上向き +1 / 下向き -1。元の向きを保持）。
    winding: i32,
}

impl Edge {
    /// 線分 a→b からエッジを作る。水平・非有限・極小は `None`。
    fn new(a: Point, b: Point) -> Option<Edge> {
        if !(a.x.is_finite() && a.y.is_finite() && b.x.is_finite() && b.y.is_finite()) {
            return None;
        }
        if a.y == b.y {
            return None; // 水平線はスキャンラインに寄与しない。
        }
        // 上が小さい y になるよう正規化しつつ、巻き方向を記録。
        let (top, bottom, winding) = if a.y < b.y { (a, b, 1) } else { (b, a, -1) };
        let dy = bottom.y - top.y;
        if dy <= 0.0 || !dy.is_finite() {
            return None;
        }
        let dxdy = (bottom.x - top.x) / dy;
        if !dxdy.is_finite() {
            return None;
        }
        Some(Edge {
            y_top: top.y,
            y_bottom: bottom.y,
            x_at_top: top.x,
            dxdy,
            winding,
        })
    }

    /// 走査 y での x 座標。
    fn x_at(&self, y: f64) -> f64 {
        self.x_at_top + (y - self.y_top) * self.dxdy
    }
}

// --- ストローク → 塗りパス変換 -------------------------------------------

/// ストロークをアウトライン（塗りパス）へ変換する。
///
/// `path` は**ユーザー空間**のまま受け取り、線幅 `style.width` もユーザー空間。
/// 呼び出し側が戻り値を CTM でデバイス空間へ変換してから
/// `fill_path(NonZero)` する。`flatten_tolerance` はユーザー空間での平坦化誤差
/// （呼び出し側が CTM の [`Matrix::approx_scale`] で割って渡す）。
///
/// 平坦化した折れ線の各セグメントを幅 `width` の矩形に展開し、接合と端を
/// `style` に従って補う。結果は重なり合う凸ポリゴンの集合になるので、
/// 塗るときは必ず [`FillRule::NonZero`] を使うこと。
pub fn stroke_to_path(path: &Path, style: &StrokeStyle, flatten_tolerance: f64) -> Path {
    let tol = if flatten_tolerance.is_finite() && flatten_tolerance > 0.0 {
        flatten_tolerance
    } else {
        0.25
    };
    // 線幅は半幅で扱う。0 や非有限はごく細い線へクランプ。
    let width = if style.width.is_finite() && style.width > 0.0 {
        style.width
    } else {
        // 最細線: トレランス相当の細い幅。
        tol.max(1e-3)
    };
    let half = width / 2.0;
    let miter_limit = if style.miter_limit.is_finite() && style.miter_limit >= 1.0 {
        style.miter_limit
    } else {
        10.0
    };

    let mut out = Path::new();
    let polylines = path.flatten(tol);

    for poly in &polylines {
        // 連続する重複点を除去。
        let pts = dedup_points(poly);
        let closed = is_closed_polyline(&pts);
        // 閉路は末尾の重複始点を落とす。
        let core: &[Point] = if closed && pts.len() >= 2 {
            &pts[..pts.len() - 1]
        } else {
            &pts
        };

        if core.is_empty() {
            continue;
        }
        if core.len() == 1 {
            // 点だけのサブパス: Round/Square キャップなら点マーカー。
            stroke_dot(&mut out, core[0], half, style.cap, tol);
            continue;
        }

        // ダッシュ分解（実線ならそのまま 1 本）。
        let dashes = apply_dash(core, closed, &style.dash, style.dash_phase);
        for (seg_pts, seg_closed) in dashes {
            stroke_polyline(
                &mut out,
                &seg_pts,
                seg_closed,
                half,
                style,
                miter_limit,
                tol,
            );
        }
    }

    out
}

/// 1 本の折れ線（ダッシュ適用後の連続区間）をアウトライン化して `out` に足す。
fn stroke_polyline(
    out: &mut Path,
    pts: &[Point],
    closed: bool,
    half: f64,
    style: &StrokeStyle,
    miter_limit: f64,
    tol: f64,
) {
    if pts.len() < 2 {
        if pts.len() == 1 {
            stroke_dot(out, pts[0], half, style.cap, tol);
        }
        return;
    }

    let n = pts.len();
    // 各セグメントを矩形として出す。
    let seg_count = if closed { n } else { n - 1 };
    for i in 0..seg_count {
        let a = pts[i];
        let b = pts[(i + 1) % n];
        emit_segment_quad(out, a, b, half);
    }

    // 接合（内側の頂点ごと）。
    let join_count = if closed { n } else { n - 1 };
    for i in 0..join_count {
        // 頂点 idx での接合: 前セグメント end と次セグメント start。
        let prev_i = if i == 0 {
            if closed {
                n - 1
            } else {
                continue;
            }
        } else {
            i - 1
        };
        let v = pts[i];
        let a = pts[prev_i];
        let b = pts[(i + 1) % n];
        emit_join(out, a, v, b, half, style.join, miter_limit, tol);
    }

    // 端キャップ（開いた線のみ）。
    if !closed {
        // 始点キャップ: pts[0]、方向は pts[1]→pts[0] の外向き。
        emit_cap(out, pts[1], pts[0], half, style.cap, tol);
        // 終点キャップ: pts[n-1]、方向は pts[n-2]→pts[n-1]。
        emit_cap(out, pts[n - 2], pts[n - 1], half, style.cap, tol);
    }
}

/// 線分 a→b を幅 2·half の矩形としてパスに追加する。
fn emit_segment_quad(out: &mut Path, a: Point, b: Point, half: f64) {
    let (nx, ny) = match unit_normal(a, b) {
        Some(n) => n,
        None => return,
    };
    let ox = nx * half;
    let oy = ny * half;
    out.move_to(a.x + ox, a.y + oy);
    out.line_to(b.x + ox, b.y + oy);
    out.line_to(b.x - ox, b.y - oy);
    out.line_to(a.x - ox, a.y - oy);
    out.close();
}

/// 頂点 v での接合（a→v と v→b の間）を `join` 種別に従って追加する。
#[allow(clippy::too_many_arguments)]
fn emit_join(
    out: &mut Path,
    a: Point,
    v: Point,
    b: Point,
    half: f64,
    join: LineJoin,
    miter_limit: f64,
    tol: f64,
) {
    let n0 = match unit_normal(a, v) {
        Some(n) => n,
        None => return,
    };
    let n1 = match unit_normal(v, b) {
        Some(n) => n,
        None => return,
    };
    // 接合は外側の隙間を埋める三角形/円弧。両側のどちらが外かは曲がる向き次第。
    // ここでは両側に向きの一致する側のみ三角形を出す Bevel を基本に、
    // Round は扇形、Miter は尖り点を加える。
    let dir0 = direction(a, v);
    let dir1 = direction(v, b);
    let (d0x, d0y) = match dir0 {
        Some(d) => d,
        None => return,
    };
    let (d1x, d1y) = match dir1 {
        Some(d) => d,
        None => return,
    };
    // 外積で曲がる向きを判定（正なら左折、負なら右折）。
    let cross = d0x * d1y - d0y * d1x;
    if cross.abs() < 1e-12 {
        return; // ほぼ直線、接合不要。
    }
    // 外側の法線符号（曲がる向きと逆側が外）。
    let sign = if cross > 0.0 { -1.0 } else { 1.0 };
    let p0 = Point::new(v.x + n0.0 * half * sign, v.y + n0.1 * half * sign);
    let p1 = Point::new(v.x + n1.0 * half * sign, v.y + n1.1 * half * sign);

    match join {
        LineJoin::Bevel => {
            triangle(out, v, p0, p1);
        }
        LineJoin::Round => {
            round_join(out, v, p0, p1, half, tol);
        }
        LineJoin::Miter => {
            // マイター点 = 2 辺のオフセット線の交点。
            if let Some(m) = miter_point(p0, (d0x, d0y), p1, (d1x, d1y)) {
                // マイター長 / half ＝ 1/sin(θ/2)。limit 超過なら Bevel。
                let mdx = m.x - v.x;
                let mdy = m.y - v.y;
                let mlen = (mdx * mdx + mdy * mdy).sqrt();
                if mlen <= miter_limit * half {
                    triangle(out, v, p0, m);
                    triangle(out, v, m, p1);
                } else {
                    triangle(out, v, p0, p1);
                }
            } else {
                triangle(out, v, p0, p1);
            }
        }
    }
}

/// 2 本のオフセット辺（点 p と方向 d）の交点。平行なら `None`。
fn miter_point(p0: Point, d0: (f64, f64), p1: Point, d1: (f64, f64)) -> Option<Point> {
    let denom = d0.0 * d1.1 - d0.1 * d1.0;
    if denom.abs() < 1e-12 {
        return None;
    }
    // p0 + t·d0 = p1 + s·d1 を解く。
    let t = ((p1.x - p0.x) * d1.1 - (p1.y - p0.y) * d1.0) / denom;
    let m = Point::new(p0.x + t * d0.0, p0.y + t * d0.1);
    if m.x.is_finite() && m.y.is_finite() {
        Some(m)
    } else {
        None
    }
}

/// 端キャップを追加する。`from`→`to` が線の向き、`to` が端点。
fn emit_cap(out: &mut Path, from: Point, to: Point, half: f64, cap: LineCap, tol: f64) {
    let (dx, dy) = match direction(from, to) {
        Some(d) => d,
        None => return,
    };
    let (nx, ny) = (-dy, dx); // 左法線
    match cap {
        LineCap::Butt => {} // 何も足さない
        LineCap::Square => {
            // 端点を線方向へ half だけ延長した矩形。
            let e = Point::new(to.x + dx * half, to.y + dy * half);
            let p0 = Point::new(to.x + nx * half, to.y + ny * half);
            let p1 = Point::new(to.x - nx * half, to.y - ny * half);
            let q0 = Point::new(e.x + nx * half, e.y + ny * half);
            let q1 = Point::new(e.x - nx * half, e.y - ny * half);
            out.move_to(p0.x, p0.y);
            out.line_to(q0.x, q0.y);
            out.line_to(q1.x, q1.y);
            out.line_to(p1.x, p1.y);
            out.close();
        }
        LineCap::Round => {
            // 端点を中心に半径 half の半円。
            let p0 = Point::new(to.x + nx * half, to.y + ny * half);
            let p1 = Point::new(to.x - nx * half, to.y - ny * half);
            // 半円を弧で（外向き＝線方向側へ膨らむ）。
            arc_fan(out, to, p0, p1, half, (dx, dy), tol);
        }
    }
}

/// 点だけのサブパスのマーカー（Round=円、Square=正方形）。Butt は何もしない。
fn stroke_dot(out: &mut Path, c: Point, half: f64, cap: LineCap, tol: f64) {
    if !c.x.is_finite() || !c.y.is_finite() {
        return;
    }
    match cap {
        LineCap::Butt => {}
        LineCap::Square => {
            out.move_to(c.x - half, c.y - half);
            out.line_to(c.x + half, c.y - half);
            out.line_to(c.x + half, c.y + half);
            out.line_to(c.x - half, c.y + half);
            out.close();
        }
        LineCap::Round => {
            full_circle(out, c, half, tol);
        }
    }
}

/// 三角形 (a, b, c) を塗りパスとして追加。
fn triangle(out: &mut Path, a: Point, b: Point, c: Point) {
    if !(a.x.is_finite() && b.x.is_finite() && c.x.is_finite()) {
        return;
    }
    out.move_to(a.x, a.y);
    out.line_to(b.x, b.y);
    out.line_to(c.x, c.y);
    out.close();
}

/// 中心 v から p0→p1 へ向かう円弧の扇形を多角形近似で追加（Round join 用）。
fn round_join(out: &mut Path, v: Point, p0: Point, p1: Point, r: f64, tol: f64) {
    let a0 = (p0.y - v.y).atan2(p0.x - v.x);
    let mut a1 = (p1.y - v.y).atan2(p1.x - v.x);
    // 短い方の弧を選ぶ。
    let mut delta = a1 - a0;
    while delta > std::f64::consts::PI {
        delta -= 2.0 * std::f64::consts::PI;
    }
    while delta < -std::f64::consts::PI {
        delta += 2.0 * std::f64::consts::PI;
    }
    a1 = a0 + delta;
    let steps = arc_steps(r, delta.abs(), tol);
    let mut prev = p0;
    for i in 1..=steps {
        let t = i as f64 / steps as f64;
        let ang = a0 + delta * t;
        let q = Point::new(v.x + r * ang.cos(), v.y + r * ang.sin());
        triangle(out, v, prev, q);
        prev = q;
    }
    let _ = a1;
}

/// 端点 c を中心に p0→p1 を結ぶ半円（線方向 `dir` 側へ膨らむ）を扇形で追加。
fn arc_fan(out: &mut Path, c: Point, p0: Point, p1: Point, r: f64, dir: (f64, f64), tol: f64) {
    let a0 = (p0.y - c.y).atan2(p0.x - c.x);
    let a1 = (p1.y - c.y).atan2(p1.x - c.x);
    // 線方向 dir の角度の側を通る半円にする。
    let dir_ang = dir.1.atan2(dir.0);
    // a0 から a1 へ、dir_ang を経由する向きの delta（π or -π）を選ぶ。
    let mut delta = a1 - a0;
    while delta > std::f64::consts::PI {
        delta -= 2.0 * std::f64::consts::PI;
    }
    while delta < -std::f64::consts::PI {
        delta += 2.0 * std::f64::consts::PI;
    }
    // 中点角が dir_ang に近い向きを採用。
    let mid = a0 + delta / 2.0;
    if angle_diff(mid, dir_ang).abs() > std::f64::consts::FRAC_PI_2 {
        delta = if delta > 0.0 {
            delta - 2.0 * std::f64::consts::PI
        } else {
            delta + 2.0 * std::f64::consts::PI
        };
    }
    let steps = arc_steps(r, delta.abs(), tol);
    let mut prev = p0;
    for i in 1..=steps {
        let t = i as f64 / steps as f64;
        let ang = a0 + delta * t;
        let q = Point::new(c.x + r * ang.cos(), c.y + r * ang.sin());
        triangle(out, c, prev, q);
        prev = q;
    }
}

/// 完全な円を扇形（三角形列）で追加（Round dot / dash 端点用）。
fn full_circle(out: &mut Path, c: Point, r: f64, tol: f64) {
    let steps = arc_steps(r, 2.0 * std::f64::consts::PI, tol).max(8);
    let mut prev = Point::new(c.x + r, c.y);
    for i in 1..=steps {
        let ang = 2.0 * std::f64::consts::PI * (i as f64 / steps as f64);
        let q = Point::new(c.x + r * ang.cos(), c.y + r * ang.sin());
        triangle(out, c, prev, q);
        prev = q;
    }
}

/// 円弧の分割数（半径 r・中心角 angle・トレランス tol から）。
fn arc_steps(r: f64, angle: f64, tol: f64) -> u32 {
    if !(r.is_finite() && angle.is_finite()) || r <= 0.0 || angle <= 0.0 {
        return 1;
    }
    let tol = tol.max(1e-3);
    // 1 ステップあたりの最大角 ≈ 2·acos(1 - tol/r)。
    let ratio = (1.0 - tol / r).clamp(-1.0, 1.0);
    let max_angle = 2.0 * ratio.acos();
    if max_angle <= 1e-6 {
        return 64;
    }
    ((angle / max_angle).ceil() as u32).clamp(1, 256)
}

/// 2 角度の差を [-π, π] に正規化。
fn angle_diff(a: f64, b: f64) -> f64 {
    let mut d = a - b;
    while d > std::f64::consts::PI {
        d -= 2.0 * std::f64::consts::PI;
    }
    while d < -std::f64::consts::PI {
        d += 2.0 * std::f64::consts::PI;
    }
    d
}

/// 線分 a→b の単位法線（左法線）。長さ 0・非有限は `None`。
fn unit_normal(a: Point, b: Point) -> Option<(f64, f64)> {
    let d = direction(a, b)?;
    Some((-d.1, d.0))
}

/// 線分 a→b の単位方向ベクトル。長さ 0・非有限は `None`。
fn direction(a: Point, b: Point) -> Option<(f64, f64)> {
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    let len = (dx * dx + dy * dy).sqrt();
    if !len.is_finite() || len <= 1e-12 {
        return None;
    }
    Some((dx / len, dy / len))
}

/// 折れ線の連続重複点を除く。
fn dedup_points(poly: &[Point]) -> Vec<Point> {
    let mut out: Vec<Point> = Vec::with_capacity(poly.len());
    for &p in poly {
        if !p.x.is_finite() || !p.y.is_finite() {
            continue;
        }
        if let Some(&last) = out.last() {
            if (last.x - p.x).abs() < 1e-12 && (last.y - p.y).abs() < 1e-12 {
                continue;
            }
        }
        out.push(p);
    }
    out
}

/// 折れ線が閉じている（始点と終点が一致）か。
fn is_closed_polyline(pts: &[Point]) -> bool {
    if pts.len() < 3 {
        return false;
    }
    let a = pts[0];
    let b = pts[pts.len() - 1];
    (a.x - b.x).abs() < 1e-9 && (a.y - b.y).abs() < 1e-9
}

/// ダッシュ配列を折れ線へ適用し、(描画区間, 閉じているか) の列を返す。
///
/// 不正なダッシュ（空・全ゼロ・負・非有限）は実線として `[(全体, closed)]` を返す。
fn apply_dash(pts: &[Point], closed: bool, dash: &[f64], phase: f64) -> Vec<(Vec<Point>, bool)> {
    // 妥当性チェック。
    let valid = !dash.is_empty()
        && dash.iter().all(|d| d.is_finite() && *d >= 0.0)
        && dash.iter().any(|d| *d > 0.0);
    if !valid {
        return vec![(pts.to_vec(), closed)];
    }

    // 閉路はダッシュ適用のため始点を末尾に足して開いた線として扱う。
    let mut work: Vec<Point> = pts.to_vec();
    if closed {
        if let Some(&first) = pts.first() {
            work.push(first);
        }
    }

    // パターン総長（奇数長配列は 2 周で偶数化される PDF 仕様だが、
    // ここでは循環インデックスで実質同等に扱う）。
    let pattern: Vec<f64> = dash.iter().map(|d| d.max(0.0)).collect();
    let total: f64 = pattern.iter().sum();
    if total <= 0.0 {
        return vec![(pts.to_vec(), closed)];
    }

    // 位相を 0..(パターン周期) に正規化（奇数長は 2 倍周期）。
    let period = if pattern.len() % 2 == 1 {
        total * 2.0
    } else {
        total
    };
    let mut phase = if phase.is_finite() { phase } else { 0.0 };
    phase = phase.rem_euclid(period);

    // 現在のダッシュインデックスと残り長を位相から求める。
    let mut idx = 0usize;
    let mut remain = pattern[0];
    let mut on = true; // 偶数インデックス＝ON
    {
        let mut p = phase;
        while p >= remain {
            p -= remain;
            idx = (idx + 1) % pattern.len();
            remain = pattern[idx];
            on = idx.is_multiple_of(2);
        }
        remain -= p;
    }

    let mut result: Vec<(Vec<Point>, bool)> = Vec::new();
    let mut current: Vec<Point> = Vec::new();
    if on && !work.is_empty() {
        current.push(work[0]);
    }

    for w in work.windows(2) {
        let a = w[0];
        let b = w[1];
        let seg_len = ((b.x - a.x).powi(2) + (b.y - a.y).powi(2)).sqrt();
        if !seg_len.is_finite() || seg_len <= 0.0 {
            continue;
        }
        let mut pos = 0.0; // a からの距離
        while pos < seg_len {
            let step = remain.min(seg_len - pos);
            let t0 = pos / seg_len;
            let t1 = (pos + step) / seg_len;
            let q1 = Point::new(a.x + (b.x - a.x) * t1, a.y + (b.y - a.y) * t1);
            if on {
                if current.is_empty() {
                    let q0 = Point::new(a.x + (b.x - a.x) * t0, a.y + (b.y - a.y) * t0);
                    current.push(q0);
                }
                current.push(q1);
            }
            pos += step;
            remain -= step;
            if remain <= 1e-12 {
                // 次のダッシュ要素へ。
                if on && current.len() >= 2 {
                    result.push((std::mem::take(&mut current), false));
                } else {
                    current.clear();
                }
                idx = (idx + 1) % pattern.len();
                remain = pattern[idx];
                on = idx.is_multiple_of(2);
                // ゼロ長要素を飛ばす。
                let mut guard = 0;
                while remain <= 1e-12 && guard < pattern.len() {
                    idx = (idx + 1) % pattern.len();
                    remain = pattern[idx];
                    on = idx.is_multiple_of(2);
                    guard += 1;
                }
            }
        }
    }
    if on && current.len() >= 2 {
        result.push((current, false));
    }
    if result.is_empty() {
        // 全部 OFF になった場合でも空を返す（描かない）。
        return result;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 矩形パスを作る補助。
    fn rect(x0: f64, y0: f64, x1: f64, y1: f64, ccw: bool) -> Path {
        let mut p = Path::new();
        if ccw {
            p.move_to(x0, y0);
            p.line_to(x1, y0);
            p.line_to(x1, y1);
            p.line_to(x0, y1);
        } else {
            p.move_to(x0, y0);
            p.line_to(x0, y1);
            p.line_to(x1, y1);
            p.line_to(x1, y0);
        }
        p.close();
        p
    }

    #[test]
    fn fill_integer_rect_full_coverage() {
        let mut pm = Pixmap::new(10, 10);
        let path = rect(2.0, 2.0, 8.0, 8.0, true);
        fill_path(&mut pm, &path, FillRule::NonZero, [0, 0, 0], 255, None);
        // 内部は黒（全被覆）。
        assert_eq!(pm.pixel(5, 5), Some([0, 0, 0]));
        assert_eq!(pm.pixel(3, 3), Some([0, 0, 0]));
        // 外部は白。
        assert_eq!(pm.pixel(0, 0), Some([255, 255, 255]));
        assert_eq!(pm.pixel(9, 9), Some([255, 255, 255]));
    }

    #[test]
    fn fill_half_pixel_boundary_is_half() {
        // x=2.5 から始まる矩形 → 列 2 は約半分被覆。
        let mut pm = Pixmap::new(10, 10);
        let path = rect(2.5, 1.0, 8.0, 9.0, true);
        fill_path(&mut pm, &path, FillRule::NonZero, [0, 0, 0], 255, None);
        // 列 2 の中ほど: 白→黒へ約 50% 合成 ≈ 127 前後。
        let px = pm.pixel(2, 5).unwrap();
        assert!(
            (px[0] as i32 - 127).abs() <= 6,
            "境界列 px={:?} が約 127 でない",
            px
        );
        // 列 3 は満被覆（黒）。
        assert_eq!(pm.pixel(3, 5), Some([0, 0, 0]));
    }

    #[test]
    fn nonzero_vs_evenodd_double_wound() {
        // 同方向に二重に巻いた同一矩形。
        let mut path = rect(2.0, 2.0, 8.0, 8.0, true);
        // 同方向でもう 1 周。
        path.move_to(2.0, 2.0);
        path.line_to(8.0, 2.0);
        path.line_to(8.0, 8.0);
        path.line_to(2.0, 8.0);
        path.close();

        // NonZero → 塗られる。
        let mut pm1 = Pixmap::new(10, 10);
        fill_path(&mut pm1, &path, FillRule::NonZero, [0, 0, 0], 255, None);
        assert_eq!(pm1.pixel(5, 5), Some([0, 0, 0]));

        // EvenOdd → 巻き数 2（偶数）で抜ける。
        let mut pm2 = Pixmap::new(10, 10);
        fill_path(&mut pm2, &path, FillRule::EvenOdd, [0, 0, 0], 255, None);
        assert_eq!(pm2.pixel(5, 5), Some([255, 255, 255]));
    }

    #[test]
    fn donut_nonzero_has_hole() {
        // 外側 CCW + 内側逆向き（CW）→ NonZero で穴あき。
        let mut path = rect(1.0, 1.0, 9.0, 9.0, true);
        let inner = rect(3.0, 3.0, 7.0, 7.0, false);
        // 内側セグメントを追加。
        let polys = inner.flatten(0.25);
        if let Some(pl) = polys.first() {
            path.move_to(pl[0].x, pl[0].y);
            for q in &pl[1..] {
                path.line_to(q.x, q.y);
            }
            path.close();
        }
        let mut pm = Pixmap::new(10, 10);
        fill_path(&mut pm, &path, FillRule::NonZero, [0, 0, 0], 255, None);
        // 穴の中心は白。
        assert_eq!(pm.pixel(5, 5), Some([255, 255, 255]));
        // リング部分は黒。
        assert_eq!(pm.pixel(2, 5), Some([0, 0, 0]));
    }

    #[test]
    fn stroke_horizontal_line_makes_band() {
        // 幅 4 の水平線 → 高さ 4 の帯。
        let mut path = Path::new();
        path.move_to(2.0, 10.0);
        path.line_to(18.0, 10.0);
        let style = StrokeStyle {
            width: 4.0,
            ..Default::default()
        };
        let outline = stroke_to_path(&path, &style, 0.25);
        let mut pm = Pixmap::new(20, 20);
        fill_path(&mut pm, &outline, FillRule::NonZero, [0, 0, 0], 255, None);

        // 列 10 で黒いピクセルの高さを数える（被覆 > 半分）。
        let mut count: i32 = 0;
        for y in 0..20 {
            if let Some(px) = pm.pixel(10, y) {
                if px[0] < 128 {
                    count += 1;
                }
            }
        }
        // 中心 y=10、半幅 2 → y=8..12 あたりの 4 ピクセル。
        assert!((count - 4).abs() <= 1, "帯の高さ {} が約 4 でない", count);
    }

    #[test]
    fn stroke_dash_creates_gaps() {
        // 長い水平線にダッシュ [4, 4]。
        let mut path = Path::new();
        path.move_to(0.0, 10.0);
        path.line_to(40.0, 10.0);
        let style = StrokeStyle {
            width: 2.0,
            dash: vec![4.0, 4.0],
            ..Default::default()
        };
        let outline = stroke_to_path(&path, &style, 0.25);
        let mut pm = Pixmap::new(40, 20);
        fill_path(&mut pm, &outline, FillRule::NonZero, [0, 0, 0], 255, None);

        // 描かれる区間と空白区間が交互にあること。
        let painted: Vec<bool> = (0..40)
            .map(|x| pm.pixel(x, 10).map(|p| p[0] < 128).unwrap_or(false))
            .collect();
        let any_on = painted.iter().any(|&b| b);
        let any_off = painted.iter().any(|&b| !b);
        assert!(any_on && any_off, "ダッシュの ON/OFF が両方ない");
        // 最初の 4px 付近は ON、次の 4px 付近は OFF。
        assert!(painted[1], "先頭ダッシュが描かれていない");
        assert!(!painted[6], "ギャップが空いていない");
    }

    #[test]
    fn stroke_zero_dash_is_solid() {
        let mut path = Path::new();
        path.move_to(0.0, 5.0);
        path.line_to(20.0, 5.0);
        let style = StrokeStyle {
            width: 2.0,
            dash: vec![0.0, 0.0],
            ..Default::default()
        };
        let outline = stroke_to_path(&path, &style, 0.25);
        let mut pm = Pixmap::new(20, 10);
        fill_path(&mut pm, &outline, FillRule::NonZero, [0, 0, 0], 255, None);
        // 不正ダッシュ → 実線。途中も描かれる。
        assert!(pm.pixel(10, 5).map(|p| p[0] < 128).unwrap_or(false));
    }

    #[test]
    fn round_dot_marker_drawn() {
        // 点だけのサブパス + Round キャップ → 円マーカー。
        let mut path = Path::new();
        path.move_to(10.0, 10.0);
        let style = StrokeStyle {
            width: 6.0,
            cap: LineCap::Round,
            ..Default::default()
        };
        let outline = stroke_to_path(&path, &style, 0.25);
        let mut pm = Pixmap::new(20, 20);
        fill_path(&mut pm, &outline, FillRule::NonZero, [0, 0, 0], 255, None);
        // 中心はほぼ黒（扇形の頂点が集まる中心は丸め誤差で数 LSB 残りうる）。
        assert!(pm.pixel(10, 10).map(|p| p[0] < 8).unwrap_or(false));
        // 角（半径外）は白。
        assert_eq!(pm.pixel(14, 14), Some([255, 255, 255]));
    }

    #[test]
    fn butt_dot_marker_invisible() {
        let mut path = Path::new();
        path.move_to(10.0, 10.0);
        let style = StrokeStyle {
            width: 6.0,
            cap: LineCap::Butt,
            ..Default::default()
        };
        let outline = stroke_to_path(&path, &style, 0.25);
        assert!(outline.is_empty());
    }

    #[test]
    fn clip_mask_blocks_outside() {
        // クリップ: 左半分だけ通すマスク。
        let clip_path = rect(0.0, 0.0, 5.0, 10.0, true);
        let mask = Mask::from_path(&clip_path, FillRule::NonZero, 10, 10);
        // 全面黒で塗るが、クリップで左半分のみ。
        let fill = rect(0.0, 0.0, 10.0, 10.0, true);
        let mut pm = Pixmap::new(10, 10);
        fill_path(
            &mut pm,
            &fill,
            FillRule::NonZero,
            [0, 0, 0],
            255,
            Some(&mask),
        );
        assert_eq!(pm.pixel(2, 5), Some([0, 0, 0])); // クリップ内
        assert_eq!(pm.pixel(8, 5), Some([255, 255, 255])); // クリップ外
    }

    #[test]
    fn mask_intersect_multiplies() {
        let mut a = Mask::from_path(&rect(0.0, 0.0, 6.0, 10.0, true), FillRule::NonZero, 10, 10);
        let b = Mask::from_path(&rect(4.0, 0.0, 10.0, 10.0, true), FillRule::NonZero, 10, 10);
        a.intersect(&b);
        // 重なる 4..6 のみ通る。
        assert_eq!(a.coverage(5, 5), 255);
        assert_eq!(a.coverage(2, 5), 0);
        assert_eq!(a.coverage(8, 5), 0);
    }

    #[test]
    fn nan_and_offscreen_no_panic() {
        let mut pm = Pixmap::new(10, 10);
        // NaN を含むパス。
        let mut p = Path::new();
        p.move_to(f64::NAN, 0.0);
        p.line_to(5.0, 5.0);
        p.line_to(f64::INFINITY, 1.0);
        p.close();
        fill_path(&mut pm, &p, FillRule::NonZero, [0, 0, 0], 255, None);

        // 完全にキャンバス外（負・巨大）。
        let off = rect(-1e9, -1e9, -100.0, -100.0, true);
        fill_path(&mut pm, &off, FillRule::NonZero, [0, 0, 0], 255, None);
        let huge = rect(1e9, 1e9, 2e9, 2e9, true);
        fill_path(&mut pm, &huge, FillRule::NonZero, [0, 0, 0], 255, None);

        // 空パス。
        let empty = Path::new();
        fill_path(&mut pm, &empty, FillRule::NonZero, [0, 0, 0], 255, None);

        // ストロークも NaN/空で落ちない。
        let _ = stroke_to_path(&p, &StrokeStyle::default(), 0.25);
        let _ = stroke_to_path(&empty, &StrokeStyle::default(), 0.25);
    }

    #[test]
    fn open_subpath_auto_closed_on_fill() {
        // 閉じていない三角形 → 塗り時に自動クローズ。
        let mut p = Path::new();
        p.move_to(2.0, 2.0);
        p.line_to(8.0, 2.0);
        p.line_to(5.0, 8.0);
        // close を呼ばない。
        let mut pm = Pixmap::new(10, 10);
        fill_path(&mut pm, &p, FillRule::NonZero, [0, 0, 0], 255, None);
        // 三角形内部は塗られる。
        assert_eq!(pm.pixel(5, 4), Some([0, 0, 0]));
    }
}
