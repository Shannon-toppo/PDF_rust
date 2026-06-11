//! PDF 関数オブジェクトのインタプリタ（PDF 32000-1:2008 §7.10）。
//!
//! ## 対応タイプ
//!
//! | タイプ | 種別 |
//! |---|---|
//! | Type 0 | サンプル関数（多次元線形補間。3 次元以上は最近傍） |
//! | Type 2 | 指数補間関数 |
//! | Type 3 | 継ぎ接ぎ関数 |
//! | Type 4 | PostScript 電卓関数 |
//!
//! ## 耐故障性
//!
//! 関数データは信頼できない入力として扱う。
//! パース・評価エラーで panic しない。不正データは `0.0` 詰めで縮退する。

use crate::document::Document;
use crate::error::{PdfError, Result};
use crate::object::{Object, Stream};

// ============================================================
// 公開 API
// ============================================================

/// PDF 関数（§7.10）。
///
/// [`PdfFunction::from_object`] で構築し、[`PdfFunction::eval`] で評価する。
#[derive(Debug, Clone)]
pub struct PdfFunction {
    /// 実装の詳細（タイプ別に分岐）。
    inner: FunctionInner,
    /// 入力クランプ範囲（`[min, max]` のペアが n_inputs 個）。
    domain: Vec<[f64; 2]>,
    /// 出力クランプ範囲（`[min, max]` のペアが n_outputs 個）。`None` はクランプなし。
    range: Option<Vec<[f64; 2]>>,
}

impl PdfFunction {
    // --------------------------------------------------------
    // 構築
    // --------------------------------------------------------

    /// 関数オブジェクト（辞書またはストリーム。間接参照可）から構築する。
    pub fn from_object(doc: &Document, obj: &Object) -> Result<PdfFunction> {
        let obj = doc.resolve(obj);
        let dict = obj.as_dict()?;

        // /FunctionType
        let func_type = doc
            .dict_get(dict, "FunctionType")
            .ok_or(PdfError::MissingKey("FunctionType"))?
            .as_int()
            .unwrap_or(-1);

        // /Domain
        let domain = parse_number_pair_array(
            doc,
            dict.get("Domain").ok_or(PdfError::MissingKey("Domain"))?,
        )?;
        if domain.is_empty() || domain.len() % 2 != 0 {
            return Err(PdfError::Invalid(
                "/Domain must have an even number of values".into(),
            ));
        }
        let n_inputs = domain.len() / 2;
        let domain_pairs: Vec<[f64; 2]> = domain.chunks_exact(2).map(|c| [c[0], c[1]]).collect();

        // /Range（オプション）
        let range_pairs: Option<Vec<[f64; 2]>> = match dict.get("Range") {
            None => None,
            Some(r) => {
                let rv = parse_number_pair_array(doc, r)?;
                if rv.len() % 2 != 0 {
                    None
                } else {
                    Some(rv.chunks_exact(2).map(|c| [c[0], c[1]]).collect())
                }
            }
        };

        let inner = match func_type {
            0 => {
                let stream = obj.as_stream()?;
                build_type0(doc, dict, stream, n_inputs, range_pairs.as_deref())?
            }
            2 => build_type2(doc, dict, n_inputs, range_pairs.as_deref())?,
            3 => build_type3(doc, dict, n_inputs, range_pairs.as_deref())?,
            4 => {
                let stream = obj.as_stream()?;
                build_type4(doc, stream, n_inputs, range_pairs.as_deref())?
            }
            _ => {
                return Err(PdfError::Invalid(format!(
                    "unsupported /FunctionType {}",
                    func_type
                )))
            }
        };

        Ok(PdfFunction {
            inner,
            domain: domain_pairs,
            range: range_pairs,
        })
    }

    // --------------------------------------------------------
    // 情報アクセス
    // --------------------------------------------------------

    /// 入力次元数（`/Domain` の要素数の半分）。
    pub fn n_inputs(&self) -> usize {
        self.domain.len()
    }

    /// 出力次元数。Type 0/2/3 では常に確定。
    /// Type 4 は `/Range` があれば確定、なければ `None`。
    pub fn n_outputs(&self) -> Option<usize> {
        match &self.inner {
            FunctionInner::Type0(f) => Some(f.n_outputs),
            FunctionInner::Type2(f) => Some(f.c0.len()),
            FunctionInner::Type3(f) => f.functions.first().and_then(|g| g.n_outputs()),
            FunctionInner::Type4(_) => self.range.as_ref().map(|r| r.len()),
        }
    }

    // --------------------------------------------------------
    // 評価
    // --------------------------------------------------------

    /// 関数を評価する。
    ///
    /// - 入力は `/Domain` でクランプしてから評価する。
    /// - 出力は `/Range` があればクランプする。
    /// - 入力長が不足している場合は末尾を `0.0` で補う（縮退）。
    /// - エラーが生じた場合は `0.0` を n_outputs 個返す（縮退）。
    pub fn eval(&self, inputs: &[f64]) -> Vec<f64> {
        // 入力をクランプ
        let n = self.domain.len();
        let mut clamped: Vec<f64> = Vec::with_capacity(n);
        for (i, dom) in self.domain.iter().enumerate() {
            let v = inputs.get(i).copied().unwrap_or(0.0);
            clamped.push(clamp(v, dom[0], dom[1]));
        }

        let mut out = match &self.inner {
            FunctionInner::Type0(f) => f.eval(&clamped),
            FunctionInner::Type2(f) => f.eval(&clamped),
            FunctionInner::Type3(f) => f.eval(&clamped),
            FunctionInner::Type4(f) => f.eval(&clamped),
        };

        // 出力をクランプ
        if let Some(range) = &self.range {
            for (v, r) in out.iter_mut().zip(range.iter()) {
                *v = clamp(*v, r[0], r[1]);
            }
        }

        out
    }
}

// ============================================================
// 内部実装
// ============================================================

#[derive(Debug, Clone)]
enum FunctionInner {
    Type0(Type0Function),
    Type2(Type2Function),
    Type3(Type3Function),
    Type4(Type4Function),
}

// ============================================================
// Type 0: サンプル関数
// ============================================================

/// Type 0 サンプル関数の内部表現。
#[derive(Debug, Clone)]
struct Type0Function {
    /// 各入力次元のサンプル数（`/Size`）。
    size: Vec<usize>,
    /// 出力次元数（`/Range` の要素数の半分）。
    n_outputs: usize,
    /// 各入力次元の Encode 範囲（`[e0, e1]`、既定は `[0, size_i - 1]`）。
    encode: Vec<[f64; 2]>,
    /// 各出力次元の Decode 範囲（`[d0, d1]`、既定は Range の各ペア）。
    decode: Vec<[f64; 2]>,
    /// サンプルテーブル。行優先で展開済み（整数化済み。`[0, 2^bps - 1]`）。
    /// インデックス: `((i_m * size_{m-1} + i_{m-1}) * ... * size_0 + i_0) * n_outputs + ch`
    samples: Vec<u32>,
    /// ビット深度（`/BitsPerSample`）。
    bits_per_sample: u32,
}

