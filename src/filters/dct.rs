//! baseline / extended-sequential / progressive JPEG（DCTDecode）デコーダの
//! フルスクラッチ実装。
//!
//! ITU-T T.81 (ISO/IEC 10918-1) のうち、以下に対応する:
//!
//! - **SOF0**（baseline DCT, ハフマン, 8bit）・**SOF1**（extended sequential, 8bit）・
//!   **SOF2**（progressive DCT, ハフマン, 8bit）。progressive はスペクトル選択
//!   （Ss/Se）と逐次近似（Ah/Al）の複数スキャンを係数バッファに蓄積し、全スキャン
//!   完了後にデクオンタイズ + 逆 DCT する（[`Decoder::finalize_progressive`]）。
//!   12bit 精度・算術符号化は明示エラー。
//! - マーカー: SOI / APP0–APP15 / COM / DQT / DHT / DRI / SOS / EOI。
//!   その他の不明セグメントは長さフィールドで読み飛ばす。
//! - ハフマン復号（DC/AC を各 4 テーブルまで）。
//! - デクオンタイズ + 8x8 逆 DCT（[`idct_8x8`]。AAN 系ではなく可読性優先の
//!   分離型浮動小数 IDCT。各次元 8 点 1 次元 IDCT を 2 回適用する）。
//! - サンプリングファクタ 1〜4（4:4:4 / 4:2:2 / 4:2:0 / 4:1:1 等）。クロマの
//!   アップサンプリングは**中心揃えの双線形補間**で行う（最近傍より滑らかで、
//!   一般的な JPEG デコーダの出力に一致する）。
//! - リスタートマーカー（DRI / RST0–RST7）。
//!
//! ## 色変換と出力フォーマット
//!
//! 出力 [`DecodedImage`] はコンポーネントインターリーブの 8bit サンプル列で、
//! `components` に応じて以下のとおり正規化済み:
//!
//! - 1 成分: グレースケールのまま。
//! - 3 成分: YCbCr→RGB（ITU-R BT.601）。ただし Adobe APP14 の `transform=0`
//!   なら変換せず RGB として扱う。
//! - 4 成分: Adobe APP14 の `transform=2` なら YCCK→CMYK 変換、なければそのまま CMYK。
//!   **Adobe 製 JPEG（APP14 セグメントを持つもの）は CMYK サンプルが反転している**
//!   慣習があるため、APP14 が存在する場合は最終的に各チャネルを反転し、
//!   出力は常に「0 = インクなし 〜 255 = 最大インク」に正規化する。
//!
//! ## 安全性
//!
//! JPEG は信頼できない入力として扱う。すべての配列アクセスは `get(..)`、
//! 算術は checked / saturating を用い、壊れた・切り詰められた入力でも panic
//! しない。デコード後のピクセル総数には上限ガード（[`MAX_PIXELS`]）を設ける。

use crate::error::{PdfError, Result};

fn err(msg: impl Into<String>) -> PdfError {
    PdfError::Filter(msg.into())
}

/// デコード後ピクセル総数の上限（幅 × 高さ）。過大な割り当てを防ぐ。
const MAX_PIXELS: u64 = 1 << 28;

/// デコード済み画像（コンポーネントインターリーブの 8bit サンプル列）。
pub struct DecodedImage {
    /// 画像の幅（ピクセル）。
    pub width: u32,
    /// 画像の高さ（ピクセル）。
    pub height: u32,
    /// 成分数。1 = グレースケール, 3 = RGB（YCbCr から変換済み）, 4 = CMYK。
    pub components: u8,
    /// `width * height * components` バイト。行優先、成分インターリーブ。
    pub data: Vec<u8>,
}

// ---------------------------------------------------------------------------
// ジグザグ順（T.81 Figure A.6）
// ---------------------------------------------------------------------------

/// ジグザグスキャン順。係数列のインデックス i に対する自然順（行優先 8x8）の位置。
#[rustfmt::skip]
const ZIGZAG: [usize; 64] = [
     0,  1,  8, 16,  9,  2,  3, 10,
    17, 24, 32, 25, 18, 11,  4,  5,
    12, 19, 26, 33, 40, 48, 41, 34,
    27, 20, 13,  6,  7, 14, 21, 28,
    35, 42, 49, 56, 57, 50, 43, 36,
    29, 22, 15, 23, 30, 37, 44, 51,
    58, 59, 52, 45, 38, 31, 39, 46,
    53, 60, 61, 54, 47, 55, 62, 63,
];

// ---------------------------------------------------------------------------
// ハフマンテーブル
// ---------------------------------------------------------------------------

/// ハフマンテーブル。BITS（各符号長のコード数）と HUFFVAL から構築する。
///
/// 復号は単純な「最大 16bit を 1 ビットずつ進めて符号長ごとに照合」方式。
/// 高速化ルックアップは設けていない（正しさ優先）。
#[derive(Clone, Default)]
struct HuffmanTable {
    /// 各符号長 len(1..=16) における最小コード値。`min_code[len-1]`。
    min_code: [i32; 16],
    /// 各符号長における最大コード値（+1 ではなく実際の最大、無ければ -1）。
    max_code: [i32; 16],
    /// 各符号長の先頭が `huffval` の何番目から始まるか。
    val_ptr: [usize; 16],
    /// シンボル値の並び（HUFFVAL）。
    huffval: Vec<u8>,
}

impl HuffmanTable {
    /// BITS（counts[0]=符号長1 のコード数 … counts[15]=符号長16）と
    /// HUFFVAL から復号用テーブルを構築する（T.81 Annex C / F.2.2.3）。
    fn build(counts: &[u8; 16], huffval: Vec<u8>) -> Self {
        let mut t = HuffmanTable {
            huffval,
            ..Default::default()
        };
        // HUFFSIZE / HUFFCODE を生成
        let mut code: i32 = 0;
        let mut k: usize = 0; // huffval 上の位置
        for (len, &count) in counts.iter().enumerate() {
            let n = count as usize;
            if n == 0 {
                t.max_code[len] = -1;
                t.min_code[len] = 0;
                t.val_ptr[len] = 0;
            } else {
                t.val_ptr[len] = k;
                t.min_code[len] = code;
                code += n as i32;
                t.max_code[len] = code - 1;
                k += n;
            }
            code <<= 1;
        }
        t
    }

    /// 1 シンボルを復号する（T.81 F.2.2.3 の DECODE 手続き）。
    fn decode(&self, br: &mut BitReader) -> Result<u8> {
        let mut code: i32 = 0;
        for len in 0..16 {
            code = (code << 1) | br.read_bit()? as i32;
            if self.max_code[len] >= 0 && code <= self.max_code[len] {
                let idx = self.val_ptr[len] + (code - self.min_code[len]) as usize;
                return self
                    .huffval
                    .get(idx)
                    .copied()
                    .ok_or_else(|| err("ハフマン値インデックスが範囲外"));
            }
        }
        Err(err("ハフマン符号が復号できない（16bit 超）"))
    }
}

// ---------------------------------------------------------------------------
// ビットリーダ（エントロピー符号化データ用, MSB ファースト + バイトスタッフィング）
// ---------------------------------------------------------------------------

