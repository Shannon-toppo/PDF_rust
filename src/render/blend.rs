//! 透明度モデルのブレンドモード（§11.3.5）。
//!
//! PDF のブレンドモード `B(Cb, Cs)` を実装する。`Cb` は背景色、`Cs` はソース
//! 色で、戻り値は両者を当該モードで混ぜた色。コンポジット式
//! `Co = αs·B(Cb,Cs) + (1 − αs)·Cb`（背景不透明時の単純化）と組み合わせて使う。
//!
//! 分離可能モード（Normal/Multiply/Screen/Overlay/Darken/Lighten/ColorDodge/
//! ColorBurn/HardLight/SoftLight/Difference/Exclusion）はチャネル独立。
//! 非分離可能モード（Hue/Saturation/Color/Luminosity）は HSL/輝度空間で
//! まとめて変換する。
//!
//! 入力 0–255 を内部で 0..1 の f32 に正規化し、最終結果は 0..255 へ丸める。
//! 仕様にない名前は [`BlendMode::Normal`] にフォールバックする（耐故障性）。

/// PDF のブレンドモード。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BlendMode {
    /// `Cs` をそのまま採用（ExtGState 未設定時の既定）。
    #[default]
    Normal,
    Multiply,
    Screen,
    Overlay,
    Darken,
    Lighten,
    ColorDodge,
    ColorBurn,
    HardLight,
    SoftLight,
    Difference,
    Exclusion,
    Hue,
    Saturation,
    Color,
    Luminosity,
}

impl BlendMode {
    /// PDF の `/BM` 名（`Normal`, `Multiply`, …）からモードへ変換する。
    /// 不明名は [`BlendMode::Normal`]（仕様の挙動）。
    pub fn from_name(name: &str) -> BlendMode {
        match name {
            "Normal" | "Compatible" => BlendMode::Normal,
            "Multiply" => BlendMode::Multiply,
            "Screen" => BlendMode::Screen,
            "Overlay" => BlendMode::Overlay,
            "Darken" => BlendMode::Darken,
            "Lighten" => BlendMode::Lighten,
            "ColorDodge" => BlendMode::ColorDodge,
            "ColorBurn" => BlendMode::ColorBurn,
            "HardLight" => BlendMode::HardLight,
            "SoftLight" => BlendMode::SoftLight,
            "Difference" => BlendMode::Difference,
            "Exclusion" => BlendMode::Exclusion,
            "Hue" => BlendMode::Hue,
            "Saturation" => BlendMode::Saturation,
            "Color" => BlendMode::Color,
            "Luminosity" => BlendMode::Luminosity,
            _ => BlendMode::Normal,
        }
    }

    /// 分離可能（チャネル独立）モードか。`true` ならチャネルごとに
    /// [`blend_separable`] が使える。
    pub fn is_separable(self) -> bool {
        !matches!(
            self,
            BlendMode::Hue | BlendMode::Saturation | BlendMode::Color | BlendMode::Luminosity
        )
    }
}

/// 背景色 `cb` とソース色 `cs` をブレンドする（RGB 8bit）。
///
/// モードが分離可能なら [`blend_separable`]、非分離可能なら非分離可能関数を呼ぶ。
/// 戻り値は背景に対する `B(Cb, Cs)`（コンポジット前の中間値）。
pub fn blend(cb: [u8; 3], cs: [u8; 3], mode: BlendMode) -> [u8; 3] {
    if mode == BlendMode::Normal {
        return cs;
    }
    if mode.is_separable() {
        [
            blend_separable(cb[0], cs[0], mode),
            blend_separable(cb[1], cs[1], mode),
            blend_separable(cb[2], cs[2], mode),
        ]
    } else {
        blend_nonseparable(cb, cs, mode)
    }
}

