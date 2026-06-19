//! PDF 色空間の解決と RGB 変換。
//!
//! [`ColorSpace::parse`] でページリソースの色空間オブジェクト（名前・配列・間接参照）を
//! 解決し、[`ColorSpace::to_rgb`] で成分値を sRGB へ変換する。
//!
//! ## 対応色空間
//!
//! | 種別 | 処理 |
//! |---|---|
//! | DeviceGray / DeviceRGB / DeviceCMYK | 直接変換 |
//! | CalGray | DeviceGray に近似（doc コメント参照） |
//! | CalRGB | DeviceRGB に近似（doc コメント参照） |
//! | Lab | L\*a\*b\* → XYZ → sRGB（近似）|
//! | ICCBased | /N で Gray/RGB/CMYK に近似（/Alternate 優先）|
//! | Indexed | パレット引き（文字列・ストリーム両対応）|
//! | Separation / DeviceN | tint 変換 + 代替色空間 |
//! | Pattern / その他 | 黒へ縮退（Unsupported）|
//!
//! ## 耐故障性
//!
//! 不正データや未対応色空間は panic せず黒（`[0,0,0]`）へ縮退する。
//! `unwrap` / 直接インデックスは使用しない。

use crate::document::Document;
use crate::function::PdfFunction;
use crate::object::{Dictionary, Object};

// ============================================================
// 公開型
// ============================================================

/// 解決済みの色空間。
///
/// [`ColorSpace::parse`] で PDF オブジェクトから構築し、[`ColorSpace::to_rgb`] で
/// sRGB へ変換する。[`Clone`] を derive しているため `q`/`Q` の退避・復元に
/// 自動で対応できる。
#[derive(Debug, Clone)]
pub(crate) enum ColorSpace {
    /// 1 成分グレースケール（0 = 黒、1 = 白）。
    DeviceGray,
    /// 3 成分 RGB（各 0–1）。
    DeviceRGB,
    /// 4 成分 CMYK（各 0–1）。
    DeviceCMYK,
    /// L\*a\*b\* 色空間。
    ///
    /// WhitePoint と Range を読み、L\*a\*b\* → XYZ → sRGB で変換する。
    /// Bradford クロマティック適応は省略し、WhitePoint を直接 D65 正規化係数
    /// として使う近似（doc コメントに詳細あり）。
    Lab {
        /// 白色点 [Xw, Yw, Zw]（/WhitePoint 由来）。
        white: [f64; 3],
        /// a\*, b\* の Range [a_min, a_max, b_min, b_max]（既定 [-100, 100, -100, 100]）。
        range: [f64; 4],
    },
    /// Indexed 色空間（パレット参照）。
    ///
    /// 成分数は 1（インデックス値）。`to_rgb` でパレットを引いて base に変換する。
    Indexed {
        /// ベース色空間（Indexed の入れ子は PDF 仕様で禁止）。
        base: Box<ColorSpace>,
        /// 最大インデックス値（0 以上 255 以下）。
        hival: u8,
        /// パレットデータ（`base.n_components() * (hival+1)` バイト）。
        lookup: Vec<u8>,
    },
    /// Separation / DeviceN 色空間（tint 変換 + 代替色空間）。
    ///
    /// `n` は入力成分数（Separation は 1、DeviceN は名前配列の長さ）。
    /// `to_rgb` は `tint.eval(comps)` で代替空間の成分に変換してから `alt.to_rgb` を呼ぶ。
    Separation {
        /// tint 関数の入力次元数（Separation=1、DeviceN=N）。
        n: usize,
        /// 代替色空間。
        alt: Box<ColorSpace>,
        /// tint 変換関数。
        tint: PdfFunction,
    },
    /// Pattern 色空間（§8.7）。
    ///
    /// `cs /Pattern` または `cs [/Pattern BaseColorSpace]` で設定される。
    /// `base = None` は colored Tiling および Shading パターン用（scn 引数は
    /// パターン名のみ）、`base = Some(_)` は uncolored Tiling 用
    /// （scn 引数はベース色成分 N 個 + パターン名）。
    Pattern {
        /// uncolored Tiling のベース色空間。colored / shading は `None`。
        base: Option<Box<ColorSpace>>,
    },
    /// 未対応色空間。成分は無視して黒を返す。
    Unsupported,
}

impl ColorSpace {
    // --------------------------------------------------------
    // 構築
    // --------------------------------------------------------

    /// 色空間オブジェクト（名前・配列・間接参照）を解決する。
    ///
    /// `resources` はページまたは Form の /Resources 辞書で、/ColorSpace エントリで
    /// 定義された任意名の参照解決に使う。失敗した場合は [`ColorSpace::Unsupported`]
    /// を返す（panic しない）。
    pub(crate) fn parse(doc: &Document, obj: &Object, resources: &Dictionary) -> ColorSpace {
        Self::parse_inner(doc, doc.resolve(obj), resources, 0)
    }