/// エントロピー符号化セグメントを MSB ファーストで読み出すビットリーダ。
///
/// `0xFF` の直後が `0x00` の場合はスタッフィング（実データの 0xFF）として 0x00 を捨てる。
/// `0xFF` の直後が `0x00` 以外（マーカー）の場合はそこでデータ終端とみなし、
/// 以降のビット要求には 0 を返す（耐故障）。
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    bit_buf: u32,
    bit_count: u8,
    /// マーカーに到達したら true。`marker` にマーカーバイトを格納。
    hit_marker: bool,
    marker: u8,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8], pos: usize) -> Self {
        BitReader {
            data,
            pos,
            bit_buf: 0,
            bit_count: 0,
            hit_marker: false,
            marker: 0,
        }
    }

    /// 次の 1 バイトをバッファに取り込む。マーカーに当たったら hit_marker を立てる。
    fn fill_byte(&mut self) {
        if self.hit_marker {
            // マーカー以降は 0 ビットを供給（耐故障）
            self.bit_buf <<= 8;
            self.bit_count += 8;
            return;
        }
        let b = match self.data.get(self.pos) {
            Some(&b) => b,
            None => {
                // データ終端: 以降は 0 を供給
                self.hit_marker = true;
                self.bit_buf <<= 8;
                self.bit_count += 8;
                return;
            }
        };
        self.pos += 1;
        if b == 0xFF {
            // スタッフィングまたはマーカー
            match self.data.get(self.pos) {
                Some(&0x00) => {
                    self.pos += 1; // スタッフィングの 0x00 を捨て、0xFF を実データとして使う
                }
                Some(&m) if (0xD0..=0xD7).contains(&m) => {
                    // RST マーカー: ここで停止（呼び出し側が処理）
                    self.hit_marker = true;
                    self.marker = m;
                    self.pos -= 1; // 0xFF の位置に巻き戻す（マーカー処理側で読む）
                    self.bit_buf <<= 8;
                    self.bit_count += 8;
                    return;
                }
                Some(&m) => {
                    self.hit_marker = true;
                    self.marker = m;
                    self.pos -= 1;
                    self.bit_buf <<= 8;
                    self.bit_count += 8;
                    return;
                }
                None => {
                    self.hit_marker = true;
                    self.bit_buf <<= 8;
                    self.bit_count += 8;
                    return;
                }
            }
        }
        self.bit_buf = (self.bit_buf << 8) | b as u32;
        self.bit_count += 8;
    }

    /// 1 ビット読む（MSB ファースト）。
    fn read_bit(&mut self) -> Result<u32> {
        if self.bit_count == 0 {
            self.fill_byte();
        }
        self.bit_count -= 1;
        Ok((self.bit_buf >> self.bit_count) & 1)
    }

    /// n ビット（0..=16）を読み、符号なし整数として返す。
    fn read_bits(&mut self, n: u8) -> Result<i32> {
        let mut v: i32 = 0;
        for _ in 0..n {
            v = (v << 1) | self.read_bit()? as i32;
        }
        Ok(v)
    }

    /// 端数ビットを捨ててバイト境界に揃える（リスタート前に使う）。
    fn reset_to_byte(&mut self) {
        self.bit_buf = 0;
        self.bit_count = 0;
    }
}

/// ハフマンの「受信値」を符号付き整数に伸張する（T.81 F.2.2.1 の EXTEND）。
fn extend(v: i32, t: u8) -> i32 {
    if t == 0 {
        return 0;
    }
    let vt = 1 << (t - 1);
    if v < vt {
        v + (-1 << t) + 1
    } else {
        v
    }
}

// ---------------------------------------------------------------------------
// progressive スキャンの係数復号（T.81 Annex G.1.2）
//
// いずれも `block` は 1 ブロック 64 係数（自然順）。ジグザグ index `k` から
// 自然順への変換は [`ZIGZAG`] で行う。係数は逐次近似の今回ビット位置だけを
// 立てて蓄積し、デクオンタイズと逆 DCT は全スキャン完了後に行う。
// ---------------------------------------------------------------------------

/// DC 初回スキャン（Ah==0）。DC 差分を復号し、point transform `al` だけ左シフト
/// して格納する（T.81 G.1.2.1）。
fn decode_dc_first(
    block: &mut [i32],
    br: &mut BitReader,
    dc_tab: &HuffmanTable,
    pred: &mut i32,
    al: u8,
) -> Result<()> {
    let t = dc_tab.decode(br)?;
    if t > 11 {
        return Err(err("DC のビット長が不正"));
    }
    let diff = extend(br.read_bits(t)?, t);
    *pred = pred.wrapping_add(diff);
    block[0] = *pred << al;
    Ok(())
}

/// DC 精緻化スキャン（Ah!=0）。1 ビット読み、立っていれば今回ビットを足す。
fn decode_dc_refine(block: &mut [i32], br: &mut BitReader, al: u8) -> Result<()> {
    if br.read_bit()? != 0 {
        block[0] |= 1 << al;
    }
    Ok(())
}

/// AC 初回スキャン（Ah==0）。スペクトル選択 `ss..=se` の係数を復号する。
/// EOBRUN（end-of-band run）で全ゼロ帯域をまとめて飛ばす（T.81 G.1.2.2）。
fn decode_ac_first(
    block: &mut [i32],
    br: &mut BitReader,
    ac_tab: &HuffmanTable,
    ss: usize,
    se: usize,
    al: u8,
    eobrun: &mut u32,
) -> Result<()> {
    if *eobrun > 0 {
        *eobrun -= 1;
        return Ok(());
    }
    let mut k = ss;
    while k <= se {
        let rs = ac_tab.decode(br)?;
        let r = (rs >> 4) as usize;
        let s = rs & 0x0F;
        if s != 0 {
            k += r;
            if k > se {
                break;
            }
            let val = extend(br.read_bits(s)?, s);
            if let Some(&nat) = ZIGZAG.get(k) {
                block[nat] = val << al;
            }
            k += 1;
        } else {
            if r != 15 {
                // EOBn: このブロックを含め 2^r + 付加ビット ブロックが帯域内全ゼロ。
                // 現在ブロックは復号済みで break するため、後続ブロック数（= 全体 - 1）を
                // EOBRUN に積む（次回以降の呼び出し冒頭で 1 つずつ減算する）。
                *eobrun = (1u32 << r) - 1;
                if r != 0 {
                    *eobrun += br.read_bits(r as u8)? as u32;
                }
                break;
            }
            // ZRL: 16 個のゼロを飛ばす。
            k += 16;
        }
    }
    Ok(())
}