/// Type 0 関数の構築。
fn build_type0(
    doc: &Document,
    dict: &crate::object::Dictionary,
    stream: &Stream,
    n_inputs: usize,
    range: Option<&[[f64; 2]]>,
) -> Result<FunctionInner> {
    // /Range は必須
    let range = range.ok_or(PdfError::MissingKey("Range"))?;
    let n_outputs = range.len();

    // /BitsPerSample
    let bps = doc
        .dict_get(dict, "BitsPerSample")
        .ok_or(PdfError::MissingKey("BitsPerSample"))?
        .as_int()
        .unwrap_or(8) as u32;
    if !matches!(bps, 1 | 2 | 4 | 8 | 12 | 16 | 24 | 32) {
        return Err(PdfError::Invalid(format!("invalid /BitsPerSample {}", bps)));
    }

    // /Size
    let size_obj = doc
        .dict_get(dict, "Size")
        .ok_or(PdfError::MissingKey("Size"))?;
    let size_arr = size_obj
        .as_array()
        .map_err(|_| PdfError::Invalid("/Size must be an array".into()))?;
    if size_arr.len() != n_inputs {
        return Err(PdfError::Invalid(
            "/Size length must match input dimension".into(),
        ));
    }
    let size: Vec<usize> = size_arr
        .iter()
        .map(|o| doc.resolve(o).as_int().unwrap_or(1).max(1) as usize)
        .collect();

    // /Encode（既定: [0, size_i - 1]）
    let encode: Vec<[f64; 2]> = match doc.dict_get(dict, "Encode") {
        Some(o) => {
            if let Ok(arr) = o.as_array() {
                if arr.len() >= n_inputs * 2 {
                    arr.chunks_exact(2)
                        .take(n_inputs)
                        .map(|c| {
                            let a = doc.resolve(&c[0]).as_number().unwrap_or(0.0);
                            let b = doc
                                .resolve(&c[1])
                                .as_number()
                                .unwrap_or(*size.first().unwrap_or(&1) as f64 - 1.0);
                            [a, b]
                        })
                        .collect()
                } else {
                    default_encode(&size)
                }
            } else {
                default_encode(&size)
            }
        }
        None => default_encode(&size),
    };

    // /Decode（既定: Range の各ペア）
    let decode: Vec<[f64; 2]> = match doc.dict_get(dict, "Decode") {
        Some(o) => {
            if let Ok(arr) = o.as_array() {
                if arr.len() >= n_outputs * 2 {
                    arr.chunks_exact(2)
                        .take(n_outputs)
                        .map(|c| {
                            let a = doc.resolve(&c[0]).as_number().unwrap_or(0.0);
                            let b = doc.resolve(&c[1]).as_number().unwrap_or(1.0);
                            [a, b]
                        })
                        .collect()
                } else {
                    range.to_vec()
                }
            } else {
                range.to_vec()
            }
        }
        None => range.to_vec(),
    };

    // ストリームデータを展開してサンプルテーブルを構築
    let data = doc.get_stream_data(stream)?;
    let total_samples: usize = size.iter().product::<usize>() * n_outputs;
    let mut samples = Vec::with_capacity(total_samples);
    let max_val = (1u64 << bps).saturating_sub(1) as u32;

    // ビッグエンディアンビットパッキングで読み取る
    let mut bit_buf: u64 = 0;
    let mut bits_in_buf: u32 = 0;
    let mut byte_pos = 0usize;

    for _ in 0..total_samples {
        // buf に bps ビット積み込む
        while bits_in_buf < bps {
            let byte = data.get(byte_pos).copied().unwrap_or(0);
            byte_pos += 1;
            bit_buf = (bit_buf << 8) | (byte as u64);
            bits_in_buf = bits_in_buf.saturating_add(8);
        }
        let shift = bits_in_buf - bps;
        let val = ((bit_buf >> shift) & (max_val as u64)) as u32;
        bits_in_buf -= bps;
        bit_buf &= (1u64 << bits_in_buf) - 1;
        samples.push(val);
    }

    // 足りない場合は 0 で埋める（耐故障）
    while samples.len() < total_samples {
        samples.push(0);
    }

    Ok(FunctionInner::Type0(Type0Function {
        size,
        n_outputs,
        encode,
        decode,
        samples,
        bits_per_sample: bps,
    }))
}

/// Encode の既定値: `[0, size_i - 1]`。
fn default_encode(size: &[usize]) -> Vec<[f64; 2]> {
    size.iter()
        .map(|&s| [0.0, s.saturating_sub(1) as f64])
        .collect()
}

impl Type0Function {
    fn eval(&self, inputs: &[f64]) -> Vec<f64> {
        let n = self.size.len();

        // 各入力次元を Encode でサンプル座標に変換しクランプ
        let mut coords: Vec<f64> = Vec::with_capacity(n);
        for (i, &dom_val) in inputs.iter().enumerate().take(n) {
            let enc = self.encode.get(i).copied().unwrap_or([0.0, 1.0]);
            let sz = *self.size.get(i).unwrap_or(&1) as f64;
            let mapped = interpolate(dom_val, 0.0, 1.0, enc[0], enc[1]);
            coords.push(clamp(mapped, 0.0, (sz - 1.0).max(0.0)));
        }

        if n <= 2 {
            self.eval_linear_interp(&coords)
        } else {
            // 3 次元以上は最近傍（仕様準拠の線形補間は計算量が 2^n 倍かかるため省略）
            self.eval_nearest(&coords)
        }
    }

    /// 1–2 次元の多線形補間。
    fn eval_linear_interp(&self, coords: &[f64]) -> Vec<f64> {
        let max_val = (1u64 << self.bits_per_sample).saturating_sub(1) as f64;

        match coords.len() {
            1 => {
                let x = coords[0];
                let x0 = x.floor() as usize;
                let x1 = (x0 + 1).min(self.size.first().copied().unwrap_or(1).saturating_sub(1));
                let fx = x - x0 as f64;
                (0..self.n_outputs)
                    .map(|ch| {
                        let s0 = self.sample(x0, 0, 0, ch) as f64;
                        let s1 = self.sample(x1, 0, 0, ch) as f64;
                        let raw = s0 + (s1 - s0) * fx;
                        let dec = self.decode.get(ch).copied().unwrap_or([0.0, 1.0]);
                        interpolate(raw, 0.0, max_val, dec[0], dec[1])
                    })
                    .collect()
            }
            2 => {
                let x = coords[0];
                let y = coords[1];
                let x0 = x.floor() as usize;
                let y0 = y.floor() as usize;
                let x1 = (x0 + 1).min(self.size.first().copied().unwrap_or(1).saturating_sub(1));
                let y1 = (y0 + 1).min(self.size.get(1).copied().unwrap_or(1).saturating_sub(1));
                let fx = x - x0 as f64;
                let fy = y - y0 as f64;
                (0..self.n_outputs)
                    .map(|ch| {
                        let s00 = self.sample(x0, y0, 0, ch) as f64;
                        let s10 = self.sample(x1, y0, 0, ch) as f64;
                        let s01 = self.sample(x0, y1, 0, ch) as f64;
                        let s11 = self.sample(x1, y1, 0, ch) as f64;
                        let raw = s00 * (1.0 - fx) * (1.0 - fy)
                            + s10 * fx * (1.0 - fy)
                            + s01 * (1.0 - fx) * fy
                            + s11 * fx * fy;
                        let dec = self.decode.get(ch).copied().unwrap_or([0.0, 1.0]);
                        interpolate(raw, 0.0, max_val, dec[0], dec[1])
                    })
                    .collect()
            }
            _ => self.eval_nearest(coords),
        }
    }

    /// 最近傍（3 次元以上、または 0 次元の縮退）。
    fn eval_nearest(&self, coords: &[f64]) -> Vec<f64> {
        let max_val = (1u64 << self.bits_per_sample).saturating_sub(1) as f64;
        // 各次元を整数インデックスに丸める
        let idx: Vec<usize> = coords
            .iter()
            .zip(self.size.iter())
            .map(|(&c, &sz)| (c.round() as usize).min(sz.saturating_sub(1)))
            .collect();

        // 線形インデックス計算（行優先）
        let flat = self.flat_index(&idx);
        (0..self.n_outputs)
            .map(|ch| {
                let raw = self
                    .samples
                    .get(flat * self.n_outputs + ch)
                    .copied()
                    .unwrap_or(0) as f64;
                let dec = self.decode.get(ch).copied().unwrap_or([0.0, 1.0]);
                interpolate(raw, 0.0, max_val, dec[0], dec[1])
            })
            .collect()
    }

    /// 2D インデックス (ix, iy) のサンプルを取得（3D 以上は sample_nd を使う）。
    fn sample(&self, ix: usize, iy: usize, _iz: usize, ch: usize) -> u32 {
        let sx = self.size.first().copied().unwrap_or(1);
        let sy = self.size.get(1).copied().unwrap_or(1);
        let ix = ix.min(sx.saturating_sub(1));
        let iy = iy.min(sy.saturating_sub(1));
        let flat = iy * sx + ix;
        self.samples
            .get(flat * self.n_outputs + ch)
            .copied()
            .unwrap_or(0)
    }

    /// 任意次元の線形インデックス。
    fn flat_index(&self, idx: &[usize]) -> usize {
        let mut flat = 0usize;
        let mut stride = 1usize;
        for (i, &ix) in idx.iter().enumerate() {
            flat = flat.saturating_add(ix.saturating_mul(stride));
            stride = stride.saturating_mul(*self.size.get(i).unwrap_or(&1));
        }
        flat
    }
}

// ============================================================
// Type 2: 指数補間関数
// ============================================================