/// 1 チャネル分の分離可能ブレンド（0..255）。
pub fn blend_separable(b: u8, s: u8, mode: BlendMode) -> u8 {
    let bf = b as f32 / 255.0;
    let sf = s as f32 / 255.0;
    let r = match mode {
        BlendMode::Normal => sf,
        BlendMode::Multiply => bf * sf,
        BlendMode::Screen => bf + sf - bf * sf,
        BlendMode::Overlay => hard_light(sf, bf), // 引数を入れ替えた HardLight
        BlendMode::Darken => bf.min(sf),
        BlendMode::Lighten => bf.max(sf),
        BlendMode::ColorDodge => {
            if sf >= 1.0 {
                1.0
            } else {
                (bf / (1.0 - sf)).min(1.0)
            }
        }
        BlendMode::ColorBurn => {
            if sf <= 0.0 {
                0.0
            } else {
                1.0 - ((1.0 - bf) / sf).min(1.0)
            }
        }
        BlendMode::HardLight => hard_light(bf, sf),
        BlendMode::SoftLight => soft_light(bf, sf),
        BlendMode::Difference => (bf - sf).abs(),
        BlendMode::Exclusion => bf + sf - 2.0 * bf * sf,
        // 非分離可能はチャネル独立でないので、ここに来た時点で Normal にフォールバック。
        _ => sf,
    };
    to_u8(r)
}

/// HardLight: s が暗ければ Multiply(2s, b)、明るければ Screen(2s-1, b)。
fn hard_light(b: f32, s: f32) -> f32 {
    if s <= 0.5 {
        b * (2.0 * s)
    } else {
        let s2 = 2.0 * s - 1.0;
        b + s2 - b * s2
    }
}

/// SoftLight: 仕様 §11.3.5.2 の二分岐式。
fn soft_light(b: f32, s: f32) -> f32 {
    if s <= 0.5 {
        b - (1.0 - 2.0 * s) * b * (1.0 - b)
    } else {
        let d = if b <= 0.25 {
            ((16.0 * b - 12.0) * b + 4.0) * b
        } else {
            b.sqrt()
        };
        b + (2.0 * s - 1.0) * (d - b)
    }
}

/// 非分離可能モード（Hue/Saturation/Color/Luminosity）の RGB ブレンド。
fn blend_nonseparable(cb: [u8; 3], cs: [u8; 3], mode: BlendMode) -> [u8; 3] {
    let b = [
        cb[0] as f32 / 255.0,
        cb[1] as f32 / 255.0,
        cb[2] as f32 / 255.0,
    ];
    let s = [
        cs[0] as f32 / 255.0,
        cs[1] as f32 / 255.0,
        cs[2] as f32 / 255.0,
    ];
    let r = match mode {
        BlendMode::Hue => set_lum(set_sat(s, sat(b)), lum(b)),
        BlendMode::Saturation => set_lum(set_sat(b, sat(s)), lum(b)),
        BlendMode::Color => set_lum(s, lum(b)),
        BlendMode::Luminosity => set_lum(b, lum(s)),
        _ => s, // 念のため
    };
    [to_u8(r[0]), to_u8(r[1]), to_u8(r[2])]
}

/// 知覚的輝度（PDF 仕様 §11.3.5.3）。
fn lum(c: [f32; 3]) -> f32 {
    0.3 * c[0] + 0.59 * c[1] + 0.11 * c[2]
}

/// 飽和度 = max − min。
fn sat(c: [f32; 3]) -> f32 {
    c[0].max(c[1]).max(c[2]) - c[0].min(c[1]).min(c[2])
}

/// 輝度を `l` に揃える（クリップ込み）。
fn set_lum(c: [f32; 3], l: f32) -> [f32; 3] {
    let d = l - lum(c);
    clip_color([c[0] + d, c[1] + d, c[2] + d])
}

/// 色域外を輝度を保ったままクリップする（PDF §11.3.5.3）。
fn clip_color(c: [f32; 3]) -> [f32; 3] {
    let l = lum(c);
    let n = c[0].min(c[1]).min(c[2]);
    let x = c[0].max(c[1]).max(c[2]);
    let mut out = c;
    if n < 0.0 {
        let denom = l - n;
        if denom.abs() > 1e-9 {
            for v in out.iter_mut() {
                *v = l + (*v - l) * l / denom;
            }
        } else {
            out = [l, l, l];
        }
    }
    if x > 1.0 {
        let denom = x - l;
        if denom.abs() > 1e-9 {
            for v in out.iter_mut() {
                *v = l + (*v - l) * (1.0 - l) / denom;
            }
        } else {
            out = [l, l, l];
        }
    }
    out
}