/// AC 精緻化スキャン（Ah!=0）。既存の非ゼロ係数に補正ビットを足しつつ、
/// 新たに非ゼロになる係数を符号ビットで配置する（T.81 G.1.2.3）。
fn decode_ac_refine(
    block: &mut [i32],
    br: &mut BitReader,
    ac_tab: &HuffmanTable,
    ss: usize,
    se: usize,
    al: u8,
    eobrun: &mut u32,
) -> Result<()> {
    let p1: i32 = 1 << al; // 今回ビット位置の +1
    let m1: i32 = -1i32 << al; // 今回ビット位置の -1
    let mut k = ss;

    if *eobrun == 0 {
        while k <= se {
            let rs = ac_tab.decode(br)?;
            let mut r = (rs >> 4) as i32;
            let s = rs & 0x0F;
            // 新規非ゼロ係数の値（0 = この符号では新規係数なし）。
            let mut newval: i32 = 0;
            if s != 0 {
                // s は 1 のはず。符号ビットを読む。
                newval = if br.read_bit()? != 0 { p1 } else { m1 };
            } else if r != 15 {
                // EOBn: 帯域終端ランへ。
                *eobrun = 1u32 << r;
                if r != 0 {
                    *eobrun += br.read_bits(r as u8)? as u32;
                }
                break;
            }
            // r 個のゼロ係数を飛ばしつつ、途中の非ゼロ係数を精緻化する。
            while k <= se {
                let nat = match ZIGZAG.get(k) {
                    Some(&n) => n,
                    None => break,
                };
                if block[nat] != 0 {
                    if br.read_bit()? != 0 && (block[nat] & p1) == 0 {
                        block[nat] += if block[nat] >= 0 { p1 } else { m1 };
                    }
                } else {
                    if r == 0 {
                        break;
                    }
                    r -= 1;
                }
                k += 1;
            }
            // 新規非ゼロ係数を現在位置に配置する。
            if newval != 0 {
                if let Some(&nat) = ZIGZAG.get(k) {
                    if k <= se {
                        block[nat] = newval;
                    }
                }
            }
            k += 1;
        }
    }

    // 帯域終端ラン中: 残り係数の非ゼロを精緻化し、ラン数を 1 減らす。
    if *eobrun > 0 {
        while k <= se {
            if let Some(&nat) = ZIGZAG.get(k) {
                if block[nat] != 0 && br.read_bit()? != 0 && (block[nat] & p1) == 0 {
                    block[nat] += if block[nat] >= 0 { p1 } else { m1 };
                }
            }
            k += 1;
        }
        *eobrun -= 1;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 逆 DCT
// ---------------------------------------------------------------------------

/// 8x8 ブロックの逆離散コサイン変換（分離型・浮動小数）。
///
/// 入力 `block` はデクオンタイズ済み係数（自然順, 行優先）。
/// アルゴリズムは AAN 等の高速バタフライではなく、各次元に対し
/// 8 点 1 次元 IDCT（直接定義式）を適用する素朴な分離型実装（可読性優先）。
/// 出力は各サンプルに 128 を加えクランプした 0..=255 の u8。
fn idct_8x8(block: &[f32; 64]) -> [u8; 64] {
    // 余弦テーブル: cos((2x+1)*u*pi/16)
    // C(u) = 1/sqrt(2) (u=0), 1 otherwise を係数に畳み込む。
    let mut tmp = [0.0f32; 64];

    // 行方向 1D IDCT
    for y in 0..8 {
        for x in 0..8 {
            let mut s = 0.0f32;
            for u in 0..8 {
                let cu = if u == 0 {
                    std::f32::consts::FRAC_1_SQRT_2
                } else {
                    1.0
                };
                s += cu * block[y * 8 + u] * COS[x][u];
            }
            tmp[y * 8 + x] = s * 0.5;
        }
    }

    // 列方向 1D IDCT
    let mut out = [0u8; 64];
    for x in 0..8 {
        for y in 0..8 {
            let mut s = 0.0f32;
            for v in 0..8 {
                let cv = if v == 0 {
                    std::f32::consts::FRAC_1_SQRT_2
                } else {
                    1.0
                };
                s += cv * tmp[v * 8 + x] * COS[y][v];
            }
            let val = (s * 0.5 + 128.0).round();
            out[y * 8 + x] = val.clamp(0.0, 255.0) as u8;
        }
    }
    out
}

/// `COS[x][u] = cos((2x+1)*u*pi/16)`。コンパイル時定数ではなく初回参照で構築。
use std::sync::OnceLock;
static COS_TABLE: OnceLock<[[f32; 8]; 8]> = OnceLock::new();

/// 余弦テーブルへの参照を返す（遅延初期化）。
#[allow(non_snake_case)]
fn cos_table() -> &'static [[f32; 8]; 8] {
    COS_TABLE.get_or_init(|| {
        let mut t = [[0.0f32; 8]; 8];
        for (x, row) in t.iter_mut().enumerate() {
            for (u, c) in row.iter_mut().enumerate() {
                *c = (((2 * x + 1) as f32) * (u as f32) * std::f32::consts::PI / 16.0).cos();
            }
        }
        t
    })
}

// idct_8x8 から COS[x][u] の記法で使えるようにするためのプロキシ。
struct CosProxy;
impl std::ops::Index<usize> for CosProxy {
    type Output = [f32; 8];
    fn index(&self, i: usize) -> &[f32; 8] {
        &cos_table()[i]
    }
}
#[allow(non_upper_case_globals)]
const COS: CosProxy = CosProxy;

// ---------------------------------------------------------------------------
// フレーム / スキャン構造
// ---------------------------------------------------------------------------

/// フレーム成分情報。
#[derive(Clone)]
struct Component {
    id: u8,
    /// 水平サンプリングファクタ（1..=4）。
    h: u8,
    /// 垂直サンプリングファクタ（1..=4）。
    v: u8,
    /// 量子化テーブル番号。
    tq: u8,
    /// このコンポーネントの幅（ブロック単位）= ceil(mcus_x * h)。
    blocks_per_line: usize,
    /// 高さ（ブロック単位）。
    blocks_per_col: usize,
    /// 復号したサンプル平面（blocks_per_line*8 幅 × blocks_per_col*8 高）。
    plane: Vec<u8>,
    /// 平面の確保幅（= blocks_per_line * 8）。
    stride: usize,
    /// progressive 用の係数バッファ（`blocks_per_line * blocks_per_col * 64`、
    /// 1 ブロック 64 係数を自然順で保持）。baseline では未使用（空）。
    coeffs: Vec<i32>,
}

/// スキャンごとのコンポーネント割り当て。
struct ScanComponent {
    /// frame.components 上のインデックス。
    comp_index: usize,
    /// DC ハフマンテーブル番号。
    td: usize,
    /// AC ハフマンテーブル番号。
    ta: usize,
}

// ---------------------------------------------------------------------------
// デコーダ本体
// ---------------------------------------------------------------------------

/// JPEG ストリームをデコードする。
///
/// 対応外（progressive・12bit・算術符号化・暗号化等）の場合は
/// [`PdfError::Filter`] を返す。壊れた入力では可能な範囲で復号するか Err を返す。
pub fn decode(data: &[u8]) -> Result<DecodedImage> {
    let mut dec = Decoder::new(data);
    dec.run()
}

struct Decoder<'a> {
    data: &'a [u8],
    pos: usize,
    /// 量子化テーブル（最大 4）。各 64 要素（自然順）。
    qtables: [Option<[u16; 64]>; 4],
    /// DC ハフマンテーブル（最大 4）。
    huff_dc: [Option<HuffmanTable>; 4],
    /// AC ハフマンテーブル（最大 4）。
    huff_ac: [Option<HuffmanTable>; 4],
    /// リスタート間隔（MCU 数）。0 = なし。
    restart_interval: usize,
    /// フレーム情報。
    frame_width: usize,
    frame_height: usize,
    precision: u8,
    components: Vec<Component>,
    /// Adobe APP14 の color transform（None なら APP14 なし）。
    adobe_transform: Option<u8>,
    /// APP14 セグメントが存在したか（CMYK 反転判定に使う）。
    has_adobe: bool,
    sof_seen: bool,
    /// SOF2（progressive）か。true なら複数スキャンを係数バッファへ蓄積する。
    progressive: bool,
    /// progressive の係数バッファを確保済みか。
    coeffs_ready: bool,
    /// これまでに復号した SOS スキャン数（耐故障の終了判定に使う）。
    scan_count: usize,
}

impl<'a> Decoder<'a> {
    fn new(data: &'a [u8]) -> Self {
        Decoder {
            data,
            pos: 0,
            qtables: Default::default(),
            huff_dc: Default::default(),
            huff_ac: Default::default(),
            restart_interval: 0,
            frame_width: 0,
            frame_height: 0,
            precision: 8,
            components: Vec::new(),
            adobe_transform: None,
            has_adobe: false,
            sof_seen: false,
            progressive: false,
            coeffs_ready: false,
            scan_count: 0,
        }
    }

    fn u8_at(&self, i: usize) -> Result<u8> {
        self.data
            .get(i)
            .copied()
            .ok_or_else(|| err("JPEG データが途中で終端した"))
    }

    fn u16_be(&self, i: usize) -> Result<usize> {
        let hi = self.u8_at(i)? as usize;
        let lo = self.u8_at(i + 1)? as usize;
        Ok((hi << 8) | lo)
    }

    fn run(&mut self) -> Result<DecodedImage> {
        // SOI を確認
        if self.u8_at(0)? != 0xFF || self.u8_at(1)? != 0xD8 {
            return Err(err("JPEG の SOI マーカーがない"));
        }
        self.pos = 2;

        loop {
            // 次のマーカーを探す（0xFF が連続することがある）。progressive で
            // 全スキャンを読み終えた後に EOI 欠落で終端した場合は、エラーにせず
            // ここまでの係数で組み立てる（耐故障）。
            let marker = match self.next_marker() {
                Ok(m) => m,
                Err(e) => {
                    if self.sof_seen && self.scan_count > 0 {
                        break;
                    }
                    return Err(e);
                }
            };
            match marker {
                0xD9 => break, // EOI
                0xC0 | 0xC1 => {
                    self.read_sof(marker)?;
                }
                0xC2 => {
                    // SOF2: progressive
                    self.read_sof(marker)?;
                }
                0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => {
                    return Err(err(
                        "この JPEG 形式（lossless / 算術符号化 / 階層）は未対応",
                    ));
                }
                0xC4 => self.read_dht()?,
                0xDB => self.read_dqt()?,
                0xDD => self.read_dri()?,
                0xEE => self.read_app14()?,
                0xDA => {
                    // SOS: スキャンを復号する
                    self.read_sos()?;
                    // baseline は単一スキャン。progressive は複数スキャンを
                    // 読み続け、EOI（または終端）まで継続する。
                    if !self.progressive {
                        break;
                    }
                }
                0x01 | 0xD0..=0xD7 => {
                    // TEM / RST はパラメータなし（ここでは無視）
                }
                _ => {
                    // APPn / COM / その他: 長さで読み飛ばす
                    self.skip_segment()?;
                }
            }
        }

        if !self.sof_seen {
            return Err(err("SOF マーカーが見つからない"));
        }

        if self.progressive {
            // progressive は全スキャン蓄積後にデクオンタイズ + 逆 DCT する。
            if !self.coeffs_ready {
                return Err(err("progressive にスキャンがない"));
            }
            self.finalize_progressive()?;
        }

        self.assemble()
    }

    /// 次のマーカーバイトを返し、pos をその直後に進める。
    fn next_marker(&mut self) -> Result<u8> {
        // 0xFF を探す
        while self.pos < self.data.len() {
            if self.u8_at(self.pos)? == 0xFF {
                // 連続する 0xFF（フィル）を読み飛ばす
                let mut p = self.pos + 1;
                while p < self.data.len() && self.data[p] == 0xFF {
                    p += 1;
                }
                let m = self.u8_at(p)?;
                self.pos = p + 1;
                return Ok(m);
            }
            self.pos += 1;
        }
        Err(err("マーカーが見つからないまま終端した"))
    }