/// Type 2 指数補間関数の内部表現。
#[derive(Debug, Clone)]
struct Type2Function {
    /// C0（既定: 全要素 0.0）。
    c0: Vec<f64>,
    /// C1（既定: 全要素 1.0）。
    c1: Vec<f64>,
    /// 指数 N。
    n: f64,
}

/// Type 2 関数の構築。
fn build_type2(
    doc: &Document,
    dict: &crate::object::Dictionary,
    _n_inputs: usize,
    _range: Option<&[[f64; 2]]>,
) -> Result<FunctionInner> {
    let n_exp = doc
        .dict_get(dict, "N")
        .ok_or(PdfError::MissingKey("N"))?
        .as_number()
        .unwrap_or(1.0);

    // /C0（既定: [0.0]）
    let c0 = read_optional_number_array(doc, dict, "C0", &[0.0]);
    // /C1（既定: [1.0]）
    let c1 = read_optional_number_array(doc, dict, "C1", &[1.0]);

    Ok(FunctionInner::Type2(Type2Function { c0, c1, n: n_exp }))
}

impl Type2Function {
    fn eval(&self, inputs: &[f64]) -> Vec<f64> {
        // 入力は 1 次元（仕様上 Domain は 2 値 = 1 入力）
        let x = inputs.first().copied().unwrap_or(0.0).max(0.0);
        let xn = x.powf(self.n);
        self.c0
            .iter()
            .zip(self.c1.iter())
            .map(|(c0, c1)| c0 + xn * (c1 - c0))
            .collect()
    }
}

// ============================================================
// Type 3: 継ぎ接ぎ関数
// ============================================================

/// Type 3 継ぎ接ぎ関数の内部表現。
#[derive(Debug, Clone)]
struct Type3Function {
    /// 部分関数リスト。
    functions: Vec<PdfFunction>,
    /// 境界値リスト（`/Bounds`; n-1 個で n 個の部分関数に対応）。
    bounds: Vec<f64>,
    /// 各部分関数への入力再写像 `[e0, e1]`（`/Encode`; 2n 個）。
    encode: Vec<[f64; 2]>,
}

/// Type 3 関数の構築。
fn build_type3(
    doc: &Document,
    dict: &crate::object::Dictionary,
    _n_inputs: usize,
    _range: Option<&[[f64; 2]]>,
) -> Result<FunctionInner> {
    // /Functions
    let funcs_obj = doc
        .dict_get(dict, "Functions")
        .ok_or(PdfError::MissingKey("Functions"))?;
    let funcs_arr = funcs_obj
        .as_array()
        .map_err(|_| PdfError::Invalid("/Functions must be an array".into()))?
        .clone();

    let functions: Vec<PdfFunction> = funcs_arr
        .iter()
        .filter_map(|o| PdfFunction::from_object(doc, o).ok())
        .collect();

    if functions.is_empty() {
        return Err(PdfError::Invalid(
            "/Functions array is empty or all failed to parse".into(),
        ));
    }

    // /Bounds
    let bounds = match doc.dict_get(dict, "Bounds") {
        Some(o) => o
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| doc.resolve(v).as_number().ok())
                    .collect::<Vec<f64>>()
            })
            .unwrap_or_default(),
        None => Vec::new(),
    };

    // /Encode: 部分関数 i に渡す前の線形変換 [e0_i, e1_i]
    let encode: Vec<[f64; 2]> = match doc.dict_get(dict, "Encode") {
        Some(o) => {
            if let Ok(arr) = o.as_array() {
                arr.chunks_exact(2)
                    .map(|c| {
                        let a = doc.resolve(&c[0]).as_number().unwrap_or(0.0);
                        let b = doc.resolve(&c[1]).as_number().unwrap_or(1.0);
                        [a, b]
                    })
                    .collect()
            } else {
                Vec::new()
            }
        }
        None => Vec::new(),
    };

    Ok(FunctionInner::Type3(Type3Function {
        functions,
        bounds,
        encode,
    }))
}

impl Type3Function {
    fn eval(&self, inputs: &[f64]) -> Vec<f64> {
        let x = inputs.first().copied().unwrap_or(0.0);
        let k = self.functions.len();
        if k == 0 {
            return Vec::new();
        }

        // どのサブ区間かを判定
        let mut idx = k - 1;
        for (i, &b) in self.bounds.iter().enumerate() {
            if x < b {
                idx = i;
                break;
            }
        }
        idx = idx.min(k - 1);

        // サブ区間の入力ドメイン
        let dom_min = if idx == 0 {
            0.0_f64 // 実際は Domain[0]; ここでは既にクランプ済み
        } else {
            self.bounds.get(idx - 1).copied().unwrap_or(0.0)
        };
        let dom_max = self.bounds.get(idx).copied().unwrap_or(1.0); // 最後の区間は Domain[1]; ここでは 1.0 で近似

        // /Encode で再写像
        let enc = self.encode.get(idx).copied().unwrap_or([0.0, 1.0]);
        let x_enc = if (dom_max - dom_min).abs() < 1e-12 {
            enc[0]
        } else {
            interpolate(x, dom_min, dom_max, enc[0], enc[1])
        };

        // サブ関数を呼ぶ（Domain クランプは PdfFunction::eval が行う）
        match self.functions.get(idx) {
            Some(f) => f.eval(&[x_enc]),
            None => Vec::new(),
        }
    }
}

// ============================================================
// Type 4: PostScript 電卓関数
// ============================================================

/// Type 4 PostScript 電卓関数の内部表現。
#[derive(Debug, Clone)]
struct Type4Function {
    /// トークン列（手続き）。
    tokens: Vec<Ps4Token>,
}

/// PostScript 電卓のトークン。
#[derive(Debug, Clone)]
enum Ps4Token {
    /// 数値リテラル。
    Number(f64),
    /// 演算子。
    Op(Ps4Op),
    /// ネストした手続き `{ ... }`。
    Proc(Vec<Ps4Token>),
}

/// PostScript 電卓演算子。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ps4Op {
    // 算術
    Add,
    Sub,
    Mul,
    Div,
    Idiv,
    Mod,
    Neg,
    Abs,
    Ceiling,
    Floor,
    Round,
    Truncate,
    Sqrt,
    Sin,
    Cos,
    Atan,
    Exp,
    Ln,
    Log,
    Cvi,
    Cvr,
    // 比較
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
    // 論理・ビット
    And,
    Or,
    Xor,
    Not,
    Bitshift,
    True,
    False,
    // スタック
    Pop,
    Exch,
    Dup,
    Copy,
    Index,
    Roll,
    // 制御
    If,
    Ifelse,
}

/// PostScript スタック値。
#[derive(Debug, Clone)]
enum Ps4Value {
    Number(f64),
    Bool(bool),
    Proc(Vec<Ps4Token>),
}

impl Ps4Value {
    fn as_number(&self) -> f64 {
        match self {
            Ps4Value::Number(v) => *v,
            Ps4Value::Bool(b) => {
                if *b {
                    1.0
                } else {
                    0.0
                }
            }
            _ => 0.0,
        }
    }
    fn as_bool(&self) -> bool {
        match self {
            Ps4Value::Bool(b) => *b,
            Ps4Value::Number(v) => *v != 0.0,
            _ => false,
        }
    }
    fn as_int(&self) -> i64 {
        self.as_number() as i64
    }
}

/// Type 4 関数の構築。
fn build_type4(
    doc: &Document,
    stream: &Stream,
    _n_inputs: usize,
    _range: Option<&[[f64; 2]]>,
) -> Result<FunctionInner> {
    let data = doc.get_stream_data(stream)?;
    let src = std::str::from_utf8(&data).unwrap_or("");
    let raw = tokenize_ps4(src);
    // PDF Type 4 ストリームの本体は常に `{ ... }` で囲まれている。
    // 最外層が単一の手続きなら中身を展開して直接実行できるようにする。
    let tokens = if raw.len() == 1 {
        match raw.into_iter().next() {
            Some(Ps4Token::Proc(inner)) => inner,
            Some(other) => vec![other],
            None => Vec::new(),
        }
    } else {
        raw
    };
    Ok(FunctionInner::Type4(Type4Function { tokens }))
}

/// PostScript トークナイザ。`{ ... }` を再帰的に解析する。
fn tokenize_ps4(src: &str) -> Vec<Ps4Token> {
    tokenize_ps4_inner(src).0
}