    /// 再帰深さ付きの内部実装（循環参照対策）。
    fn parse_inner(doc: &Document, obj: &Object, resources: &Dictionary, depth: u32) -> ColorSpace {
        if depth > 8 {
            return ColorSpace::Unsupported;
        }
        match obj {
            // --- 名前形式 ---
            Object::Name(name) => Self::parse_name(doc, name, resources, depth),
            // --- 配列形式 ---
            Object::Array(arr) => Self::parse_array(doc, arr, resources, depth),
            // --- 間接参照は既に解決済みのはずだが念のため ---
            Object::Reference(_) => {
                let resolved = doc.resolve(obj);
                Self::parse_inner(doc, resolved, resources, depth + 1)
            }
            // --- その他は Unsupported ---
            _ => ColorSpace::Unsupported,
        }
    }

    /// 名前形式の色空間を解決する。
    fn parse_name(doc: &Document, name: &str, resources: &Dictionary, depth: u32) -> ColorSpace {
        match name {
            "DeviceGray" | "G" => ColorSpace::DeviceGray,
            "DeviceRGB" | "RGB" => ColorSpace::DeviceRGB,
            "DeviceCMYK" | "CMYK" => ColorSpace::DeviceCMYK,
            // /I はインライン画像の省略名。配列形式でないと完全解決できないため Unsupported。
            "I" => ColorSpace::Unsupported,
            // 名前 /Pattern は colored Tiling / Shading パターン用（base なし）。
            "Pattern" => ColorSpace::Pattern { base: None },
            // /Resources /ColorSpace 辞書を引いて再帰解決する。
            _ => {
                let cs_dict = match doc.dict_get(resources, "ColorSpace") {
                    Some(Object::Dictionary(d)) => d.clone(),
                    _ => return ColorSpace::Unsupported,
                };
                match doc.dict_get(&cs_dict, name) {
                    Some(obj) => {
                        let resolved = obj.clone();
                        Self::parse_inner(doc, &resolved, resources, depth + 1)
                    }
                    None => ColorSpace::Unsupported,
                }
            }
        }
    }

    /// 配列形式の色空間を解決する。
    fn parse_array(
        doc: &Document,
        arr: &[Object],
        resources: &Dictionary,
        depth: u32,
    ) -> ColorSpace {
        // 先頭要素が色空間の種別名。
        let kind = match arr.first().and_then(|o| doc.resolve(o).as_name().ok()) {
            Some(k) => k.to_owned(),
            None => return ColorSpace::Unsupported,
        };

        match kind.as_str() {
            // CalGray → DeviceGray に近似。
            // CalGray は /Gamma と /WhitePoint でトーン・色温度を調整するが、
            // ここでは sRGB への正確な変換を省略し DeviceGray として扱う。
            "CalGray" => ColorSpace::DeviceGray,

            // CalRGB → DeviceRGB に近似。
            // CalRGB は /Gamma 行列・/Matrix で補正するが省略し DeviceRGB として扱う。
            "CalRGB" => ColorSpace::DeviceRGB,

            // Lab: L*a*b* → XYZ → sRGB（近似変換。Bradford 省略）。
            "Lab" => Self::parse_lab(doc, arr),

            // ICCBased: /N（成分数）と /Alternate で解決。
            "ICCBased" => Self::parse_icc(doc, arr, resources, depth),

            // Indexed: base + hival + lookup。
            "Indexed" | "I" => Self::parse_indexed(doc, arr, resources, depth),

            // Separation: name + alt + tint。
            "Separation" => Self::parse_separation(doc, arr, resources, depth),

            // DeviceN: names + alt + tint（Separation の多成分版）。
            "DeviceN" => Self::parse_device_n(doc, arr, resources, depth),

            "Pattern" => {
                // [/Pattern] または [/Pattern BaseCS]。
                if arr.len() <= 1 {
                    ColorSpace::Pattern { base: None }
                } else {
                    let base_obj = arr.get(1).cloned().unwrap_or(Object::Null);
                    let base_cs = Self::parse_inner(doc, &base_obj, resources, depth + 1);
                    ColorSpace::Pattern {
                        base: Some(Box::new(base_cs)),
                    }
                }
            }
            // 名前形式のフォールバック（配列の先頭が名前で要素が 1 つのケース）。
            name => {
                if arr.len() == 1 {
                    Self::parse_name(doc, name, resources, depth)
                } else {
                    ColorSpace::Unsupported
                }
            }
        }
    }