    /// 長さ付きセグメントを読み飛ばす。pos はマーカー直後（長さフィールド先頭）。
    fn skip_segment(&mut self) -> Result<()> {
        let len = self.u16_be(self.pos)?;
        if len < 2 {
            return Err(err("セグメント長が不正（< 2）"));
        }
        self.pos = self
            .pos
            .checked_add(len)
            .ok_or_else(|| err("セグメント長オーバーフロー"))?;
        Ok(())
    }

    /// SOF0 / SOF1 / SOF2 を読む。SOF2 は progressive。
    fn read_sof(&mut self, marker: u8) -> Result<()> {
        self.progressive = marker == 0xC2;
        let start = self.pos;
        let len = self.u16_be(start)?;
        let prec = self.u8_at(start + 2)?;
        if prec != 8 {
            return Err(err("8bit 以外の精度の JPEG は未対応"));
        }
        self.precision = prec;
        self.frame_height = self.u16_be(start + 3)?;
        self.frame_width = self.u16_be(start + 5)?;
        let nc = self.u8_at(start + 7)? as usize;
        if nc == 0 || nc > 4 {
            return Err(err("成分数が不正（1〜4 のみ対応）"));
        }
        // ピクセル数ガード
        let pixels = (self.frame_width as u64).saturating_mul(self.frame_height as u64);
        if pixels == 0 {
            return Err(err("画像サイズが 0"));
        }
        if pixels > MAX_PIXELS {
            return Err(err("画像が大きすぎる（上限超過）"));
        }
        let mut comps = Vec::with_capacity(nc);
        let mut p = start + 8;
        for _ in 0..nc {
            let id = self.u8_at(p)?;
            let hv = self.u8_at(p + 1)?;
            let tq = self.u8_at(p + 2)?;
            let h = hv >> 4;
            let v = hv & 0x0F;
            if !(1..=4).contains(&h) || !(1..=4).contains(&v) {
                return Err(err("サンプリングファクタが範囲外（1〜4）"));
            }
            if tq > 3 {
                return Err(err("量子化テーブル番号が範囲外"));
            }
            comps.push(Component {
                id,
                h,
                v,
                tq,
                blocks_per_line: 0,
                blocks_per_col: 0,
                plane: Vec::new(),
                stride: 0,
                coeffs: Vec::new(),
            });
            p += 3;
        }
        // 長さ整合チェック（厳密でなくてよい）
        let _ = len;
        self.components = comps;
        self.sof_seen = true;
        // pos をセグメント末尾へ
        self.pos = start + len;
        Ok(())
    }

    /// DQT（量子化テーブル定義）。複数テーブルを 1 セグメントに含むことがある。
    fn read_dqt(&mut self) -> Result<()> {
        let start = self.pos;
        let len = self.u16_be(start)?;
        let end = start
            .checked_add(len)
            .ok_or_else(|| err("DQT 長オーバーフロー"))?;
        let mut p = start + 2;
        while p < end {
            let pq_tq = self.u8_at(p)?;
            p += 1;
            let pq = pq_tq >> 4; // 0=8bit, 1=16bit
            let tq = (pq_tq & 0x0F) as usize;
            if tq > 3 {
                return Err(err("DQT のテーブル番号が範囲外"));
            }
            let mut table = [0u16; 64];
            for slot in table.iter_mut() {
                let v = if pq == 0 {
                    let b = self.u8_at(p)?;
                    p += 1;
                    b as u16
                } else {
                    let v = self.u16_be(p)? as u16;
                    p += 2;
                    v
                };
                *slot = v;
            }
            // ジグザグ順 → 自然順に並べ替えて格納
            let mut natural = [0u16; 64];
            for (i, &zz) in ZIGZAG.iter().enumerate() {
                if let Some(dst) = natural.get_mut(zz) {
                    *dst = table[i];
                }
            }
            self.qtables[tq] = Some(natural);
        }
        self.pos = end;
        Ok(())
    }

    /// DHT（ハフマンテーブル定義）。複数テーブルを含むことがある。
    fn read_dht(&mut self) -> Result<()> {
        let start = self.pos;
        let len = self.u16_be(start)?;
        let end = start
            .checked_add(len)
            .ok_or_else(|| err("DHT 長オーバーフロー"))?;
        let mut p = start + 2;
        while p < end {
            let tc_th = self.u8_at(p)?;
            p += 1;
            let tc = tc_th >> 4; // 0=DC, 1=AC
            let th = (tc_th & 0x0F) as usize;
            if th > 3 || tc > 1 {
                return Err(err("DHT のテーブル番号が範囲外"));
            }
            let mut counts = [0u8; 16];
            let mut total = 0usize;
            for c in counts.iter_mut() {
                *c = self.u8_at(p)?;
                p += 1;
                total += *c as usize;
            }
            if total > 256 {
                return Err(err("ハフマンシンボル数が過大"));
            }
            let mut huffval = Vec::with_capacity(total);
            for _ in 0..total {
                huffval.push(self.u8_at(p)?);
                p += 1;
            }
            let table = HuffmanTable::build(&counts, huffval);
            if tc == 0 {
                self.huff_dc[th] = Some(table);
            } else {
                self.huff_ac[th] = Some(table);
            }
        }
        self.pos = end;
        Ok(())
    }

    /// DRI（リスタート間隔）。
    fn read_dri(&mut self) -> Result<()> {
        let start = self.pos;
        let len = self.u16_be(start)?;
        if len != 4 {
            return Err(err("DRI セグメント長が不正"));
        }
        self.restart_interval = self.u16_be(start + 2)?;
        self.pos = start + len;
        Ok(())
    }

    /// APP14（Adobe）セグメントから color transform を読む。
    fn read_app14(&mut self) -> Result<()> {
        let start = self.pos;
        let len = self.u16_be(start)?;
        let end = start
            .checked_add(len)
            .ok_or_else(|| err("APP14 長オーバーフロー"))?;
        // "Adobe" シグネチャ（5 バイト）+ version(2) + flags0(2) + flags1(2) + transform(1)
        if len >= 14 {
            let sig = self.data.get(start + 2..start + 7);
            if sig == Some(b"Adobe") {
                self.has_adobe = true;
                self.adobe_transform = self.data.get(start + 13).copied();
            }
        }
        self.pos = end;
        Ok(())
    }

    /// SOS（スキャン開始）を読み、エントロピー符号化データを復号する。
    fn read_sos(&mut self) -> Result<()> {
        let start = self.pos;
        let len = self.u16_be(start)?;
        let ns = self.u8_at(start + 2)? as usize;
        if ns == 0 || ns > self.components.len() {
            return Err(err("SOS の成分数が不正"));
        }
        let mut scan_comps = Vec::with_capacity(ns);
        let mut p = start + 3;
        for _ in 0..ns {
            let cs = self.u8_at(p)?;
            let tdta = self.u8_at(p + 1)?;
            p += 2;
            let td = (tdta >> 4) as usize;
            let ta = (tdta & 0x0F) as usize;
            if td > 3 || ta > 3 {
                return Err(err("SOS のハフマンテーブル番号が範囲外"));
            }
            let comp_index = self
                .components
                .iter()
                .position(|c| c.id == cs)
                .ok_or_else(|| err("SOS が未知の成分を参照"))?;
            scan_comps.push(ScanComponent { comp_index, td, ta });
        }
        // Ss, Se, Ah/Al。baseline では 0,63,0,0。progressive ではスキャンごとに
        // スペクトル選択（Ss..Se）と逐次近似のビット位置（Ah=前回, Al=今回）を持つ。
        let ss = self.u8_at(p)? as usize;
        let se = self.u8_at(p + 1)? as usize;
        let ahal = self.u8_at(p + 2)?;
        let ah = ahal >> 4;
        let al = ahal & 0x0F;
        self.pos = start + len;

        if self.progressive {
            if se > 63 || ss > se || ah > 13 || al > 13 {
                return Err(err("progressive の SOS パラメータが不正"));
            }
            // DC スキャン（Ss==0）は複数成分インターリーブ可、AC スキャン（Ss>0）は
            // 単一成分のみ（T.81 G.1）。
            if ss > 0 && scan_comps.len() != 1 {
                return Err(err(
                    "progressive の AC スキャンは単一成分でなければならない",
                ));
            }
            self.prepare_progressive()?;
            self.decode_progressive_scan(&scan_comps, ss, se, ah, al)?;
            self.scan_count += 1;
            Ok(())
        } else {
            self.decode_scan(&scan_comps)?;
            self.scan_count += 1;
            Ok(())
        }
    }