fn tokenize_ps4_inner(src: &str) -> (Vec<Ps4Token>, usize) {
    let bytes = src.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        // 空白読み飛ばし
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        // コメント（% 以降を行末まで読み飛ばし）
        if bytes[i] == b'%' {
            while i < bytes.len() && bytes[i] != b'\n' && bytes[i] != b'\r' {
                i += 1;
            }
            continue;
        }
        // 手続き開始
        if bytes[i] == b'{' {
            i += 1;
            let (sub, consumed) = tokenize_ps4_inner(&src[i..]);
            i += consumed;
            tokens.push(Ps4Token::Proc(sub));
            continue;
        }
        // 手続き終了
        if bytes[i] == b'}' {
            i += 1;
            return (tokens, i);
        }
        // トークン（数値またはキーワード）
        let start = i;
        while i < bytes.len()
            && !bytes[i].is_ascii_whitespace()
            && bytes[i] != b'{'
            && bytes[i] != b'}'
            && bytes[i] != b'%'
        {
            i += 1;
        }
        let word = &src[start..i];
        if let Some(tok) = parse_ps4_token(word) {
            tokens.push(tok);
        }
        // 不明トークンは読み飛ばす（耐故障）
    }
    (tokens, i)
}

/// 単語を Ps4Token に変換する。
fn parse_ps4_token(word: &str) -> Option<Ps4Token> {
    // 数値判定
    if let Ok(v) = word.parse::<f64>() {
        return Some(Ps4Token::Number(v));
    }
    // 演算子
    let op = match word {
        "add" => Ps4Op::Add,
        "sub" => Ps4Op::Sub,
        "mul" => Ps4Op::Mul,
        "div" => Ps4Op::Div,
        "idiv" => Ps4Op::Idiv,
        "mod" => Ps4Op::Mod,
        "neg" => Ps4Op::Neg,
        "abs" => Ps4Op::Abs,
        "ceiling" => Ps4Op::Ceiling,
        "floor" => Ps4Op::Floor,
        "round" => Ps4Op::Round,
        "truncate" => Ps4Op::Truncate,
        "sqrt" => Ps4Op::Sqrt,
        "sin" => Ps4Op::Sin,
        "cos" => Ps4Op::Cos,
        "atan" => Ps4Op::Atan,
        "exp" => Ps4Op::Exp,
        "ln" => Ps4Op::Ln,
        "log" => Ps4Op::Log,
        "cvi" => Ps4Op::Cvi,
        "cvr" => Ps4Op::Cvr,
        "eq" => Ps4Op::Eq,
        "ne" => Ps4Op::Ne,
        "gt" => Ps4Op::Gt,
        "ge" => Ps4Op::Ge,
        "lt" => Ps4Op::Lt,
        "le" => Ps4Op::Le,
        "and" => Ps4Op::And,
        "or" => Ps4Op::Or,
        "xor" => Ps4Op::Xor,
        "not" => Ps4Op::Not,
        "bitshift" => Ps4Op::Bitshift,
        "true" => Ps4Op::True,
        "false" => Ps4Op::False,
        "pop" => Ps4Op::Pop,
        "exch" => Ps4Op::Exch,
        "dup" => Ps4Op::Dup,
        "copy" => Ps4Op::Copy,
        "index" => Ps4Op::Index,
        "roll" => Ps4Op::Roll,
        "if" => Ps4Op::If,
        "ifelse" => Ps4Op::Ifelse,
        _ => return None,
    };
    Some(Ps4Token::Op(op))
}

/// PostScript 電卓スタック上限。
const PS4_STACK_LIMIT: usize = 100;
/// ネスト手続き深さ上限。
const PS4_DEPTH_LIMIT: u32 = 64;

impl Type4Function {
    fn eval(&self, inputs: &[f64]) -> Vec<f64> {
        let mut stack: Vec<Ps4Value> = inputs.iter().map(|&v| Ps4Value::Number(v)).collect();

        exec_tokens(&self.tokens, &mut stack, 0);

        // スタック上に残っている数値を出力とする
        stack.iter().map(|v| v.as_number()).collect()
    }
}

/// トークン列を実行する（再帰深さ制限あり）。
fn exec_tokens(tokens: &[Ps4Token], stack: &mut Vec<Ps4Value>, depth: u32) {
    if depth > PS4_DEPTH_LIMIT {
        return;
    }
    for tok in tokens {
        if stack.len() > PS4_STACK_LIMIT {
            // スタックオーバーフロー: 先頭の余分な要素を捨てる（縮退）
            let excess = stack.len() - PS4_STACK_LIMIT;
            stack.drain(0..excess);
        }
        match tok {
            Ps4Token::Number(v) => stack.push(Ps4Value::Number(*v)),
            Ps4Token::Proc(p) => stack.push(Ps4Value::Proc(p.clone())),
            Ps4Token::Op(op) => exec_op(*op, stack, depth),
        }
    }
}