    /// Lab 色空間の解析。
    fn parse_lab(doc: &Document, arr: &[Object]) -> ColorSpace {
        // [/Lab dict]
        let dict = match arr.get(1).map(|o| doc.resolve(o)) {
            Some(Object::Dictionary(d)) => d.clone(),
            _ => {
                // 辞書なしは既定値で縮退。
                return ColorSpace::Lab {
                    white: [0.95047, 1.0, 1.08883],
                    range: [-100.0, 100.0, -100.0, 100.0],
                };
            }
        };

        // /WhitePoint [Xw, Yw, Zw]（必須だが壊れていたら D50 で代替）。
        let white = match doc.dict_get(&dict, "WhitePoint") {
            Some(Object::Array(a)) => {
                let v: Vec<f64> = a
                    .iter()
                    .filter_map(|o| doc.resolve(o).as_number().ok())
                    .collect();
                [
                    v.first().copied().unwrap_or(0.96422),
                    v.get(1).copied().unwrap_or(1.0),
                    v.get(2).copied().unwrap_or(0.82521),
                ]
            }
            _ => [0.96422, 1.0, 0.82521], // D50 近似
        };

        // /Range [amin amax bmin bmax]（既定 [-100, 100, -100, 100]）。
        let range = match doc.dict_get(&dict, "Range") {
            Some(Object::Array(a)) => {
                let v: Vec<f64> = a
                    .iter()
                    .filter_map(|o| doc.resolve(o).as_number().ok())
                    .collect();
                [
                    v.first().copied().unwrap_or(-100.0),
                    v.get(1).copied().unwrap_or(100.0),
                    v.get(2).copied().unwrap_or(-100.0),
                    v.get(3).copied().unwrap_or(100.0),
                ]
            }
            _ => [-100.0, 100.0, -100.0, 100.0],
        };

        ColorSpace::Lab { white, range }
    }

    /// ICCBased 色空間の解析。
    fn parse_icc(doc: &Document, arr: &[Object], resources: &Dictionary, depth: u32) -> ColorSpace {
        // [/ICCBased stream]
        let stream = match arr.get(1).map(|o| doc.resolve(o)) {
            Some(Object::Stream(s)) => s.clone(),
            _ => return ColorSpace::Unsupported,
        };

        // /Alternate があればそれを優先して解決する。
        if let Some(alt_obj) = doc.dict_get(&stream.dict, "Alternate") {
            let alt_obj = alt_obj.clone();
            let cs = Self::parse_inner(doc, &alt_obj, resources, depth + 1);
            if !matches!(cs, ColorSpace::Unsupported) {
                return cs;
            }
        }

        // /N（成分数）で近似: 1 → Gray, 3 → RGB, 4 → CMYK。
        match doc
            .dict_get(&stream.dict, "N")
            .and_then(|o| o.as_int().ok())
        {
            Some(1) => ColorSpace::DeviceGray,
            Some(3) => ColorSpace::DeviceRGB,
            Some(4) => ColorSpace::DeviceCMYK,
            _ => ColorSpace::Unsupported,
        }
    }

    /// Indexed 色空間の解析。
    fn parse_indexed(
        doc: &Document,
        arr: &[Object],
        resources: &Dictionary,
        depth: u32,
    ) -> ColorSpace {
        // [/Indexed base hival lookup]
        let base_obj = match arr.get(1) {
            Some(o) => o.clone(),
            None => return ColorSpace::Unsupported,
        };
        let base_resolved = doc.resolve(&base_obj);
        let base = Box::new(Self::parse_inner(doc, base_resolved, resources, depth + 1));

        let hival = match arr.get(2).and_then(|o| doc.resolve(o).as_int().ok()) {
            Some(v) => v.clamp(0, 255) as u8,
            None => return ColorSpace::Unsupported,
        };

        // lookup: 文字列またはストリーム。
        let lookup_obj = match arr.get(3) {
            Some(o) => doc.resolve(o).clone(),
            None => return ColorSpace::Unsupported,
        };
        let lookup = match &lookup_obj {
            Object::String(bytes, _) => bytes.clone(),
            Object::Stream(s) => doc.get_stream_data(s).unwrap_or_default(),
            _ => return ColorSpace::Unsupported,
        };

        ColorSpace::Indexed {
            base,
            hival,
            lookup,
        }
    }

    /// Separation 色空間の解析。
    fn parse_separation(
        doc: &Document,
        arr: &[Object],
        resources: &Dictionary,
        depth: u32,
    ) -> ColorSpace {
        // [/Separation name alt tint]
        // name: arr[1]（インク名）、alt: arr[2]（代替色空間）、tint: arr[3]（変換関数）。
        let alt_obj = match arr.get(2) {
            Some(o) => o.clone(),
            None => return ColorSpace::Unsupported,
        };
        let alt_resolved = doc.resolve(&alt_obj);
        let alt = Box::new(Self::parse_inner(doc, alt_resolved, resources, depth + 1));

        let tint_obj = match arr.get(3) {
            Some(o) => o.clone(),
            None => return ColorSpace::Unsupported,
        };
        let tint = match PdfFunction::from_object(doc, &tint_obj) {
            Ok(f) => f,
            Err(_) => return ColorSpace::Unsupported,
        };

        ColorSpace::Separation { n: 1, alt, tint }
    }