    /// インターリーブされた MCU 列を復号して各成分の平面を埋める。
    fn decode_scan(&mut self, scan: &[ScanComponent]) -> Result<()> {
        // 最大サンプリングファクタ
        let hmax = self.components.iter().map(|c| c.h).max().unwrap_or(1) as usize;
        let vmax = self.components.iter().map(|c| c.v).max().unwrap_or(1) as usize;
        let mcus_x = self.frame_width.div_ceil(8 * hmax);
        let mcus_y = self.frame_height.div_ceil(8 * vmax);

        // 各成分の平面を確保
        for c in self.components.iter_mut() {
            c.blocks_per_line = mcus_x * c.h as usize;
            c.blocks_per_col = mcus_y * c.v as usize;
            c.stride = c.blocks_per_line * 8;
            let plane_h = c.blocks_per_col * 8;
            let total = c
                .stride
                .checked_mul(plane_h)
                .ok_or_else(|| err("成分平面サイズオーバーフロー"))?;
            if total as u64 > MAX_PIXELS.saturating_mul(4) {
                return Err(err("成分平面が大きすぎる"));
            }
            c.plane = vec![0u8; total];
        }

        let mut br = BitReader::new(self.data, self.pos);
        let mut pred = vec![0i32; self.components.len()];
        let mut mcu_count = 0usize;
        let interval = self.restart_interval;

        for my in 0..mcus_y {
            for mx in 0..mcus_x {
                // リスタート処理
                if interval != 0 && mcu_count != 0 && mcu_count.is_multiple_of(interval) {
                    self.handle_restart(&mut br, &mut pred)?;
                }
                // この MCU 内の各成分・各ブロックを復号
                for sc in scan {
                    let (h, v, tq) = {
                        let c = &self.components[sc.comp_index];
                        (c.h as usize, c.v as usize, c.tq as usize)
                    };
                    for by in 0..v {
                        for bx in 0..h {
                            let mut coeffs = [0f32; 64];
                            self.decode_block(
                                &mut br,
                                sc,
                                tq,
                                &mut pred[sc.comp_index],
                                &mut coeffs,
                            )?;
                            let pixels = idct_8x8(&coeffs);
                            // 平面へ書き込み
                            let c = &mut self.components[sc.comp_index];
                            let blk_x = mx * h + bx;
                            let blk_y = my * v + by;
                            let ox = blk_x * 8;
                            let oy = blk_y * 8;
                            for yy in 0..8 {
                                let row = oy + yy;
                                if row >= c.blocks_per_col * 8 {
                                    break;
                                }
                                let base = row * c.stride + ox;
                                for xx in 0..8 {
                                    if let Some(dst) = c.plane.get_mut(base + xx) {
                                        *dst = pixels[yy * 8 + xx];
                                    }
                                }
                            }
                        }
                    }
                }
                mcu_count += 1;
            }
        }
        // pos を進める（次のマーカーへ）。br.pos はマーカー手前を指す。
        self.pos = br.pos;
        Ok(())
    }

    /// リスタートマーカー（RSTn）を処理する: バイト境界に揃え、DC 予測子をリセット。
    fn handle_restart(&self, br: &mut BitReader, pred: &mut [i32]) -> Result<()> {
        self.skip_restart(br);
        for v in pred.iter_mut() {
            *v = 0;
        }
        Ok(())
    }

    /// ビットリーダをバイト境界に揃え、次の RSTn マーカーを読み飛ばす。
    /// DC 予測子や EOBRUN のリセットは呼び出し側の責務（progressive と共有）。
    fn skip_restart(&self, br: &mut BitReader) {
        br.reset_to_byte();
        // br.pos から次の 0xFF Dn マーカーを読み飛ばす
        let data = br.data;
        let mut p = br.pos;
        // フィルバイト 0xFF を進める
        while p + 1 < data.len() {
            if data[p] == 0xFF {
                let m = data[p + 1];
                if (0xD0..=0xD7).contains(&m) {
                    p += 2;
                    break;
                } else if m == 0xFF {
                    p += 1;
                    continue;
                } else if m == 0x00 {
                    // スタッフィング: マーカーではない、進めて探索継続
                    p += 2;
                    continue;
                } else {
                    // 別マーカー: リスタートが欠落。ここで止める
                    break;
                }
            }
            p += 1;
        }
        br.pos = p;
        br.hit_marker = false;
        br.marker = 0;
        br.bit_buf = 0;
        br.bit_count = 0;
    }