/// 演算子を 1 つ実行する。スタックアンダーフロー・型不一致は縮退（何もしない）。
fn exec_op(op: Ps4Op, stack: &mut Vec<Ps4Value>, depth: u32) {
    /// スタックから n 個 pop するマクロ相当のヘルパ。足りなければ None を返す。
    fn pop(stack: &mut Vec<Ps4Value>) -> Option<Ps4Value> {
        stack.pop()
    }
    fn pop2(stack: &mut Vec<Ps4Value>) -> Option<(Ps4Value, Ps4Value)> {
        let b = stack.pop()?;
        let a = stack.pop()?;
        Some((a, b))
    }

    match op {
        // ---- 算術 ----
        Ps4Op::Add => {
            if let Some((a, b)) = pop2(stack) {
                stack.push(Ps4Value::Number(a.as_number() + b.as_number()));
            }
        }
        Ps4Op::Sub => {
            if let Some((a, b)) = pop2(stack) {
                stack.push(Ps4Value::Number(a.as_number() - b.as_number()));
            }
        }
        Ps4Op::Mul => {
            if let Some((a, b)) = pop2(stack) {
                stack.push(Ps4Value::Number(a.as_number() * b.as_number()));
            }
        }
        Ps4Op::Div => {
            if let Some((a, b)) = pop2(stack) {
                let denom = b.as_number();
                if denom == 0.0 {
                    // 0 除算は 0 で縮退
                    stack.push(Ps4Value::Number(0.0));
                } else {
                    stack.push(Ps4Value::Number(a.as_number() / denom));
                }
            }
        }
        Ps4Op::Idiv => {
            if let Some((a, b)) = pop2(stack) {
                let bi = b.as_int();
                if bi == 0 {
                    stack.push(Ps4Value::Number(0.0));
                } else {
                    stack.push(Ps4Value::Number((a.as_int() / bi) as f64));
                }
            }
        }
        Ps4Op::Mod => {
            if let Some((a, b)) = pop2(stack) {
                let bi = b.as_int();
                if bi == 0 {
                    stack.push(Ps4Value::Number(0.0));
                } else {
                    stack.push(Ps4Value::Number((a.as_int() % bi) as f64));
                }
            }
        }
        Ps4Op::Neg => {
            if let Some(a) = pop(stack) {
                stack.push(Ps4Value::Number(-a.as_number()));
            }
        }
        Ps4Op::Abs => {
            if let Some(a) = pop(stack) {
                stack.push(Ps4Value::Number(a.as_number().abs()));
            }
        }
        Ps4Op::Ceiling => {
            if let Some(a) = pop(stack) {
                stack.push(Ps4Value::Number(a.as_number().ceil()));
            }
        }
        Ps4Op::Floor => {
            if let Some(a) = pop(stack) {
                stack.push(Ps4Value::Number(a.as_number().floor()));
            }
        }
        Ps4Op::Round => {
            if let Some(a) = pop(stack) {
                // PostScript の round は .5 が偶数方向なので f64::round（半切り上げ）で近似
                stack.push(Ps4Value::Number(a.as_number().round()));
            }
        }
        Ps4Op::Truncate => {
            if let Some(a) = pop(stack) {
                stack.push(Ps4Value::Number(a.as_number().trunc()));
            }
        }
        Ps4Op::Sqrt => {
            if let Some(a) = pop(stack) {
                let v = a.as_number().abs().sqrt();
                stack.push(Ps4Value::Number(v));
            }
        }
        Ps4Op::Sin => {
            // PDF 仕様: 入力は度数法
            if let Some(a) = pop(stack) {
                stack.push(Ps4Value::Number(a.as_number().to_radians().sin()));
            }
        }
        Ps4Op::Cos => {
            if let Some(a) = pop(stack) {
                stack.push(Ps4Value::Number(a.as_number().to_radians().cos()));
            }
        }
        Ps4Op::Atan => {
            // atan(num / den)、結果は 0..360 の度数
            if let Some((num, den)) = pop2(stack) {
                let rad = num.as_number().atan2(den.as_number());
                let deg = rad.to_degrees();
                // 0..360 に正規化
                let norm = if deg < 0.0 { deg + 360.0 } else { deg };
                stack.push(Ps4Value::Number(norm));
            }
        }
        Ps4Op::Exp => {
            if let Some((base, exp)) = pop2(stack) {
                stack.push(Ps4Value::Number(base.as_number().powf(exp.as_number())));
            }
        }
        Ps4Op::Ln => {
            if let Some(a) = pop(stack) {
                let v = a.as_number().abs();
                stack.push(Ps4Value::Number(if v == 0.0 { 0.0 } else { v.ln() }));
            }
        }
        Ps4Op::Log => {
            if let Some(a) = pop(stack) {
                let v = a.as_number().abs();
                stack.push(Ps4Value::Number(if v == 0.0 { 0.0 } else { v.log10() }));
            }
        }
        Ps4Op::Cvi => {
            if let Some(a) = pop(stack) {
                stack.push(Ps4Value::Number(a.as_int() as f64));
            }
        }
        Ps4Op::Cvr => {
            if let Some(a) = pop(stack) {
                stack.push(Ps4Value::Number(a.as_number()));
            }
        }
        // ---- 比較 ----
        Ps4Op::Eq => {
            if let Some((a, b)) = pop2(stack) {
                let eq = match (&a, &b) {
                    (Ps4Value::Bool(x), Ps4Value::Bool(y)) => x == y,
                    _ => (a.as_number() - b.as_number()).abs() < 1e-12,
                };
                stack.push(Ps4Value::Bool(eq));
            }
        }
        Ps4Op::Ne => {
            if let Some((a, b)) = pop2(stack) {
                let ne = match (&a, &b) {
                    (Ps4Value::Bool(x), Ps4Value::Bool(y)) => x != y,
                    _ => (a.as_number() - b.as_number()).abs() >= 1e-12,
                };
                stack.push(Ps4Value::Bool(ne));
            }
        }
        Ps4Op::Gt => {
            if let Some((a, b)) = pop2(stack) {
                stack.push(Ps4Value::Bool(a.as_number() > b.as_number()));
            }
        }
        Ps4Op::Ge => {
            if let Some((a, b)) = pop2(stack) {
                stack.push(Ps4Value::Bool(a.as_number() >= b.as_number()));
            }
        }
        Ps4Op::Lt => {
            if let Some((a, b)) = pop2(stack) {
                stack.push(Ps4Value::Bool(a.as_number() < b.as_number()));
            }
        }
        Ps4Op::Le => {
            if let Some((a, b)) = pop2(stack) {
                stack.push(Ps4Value::Bool(a.as_number() <= b.as_number()));
            }
        }
        // ---- 論理・ビット ----
        Ps4Op::And => {
            if let Some((a, b)) = pop2(stack) {
                match (&a, &b) {
                    (Ps4Value::Bool(_), _) | (_, Ps4Value::Bool(_)) => {
                        stack.push(Ps4Value::Bool(a.as_bool() && b.as_bool()));
                    }
                    _ => {
                        stack.push(Ps4Value::Number((a.as_int() & b.as_int()) as f64));
                    }
                }
            }
        }
        Ps4Op::Or => {
            if let Some((a, b)) = pop2(stack) {
                match (&a, &b) {
                    (Ps4Value::Bool(_), _) | (_, Ps4Value::Bool(_)) => {
                        stack.push(Ps4Value::Bool(a.as_bool() || b.as_bool()));
                    }
                    _ => {
                        stack.push(Ps4Value::Number((a.as_int() | b.as_int()) as f64));
                    }
                }
            }
        }
        Ps4Op::Xor => {
            if let Some((a, b)) = pop2(stack) {
                match (&a, &b) {
                    (Ps4Value::Bool(_), _) | (_, Ps4Value::Bool(_)) => {
                        stack.push(Ps4Value::Bool(a.as_bool() ^ b.as_bool()));
                    }
                    _ => {
                        stack.push(Ps4Value::Number((a.as_int() ^ b.as_int()) as f64));
                    }
                }
            }
        }
        Ps4Op::Not => {
            if let Some(a) = pop(stack) {
                match &a {
                    Ps4Value::Bool(b) => stack.push(Ps4Value::Bool(!b)),
                    _ => stack.push(Ps4Value::Number((!a.as_int()) as f64)),
                }
            }
        }
        Ps4Op::Bitshift => {
            // num shift bitshift: shift > 0 は左シフト、< 0 は右シフト
            if let Some((num, shift)) = pop2(stack) {
                let n = num.as_int();
                let s = shift.as_int();
                let result = if s >= 0 {
                    n.checked_shl(s.min(63) as u32).unwrap_or(0)
                } else {
                    n >> ((-s).min(63) as u32)
                };
                stack.push(Ps4Value::Number(result as f64));
            }
        }
        Ps4Op::True => stack.push(Ps4Value::Bool(true)),
        Ps4Op::False => stack.push(Ps4Value::Bool(false)),
        // ---- スタック ----
        Ps4Op::Pop => {
            stack.pop();
        }
        Ps4Op::Exch => {
            let len = stack.len();
            if len >= 2 {
                stack.swap(len - 1, len - 2);
            }
        }
        Ps4Op::Dup => {
            if let Some(top) = stack.last().cloned() {
                stack.push(top);
            }
        }
        Ps4Op::Copy => {
            // n copy: スタックの上位 n 要素を複製してプッシュ
            if let Some(n_val) = pop(stack) {
                let n = n_val.as_int().max(0) as usize;
                let len = stack.len();
                if n <= len {
                    let start = len - n;
                    let copy: Vec<Ps4Value> = stack[start..].to_vec();
                    stack.extend(copy);
                }
                // n > len の場合は縮退（何もしない）
            }
        }
        Ps4Op::Index => {
            // n index: スタック上から n 番目（0 始まり）をコピーしてプッシュ
            if let Some(n_val) = pop(stack) {
                let n = n_val.as_int().max(0) as usize;
                let len = stack.len();
                if n < len {
                    let v = stack[len - 1 - n].clone();
                    stack.push(v);
                }
                // 範囲外は縮退
            }
        }
        Ps4Op::Roll => {
            // n j roll: スタック上位 n 要素を j 回右ローテート（j < 0 は左）
            if let Some((n_val, j_val)) = pop2(stack) {
                let n = n_val.as_int().max(0) as usize;
                let j = j_val.as_int();
                let len = stack.len();
                if n > 0 && n <= len {
                    let start = len - n;
                    let slice = &mut stack[start..];
                    let j_norm = ((j % n as i64) + n as i64) as usize % n;
                    slice.rotate_right(j_norm);
                }
                // n > len は縮退
            }
        }
        // ---- 制御 ----
        Ps4Op::If => {
            // bool proc if
            if let Some((cond, proc)) = pop2(stack) {
                if cond.as_bool() {
                    if let Ps4Value::Proc(tokens) = proc {
                        exec_tokens(&tokens, stack, depth + 1);
                    }
                }
            }
        }
        Ps4Op::Ifelse => {
            // bool proc_true proc_false ifelse
            if stack.len() >= 3 {
                let proc_false = stack.pop().unwrap();
                let proc_true = stack.pop().unwrap();
                let cond = stack.pop().unwrap();
                let chosen = if cond.as_bool() {
                    proc_true
                } else {
                    proc_false
                };
                if let Ps4Value::Proc(tokens) = chosen {
                    exec_tokens(&tokens, stack, depth + 1);
                }
            }
        }
    }
}

// ============================================================
// ユーティリティ関数
// ============================================================

/// 線形補間: x を [x0, x1] → [y0, y1] に写像する。
#[inline]
fn interpolate(x: f64, x0: f64, x1: f64, y0: f64, y1: f64) -> f64 {
    if (x1 - x0).abs() < 1e-15 {
        return y0;
    }
    y0 + (x - x0) * (y1 - y0) / (x1 - x0)
}