    /// DeviceN 色空間の解析。
    fn parse_device_n(
        doc: &Document,
        arr: &[Object],
        resources: &Dictionary,
        depth: u32,
    ) -> ColorSpace {
        // [/DeviceN names alt tint [attributes]]
        // names: arr[1]（名前配列）、alt: arr[2]、tint: arr[3]。
        let n = match arr.get(1).map(|o| doc.resolve(o)) {
            Some(Object::Array(a)) => a.len(),
            Some(Object::Name(_)) => 1, // 単一名前の場合（仕様上は配列だが耐故障）
            _ => return ColorSpace::Unsupported,
        };
        if n == 0 {
            return ColorSpace::Unsupported;
        }

        let alt_obj = match arr.get(2) {
            Some(o) => o.clone(),
            None => return ColorSpace::Unsupported,
        };
        let alt_resolved = doc.resolve(&alt_obj);
        let alt = Box::new(Self::parse_inner(doc, alt_resolved, resources, depth + 1));

        let tint_obj = match arr.get(3) {
            Some(o) => o.clone(),
            None => return ColorSpace::Unsupported,
        };
        let tint = match PdfFunction::from_object(doc, &tint_obj) {
            Ok(f) => f,
            Err(_) => return ColorSpace::Unsupported,
        };

        ColorSpace::Separation { n, alt, tint }
    }

    // --------------------------------------------------------
    // 情報アクセス
    // --------------------------------------------------------

    /// 成分数を返す。
    ///
    /// | 色空間 | 返値 |
    /// |---|---|
    /// | DeviceGray | 1 |
    /// | DeviceRGB, CalRGB（近似）, Lab | 3 |
    /// | DeviceCMYK | 4 |
    /// | Indexed | 1（インデックス値 1 つ） |
    /// | Separation | 1 |
    /// | DeviceN（n 成分） | n |
    /// | Unsupported | 1（縮退） |
    pub(crate) fn n_components(&self) -> usize {
        match self {
            ColorSpace::DeviceGray => 1,
            ColorSpace::DeviceRGB => 3,
            ColorSpace::DeviceCMYK => 4,
            ColorSpace::Lab { .. } => 3,
            ColorSpace::Indexed { .. } => 1,
            ColorSpace::Separation { n, .. } => *n,
            // Pattern: colored/Shading は 0 成分（パターン名のみ）、uncolored は base の成分数。
            ColorSpace::Pattern { base } => base.as_ref().map(|b| b.n_components()).unwrap_or(0),
            ColorSpace::Unsupported => 1,
        }
    }

    /// 各成分の既定 Decode 範囲を返す（画像描画用）。
    ///
    /// - DeviceGray / RGB / CMYK: `[0.0, 1.0]` × n。
    /// - Indexed: `[0.0, 2^bpc - 1.0]`（インデックス範囲）。
    /// - Lab: `[0, 100]`, `[a_min, a_max]`, `[b_min, b_max]`。
    /// - Separation / DeviceN: `[0.0, 1.0]` × n。
    /// - Unsupported: `[0.0, 1.0]`。
    ///
    /// 現時点では画像 XObject 描画タスクから使用される予定のため dead_code を許容する。
    #[allow(dead_code)]
    pub(crate) fn default_decode(&self, bits_per_component: u32) -> Vec<(f64, f64)> {
        match self {
            ColorSpace::DeviceGray => vec![(0.0, 1.0)],
            ColorSpace::DeviceRGB => vec![(0.0, 1.0); 3],
            ColorSpace::DeviceCMYK => vec![(0.0, 1.0); 4],
            ColorSpace::Lab { range, .. } => {
                vec![(0.0, 100.0), (range[0], range[1]), (range[2], range[3])]
            }
            ColorSpace::Indexed { hival, .. } => {
                // 画像の各ピクセルはインデックス値（bpc ビット）。
                let max = ((1u32 << bits_per_component) - 1).min(*hival as u32) as f64;
                vec![(0.0, max)]
            }
            ColorSpace::Separation { n, .. } => vec![(0.0, 1.0); *n],
            ColorSpace::Pattern { base } => match base {
                Some(b) => b.default_decode(bits_per_component),
                None => vec![],
            },
            ColorSpace::Unsupported => vec![(0.0, 1.0)],
        }
    }

    // --------------------------------------------------------
    // 変換
    // --------------------------------------------------------