/// 飽和度を `s` に揃える（min/mid/max を比例配分。PDF §11.3.5.3）。
fn set_sat(c: [f32; 3], s: f32) -> [f32; 3] {
    // インデックスを min/mid/max で並べ替えて変換し、元の位置に戻す。
    let mut idx = [0usize, 1, 2];
    // 単純な選択ソート（要素 3 個）。
    if c[idx[0]] > c[idx[1]] {
        idx.swap(0, 1);
    }
    if c[idx[1]] > c[idx[2]] {
        idx.swap(1, 2);
    }
    if c[idx[0]] > c[idx[1]] {
        idx.swap(0, 1);
    }
    let (i_min, i_mid, i_max) = (idx[0], idx[1], idx[2]);
    let mut out = [0.0f32; 3];
    if c[i_max] > c[i_min] {
        out[i_mid] = (c[i_mid] - c[i_min]) * s / (c[i_max] - c[i_min]);
        out[i_max] = s;
    } else {
        out[i_mid] = 0.0;
        out[i_max] = 0.0;
    }
    out[i_min] = 0.0;
    out
}

/// 0..1 の f32 を 0..255 の u8 へ（四捨五入・クランプ）。
fn to_u8(v: f32) -> u8 {
    let c = if v.is_finite() {
        v.clamp(0.0, 1.0)
    } else {
        0.0
    };
    (c * 255.0 + 0.5) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Normal は常にソース。
    #[test]
    fn normal_returns_source() {
        assert_eq!(
            blend([10, 20, 30], [200, 100, 50], BlendMode::Normal),
            [200, 100, 50]
        );
    }

    /// Multiply は両者の積、Screen は補色の積の補色。
    #[test]
    fn multiply_and_screen() {
        // 黒 × X = 黒
        assert_eq!(
            blend([0, 0, 0], [200, 100, 50], BlendMode::Multiply),
            [0, 0, 0]
        );
        // 白 × X = X
        assert_eq!(
            blend([255, 255, 255], [128, 64, 32], BlendMode::Multiply),
            [128, 64, 32]
        );
        // 白 + X(Screen) = 白
        assert_eq!(
            blend([255, 255, 255], [10, 20, 30], BlendMode::Screen),
            [255, 255, 255]
        );
        // 黒 + X(Screen) = X
        assert_eq!(
            blend([0, 0, 0], [128, 64, 32], BlendMode::Screen),
            [128, 64, 32]
        );
    }

    /// Darken は min、Lighten は max。
    #[test]
    fn darken_lighten() {
        assert_eq!(
            blend([100, 200, 50], [50, 150, 200], BlendMode::Darken),
            [50, 150, 50]
        );
        assert_eq!(
            blend([100, 200, 50], [50, 150, 200], BlendMode::Lighten),
            [100, 200, 200]
        );
    }

    /// Difference は |b − s|。
    #[test]
    fn difference_basic() {
        assert_eq!(
            blend([200, 100, 50], [50, 100, 200], BlendMode::Difference),
            [150, 0, 150]
        );
    }

    /// 不明名 → Normal。
    #[test]
    fn unknown_name_falls_back() {
        assert_eq!(BlendMode::from_name("Foo"), BlendMode::Normal);
        assert_eq!(BlendMode::from_name("Multiply"), BlendMode::Multiply);
    }

    /// ColorDodge / ColorBurn の端点。
    #[test]
    fn dodge_burn_endpoints() {
        // Dodge: s=1 → 白。
        assert_eq!(blend_separable(50, 255, BlendMode::ColorDodge), 255);
        // Burn: s=0 → 黒。
        assert_eq!(blend_separable(200, 0, BlendMode::ColorBurn), 0);
    }

    /// 非分離可能: Luminosity は背景の色相＋ソースの輝度を持つ。
    #[test]
    fn luminosity_keeps_backdrop_hue() {
        // 赤背景 × 灰ソース → 同等の輝度を持つ赤系の色になる。
        let out = blend([255, 0, 0], [128, 128, 128], BlendMode::Luminosity);
        // 赤チャネルが他より明らかに大きいことを確認（色相が赤のまま）。
        assert!(
            out[0] > out[1] && out[0] > out[2],
            "Luminosity 結果: {:?}",
            out
        );
    }
}