/// `v` を `[lo, hi]` にクランプする。
#[inline]
fn clamp(v: f64, lo: f64, hi: f64) -> f64 {
    if lo <= hi {
        v.max(lo).min(hi)
    } else {
        // lo > hi は仕様上あり得ないが耐故障で範囲を反転して扱う
        v.max(hi).min(lo)
    }
}

/// 辞書の数値配列エントリを読み取る補助。
/// なければ `default` を返す。
fn read_optional_number_array(
    doc: &Document,
    dict: &crate::object::Dictionary,
    key: &str,
    default: &[f64],
) -> Vec<f64> {
    match doc.dict_get(dict, key) {
        Some(o) => match o.as_array() {
            Ok(arr) => arr
                .iter()
                .filter_map(|v| doc.resolve(v).as_number().ok())
                .collect(),
            Err(_) => default.to_vec(),
        },
        None => default.to_vec(),
    }
}

/// オブジェクトから f64 配列を読み取る（Array のみ対応）。
fn parse_number_pair_array(doc: &Document, obj: &Object) -> Result<Vec<f64>> {
    let obj = doc.resolve(obj);
    let arr = obj
        .as_array()
        .map_err(|_| PdfError::Invalid("expected number array".into()))?;
    Ok(arr
        .iter()
        .filter_map(|o| doc.resolve(o).as_number().ok())
        .collect())
}