    /// 成分値（0.0–1.0。ただし Lab は L/a/b 値、Indexed はインデックス）→ sRGB。
    ///
    /// 成分値が不足している場合は 0.0 で補う（縮退）。
    /// 未対応・エラーは `[0, 0, 0]`（黒）を返す。
    pub(crate) fn to_rgb(&self, comps: &[f64]) -> [u8; 3] {
        match self {
            ColorSpace::DeviceGray => {
                let v = comps.first().copied().unwrap_or(0.0);
                let g = comp(v);
                [g, g, g]
            }
            ColorSpace::DeviceRGB => {
                let r = comp(comps.first().copied().unwrap_or(0.0));
                let g = comp(comps.get(1).copied().unwrap_or(0.0));
                let b = comp(comps.get(2).copied().unwrap_or(0.0));
                [r, g, b]
            }
            ColorSpace::DeviceCMYK => {
                let c = comps.first().copied().unwrap_or(0.0);
                let m = comps.get(1).copied().unwrap_or(0.0);
                let y = comps.get(2).copied().unwrap_or(0.0);
                let k = comps.get(3).copied().unwrap_or(0.0);
                cmyk_to_rgb(c, m, y, k)
            }
            ColorSpace::Lab { white, range } => lab_to_rgb(comps, white, range),
            ColorSpace::Indexed {
                base,
                hival,
                lookup,
            } => {
                let idx = comps.first().copied().unwrap_or(0.0);
                let idx = (idx.round() as usize).min(*hival as usize);
                let n = base.n_components();
                let offset = idx * n;
                // lookup は 0–255 バイト列→ base 色空間の成分として読む。
                let base_comps: Vec<f64> = (0..n)
                    .map(|i| {
                        lookup
                            .get(offset + i)
                            .copied()
                            .map(|b| b as f64 / 255.0)
                            .unwrap_or(0.0)
                    })
                    .collect();
                base.to_rgb(&base_comps)
            }
            ColorSpace::Separation { alt, tint, .. } => {
                let out = tint.eval(comps);
                alt.to_rgb(&out)
            }
            // Pattern: 直接 to_rgb は使われない（パターン描画は別経路）。
            // uncolored の base 成分があれば base で評価、なければ黒。
            ColorSpace::Pattern { base } => match base {
                Some(b) => b.to_rgb(comps),
                None => [0, 0, 0],
            },
            ColorSpace::Unsupported => [0, 0, 0],
        }
    }
}

// ============================================================
// 色変換ヘルパ
// ============================================================

/// 0–1 の値を 0–255 にクランプして変換する。
#[inline]
pub(crate) fn comp(v: f64) -> u8 {
    let c = if v.is_finite() {
        v.clamp(0.0, 1.0)
    } else {
        0.0
    };
    (c * 255.0 + 0.5) as u8
}

/// CMYK → RGB（r = 1 − min(1, c + k)）。
///
/// state.rs の `cmyk_rgb` と同じ式。
#[inline]
pub(crate) fn cmyk_to_rgb(c: f64, m: f64, y: f64, k: f64) -> [u8; 3] {
    let conv = |v: f64, k: f64| -> u8 {
        let vv = if v.is_finite() { v.max(0.0) } else { 0.0 };
        let kk = if k.is_finite() { k.max(0.0) } else { 0.0 };
        let val = (1.0 - (vv + kk).min(1.0)).clamp(0.0, 1.0);
        (val * 255.0 + 0.5) as u8
    };
    [conv(c, k), conv(m, k), conv(y, k)]
}

/// L\*a\*b\* → sRGB 変換（近似）。
///
/// ## 近似内容
///
/// 1. L\*a\*b\* → XYZ の CIE 標準変換を実施。
/// 2. XYZ を /WhitePoint で直接正規化（D65 への Bradford 適応は省略）。
/// 3. 正規化済み XYZ → sRGB（IEC 61966-2-1 の線形変換行列）。
/// 4. sRGB ガンマ（2.4 乗逆変換）を適用。
///
/// Bradford 適応を省略するため、D50 白色点の Lab 画像では白がわずかに
/// 黄みがかる場合があるが、実用的な精度は確保している。
fn lab_to_rgb(comps: &[f64], white: &[f64; 3], _range: &[f64; 4]) -> [u8; 3] {
    let l = comps.first().copied().unwrap_or(0.0).clamp(0.0, 100.0);
    let a = comps.get(1).copied().unwrap_or(0.0);
    let b = comps.get(2).copied().unwrap_or(0.0);

    // L*a*b* → XYZ（CIE 標準）。
    let fy = (l + 16.0) / 116.0;
    let fx = a / 500.0 + fy;
    let fz = fy - b / 200.0;

    let cube = |t: f64| -> f64 {
        if t > 6.0 / 29.0 {
            t * t * t
        } else {
            3.0 * (6.0 / 29.0) * (6.0 / 29.0) * (t - 4.0 / 29.0)
        }
    };

    let xw = white[0].max(1e-10);
    let yw = white[1].max(1e-10);
    let zw = white[2].max(1e-10);

    let x = cube(fx) * xw;
    let y = cube(fy) * yw;
    let z = cube(fz) * zw;

    // XYZ（D65 相対。ここでは WhitePoint を D65 として近似）→ 線形 sRGB。
    // IEC 61966-2-1 行列。
    let r_lin = 3.2404542 * x - 1.5371385 * y - 0.4985314 * z;
    let g_lin = -0.9692660 * x + 1.8760108 * y + 0.0415560 * z;
    let b_lin = 0.0556434 * x - 0.2040259 * y + 1.0572252 * z;

    // sRGB ガンマ適用（IEC 61966-2-1）。
    let gamma = |v: f64| -> f64 {
        let v = v.clamp(0.0, 1.0);
        if v <= 0.0031308 {
            12.92 * v
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        }
    };

    [comp(gamma(r_lin)), comp(gamma(g_lin)), comp(gamma(b_lin))]
}