    /// 1 ブロック（8x8）を復号してデクオンタイズした係数（自然順）を返す。
    fn decode_block(
        &self,
        br: &mut BitReader,
        sc: &ScanComponent,
        tq: usize,
        pred: &mut i32,
        out: &mut [f32; 64],
    ) -> Result<()> {
        let qt = self
            .qtables
            .get(tq)
            .and_then(|t| t.as_ref())
            .ok_or_else(|| err("参照された量子化テーブルが未定義"))?;
        let dc_tab = self
            .huff_dc
            .get(sc.td)
            .and_then(|t| t.as_ref())
            .ok_or_else(|| err("参照された DC ハフマンテーブルが未定義"))?;
        let ac_tab = self
            .huff_ac
            .get(sc.ta)
            .and_then(|t| t.as_ref())
            .ok_or_else(|| err("参照された AC ハフマンテーブルが未定義"))?;

        // 自然順の係数バッファ（ジグザグ→自然はここで変換）
        let mut coef = [0i32; 64];

        // DC 係数
        let t = dc_tab.decode(br)?;
        if t > 11 {
            return Err(err("DC のビット長が不正"));
        }
        let diff = extend(br.read_bits(t)?, t);
        *pred = pred.wrapping_add(diff);
        coef[0] = *pred;

        // AC 係数
        let mut k = 1usize;
        while k < 64 {
            let rs = ac_tab.decode(br)?;
            let r = (rs >> 4) as usize; // ゼロランレングス
            let s = rs & 0x0F; // ビット長
            if s == 0 {
                if r == 15 {
                    k += 16; // ZRL: 16 個のゼロ
                    continue;
                }
                break; // EOB
            }
            k += r;
            if k >= 64 {
                break;
            }
            let val = extend(br.read_bits(s)?, s);
            // ジグザグ index k → 自然順
            if let Some(&nat) = ZIGZAG.get(k) {
                coef[nat] = val;
            }
            k += 1;
        }

        // デクオンタイズ（自然順同士の乗算）
        for i in 0..64 {
            out[i] = (coef[i] as f32) * (qt[i] as f32);
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // progressive 経路
    // -----------------------------------------------------------------------

    /// progressive の係数バッファを確保する（最初の SOS で 1 回だけ）。
    ///
    /// 各成分のブロック格子は baseline と同じ MCU 整列（`blocks_per_line =
    /// mcus_x * h`）で確保する。非インターリーブスキャンはこの格子の一部だけを
    /// 走査する。
    fn prepare_progressive(&mut self) -> Result<()> {
        if self.coeffs_ready {
            return Ok(());
        }
        let hmax = self.components.iter().map(|c| c.h).max().unwrap_or(1) as usize;
        let vmax = self.components.iter().map(|c| c.v).max().unwrap_or(1) as usize;
        let mcus_x = self.frame_width.div_ceil(8 * hmax);
        let mcus_y = self.frame_height.div_ceil(8 * vmax);
        for c in self.components.iter_mut() {
            c.blocks_per_line = mcus_x * c.h as usize;
            c.blocks_per_col = mcus_y * c.v as usize;
            c.stride = c.blocks_per_line * 8;
            let nblocks = c
                .blocks_per_line
                .checked_mul(c.blocks_per_col)
                .ok_or_else(|| err("ブロック数オーバーフロー"))?;
            let total = nblocks
                .checked_mul(64)
                .ok_or_else(|| err("係数バッファサイズオーバーフロー"))?;
            if total as u64 > MAX_PIXELS.saturating_mul(4) {
                return Err(err("progressive の係数バッファが大きすぎる"));
            }
            c.coeffs = vec![0i32; total];
        }
        self.coeffs_ready = true;
        Ok(())
    }

    /// progressive のスキャンを 1 つ復号し、係数バッファを更新する。
    ///
    /// - `ss`/`se`: スペクトル選択（係数のジグザグ範囲）。`ss==0` は DC スキャン。
    /// - `ah`/`al`: 逐次近似。`ah==0` が初回スキャン、`ah!=0` が精緻化スキャン。
    fn decode_progressive_scan(
        &mut self,
        scan: &[ScanComponent],
        ss: usize,
        se: usize,
        ah: u8,
        al: u8,
    ) -> Result<()> {
        // 借用競合回避のため、必要なハフマンテーブルを先にクローンする。
        let dc_tabs: Vec<Option<HuffmanTable>> = scan
            .iter()
            .map(|sc| self.huff_dc.get(sc.td).and_then(|t| t.clone()))
            .collect();
        let ac_tabs: Vec<Option<HuffmanTable>> = scan
            .iter()
            .map(|sc| self.huff_ac.get(sc.ta).and_then(|t| t.clone()))
            .collect();

        let mut br = BitReader::new(self.data, self.pos);
        let interval = self.restart_interval;

        if scan.len() > 1 {
            // インターリーブ（DC のみ）。MCU 単位で各成分の h*v ブロックを走査。
            let hmax = self.components.iter().map(|c| c.h).max().unwrap_or(1) as usize;
            let vmax = self.components.iter().map(|c| c.v).max().unwrap_or(1) as usize;
            let mcus_x = self.frame_width.div_ceil(8 * hmax);
            let mcus_y = self.frame_height.div_ceil(8 * vmax);
            let mut pred = vec![0i32; self.components.len()];
            let mut mcu_count = 0usize;

            for my in 0..mcus_y {
                for mx in 0..mcus_x {
                    if interval != 0 && mcu_count != 0 && mcu_count.is_multiple_of(interval) {
                        self.skip_restart(&mut br);
                        for v in pred.iter_mut() {
                            *v = 0;
                        }
                    }
                    for (si, sc) in scan.iter().enumerate() {
                        let ci = sc.comp_index;
                        let (h, v, bpl) = {
                            let c = &self.components[ci];
                            (c.h as usize, c.v as usize, c.blocks_per_line)
                        };
                        for by in 0..v {
                            for bx in 0..h {
                                let blk_x = mx * h + bx;
                                let blk_y = my * v + by;
                                let idx = (blk_y * bpl + blk_x) * 64;
                                let block = match self.components[ci].coeffs.get_mut(idx..idx + 64)
                                {
                                    Some(b) => b,
                                    None => continue,
                                };
                                if ah == 0 {
                                    let dc = dc_tabs[si]
                                        .as_ref()
                                        .ok_or_else(|| err("DC ハフマンテーブルが未定義"))?;
                                    decode_dc_first(block, &mut br, dc, &mut pred[ci], al)?;
                                } else {
                                    decode_dc_refine(block, &mut br, al)?;
                                }
                            }
                        }
                    }
                    mcu_count += 1;
                }
            }
        } else {
            // 非インターリーブ（単一成分）。成分自身のブロック格子を走査する。
            let sc = &scan[0];
            let ci = sc.comp_index;
            let hmax = self.components.iter().map(|c| c.h).max().unwrap_or(1) as usize;
            let vmax = self.components.iter().map(|c| c.v).max().unwrap_or(1) as usize;
            let (h, v, bpl) = {
                let c = &self.components[ci];
                (c.h as usize, c.v as usize, c.blocks_per_line)
            };
            // 非インターリーブのブロック数 = ceil(成分サンプル数 / 8)。
            let comp_w = (self.frame_width * h).div_ceil(hmax);
            let comp_h = (self.frame_height * v).div_ceil(vmax);
            let ni_bpl = comp_w.div_ceil(8);
            let ni_bpc = comp_h.div_ceil(8);

            let dc_tab = dc_tabs[0].clone();
            let ac_tab = ac_tabs[0].clone();
            let mut pred = 0i32;
            let mut eobrun = 0u32;
            let mut count = 0usize;

            for by in 0..ni_bpc {
                for bx in 0..ni_bpl {
                    if interval != 0 && count != 0 && count.is_multiple_of(interval) {
                        self.skip_restart(&mut br);
                        pred = 0;
                        eobrun = 0;
                    }
                    let idx = (by * bpl + bx) * 64;
                    let block = match self.components[ci].coeffs.get_mut(idx..idx + 64) {
                        Some(b) => b,
                        None => {
                            count += 1;
                            continue;
                        }
                    };
                    if ss == 0 {
                        // DC スキャン
                        if ah == 0 {
                            let dc = dc_tab
                                .as_ref()
                                .ok_or_else(|| err("DC ハフマンテーブルが未定義"))?;
                            decode_dc_first(block, &mut br, dc, &mut pred, al)?;
                        } else {
                            decode_dc_refine(block, &mut br, al)?;
                        }
                    } else {
                        // AC スキャン
                        let ac = ac_tab
                            .as_ref()
                            .ok_or_else(|| err("AC ハフマンテーブルが未定義"))?;
                        if ah == 0 {
                            decode_ac_first(block, &mut br, ac, ss, se, al, &mut eobrun)?;
                        } else {
                            decode_ac_refine(block, &mut br, ac, ss, se, al, &mut eobrun)?;
                        }
                    }
                    count += 1;
                }
            }
        }

        self.pos = br.pos;
        Ok(())
    }

    /// 全 progressive スキャン完了後、係数バッファをデクオンタイズ + 逆 DCT して
    /// 各成分の平面を埋める。
    fn finalize_progressive(&mut self) -> Result<()> {
        for ci in 0..self.components.len() {
            let tq = self.components[ci].tq as usize;
            let qt = self
                .qtables
                .get(tq)
                .and_then(|t| *t)
                .ok_or_else(|| err("参照された量子化テーブルが未定義"))?;
            let (bpl, bpc, stride) = {
                let c = &self.components[ci];
                (c.blocks_per_line, c.blocks_per_col, c.stride)
            };
            let plane_h = bpc * 8;
            let total = stride
                .checked_mul(plane_h)
                .ok_or_else(|| err("成分平面サイズオーバーフロー"))?;
            let mut plane = vec![0u8; total];
            for by in 0..bpc {
                for bx in 0..bpl {
                    let idx = (by * bpl + bx) * 64;
                    let mut block = [0f32; 64];
                    {
                        let coeffs = &self.components[ci].coeffs;
                        for (i, slot) in block.iter_mut().enumerate() {
                            let c = coeffs.get(idx + i).copied().unwrap_or(0);
                            *slot = (c as f32) * (qt[i] as f32);
                        }
                    }
                    let pixels = idct_8x8(&block);
                    let ox = bx * 8;
                    let oy = by * 8;
                    for yy in 0..8 {
                        let row = oy + yy;
                        let base = row * stride + ox;
                        for xx in 0..8 {
                            if let Some(dst) = plane.get_mut(base + xx) {
                                *dst = pixels[yy * 8 + xx];
                            }
                        }
                    }
                }
            }
            self.components[ci].plane = plane;
        }
        Ok(())
    }

    /// 復号済みの各成分平面から最終画像を組み立てる（アップサンプリング + 色変換）。
    fn assemble(&self) -> Result<DecodedImage> {
        let w = self.frame_width;
        let h = self.frame_height;
        let nc = self.components.len();
        let hmax = self.components.iter().map(|c| c.h).max().unwrap_or(1) as usize;
        let vmax = self.components.iter().map(|c| c.v).max().unwrap_or(1) as usize;

        let out_comps = nc as u8;
        let total = (w as u64)
            .checked_mul(h as u64)
            .and_then(|x| x.checked_mul(nc as u64))
            .ok_or_else(|| err("出力サイズオーバーフロー"))?;
        if total > MAX_PIXELS.saturating_mul(4) {
            return Err(err("出力が大きすぎる"));
        }
        let mut data = vec![0u8; total as usize];

        // 各ピクセルについて、各成分をアップサンプリングして取得する。
        //
        // サブサンプリングされた成分（クロマ）は**中心揃えの双線形補間**で
        // アップサンプリングする（box=最近傍よりエッジが滑らかになり、一般的な
        // JPEG デコーダの出力に一致する）。等倍の成分（通常は輝度）は補間係数が
        // 0 になり、そのまま元のサンプルを返す。
        for y in 0..h {
            // 各成分の垂直方向のソース座標と補間係数を先に求める
            for x in 0..w {
                let pi = (y * w + x) * nc;
                for (ci, c) in self.components.iter().enumerate() {
                    let plane_w = c.blocks_per_line * 8;
                    let plane_h = c.blocks_per_col * 8;
                    let val = sample_bilinear(
                        &c.plane,
                        c.stride,
                        plane_w,
                        plane_h,
                        x,
                        y,
                        c.h as usize,
                        c.v as usize,
                        hmax,
                        vmax,
                    );
                    if let Some(slot) = data.get_mut(pi + ci) {
                        *slot = val;
                    }
                }
            }
        }

        // 色変換
        self.color_transform(&mut data, nc);

        Ok(DecodedImage {
            width: w as u32,
            height: h as u32,
            components: out_comps,
            data,
        })
    }

    /// 成分インターリーブ済みデータに対し、成分数と APP14 に応じた色変換を行う。
    fn color_transform(&self, data: &mut [u8], nc: usize) {
        match nc {
            3 => {
                // transform=0（Adobe 明示 RGB）なら変換しない。それ以外は YCbCr→RGB。
                let do_ycc = self.adobe_transform != Some(0);
                if do_ycc {
                    for px in data.chunks_mut(3) {
                        if px.len() < 3 {
                            break;
                        }
                        let (r, g, b) = ycbcr_to_rgb(px[0], px[1], px[2]);
                        px[0] = r;
                        px[1] = g;
                        px[2] = b;
                    }
                }
            }
            4 => {
                // transform=2 なら YCCK→CMYK、それ以外はそのまま CMYK。
                let do_ycck = self.adobe_transform == Some(2);
                for px in data.chunks_mut(4) {
                    if px.len() < 4 {
                        break;
                    }
                    if do_ycck {
                        // YCCK: Y,Cb,Cr,K。YCbCr→RGB したのち C=255-R 等で CMY を得る。
                        let (r, g, b) = ycbcr_to_rgb(px[0], px[1], px[2]);
                        px[0] = 255 - r;
                        px[1] = 255 - g;
                        px[2] = 255 - b;
                        // px[3]=K はそのまま
                    }
                    // Adobe 製 CMYK はサンプルが反転している慣習。APP14 があれば反転して
                    // 「0=インクなし〜255=最大インク」に正規化する。
                    if self.has_adobe {
                        for v in px.iter_mut() {
                            *v = 255 - *v;
                        }
                    }
                }
            }
            _ => {} // 1 成分グレースケールはそのまま
        }
    }
}

/// 成分平面を中心揃えの双線形補間でサンプリングする。
///
/// 出力ピクセル `(x, y)` に対応する成分座標を
/// `src = (out + 0.5) * factor / max - 0.5` で求め、両隣の低解像度サンプルを
/// 線形補間する。等倍成分（factor == max）では補間係数が 0 となり元サンプルを返す。
#[allow(clippy::too_many_arguments)]
fn sample_bilinear(
    plane: &[u8],
    stride: usize,
    plane_w: usize,
    plane_h: usize,
    x: usize,
    y: usize,
    h: usize,
    v: usize,
    hmax: usize,
    vmax: usize,
) -> u8 {
    // 水平方向のソース位置と補間係数
    let (x0, x1, fx) = src_coord(x, h, hmax, plane_w);
    let (y0, y1, fy) = src_coord(y, v, vmax, plane_h);

    let get =
        |sx: usize, sy: usize| -> f32 { plane.get(sy * stride + sx).copied().unwrap_or(0) as f32 };
    let top = get(x0, y0) * (1.0 - fx) + get(x1, y0) * fx;
    let bot = get(x0, y1) * (1.0 - fx) + get(x1, y1) * fx;
    let val = top * (1.0 - fy) + bot * fy;
    val.round().clamp(0.0, 255.0) as u8
}

/// 1 次元の中心揃えソース座標を返す: `(lo, hi, frac)`。
/// `out` 出力座標に対し、補間元となる 2 つの低解像度インデックスと係数を求める。
fn src_coord(out: usize, factor: usize, max: usize, plane_len: usize) -> (usize, usize, f32) {
    // src = (out + 0.5) * factor / max - 0.5
    let num = (2 * out + 1) * factor; // = (out+0.5)*factor*2
    let denom = (2 * max) as f32;
    let src = num as f32 / denom - 0.5;
    if src <= 0.0 {
        return (0, 0, 0.0);
    }
    let lo = src.floor() as usize;
    let frac = src - lo as f32;
    let last = plane_len.saturating_sub(1);
    let lo_c = lo.min(last);
    let hi_c = (lo + 1).min(last);
    (lo_c, hi_c, frac)
}

/// YCbCr→RGB 変換（ITU-R BT.601, full range）。
fn ycbcr_to_rgb(y: u8, cb: u8, cr: u8) -> (u8, u8, u8) {
    let y = y as f32;
    let cb = cb as f32 - 128.0;
    let cr = cr as f32 - 128.0;
    let r = y + 1.402 * cr;
    let g = y - 0.344136 * cb - 0.714136 * cr;
    let b = y + 1.772 * cb;
    (
        r.round().clamp(0.0, 255.0) as u8,
        g.round().clamp(0.0, 255.0) as u8,
        b.round().clamp(0.0, 255.0) as u8,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // テストベクタは外部ツール（.NET System.Drawing）で生成済み（tests/fixtures）。
    // ここでは内部 3 段（ハフマン / IDCT / 色変換）を実装から独立に検証する単体テスト。

    #[test]
    fn idct_dc_only() {
        // DC のみ（全係数 0 以外は DC=val*8）→ 平坦なブロックになるはず。
        // 逆 DCT 後の定数 = DC_coeff / 8 + 128（DC 項のみ, 係数=val）。
        let mut block = [0f32; 64];
        block[0] = 8.0 * 100.0; // DC 係数（デクオンタイズ済みのつもり）
        let out = idct_8x8(&block);
        // 1D IDCT 2 回で DC 項の寄与 = block[0] * (1/sqrt2) * 0.5 * (1/sqrt2) * 0.5
        //   = block[0] * 0.25 * 0.5 = block[0]/8。+128。
        let expected = (8.0f32 * 100.0 / 8.0 + 128.0).round() as u8;
        for &v in out.iter() {
            assert_eq!(v, expected);
        }
    }

    #[test]
    fn idct_zero_is_mid_gray() {
        let block = [0f32; 64];
        let out = idct_8x8(&block);
        for &v in out.iter() {
            assert_eq!(v, 128);
        }
    }

    #[test]
    fn extend_values() {
        // T.81 EXTEND の典型例
        assert_eq!(extend(0, 0), 0);
        assert_eq!(extend(0b1, 1), 1);
        assert_eq!(extend(0b0, 1), -1);
        assert_eq!(extend(0b11, 2), 3);
        assert_eq!(extend(0b00, 2), -3);
        assert_eq!(extend(0b10, 2), 2);
    }

    #[test]
    fn ycbcr_gray() {
        // Cb=Cr=128 のグレーは R=G=B=Y
        let (r, g, b) = ycbcr_to_rgb(100, 128, 128);
        assert_eq!((r, g, b), (100, 100, 100));
    }

    #[test]
    fn ycbcr_red() {
        // Y=76, Cb=85, Cr=255 は概ね赤（BT.601 で純赤の YCbCr）
        let (r, g, b) = ycbcr_to_rgb(76, 85, 255);
        assert!(r > 240, "r={r}");
        assert!(g < 20, "g={g}");
        assert!(b < 20, "b={b}");
    }

    #[test]
    fn huffman_build_and_decode() {
        // 符号長 2 のコードが 3 個（00,01,10）、符号長 3 が 2 個（110,111）。
        // counts: len2=3, len3=2
        let mut counts = [0u8; 16];
        counts[1] = 3; // 符号長 2
        counts[2] = 2; // 符号長 3
        let huffval = vec![10, 20, 30, 40, 50];
        let table = HuffmanTable::build(&counts, huffval);
        // ビット列 "00" -> 10, "01" -> 20, "10" -> 30, "110" -> 40, "111" -> 50
        // バイト境界に詰める: 00 01 10 11 0111 1... => 0b00_01_10_11, 0b0111_1xxx
        let data = [0b0001_1011u8, 0b0111_1000u8];
        let mut br = BitReader::new(&data, 0);
        assert_eq!(table.decode(&mut br).unwrap(), 10);
        assert_eq!(table.decode(&mut br).unwrap(), 20);
        assert_eq!(table.decode(&mut br).unwrap(), 30);
        assert_eq!(table.decode(&mut br).unwrap(), 40);
        assert_eq!(table.decode(&mut br).unwrap(), 50);
    }

    #[test]
    fn bitreader_stuffing() {
        // 0xFF 0x00 はスタッフィング → 0xFF を 1 バイトとして供給
        let data = [0xFFu8, 0x00, 0x0F];
        let mut br = BitReader::new(&data, 0);
        let v = br.read_bits(8).unwrap();
        assert_eq!(v, 0xFF);
        let v2 = br.read_bits(8).unwrap();
        assert_eq!(v2, 0x0F);
    }

    #[test]
    fn bitreader_marker_stops() {
        // 0xFF 0xD9 (EOI) でマーカー停止 → 以降 0 ビット
        let data = [0b1010_0000u8, 0xFF, 0xD9];
        let mut br = BitReader::new(&data, 0);
        assert_eq!(br.read_bits(4).unwrap(), 0b1010);
        // 残り 4 ビット読むとバイト境界、その次の fill でマーカー検出
        let _ = br.read_bits(4).unwrap();
        let after = br.read_bits(8).unwrap();
        assert_eq!(after, 0);
        assert!(br.hit_marker);
    }

    #[test]
    fn dqt_zigzag_to_natural() {
        // DQT を 1 つ含む最小セグメントを食わせ、自然順に並ぶことを確認。
        // pq=0, tq=0, 64 バイト（ジグザグ順に 0..63 を入れる）
        let mut seg = Vec::new();
        let body_len = 2 + 1 + 64;
        seg.push((body_len >> 8) as u8);
        seg.push((body_len & 0xFF) as u8);
        seg.push(0x00); // pq=0 tq=0
        for i in 0..64u8 {
            seg.push(i);
        }
        let mut dec = Decoder::new(&seg);
        dec.pos = 0;
        dec.read_dqt().unwrap();
        let qt = dec.qtables[0].unwrap();
        // ジグザグ i=0→自然0, i=1→自然1, i=2→自然8 ...
        assert_eq!(qt[0], 0);
        assert_eq!(qt[1], 1);
        assert_eq!(qt[8], 2);
        assert_eq!(qt[16], 3);
    }

    #[test]
    fn src_coord_centered() {
        // 等倍（factor==max）: 補間係数 0 なので hi は使われず、lo がそのまま採用される。
        let (lo, _hi, f) = src_coord(5, 2, 2, 16);
        assert_eq!(lo, 5);
        assert_eq!(f, 0.0);
        // 2x アップ（クロマ 1 → 輝度 2）: 出力 0 はソース先頭にクランプ。
        let (lo, _hi, f) = src_coord(0, 1, 2, 8);
        assert_eq!(lo, 0);
        assert_eq!(f, 0.0);
        // 出力 2（src=(2.5)*0.5-0.5=0.75）→ lo=0, frac=0.75
        let (lo, hi, f) = src_coord(2, 1, 2, 8);
        assert_eq!((lo, hi), (0, 1));
        assert!((f - 0.75).abs() < 1e-5, "frac={f}");
    }

    #[test]
    fn cmyk_adobe_inversion_unit() {
        // CMYK JPEG は System.Drawing で生成できないため、APP14 反転ロジックを
        // 合成データで単体検証する（この旨はテストコメントに明記）。
        // has_adobe=true, transform=0（YCCK 変換なし）の純 CMYK 反転。
        let mut dec = Decoder::new(&[]);
        dec.has_adobe = true;
        dec.adobe_transform = Some(0);
        // 1 ピクセル CMYK = [10, 20, 30, 40]（Adobe 反転前 = 格納値）
        let mut data = vec![10u8, 20, 30, 40];
        dec.color_transform(&mut data, 4);
        // 反転後 = 255 - v
        assert_eq!(data, vec![245, 235, 225, 215]);
    }

    #[test]
    fn prog_dc_first_and_refine() {
        // DC 初回: カテゴリ 3（符号長 1 のコード "0"）→ 3bit "101"=5、extend(5,3)=5。
        // al=1 なので block[0] = pred(5) << 1 = 10。
        let mut counts = [0u8; 16];
        counts[0] = 1; // 符号長 1 のコード 1 個
        let tab = HuffmanTable::build(&counts, vec![3]);
        let data = [0b0101_0000u8]; // "0"(=cat3) + "101"(=5)
        let mut br = BitReader::new(&data, 0);
        let mut block = [0i32; 64];
        let mut pred = 0i32;
        decode_dc_first(&mut block, &mut br, &tab, &mut pred, 1).unwrap();
        assert_eq!(pred, 5);
        assert_eq!(block[0], 10);

        // DC 精緻化: ビット 1 → al=2 なので +4。
        let data = [0b1000_0000u8];
        let mut br = BitReader::new(&data, 0);
        decode_dc_refine(&mut block, &mut br, 2).unwrap();
        assert_eq!(block[0], 14);
    }

    #[test]
    fn prog_ac_first_eobrun_off_by_one() {
        // AC テーブル: "00"->0x00（EOB0）, "01"->0x10（EOB, r=1）。
        let mut counts = [0u8; 16];
        counts[1] = 2; // 符号長 2 のコード 2 個
        let tab = HuffmanTable::build(&counts, vec![0x00, 0x10]);

        // EOB0（"00"）: 現在ブロックのみ帯域終端 → 後続スキップは 0。
        let data = [0b0000_0000u8];
        let mut br = BitReader::new(&data, 0);
        let mut block = [0i32; 64];
        let mut eobrun = 0u32;
        decode_ac_first(&mut block, &mut br, &tab, 1, 63, 0, &mut eobrun).unwrap();
        assert_eq!(eobrun, 0, "EOB0 は後続 0 ブロック");

        // EOB（r=1, "01"）+ 付加 1bit "1": eobrun = (1<<1)-1 + 1 = 2。
        let data = [0b0110_0000u8]; // "01" + "1"
        let mut br = BitReader::new(&data, 0);
        let mut eobrun = 0u32;
        decode_ac_first(&mut block, &mut br, &tab, 1, 63, 0, &mut eobrun).unwrap();
        assert_eq!(eobrun, 2, "EOB1+1bit は後続 2 ブロック");

        // eobrun>0 の状態で呼ぶと 1 減らして即 return（ビット消費なし）。
        let data = [0b1111_1111u8];
        let mut br = BitReader::new(&data, 0);
        let pos0 = br.pos;
        let mut eobrun = 3u32;
        decode_ac_first(&mut block, &mut br, &tab, 1, 63, 0, &mut eobrun).unwrap();
        assert_eq!(eobrun, 2);
        assert_eq!(br.pos, pos0, "スキップ中はビットを読まない");
    }

    #[test]
    fn prog_ac_first_places_coefficient() {
        // "0"->0x21（r=2, s=1）: 2 ゼロ飛ばして位置 ss+2 に係数を置く。
        let mut counts = [0u8; 16];
        counts[0] = 1;
        let tab = HuffmanTable::build(&counts, vec![0x21]);
        // "0"(=0x21) + s=1 の値ビット "1"(=1, extend(1,1)=1)。al=2 → 1<<2=4。
        let data = [0b0100_0000u8];
        let mut br = BitReader::new(&data, 0);
        let mut block = [0i32; 64];
        let mut eobrun = 0u32;
        decode_ac_first(&mut block, &mut br, &tab, 1, 63, 2, &mut eobrun).unwrap();
        // ss=1 から r=2 個飛ばし → ジグザグ index 3 に配置。
        let nat = ZIGZAG[3];
        assert_eq!(block[nat], 4);
        assert_eq!(eobrun, 0);
    }

    #[test]
    fn prog_ac_refine_correction_bit() {
        // 既存の非ゼロ係数に補正ビットを足す（EOB 経路）。
        let mut counts = [0u8; 16];
        counts[1] = 2;
        let tab = HuffmanTable::build(&counts, vec![0x00, 0x10]);
        let mut block = [0i32; 64];
        let nat = ZIGZAG[1];
        block[nat] = 2; // 前スキャンで立った正の係数
                        // "00"(=EOB0) + 補正ビット "1"。al=0 → p1=1。2 は (2 & 1)==0 かつ正 → +1 = 3。
        let data = [0b0010_0000u8];
        let mut br = BitReader::new(&data, 0);
        let mut eobrun = 0u32;
        decode_ac_refine(&mut block, &mut br, &tab, 1, 63, 0, &mut eobrun).unwrap();
        assert_eq!(block[nat], 3);
        assert_eq!(eobrun, 0);
    }

    #[test]
    fn cmyk_ycck_transform_unit() {
        // YCCK→CMYK（transform=2）+ Adobe 反転の合成テスト。
        let mut dec = Decoder::new(&[]);
        dec.has_adobe = true;
        dec.adobe_transform = Some(2);
        // Y=128,Cb=128,Cr=128,K=0 → YCbCr(128,128,128)=RGB(128,128,128)
        //   CMY = 255-128 = 127, K=0。Adobe 反転で 255-127=128, K→255。
        let mut data = vec![128u8, 128, 128, 0];
        dec.color_transform(&mut data, 4);
        assert_eq!(data, vec![128, 128, 128, 255]);
    }
}