// ============================================================
// テスト
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Document;
    use crate::object::{Dictionary, Object, Stream};

    // --------------------------------------------------------
    // ヘルパ: テスト用に最小限の Document + Object を組み立てる
    // --------------------------------------------------------

    /// 関数辞書から PdfFunction を構築するヘルパ。
    fn make_func_from_dict(dict: Dictionary) -> PdfFunction {
        let doc = Document::new();
        let obj = Object::Dictionary(dict);
        PdfFunction::from_object(&doc, &obj).expect("PdfFunction::from_object failed")
    }

    /// 関数ストリームから PdfFunction を構築するヘルパ。
    fn make_func_from_stream(dict: Dictionary, data: Vec<u8>) -> PdfFunction {
        let doc = Document::new();
        let stream = Stream::new(dict, data);
        let obj = Object::Stream(stream);
        PdfFunction::from_object(&doc, &obj).expect("PdfFunction::from_object failed")
    }

    // --------------------------------------------------------
    // Type 2: 指数補間
    // --------------------------------------------------------

    #[test]
    fn type2_linear_n1() {
        // N=1, C0=[0.0], C1=[1.0] → 線形補間
        let mut d = Dictionary::new();
        d.set("FunctionType", Object::Integer(2));
        d.set(
            "Domain",
            Object::Array(vec![Object::Real(0.0), Object::Real(1.0)]),
        );
        d.set("N", Object::Integer(1));
        // C0/C1 は既定値を使う
        let f = make_func_from_dict(d);

        assert_eq!(f.n_inputs(), 1);
        let out = f.eval(&[0.0]);
        assert!((out[0] - 0.0).abs() < 1e-9, "f(0) = {}", out[0]);
        let out = f.eval(&[1.0]);
        assert!((out[0] - 1.0).abs() < 1e-9, "f(1) = {}", out[0]);
        let out = f.eval(&[0.5]);
        assert!((out[0] - 0.5).abs() < 1e-9, "f(0.5) = {}", out[0]);
    }

    #[test]
    fn type2_quadratic_n2() {
        // N=2, C0=[0.0, 0.0], C1=[1.0, 4.0] → x^2, 4x^2
        let mut d = Dictionary::new();
        d.set("FunctionType", Object::Integer(2));
        d.set(
            "Domain",
            Object::Array(vec![Object::Real(0.0), Object::Real(1.0)]),
        );
        d.set("N", Object::Real(2.0));
        d.set(
            "C0",
            Object::Array(vec![Object::Real(0.0), Object::Real(0.0)]),
        );
        d.set(
            "C1",
            Object::Array(vec![Object::Real(1.0), Object::Real(4.0)]),
        );
        let f = make_func_from_dict(d);

        let out = f.eval(&[0.5]);
        // 0 + 0.5^2 * (1 - 0) = 0.25
        assert!((out[0] - 0.25).abs() < 1e-9, "out[0] = {}", out[0]);
        // 0 + 0.5^2 * (4 - 0) = 1.0
        assert!((out[1] - 1.0).abs() < 1e-9, "out[1] = {}", out[1]);
    }

    #[test]
    fn type2_domain_clamp() {
        // 入力が Domain を超えた場合クランプされる
        let mut d = Dictionary::new();
        d.set("FunctionType", Object::Integer(2));
        d.set(
            "Domain",
            Object::Array(vec![Object::Real(0.0), Object::Real(1.0)]),
        );
        d.set("N", Object::Integer(1));
        let f = make_func_from_dict(d);

        // 2.0 → クランプで 1.0 → f(1) = 1.0
        let out = f.eval(&[2.0]);
        assert!((out[0] - 1.0).abs() < 1e-9);
        // -0.5 → クランプで 0.0 → f(0) = 0.0
        let out = f.eval(&[-0.5]);
        assert!((out[0] - 0.0).abs() < 1e-9);
    }

    #[test]
    fn type2_range_clamp() {
        // Range でクランプされる
        let mut d = Dictionary::new();
        d.set("FunctionType", Object::Integer(2));
        d.set(
            "Domain",
            Object::Array(vec![Object::Real(0.0), Object::Real(1.0)]),
        );
        d.set(
            "Range",
            Object::Array(vec![Object::Real(0.0), Object::Real(0.8)]),
        );
        d.set("N", Object::Integer(1));
        d.set("C1", Object::Array(vec![Object::Real(2.0)])); // x * 2 になる
        let f = make_func_from_dict(d);

        // f(1.0) = 2.0 だが Range=[0,0.8] でクランプ → 0.8
        let out = f.eval(&[1.0]);
        assert!((out[0] - 0.8).abs() < 1e-9, "out[0] = {}", out[0]);
    }

    // --------------------------------------------------------
    // Type 3: 継ぎ接ぎ
    // --------------------------------------------------------

    fn make_type2_obj(c0: f64, c1: f64) -> Object {
        let mut d = Dictionary::new();
        d.set("FunctionType", Object::Integer(2));
        d.set(
            "Domain",
            Object::Array(vec![Object::Real(0.0), Object::Real(1.0)]),
        );
        d.set("N", Object::Integer(1));
        d.set("C0", Object::Array(vec![Object::Real(c0)]));
        d.set("C1", Object::Array(vec![Object::Real(c1)]));
        Object::Dictionary(d)
    }

    #[test]
    fn type3_two_segments() {
        // [0,0.5] → f1(x), [0.5,1] → f2(x)
        // f1: C0=0, C1=0.5 → 0..0.5 の線形写像
        // f2: C0=0.5, C1=1.0 → 0..1 の線形写像
        // Encode: f1 は [0,1], f2 は [0,1]（サブ区間全体を 0-1 に写す）
        let mut d = Dictionary::new();
        d.set("FunctionType", Object::Integer(3));
        d.set(
            "Domain",
            Object::Array(vec![Object::Real(0.0), Object::Real(1.0)]),
        );
        d.set(
            "Functions",
            Object::Array(vec![make_type2_obj(0.0, 0.5), make_type2_obj(0.5, 1.0)]),
        );
        d.set("Bounds", Object::Array(vec![Object::Real(0.5)]));
        d.set(
            "Encode",
            Object::Array(vec![
                Object::Real(0.0),
                Object::Real(1.0), // f1
                Object::Real(0.0),
                Object::Real(1.0), // f2
            ]),
        );

        let doc = Document::new();
        let obj = Object::Dictionary(d);
        let f = PdfFunction::from_object(&doc, &obj).unwrap();

        // x=0.25 は f1 の区間 [0,0.5]。
        // Encode で 0.25 → interpolate(0.25, 0, 0.5, 0, 1) = 0.5
        // f1(0.5) = 0 + 0.5*(0.5-0) = 0.25
        let out = f.eval(&[0.25]);
        assert!((out[0] - 0.25).abs() < 1e-6, "x=0.25 → {}", out[0]);

        // x=0.75 は f2 の区間 [0.5,1]。
        // Encode で 0.75 → interpolate(0.75, 0.5, 1.0, 0, 1) = 0.5
        // f2(0.5) = 0.5 + 0.5*(1.0-0.5) = 0.75
        let out = f.eval(&[0.75]);
        assert!((out[0] - 0.75).abs() < 1e-6, "x=0.75 → {}", out[0]);
    }

    #[test]
    fn type3_boundary_switch() {
        // x=0.5 ちょうどはどちらの区間か（PDF 仕様: 最初の区間が [Domain[0], Bounds[0]]）
        // x < 0.5 → f1, x >= 0.5 → f2
        let mut d = Dictionary::new();
        d.set("FunctionType", Object::Integer(3));
        d.set(
            "Domain",
            Object::Array(vec![Object::Real(0.0), Object::Real(1.0)]),
        );
        d.set(
            "Functions",
            Object::Array(vec![
                make_type2_obj(0.0, 1.0), // f1: C0=0, C1=1 → x そのまま
                make_type2_obj(2.0, 3.0), // f2: C0=2, C1=3 → x+2
            ]),
        );
        d.set("Bounds", Object::Array(vec![Object::Real(0.5)]));
        d.set(
            "Encode",
            Object::Array(vec![
                Object::Real(0.0),
                Object::Real(1.0),
                Object::Real(0.0),
                Object::Real(1.0),
            ]),
        );

        let doc = Document::new();
        let obj = Object::Dictionary(d);
        let f = PdfFunction::from_object(&doc, &obj).unwrap();

        // x=0.4 → f1。Encode(0.4, 0, 0.5) = 0.8 → f1(0.8) = 0.8
        let out = f.eval(&[0.4]);
        assert!((out[0] - 0.8).abs() < 1e-6, "x=0.4 → {}", out[0]);

        // x=0.6 → f2。Encode(0.6, 0.5, 1.0) = 0.2 → f2(0.2) = 2+0.2=2.2
        let out = f.eval(&[0.6]);
        assert!((out[0] - 2.2).abs() < 1e-6, "x=0.6 → {}", out[0]);
    }

    // --------------------------------------------------------
    // Type 0: サンプル関数
    // --------------------------------------------------------

    /// 8bit の 1D サンプル関数を作る（サイズ 3、値 [0, 128, 255]）。
    /// Domain=[0,1], Range=[0,1]、Encode/Decode は既定。
    fn make_type0_1d_8bit() -> PdfFunction {
        // サンプルバイト列: [0, 128, 255]
        let data = vec![0u8, 128, 255];
        let mut d = Dictionary::new();
        d.set("FunctionType", Object::Integer(0));
        d.set(
            "Domain",
            Object::Array(vec![Object::Real(0.0), Object::Real(1.0)]),
        );
        d.set(
            "Range",
            Object::Array(vec![Object::Real(0.0), Object::Real(1.0)]),
        );
        d.set("Size", Object::Array(vec![Object::Integer(3)]));
        d.set("BitsPerSample", Object::Integer(8));
        make_func_from_stream(d, data)
    }

    #[test]
    fn type0_1d_8bit_endpoints() {
        let f = make_type0_1d_8bit();
        // x=0.0 → coords=0.0 → s[0]=0 → decode: 0/255 = 0.0
        let out = f.eval(&[0.0]);
        assert!((out[0] - 0.0).abs() < 1e-4, "x=0 → {}", out[0]);
        // x=1.0 → coords=2.0 → s[2]=255 → decode: 255/255 = 1.0
        let out = f.eval(&[1.0]);
        assert!((out[0] - 1.0).abs() < 1e-4, "x=1 → {}", out[0]);
    }

    #[test]
    fn type0_1d_8bit_midpoint() {
        let f = make_type0_1d_8bit();
        // x=0.5 → coords=1.0 → s[1]=128 → decode: 128/255 ≈ 0.5020
        let out = f.eval(&[0.5]);
        let expected = 128.0_f64 / 255.0;
        assert!((out[0] - expected).abs() < 1e-4, "x=0.5 → {}", out[0]);
    }

    #[test]
    fn type0_1d_8bit_interpolated() {
        let f = make_type0_1d_8bit();
        // x=0.25 → Encode(0.25, 0, 1) → coords = 0.25 * 2 = 0.5
        // 線形補間: s[0]=0, s[1]=128 → raw = 0 + (128-0)*0.5 = 64
        // decode: 64/255 ≈ 0.251
        let out = f.eval(&[0.25]);
        let expected = 64.0_f64 / 255.0;
        assert!(
            (out[0] - expected).abs() < 1e-3,
            "x=0.25 → {} (expected {})",
            out[0],
            expected
        );
    }

    #[test]
    fn type0_1bit_samples() {
        // 1ビットサンプル: 2 サンプル [0, 1] → バイト = 0b0100_0000 = 0x40
        // ビット列: bit7=0, bit6=1 → samples[0]=0, samples[1]=1
        let data = vec![0b0100_0000u8];
        let mut d = Dictionary::new();
        d.set("FunctionType", Object::Integer(0));
        d.set(
            "Domain",
            Object::Array(vec![Object::Real(0.0), Object::Real(1.0)]),
        );
        d.set(
            "Range",
            Object::Array(vec![Object::Real(0.0), Object::Real(1.0)]),
        );
        d.set("Size", Object::Array(vec![Object::Integer(2)]));
        d.set("BitsPerSample", Object::Integer(1));
        let f = make_func_from_stream(d, data);

        // x=0 → coords=0 → sample[0]=0 → decode 0/1=0.0
        let out = f.eval(&[0.0]);
        assert!((out[0] - 0.0).abs() < 1e-9, "1bit x=0 → {}", out[0]);
        // x=1 → coords=1 → sample[1]=1 → decode 1/1=1.0
        let out = f.eval(&[1.0]);
        assert!((out[0] - 1.0).abs() < 1e-9, "1bit x=1 → {}", out[0]);
    }

    #[test]
    fn type0_16bit_samples() {
        // 16bit: 1 サンプル、値 = 32768 (= 0x8000) → decode: 32768/65535 ≈ 0.5000
        let data = vec![0x80u8, 0x00];
        let mut d = Dictionary::new();
        d.set("FunctionType", Object::Integer(0));
        d.set(
            "Domain",
            Object::Array(vec![Object::Real(0.0), Object::Real(1.0)]),
        );
        d.set(
            "Range",
            Object::Array(vec![Object::Real(0.0), Object::Real(1.0)]),
        );
        d.set("Size", Object::Array(vec![Object::Integer(1)]));
        d.set("BitsPerSample", Object::Integer(16));
        let f = make_func_from_stream(d, data);
        let out = f.eval(&[0.0]);
        let expected = 32768.0_f64 / 65535.0;
        assert!(
            (out[0] - expected).abs() < 1e-4,
            "16bit → {} (expected {})",
            out[0],
            expected
        );
    }

    #[test]
    fn type0_4bit_samples() {
        // 4bit: 2 サンプル [0x0, 0xF] → バイト = 0x0F
        // sample[0]=0, sample[1]=15
        let data = vec![0x0Fu8];
        let mut d = Dictionary::new();
        d.set("FunctionType", Object::Integer(0));
        d.set(
            "Domain",
            Object::Array(vec![Object::Real(0.0), Object::Real(1.0)]),
        );
        d.set(
            "Range",
            Object::Array(vec![Object::Real(0.0), Object::Real(1.0)]),
        );
        d.set("Size", Object::Array(vec![Object::Integer(2)]));
        d.set("BitsPerSample", Object::Integer(4));
        let f = make_func_from_stream(d, data);

        // x=0 → sample[0]=0 → 0/15 = 0
        let out = f.eval(&[0.0]);
        assert!((out[0] - 0.0).abs() < 1e-9, "4bit x=0 → {}", out[0]);
        // x=1 → sample[1]=15 → 15/15 = 1.0
        let out = f.eval(&[1.0]);
        assert!((out[0] - 1.0).abs() < 1e-9, "4bit x=1 → {}", out[0]);
    }

    #[test]
    fn type0_custom_encode_decode() {
        // Domain=[0,1], Size=[3], Encode=[1,3]（既定ではなく左端を 1 から始める）
        // Decode=[0.0, 0.5]（出力を半分にスケール）
        // データ: [0, 128, 255] (8bit)
        let data = vec![0u8, 128, 255];
        let mut d = Dictionary::new();
        d.set("FunctionType", Object::Integer(0));
        d.set(
            "Domain",
            Object::Array(vec![Object::Real(0.0), Object::Real(1.0)]),
        );
        d.set(
            "Range",
            Object::Array(vec![Object::Real(0.0), Object::Real(1.0)]),
        );
        d.set("Size", Object::Array(vec![Object::Integer(4)])); // size=4 だが data は 3 バイト
        d.set("BitsPerSample", Object::Integer(8));
        d.set(
            "Encode",
            Object::Array(vec![
                Object::Real(0.0),
                Object::Real(2.0), // [0,1] → [0,2] のサンプル座標
            ]),
        );
        d.set(
            "Decode",
            Object::Array(vec![
                Object::Real(0.0),
                Object::Real(0.5), // raw を [0, 0.5] にマップ
            ]),
        );
        let f = make_func_from_stream(d, data);

        // x=0.5 → Encode(0.5, 0, 1) → 0.5 * 2 = 1.0
        // 線形補間: s[1]=128 ちょうど → raw=128
        // Decode: 128/255 * 0.5 ≈ 0.2510
        let out = f.eval(&[0.5]);
        let expected = (128.0_f64 / 255.0) * 0.5;
        assert!(
            (out[0] - expected).abs() < 1e-3,
            "custom enc/dec → {} (expected {})",
            out[0],
            expected
        );
    }

    // --------------------------------------------------------
    // Type 4: PostScript 電卓
    // --------------------------------------------------------

    fn make_type4(ps_src: &str, n_outputs: usize) -> PdfFunction {
        let mut d = Dictionary::new();
        d.set("FunctionType", Object::Integer(4));
        d.set(
            "Domain",
            Object::Array(vec![
                Object::Real(0.0),
                Object::Real(1.0),
                Object::Real(0.0),
                Object::Real(1.0),
            ]),
        );
        let range: Vec<Object> = (0..n_outputs)
            .flat_map(|_| vec![Object::Real(0.0), Object::Real(1.0)])
            .collect();
        d.set("Range", Object::Array(range));
        let src = ps_src.as_bytes().to_vec();
        // ストリームなのでヘッダを組み立てる
        make_func_from_stream(d, src)
    }

    #[test]
    fn type4_add() {
        let f = make_type4("{ add }", 1);
        let out = f.eval(&[0.3, 0.4]);
        assert!((out[0] - 0.7).abs() < 1e-9, "add: {}", out[0]);
    }

    #[test]
    fn type4_max_via_conditional() {
        // `{ 2 copy lt { exch } if pop }` → max(a, b)
        // スタック [a, b]: lt で a<b なら exch して [b, a] → pop a → b が残る
        // a>=b なら exch なし → [a, b] → pop b → a が残る
        let f = make_type4("{ 2 copy lt { exch } if pop }", 1);
        let out = f.eval(&[0.3, 0.7]);
        assert!((out[0] - 0.7).abs() < 1e-6, "max(0.3,0.7) = {}", out[0]);
        let out = f.eval(&[0.8, 0.2]);
        assert!((out[0] - 0.8).abs() < 1e-6, "max(0.8,0.2) = {}", out[0]);
    }

    #[test]
    fn type4_sin_degrees() {
        // sin は度数法（PDF 仕様）
        let mut d = Dictionary::new();
        d.set("FunctionType", Object::Integer(4));
        d.set(
            "Domain",
            Object::Array(vec![Object::Real(0.0), Object::Real(360.0)]),
        );
        d.set(
            "Range",
            Object::Array(vec![Object::Real(-1.0), Object::Real(1.0)]),
        );
        let f = make_func_from_stream(d, b"{ sin }".to_vec());

        // sin(90°) = 1.0
        let out = f.eval(&[90.0]);
        assert!((out[0] - 1.0).abs() < 1e-9, "sin(90) = {}", out[0]);
        // sin(0°) = 0.0
        let out = f.eval(&[0.0]);
        assert!(out[0].abs() < 1e-9, "sin(0) = {}", out[0]);
    }

    #[test]
    fn type4_atan_degrees() {
        // atan(1, 0) = 90.0°
        let mut d = Dictionary::new();
        d.set("FunctionType", Object::Integer(4));
        d.set(
            "Domain",
            Object::Array(vec![
                Object::Real(-10.0),
                Object::Real(10.0),
                Object::Real(-10.0),
                Object::Real(10.0),
            ]),
        );
        d.set(
            "Range",
            Object::Array(vec![Object::Real(0.0), Object::Real(360.0)]),
        );
        let f = make_func_from_stream(d, b"{ atan }".to_vec());

        let out = f.eval(&[1.0, 0.0]);
        assert!((out[0] - 90.0).abs() < 1e-6, "atan(1,0) = {}", out[0]);
    }

    #[test]
    fn type4_div_zero_no_panic() {
        // 0 除算でも panic しない
        let f = make_type4("{ div }", 1);
        let out = f.eval(&[1.0, 0.0]);
        // 縮退値 0.0 を返す
        assert_eq!(out[0], 0.0);
    }

    #[test]
    fn type4_stack_underflow_no_panic() {
        // スタックアンダーフロー（pop しすぎ）でも panic しない
        let f = make_type4("{ pop pop pop pop pop }", 1);
        let _out = f.eval(&[0.5, 0.5]); // panic しないことを確認
    }

    #[test]
    fn type4_invalid_token_no_panic() {
        // 不正トークンを含んでいても panic しない
        let f = make_type4("{ @@@@invalid 1 add }", 1);
        let _out = f.eval(&[0.3, 0.4]);
    }

    #[test]
    fn type4_ifelse() {
        // 条件に応じて C0/C1 を返す
        // { 0.5 ge { 1.0 } { 0.0 } ifelse }
        let mut d = Dictionary::new();
        d.set("FunctionType", Object::Integer(4));
        d.set(
            "Domain",
            Object::Array(vec![Object::Real(0.0), Object::Real(1.0)]),
        );
        d.set(
            "Range",
            Object::Array(vec![Object::Real(0.0), Object::Real(1.0)]),
        );
        let src = b"{ 0.5 ge { 1.0 } { 0.0 } ifelse }".to_vec();
        let f = make_func_from_stream(d, src);

        let out = f.eval(&[0.8]);
        assert!((out[0] - 1.0).abs() < 1e-9, "0.8 >= 0.5 → {}", out[0]);
        let out = f.eval(&[0.3]);
        assert!((out[0] - 0.0).abs() < 1e-9, "0.3 < 0.5 → {}", out[0]);
    }

    #[test]
    fn type4_nested_proc_depth_limit_no_panic() {
        // 深くネストした手続きでも panic しない（深さ上限で切り捨て）
        // 64 段より深い再帰が必要な式
        let deep = "{ if ".repeat(70) + "1 " + "} ".repeat(70).as_str();
        let mut d = Dictionary::new();
        d.set("FunctionType", Object::Integer(4));
        d.set(
            "Domain",
            Object::Array(vec![Object::Real(0.0), Object::Real(1.0)]),
        );
        d.set(
            "Range",
            Object::Array(vec![Object::Real(0.0), Object::Real(1.0)]),
        );
        let f = make_func_from_stream(d, deep.into_bytes());
        let _out = f.eval(&[0.5]);
    }

    #[test]
    fn type4_arithmetic_chain() {
        // { dup mul } → x^2
        let mut d = Dictionary::new();
        d.set("FunctionType", Object::Integer(4));
        d.set(
            "Domain",
            Object::Array(vec![Object::Real(0.0), Object::Real(1.0)]),
        );
        d.set(
            "Range",
            Object::Array(vec![Object::Real(0.0), Object::Real(1.0)]),
        );
        let f = make_func_from_stream(d, b"{ dup mul }".to_vec());
        let out = f.eval(&[0.5]);
        assert!((out[0] - 0.25).abs() < 1e-9, "0.5^2 = {}", out[0]);
    }

    // --------------------------------------------------------
    // 共通: 間接参照経由の構築
    // --------------------------------------------------------

    #[test]
    fn indirect_ref_construction() {
        let mut doc = Document::new();
        let mut d = Dictionary::new();
        d.set("FunctionType", Object::Integer(2));
        d.set(
            "Domain",
            Object::Array(vec![Object::Real(0.0), Object::Real(1.0)]),
        );
        d.set("N", Object::Integer(1));
        let id = doc.add_object(Object::Dictionary(d));
        let obj = Object::Reference(id);
        let f = PdfFunction::from_object(&doc, &obj).expect("間接参照からの構築に失敗");
        let out = f.eval(&[0.5]);
        assert!((out[0] - 0.5).abs() < 1e-9);
    }
}