// ============================================================
// テスト
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Document;
    use crate::object::{Dictionary, Object, Stream};

    // --- ヘルパ ---

    fn name_obj(s: &str) -> Object {
        Object::name(s)
    }

    fn arr(items: Vec<Object>) -> Object {
        Object::Array(items)
    }

    fn num(v: f64) -> Object {
        Object::Real(v)
    }

    fn int_obj(v: i64) -> Object {
        Object::Integer(v)
    }

    fn empty_resources() -> Dictionary {
        Dictionary::new()
    }

    // --- DeviceGray ---

    #[test]
    fn device_gray_name() {
        let doc = Document::new();
        let cs = ColorSpace::parse(&doc, &name_obj("DeviceGray"), &empty_resources());
        assert!(matches!(cs, ColorSpace::DeviceGray));
        assert_eq!(cs.n_components(), 1);
        assert_eq!(cs.to_rgb(&[0.0]), [0, 0, 0]);
        assert_eq!(cs.to_rgb(&[1.0]), [255, 255, 255]);
    }

    // --- DeviceRGB ---

    #[test]
    fn device_rgb_name() {
        let doc = Document::new();
        let cs = ColorSpace::parse(&doc, &name_obj("DeviceRGB"), &empty_resources());
        assert!(matches!(cs, ColorSpace::DeviceRGB));
        assert_eq!(cs.n_components(), 3);
        assert_eq!(cs.to_rgb(&[1.0, 0.0, 0.0]), [255, 0, 0]);
    }

    // --- DeviceCMYK ---

    #[test]
    fn device_cmyk_name() {
        let doc = Document::new();
        let cs = ColorSpace::parse(&doc, &name_obj("DeviceCMYK"), &empty_resources());
        assert!(matches!(cs, ColorSpace::DeviceCMYK));
        assert_eq!(cs.n_components(), 4);
        // 純シアン → (0, 255, 255)
        assert_eq!(cs.to_rgb(&[1.0, 0.0, 0.0, 0.0]), [0, 255, 255]);
        // 純黒 → (0, 0, 0)
        assert_eq!(cs.to_rgb(&[0.0, 0.0, 0.0, 1.0]), [0, 0, 0]);
    }

    // --- ICCBased /N 近似 ---

    #[test]
    fn icc_based_n1_approximates_gray() {
        let doc = Document::new();
        let mut stream_dict = Dictionary::new();
        stream_dict.set("N", int_obj(1));
        let stream = Stream::new(stream_dict, vec![]);
        let cs_arr = arr(vec![name_obj("ICCBased"), Object::Stream(stream)]);
        let cs = ColorSpace::parse(&doc, &cs_arr, &empty_resources());
        assert!(matches!(cs, ColorSpace::DeviceGray));
        assert_eq!(cs.n_components(), 1);
    }

    #[test]
    fn icc_based_n3_approximates_rgb() {
        let doc = Document::new();
        let mut stream_dict = Dictionary::new();
        stream_dict.set("N", int_obj(3));
        let stream = Stream::new(stream_dict, vec![]);
        let cs_arr = arr(vec![name_obj("ICCBased"), Object::Stream(stream)]);
        let cs = ColorSpace::parse(&doc, &cs_arr, &empty_resources());
        assert!(matches!(cs, ColorSpace::DeviceRGB));
    }

    #[test]
    fn icc_based_n4_approximates_cmyk() {
        let doc = Document::new();
        let mut stream_dict = Dictionary::new();
        stream_dict.set("N", int_obj(4));
        let stream = Stream::new(stream_dict, vec![]);
        let cs_arr = arr(vec![name_obj("ICCBased"), Object::Stream(stream)]);
        let cs = ColorSpace::parse(&doc, &cs_arr, &empty_resources());
        assert!(matches!(cs, ColorSpace::DeviceCMYK));
    }

    // --- Indexed（文字列 lookup）---

    #[test]
    fn indexed_string_lookup() {
        let doc = Document::new();
        // [/Indexed /DeviceRGB 1 "\xFF\x00\x00\x00\xFF\x00"]
        // インデックス 0 → 赤 (1,0,0), インデックス 1 → 緑 (0,1,0)。
        let lookup_bytes: Vec<u8> = vec![255, 0, 0, 0, 255, 0];
        let lookup_obj = Object::String(lookup_bytes, crate::object::StringFormat::Literal);
        let cs_arr = arr(vec![
            name_obj("Indexed"),
            name_obj("DeviceRGB"),
            int_obj(1),
            lookup_obj,
        ]);
        let cs = ColorSpace::parse(&doc, &cs_arr, &empty_resources());
        assert!(matches!(cs, ColorSpace::Indexed { .. }));
        assert_eq!(cs.n_components(), 1);
        assert_eq!(cs.to_rgb(&[0.0]), [255, 0, 0]);
        assert_eq!(cs.to_rgb(&[1.0]), [0, 255, 0]);
    }

    // --- Indexed（ストリーム lookup）---

    #[test]
    fn indexed_stream_lookup() {
        let doc = Document::new();
        // lookup をストリームとして渡す（フィルタなしのストリーム）。
        let lookup_bytes: Vec<u8> = vec![0, 0, 0, 255, 255, 255]; // 黒・白
        let stream = Stream::new(Dictionary::new(), lookup_bytes);
        let cs_arr = arr(vec![
            name_obj("Indexed"),
            name_obj("DeviceRGB"),
            int_obj(1),
            Object::Stream(stream),
        ]);
        let cs = ColorSpace::parse(&doc, &cs_arr, &empty_resources());
        assert!(matches!(cs, ColorSpace::Indexed { .. }));
        assert_eq!(cs.to_rgb(&[0.0]), [0, 0, 0]);
        assert_eq!(cs.to_rgb(&[1.0]), [255, 255, 255]);
    }

    // --- Separation（Type 2 関数で白→単色）---

    #[test]
    fn separation_tint_type2() {
        let doc = Document::new();
        // Separation: tint=0 → 白, tint=1 → 赤 (RGB) になる Type2 関数。
        // C0=[1,0,0] C1=[0,0,0] ではなく、
        // C0=[1,1,1], C1=[1,0,0] とする（tint=0→白、tint=1→赤）。
        let mut func_dict = Dictionary::new();
        func_dict.set("FunctionType", int_obj(2));
        func_dict.set("Domain", arr(vec![num(0.0), num(1.0)]));
        func_dict.set("N", int_obj(1));
        func_dict.set("C0", arr(vec![num(1.0), num(1.0), num(1.0)])); // 白
        func_dict.set("C1", arr(vec![num(1.0), num(0.0), num(0.0)])); // 赤

        let cs_arr = arr(vec![
            name_obj("Separation"),
            name_obj("CustomInk"),
            name_obj("DeviceRGB"),
            Object::Dictionary(func_dict),
        ]);
        let cs = ColorSpace::parse(&doc, &cs_arr, &empty_resources());
        assert!(matches!(cs, ColorSpace::Separation { n: 1, .. }));
        assert_eq!(cs.n_components(), 1);

        // tint=0 → C0 → 白 (255, 255, 255)
        assert_eq!(cs.to_rgb(&[0.0]), [255, 255, 255]);
        // tint=1 → C1 → 赤 (255, 0, 0)
        assert_eq!(cs.to_rgb(&[1.0]), [255, 0, 0]);
    }

    // --- DeviceN ---

    #[test]
    fn device_n_two_components() {
        let doc = Document::new();
        // [/DeviceN [/C /M] /DeviceCMYK tint_func]
        // tint: 2 入力 → 4 出力（CMYK）。入力 0 → CMYK 成分 0, 入力 1 → CMYK 成分 1。
        // Type 2, N=1, C0=[0,0,0,0], C1=[1,0,0,0] × 2 成分だと DeviceN 向きではないので
        // ここでは単純に tint が成立する構成をテストする。
        // Type 2: C0=[0,0,0,0], C1=[1,0,0,0] → 入力 1 成分が CMYK 4 成分に写像される
        //（DeviceN は n 入力を取るが、PdfFunction の入力は 1 次元の Type2 で近似テスト）。
        let mut func_dict = Dictionary::new();
        func_dict.set("FunctionType", int_obj(2));
        func_dict.set("Domain", arr(vec![num(0.0), num(1.0)]));
        func_dict.set("N", int_obj(1));
        func_dict.set("C0", arr(vec![num(0.0), num(0.0), num(0.0), num(0.0)]));
        func_dict.set("C1", arr(vec![num(1.0), num(0.0), num(0.0), num(0.0)]));

        let names_arr = arr(vec![name_obj("C"), name_obj("M")]);
        let cs_arr = arr(vec![
            name_obj("DeviceN"),
            names_arr,
            name_obj("DeviceCMYK"),
            Object::Dictionary(func_dict),
        ]);
        let cs = ColorSpace::parse(&doc, &cs_arr, &empty_resources());
        // n=2（名前配列の長さ）
        assert!(matches!(cs, ColorSpace::Separation { n: 2, .. }));
        assert_eq!(cs.n_components(), 2);
    }

    // --- Lab ---

    #[test]
    fn lab_white_is_white() {
        // L=100, a=0, b=0 → 白（255, 255, 255）に近い値。
        let cs = ColorSpace::Lab {
            white: [0.95047, 1.0, 1.08883],
            range: [-128.0, 127.0, -128.0, 127.0],
        };
        let rgb = cs.to_rgb(&[100.0, 0.0, 0.0]);
        // 各チャンネルが 250 以上なら「白に近い」と判断する。
        assert!(
            rgb[0] >= 250 && rgb[1] >= 250 && rgb[2] >= 250,
            "L=100 → {:?}",
            rgb
        );
    }

    #[test]
    fn lab_black_is_black() {
        // L=0, a=0, b=0 → 黒（0, 0, 0）に近い値。
        let cs = ColorSpace::Lab {
            white: [0.95047, 1.0, 1.08883],
            range: [-128.0, 127.0, -128.0, 127.0],
        };
        let rgb = cs.to_rgb(&[0.0, 0.0, 0.0]);
        assert!(rgb[0] <= 5 && rgb[1] <= 5 && rgb[2] <= 5, "L=0 → {:?}", rgb);
    }

    #[test]
    fn lab_positive_a_is_reddish() {
        // L=50, a=+60, b=0 → 赤みがかった色（R > B）。
        let cs = ColorSpace::Lab {
            white: [0.95047, 1.0, 1.08883],
            range: [-128.0, 127.0, -128.0, 127.0],
        };
        let rgb = cs.to_rgb(&[50.0, 60.0, 0.0]);
        // 赤成分が青成分より大きい。
        assert!(rgb[0] > rgb[2], "a=+60 → {:?}", rgb);
    }

    #[test]
    fn lab_negative_a_is_greenish() {
        // L=50, a=-60, b=0 → 緑みがかった色（G > R）。
        let cs = ColorSpace::Lab {
            white: [0.95047, 1.0, 1.08883],
            range: [-128.0, 127.0, -128.0, 127.0],
        };
        let rgb = cs.to_rgb(&[50.0, -60.0, 0.0]);
        assert!(rgb[1] > rgb[0], "a=-60 → {:?}", rgb);
    }

    // --- Unsupported ---

    #[test]
    fn pattern_name_is_colored_pattern() {
        let doc = Document::new();
        let cs = ColorSpace::parse(&doc, &name_obj("Pattern"), &empty_resources());
        assert!(matches!(cs, ColorSpace::Pattern { base: None }));
        // 直接 to_rgb は使われない経路だが、念のため黒に縮退することを確認。
        assert_eq!(cs.to_rgb(&[0.5]), [0, 0, 0]);
        // colored Tiling / Shading は scn でパターン名だけを受けるため成分数 0。
        assert_eq!(cs.n_components(), 0);
    }

    #[test]
    fn pattern_array_with_base_is_uncolored() {
        let doc = Document::new();
        // [/Pattern /DeviceRGB] = uncolored Tiling、scn はベース色 3 成分 + 名前。
        let cs_arr = arr(vec![name_obj("Pattern"), name_obj("DeviceRGB")]);
        let cs = ColorSpace::parse(&doc, &cs_arr, &empty_resources());
        assert!(matches!(cs, ColorSpace::Pattern { base: Some(_) }));
        assert_eq!(cs.n_components(), 3);
    }

    #[test]
    fn unknown_name_is_unsupported() {
        let doc = Document::new();
        let cs = ColorSpace::parse(&doc, &name_obj("UnknownCS"), &empty_resources());
        assert!(matches!(cs, ColorSpace::Unsupported));
    }

    // --- CalGray / CalRGB 近似 ---

    #[test]
    fn cal_gray_approximated_as_device_gray() {
        let doc = Document::new();
        let cs_arr = arr(vec![
            name_obj("CalGray"),
            Object::Dictionary(Dictionary::new()),
        ]);
        let cs = ColorSpace::parse(&doc, &cs_arr, &empty_resources());
        assert!(matches!(cs, ColorSpace::DeviceGray));
    }

    #[test]
    fn cal_rgb_approximated_as_device_rgb() {
        let doc = Document::new();
        let cs_arr = arr(vec![
            name_obj("CalRGB"),
            Object::Dictionary(Dictionary::new()),
        ]);
        let cs = ColorSpace::parse(&doc, &cs_arr, &empty_resources());
        assert!(matches!(cs, ColorSpace::DeviceRGB));
    }

    // --- リソース /ColorSpace 辞書参照 ---

    #[test]
    fn resource_colorspace_lookup() {
        let doc = Document::new();
        // /Resources /ColorSpace /CS1 = [/ICCBased stream(N=3)]
        let mut stream_dict = Dictionary::new();
        stream_dict.set("N", int_obj(3));
        let icc_stream = Object::Stream(Stream::new(stream_dict, vec![]));
        let icc_arr = arr(vec![name_obj("ICCBased"), icc_stream]);

        let mut cs_dict = Dictionary::new();
        cs_dict.set("CS1", icc_arr);

        let mut resources = Dictionary::new();
        resources.set("ColorSpace", Object::Dictionary(cs_dict));

        let cs = ColorSpace::parse(&doc, &name_obj("CS1"), &resources);
        assert!(matches!(cs, ColorSpace::DeviceRGB));
    }

    // --- default_decode ---

    #[test]
    fn default_decode_indexed() {
        let cs = ColorSpace::Indexed {
            base: Box::new(ColorSpace::DeviceRGB),
            hival: 255,
            lookup: vec![],
        };
        let ranges = cs.default_decode(8);
        assert_eq!(ranges, vec![(0.0, 255.0)]);
    }

    #[test]
    fn default_decode_lab() {
        let cs = ColorSpace::Lab {
            white: [0.95047, 1.0, 1.08883],
            range: [-100.0, 100.0, -100.0, 100.0],
        };
        let ranges = cs.default_decode(8);
        assert_eq!(ranges, vec![(0.0, 100.0), (-100.0, 100.0), (-100.0, 100.0)]);
    }
}
