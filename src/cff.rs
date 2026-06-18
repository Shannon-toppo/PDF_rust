//! CFF（Compact Font Format）フォントのパーサと Type 2 チャーストリング解釈器。
//!
//! PDF の `/FontFile3`（Subtype `Type1C` / `CIDFontType0C` / `OpenType`）と、
//! OTTO sfnt の `CFF ` テーブルをパースし、Type 2 チャーストリングを
//! グリフアウトライン（[`OutlineSegment`]）へ解釈する。レンダラ
//! （[`crate::render`]）がこれを使って CFF/OpenType フォントを描画する。
//!
//! 仕様の出典は Adobe Technical Note #5176（CFF）と #5177（Type 2）。
//!
//! ## 対応範囲
//!
//! - 非 CID CFF（Type1C）と CID キー付き CFF（CIDFontType0C）。
//! - Type 2 チャーストリング全演算子（hstem/vstem・hintmask・flex 系・seac 互換）。
//! - charset format 0/1/2、Encoding format 0/1（+supplements）。
//! - FDSelect format 0/3、FDArray・Private DICT・ローカル/グローバル Subr。
//!
//! ## スコープ外
//!
//! - 旧式 Type1（eexec）: 別物。本モジュールは扱わない。
//! - CFF2（major version != 1）: エラーにする。
//! - 埋め込み（サブセット化）: 本モジュールは読み取り専用。
//!
//! ## 耐故障性
//!
//! 入力は信頼できないデータとして扱う。境界検査（`data.get(..)`）と
//! checked 演算のみを使い、不正なファイルでも panic しない。壊れた
//! チャーストリングは「描けた分まで返す」で吸収する。

use std::collections::HashMap;

use crate::truetype::{font_err, OutlineSegment};
use crate::Result;

/// 解析済みの CFF フォント 1 書体分。
#[derive(Clone)]
pub struct CffFont {
    /// CFF データ全体。
    data: Vec<u8>,
    /// CharStrings INDEX の各エントリ範囲（添字 = GID）。
    char_strings: Vec<(usize, usize)>,
    /// Global Subr INDEX の各エントリ範囲。
    gsubrs: Vec<(usize, usize)>,
    /// FD（フォント辞書）ごとの Private 情報。
    /// 非 CID は 1 件。空なら既定値 1 件を入れる。
    privates: Vec<PrivateInfo>,
    /// GID → FD インデックスの対応。
    fd_select: FdSelect,
    /// GID → SID（非 CID）/ CID（CID キー付き）。Encoding supplements の
    /// SID→GID 逆引きで使う（現在は code_to_gid 経由で間接利用）。
    #[allow(dead_code)]
    charset: Vec<u16>,
    /// CID → GID（CID キー付きのみ）。
    cid_to_gid: HashMap<u16, u16>,
    /// グリフ名 → GID（非 CID のみ）。
    name_to_gid: HashMap<String, u16>,
    /// Unicode → GID（非 CID のみ）。
    unicode_to_gid: HashMap<char, u16>,
    /// built-in Encoding による code → GID（非 CID のみ、256 要素）。
    code_to_gid: Vec<u16>,
    /// FontMatrix（既定 `[0.001, 0, 0, 0.001, 0, 0]`）。
    font_matrix: [f64; 6],
    /// CID キー付きフォントか。
    is_cid: bool,
}

/// FD（フォント辞書）1 つ分の Private 情報。
#[derive(Clone)]
struct PrivateInfo {
    /// ローカル Subr INDEX の各エントリ範囲。
    subrs: Vec<(usize, usize)>,
    default_width_x: f64,
    nominal_width_x: f64,
}

impl Default for PrivateInfo {
    fn default() -> PrivateInfo {
        PrivateInfo {
            subrs: Vec::new(),
            default_width_x: 0.0,
            nominal_width_x: 0.0,
        }
    }
}

/// GID → FD インデックスの選択方式。
#[derive(Clone)]
enum FdSelect {
    /// すべて FD 0（非 CID もしくは FDSelect 省略）。
    Single,
    /// GID ごとに FD 番号を持つ（format 0）。
    PerGlyph(Vec<u8>),
    /// レンジ方式（format 3）。(first GID, fd) の列 + sentinel。
    Ranges(Vec<(u16, u8)>, u16),
}

impl FdSelect {
    /// GID から FD インデックスを引く。
    fn fd_for(&self, gid: u16) -> usize {
        match self {
            FdSelect::Single => 0,
            FdSelect::PerGlyph(v) => v.get(gid as usize).copied().unwrap_or(0) as usize,
            FdSelect::Ranges(ranges, sentinel) => {
                if gid >= *sentinel {
                    return 0;
                }
                // ranges は first 昇順。gid <= first となる直前のレンジ。
                let mut fd = 0u8;
                for &(first, f) in ranges {
                    if gid >= first {
                        fd = f;
                    } else {
                        break;
                    }
                }
                fd as usize
            }
        }
    }
}

impl std::fmt::Debug for CffFont {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CffFont")
            .field("num_glyphs", &self.char_strings.len())
            .field("is_cid", &self.is_cid)
            .finish()
    }
}

// --- バイト読み出し補助（境界検査・big-endian） ---

/// `data[pos]` を読む。範囲外は `None`。
fn read_u8(data: &[u8], pos: usize) -> Option<u8> {
    data.get(pos).copied()
}

/// `data[pos..pos+2]` を big-endian u16 として読む。
fn read_u16(data: &[u8], pos: usize) -> Option<u16> {
    let b = data.get(pos..pos.checked_add(2)?)?;
    Some(u16::from_be_bytes([b[0], b[1]]))
}

/// オフセットを `off_size` バイト（1..=4）の big-endian で読む。
fn read_offset(data: &[u8], pos: usize, off_size: u8) -> Option<usize> {
    let n = off_size as usize;
    if !(1..=4).contains(&n) {
        return None;
    }
    let b = data.get(pos..pos.checked_add(n)?)?;
    let mut v = 0usize;
    for &byte in b {
        v = v.checked_mul(256)?.checked_add(byte as usize)?;
    }
    Some(v)
}

impl CffFont {
    /// CFF データをパースする。
    pub fn parse(data: Vec<u8>) -> Result<CffFont> {
        // ヘッダ: major(1), minor(1), hdrSize(1), offSize(1)。
        let major = read_u8(&data, 0).ok_or_else(|| font_err("CFF ヘッダが短すぎます"))?;
        if major != 1 {
            return Err(font_err(
                "CFF major version が 1 ではありません（CFF2 非対応）",
            ));
        }
        let hdr_size =
            read_u8(&data, 2).ok_or_else(|| font_err("CFF hdrSize が読めません"))? as usize;

        // Name INDEX（読み飛ばす）。
        let (_, after_name) = parse_index(&data, hdr_size)?;
        // Top DICT INDEX。
        let (top_dicts, after_top) = parse_index(&data, after_name)?;
        // String INDEX。
        let (strings, after_strings) = parse_index(&data, after_top)?;
        // Global Subr INDEX。
        let (gsubrs, _after_gsubr) = parse_index(&data, after_strings)?;

        // Top DICT は 1 件目を使う。
        let (td_start, td_end) = *top_dicts
            .first()
            .ok_or_else(|| font_err("Top DICT INDEX が空です"))?;
        let top_dict = parse_dict(data.get(td_start..td_end).unwrap_or(&[]));

        // CharstringType（既定 2、それ以外はエラー）。
        if let Some(v) = top_dict.get_op(0x0c06).and_then(|ops| ops.first()) {
            if (*v as i64) != 2 {
                return Err(font_err("CharstringType が 2 ではありません"));
            }
        }

        // CharStrings INDEX。
        let cs_off = top_dict
            .get_op(17)
            .and_then(|ops| ops.first())
            .map(|v| *v as usize)
            .ok_or_else(|| font_err("CharStrings オフセットがありません"))?;
        let (char_strings, _) = parse_index(&data, cs_off)?;
        let num_glyphs = char_strings.len();

        // FontMatrix（既定 [0.001,0,0,0.001,0,0]）。
        let font_matrix = match top_dict.get_op(0x0c07) {
            Some(ops) if ops.len() >= 6 => [ops[0], ops[1], ops[2], ops[3], ops[4], ops[5]],
            _ => [0.001, 0.0, 0.0, 0.001, 0.0, 0.0],
        };

        // CID 判定（ROS = 0x0c1e）。
        let is_cid = top_dict.get_op(0x0c1e).is_some();

        // Private DICT・FDArray・FDSelect。
        let (privates, fd_select) = if is_cid {
            parse_cid_fds(&data, &top_dict, num_glyphs)?
        } else {
            // 非 CID は Top DICT の Private を 1 件。
            let priv_info = parse_private(&data, &top_dict);
            (vec![priv_info], FdSelect::Single)
        };

        // charset。
        let charset = parse_charset(&data, &top_dict, num_glyphs);

        // CID → GID 逆引き（CID キー付きのみ）。
        let mut cid_to_gid = HashMap::new();
        if is_cid {
            for (gid, &cid) in charset.iter().enumerate() {
                cid_to_gid.entry(cid).or_insert(gid as u16);
            }
        }

        // 非 CID: グリフ名・Unicode・built-in Encoding を構築。
        let mut name_to_gid = HashMap::new();
        let mut unicode_to_gid = HashMap::new();
        let mut code_to_gid = vec![0u16; 256];
        if !is_cid {
            for (gid, &sid) in charset.iter().enumerate() {
                let name = sid_to_string(sid, &strings, &data);
                if let Some(name) = name {
                    name_to_gid.entry(name.clone()).or_insert(gid as u16);
                    if let Some(c) = crate::encoding::glyph_name_to_unicode(&name) {
                        unicode_to_gid.entry(c).or_insert(gid as u16);
                    }
                }
            }
            // built-in Encoding。
            parse_encoding(
                &data,
                &top_dict,
                &charset,
                &strings,
                &mut code_to_gid,
                &unicode_to_gid,
            );
        }

        Ok(CffFont {
            data,
            char_strings,
            gsubrs,
            privates,
            fd_select,
            charset,
            cid_to_gid,
            name_to_gid,
            unicode_to_gid,
            code_to_gid,
            font_matrix,
            is_cid,
        })
    }

    /// グリフ数。
    pub fn num_glyphs(&self) -> u16 {
        self.char_strings.len().min(u16::MAX as usize) as u16
    }

    /// CID キー付きフォントか。
    pub fn is_cid(&self) -> bool {
        self.is_cid
    }

    /// FontMatrix（通常 `[0.001, 0, 0, 0.001, 0, 0]`）。
    pub fn font_matrix(&self) -> [f64; 6] {
        self.font_matrix
    }

    /// CID → GID（CID キー付きのみ）。非 CID では常に `None`。
    pub fn gid_for_cid(&self, cid: u16) -> Option<u16> {
        self.cid_to_gid.get(&cid).copied()
    }

    /// グリフ名 → GID（非 CID のみ）。
    pub fn gid_by_name(&self, name: &str) -> Option<u16> {
        self.name_to_gid.get(name).copied()
    }

    /// Unicode → GID（非 CID のみ）。
    pub fn gid_by_unicode(&self, c: char) -> Option<u16> {
        self.unicode_to_gid.get(&c).copied()
    }

    /// built-in Encoding による code → GID（非 CID のみ）。
    pub fn gid_by_code(&self, code: u8) -> Option<u16> {
        let gid = *self.code_to_gid.get(code as usize)?;
        if gid == 0 {
            None
        } else {
            Some(gid)
        }
    }

    /// グリフアウトライン（チャーストリング単位、y 上向き）。
    /// 空グリフは `Some(vec![])`、gid 範囲外は `None`。
    pub fn glyph_outline(&self, gid: u16) -> Option<Vec<OutlineSegment>> {
        self.glyph_outline_and_advance(gid).map(|(o, _)| o)
    }

    /// グリフアウトラインと advance 幅（チャーストリング単位）。
    pub fn glyph_outline_and_advance(&self, gid: u16) -> Option<(Vec<OutlineSegment>, f64)> {
        let (cs_start, cs_end) = *self.char_strings.get(gid as usize)?;
        let cs = self.data.get(cs_start..cs_end)?;
        let fd = self.fd_select.fd_for(gid);
        let private = self.privates.get(fd).cloned().unwrap_or_default();

        let mut interp = Type2Interp::new(self, private);
        interp.run(cs, 0);
        interp.finish();
        let advance = interp.advance();
        Some((interp.segments, advance))
    }
}

/// SID → 文字列（標準文字列表 or String INDEX）。
fn sid_to_string(sid: u16, strings: &[(usize, usize)], data: &[u8]) -> Option<String> {
    if (sid as usize) < STANDARD_STRINGS.len() {
        return Some(STANDARD_STRINGS[sid as usize].to_string());
    }
    let idx = sid as usize - STANDARD_STRINGS.len();
    let (start, end) = *strings.get(idx)?;
    let bytes = data.get(start..end)?;
    Some(bytes.iter().map(|&b| b as char).collect())
}

/// INDEX を解析し、`(各エントリの (start,end) 範囲, INDEX 終端の次オフセット)` を返す。
///
/// INDEX 形式: count(u16)。count=0 なら 2 バイトで終端。続いて offSize(u8)、
/// offset[count+1]（各 offSize バイト、1 始まり）、データ本体。
fn parse_index(data: &[u8], pos: usize) -> Result<(Vec<(usize, usize)>, usize)> {
    let count = read_u16(data, pos).ok_or_else(|| font_err("INDEX count が読めません"))? as usize;
    if count == 0 {
        // count=0 は 2 バイトで終端。
        return Ok((
            Vec::new(),
            pos.checked_add(2)
                .ok_or_else(|| font_err("INDEX オフセット計算"))?,
        ));
    }
    let off_size = read_u8(data, pos + 2).ok_or_else(|| font_err("INDEX offSize が読めません"))?;
    if !(1..=4).contains(&off_size) {
        return Err(font_err("INDEX offSize が範囲外"));
    }
    // オフセット配列は pos+3 から (count+1) 個、各 off_size バイト。
    let offset_array_start = pos.checked_add(3).ok_or_else(|| font_err("INDEX 計算"))?;
    let mut offsets = Vec::with_capacity(count + 1);
    for i in 0..=count {
        let op = offset_array_start
            .checked_add(
                i.checked_mul(off_size as usize)
                    .ok_or_else(|| font_err("INDEX 計算"))?,
            )
            .ok_or_else(|| font_err("INDEX 計算"))?;
        let o = read_offset(data, op, off_size)
            .ok_or_else(|| font_err("INDEX オフセットが読めません"))?;
        offsets.push(o);
    }
    // データ本体の基準位置 = オフセット配列の末尾 − 1（offset は 1 始まり）。
    let data_base = offset_array_start
        .checked_add(
            (count + 1)
                .checked_mul(off_size as usize)
                .ok_or_else(|| font_err("INDEX 計算"))?,
        )
        .ok_or_else(|| font_err("INDEX 計算"))?;
    // base は最初のオフセット（=1）を引いた位置。
    let base = data_base
        .checked_sub(1)
        .ok_or_else(|| font_err("INDEX 計算"))?;

    let mut entries = Vec::with_capacity(count);
    for i in 0..count {
        let start = base
            .checked_add(offsets[i])
            .ok_or_else(|| font_err("INDEX 計算"))?;
        let end = base
            .checked_add(offsets[i + 1])
            .ok_or_else(|| font_err("INDEX 計算"))?;
        if start > end || end > data.len() {
            return Err(font_err("INDEX エントリが範囲外"));
        }
        entries.push((start, end));
    }
    let last = offsets[count];
    let index_end = base
        .checked_add(last)
        .ok_or_else(|| font_err("INDEX 計算"))?;
    Ok((entries, index_end))
}

/// パース済みの DICT。op キー → オペランド列（実数）。
struct Dict {
    /// (op キー, オペランド列)。op キーは escape 演算子を `0x0c00 | b1` で表す。
    entries: Vec<(u16, Vec<f64>)>,
}

impl Dict {
    /// 指定 op のオペランド列を返す。
    fn get_op(&self, op: u16) -> Option<&[f64]> {
        self.entries
            .iter()
            .find(|(k, _)| *k == op)
            .map(|(_, v)| v.as_slice())
    }
}

/// DICT データをパースする。壊れていても読めた分まで返す（panic しない）。
fn parse_dict(data: &[u8]) -> Dict {
    let mut entries: Vec<(u16, Vec<f64>)> = Vec::new();
    let mut operands: Vec<f64> = Vec::new();
    let mut pos = 0usize;
    while pos < data.len() {
        let b0 = data[pos];
        if b0 <= 21 {
            // 演算子。
            let op = if b0 == 0x0c {
                // escape: 次バイトと合成。
                let Some(b1) = read_u8(data, pos + 1) else {
                    break;
                };
                pos += 2;
                0x0c00 | b1 as u16
            } else {
                pos += 1;
                b0 as u16
            };
            entries.push((op, std::mem::take(&mut operands)));
        } else if b0 == 28 {
            // i16。
            let Some(v) = read_u16(data, pos + 1) else {
                break;
            };
            operands.push((v as i16) as f64);
            pos += 3;
        } else if b0 == 29 {
            // i32。
            let Some(b) = data.get(pos + 1..pos + 5) else {
                break;
            };
            let v = i32::from_be_bytes([b[0], b[1], b[2], b[3]]);
            operands.push(v as f64);
            pos += 5;
        } else if b0 == 30 {
            // 実数（ニブルエンコード）。
            let (val, next) = parse_real(data, pos + 1);
            operands.push(val);
            pos = next;
        } else if (32..=246).contains(&b0) {
            operands.push(b0 as f64 - 139.0);
            pos += 1;
        } else if (247..=250).contains(&b0) {
            let Some(b1) = read_u8(data, pos + 1) else {
                break;
            };
            operands.push((b0 as f64 - 247.0) * 256.0 + b1 as f64 + 108.0);
            pos += 2;
        } else if (251..=254).contains(&b0) {
            let Some(b1) = read_u8(data, pos + 1) else {
                break;
            };
            operands.push(-(b0 as f64 - 251.0) * 256.0 - b1 as f64 - 108.0);
            pos += 2;
        } else {
            // 予約（22..=27, 31, 255）。読み飛ばす。
            pos += 1;
        }
    }
    Dict { entries }
}

/// DICT の実数オペランド（b0=30）をパースする。`pos` はニブル列の先頭。
/// 返り値は `(値, 次の読み出し位置)`。
fn parse_real(data: &[u8], pos: usize) -> (f64, usize) {
    let mut s = String::new();
    let mut p = pos;
    'outer: while p < data.len() {
        let byte = data[p];
        p += 1;
        for &nibble in &[byte >> 4, byte & 0x0f] {
            match nibble {
                0..=9 => s.push((b'0' + nibble) as char),
                0xa => s.push('.'),
                0xb => s.push('E'),
                0xc => s.push_str("E-"),
                0xe => s.push('-'),
                0xf => break 'outer,
                _ => {} // 0xd は予約。無視。
            }
        }
    }
    let val = s.parse::<f64>().unwrap_or(0.0);
    (val, p)
}

/// CID フォントの FDArray・FDSelect・各 FD の Private を解析する。
fn parse_cid_fds(
    data: &[u8],
    top_dict: &Dict,
    num_glyphs: usize,
) -> Result<(Vec<PrivateInfo>, FdSelect)> {
    // FDArray（0x0c24）: FD ごとの Font DICT INDEX。
    let fd_array_off = top_dict
        .get_op(0x0c24)
        .and_then(|ops| ops.first())
        .map(|v| *v as usize)
        .ok_or_else(|| font_err("CID フォントに FDArray がありません"))?;
    let (fd_dicts, _) = parse_index(data, fd_array_off)?;
    let mut privates = Vec::with_capacity(fd_dicts.len());
    for (start, end) in fd_dicts {
        let fd_dict = parse_dict(data.get(start..end).unwrap_or(&[]));
        privates.push(parse_private(data, &fd_dict));
    }
    if privates.is_empty() {
        privates.push(PrivateInfo::default());
    }

    // FDSelect（0x0c25）。
    let fd_select = match top_dict.get_op(0x0c25).and_then(|ops| ops.first()) {
        Some(v) => parse_fd_select(data, *v as usize, num_glyphs),
        None => FdSelect::Single,
    };

    Ok((privates, fd_select))
}

/// FDSelect を解析する（format 0 / 3）。
fn parse_fd_select(data: &[u8], pos: usize, num_glyphs: usize) -> FdSelect {
    let Some(format) = read_u8(data, pos) else {
        return FdSelect::Single;
    };
    match format {
        0 => {
            // format 0: GID ごとに u8。
            let mut v = Vec::with_capacity(num_glyphs);
            for i in 0..num_glyphs {
                match read_u8(data, pos + 1 + i) {
                    Some(fd) => v.push(fd),
                    None => break,
                }
            }
            FdSelect::PerGlyph(v)
        }
        3 => {
            // format 3: nRanges(u16), Range3[(first u16, fd u8)], sentinel(u16)。
            let Some(n_ranges) = read_u16(data, pos + 1) else {
                return FdSelect::Single;
            };
            let mut ranges = Vec::with_capacity(n_ranges as usize);
            let mut p = pos + 3;
            for _ in 0..n_ranges {
                let (Some(first), Some(fd)) = (read_u16(data, p), read_u8(data, p + 2)) else {
                    return FdSelect::Ranges(ranges, 0);
                };
                ranges.push((first, fd));
                p += 3;
            }
            let sentinel = read_u16(data, p).unwrap_or(u16::MAX);
            FdSelect::Ranges(ranges, sentinel)
        }
        _ => FdSelect::Single,
    }
}

/// Private DICT を解析する（Subrs は Private 先頭からの相対オフセット）。
fn parse_private(data: &[u8], parent: &Dict) -> PrivateInfo {
    let mut info = PrivateInfo::default();
    // Private(18) = [size, offset]。
    let Some(ops) = parent.get_op(18) else {
        return info;
    };
    if ops.len() < 2 {
        return info;
    }
    let size = ops[0] as usize;
    let offset = ops[1] as usize;
    let Some(end) = offset.checked_add(size) else {
        return info;
    };
    let Some(priv_data) = data.get(offset..end.min(data.len())) else {
        return info;
    };
    let priv_dict = parse_dict(priv_data);

    if let Some(v) = priv_dict.get_op(20).and_then(|o| o.first()) {
        info.default_width_x = *v;
    }
    if let Some(v) = priv_dict.get_op(21).and_then(|o| o.first()) {
        info.nominal_width_x = *v;
    }
    // Subrs(19): Private 先頭からの相対オフセット。
    if let Some(v) = priv_dict.get_op(19).and_then(|o| o.first()) {
        if let Some(subr_off) = offset.checked_add(*v as usize) {
            if let Ok((subrs, _)) = parse_index(data, subr_off) {
                info.subrs = subrs;
            }
        }
    }
    info
}

/// charset を解析する（GID → SID/CID）。
fn parse_charset(data: &[u8], top_dict: &Dict, num_glyphs: usize) -> Vec<u16> {
    let off = top_dict
        .get_op(15)
        .and_then(|ops| ops.first())
        .map(|v| *v as i64)
        .unwrap_or(0);
    // offset 0/1/2 は既定 charset（恒等で近似）。
    if off <= 2 {
        return (0..num_glyphs as u16).collect();
    }
    let pos = off as usize;
    let mut charset = Vec::with_capacity(num_glyphs);
    // GID 0 は常に SID 0（.notdef）。
    charset.push(0u16);
    let Some(format) = read_u8(data, pos) else {
        return (0..num_glyphs as u16).collect();
    };
    match format {
        0 => {
            // format 0: SID 列（GID 1 から）。
            let mut p = pos + 1;
            while charset.len() < num_glyphs {
                match read_u16(data, p) {
                    Some(sid) => charset.push(sid),
                    None => break,
                }
                p += 2;
            }
        }
        1 | 2 => {
            // レンジ: first(u16) + nLeft(format1=u8, format2=u16)。
            let mut p = pos + 1;
            while charset.len() < num_glyphs {
                let Some(first) = read_u16(data, p) else {
                    break;
                };
                p += 2;
                let n_left = if format == 1 {
                    let Some(n) = read_u8(data, p) else {
                        break;
                    };
                    p += 1;
                    n as u32
                } else {
                    let Some(n) = read_u16(data, p) else {
                        break;
                    };
                    p += 2;
                    n as u32
                };
                for i in 0..=n_left {
                    if charset.len() >= num_glyphs {
                        break;
                    }
                    // u16 オーバーフローは saturating。
                    let sid = (first as u32 + i).min(u16::MAX as u32) as u16;
                    charset.push(sid);
                }
            }
        }
        _ => {}
    }
    // 不足分は恒等で埋める（壊れた charset の耐故障）。
    while charset.len() < num_glyphs {
        let gid = charset.len() as u16;
        charset.push(gid);
    }
    charset
}

/// built-in Encoding（非 CID）を解析して code → GID を埋める。
fn parse_encoding(
    data: &[u8],
    top_dict: &Dict,
    charset: &[u16],
    strings: &[(usize, usize)],
    code_to_gid: &mut [u16],
    unicode_to_gid: &HashMap<char, u16>,
) {
    let off = top_dict
        .get_op(16)
        .and_then(|ops| ops.first())
        .map(|v| *v as i64)
        .unwrap_or(0);
    // offset 0 = Standard、1 = Expert（Standard で近似）。
    if off <= 1 {
        // StandardEncoding: code → 文字 → GID。
        for code in 0u32..256 {
            if let Some(c) = crate::encoding::standard_encoding(code as u8) {
                if let Some(&gid) = unicode_to_gid.get(&c) {
                    if let Some(slot) = code_to_gid.get_mut(code as usize) {
                        *slot = gid;
                    }
                }
            }
        }
        return;
    }
    let pos = off as usize;
    let Some(format) = read_u8(data, pos) else {
        return;
    };
    let base_format = format & 0x7f;
    let mut p = pos + 1;
    match base_format {
        0 => {
            // format 0: nCodes(u8), code[nCodes]（GID は 1 から順次）。
            let Some(n_codes) = read_u8(data, p) else {
                return;
            };
            p += 1;
            for i in 0..n_codes as usize {
                let Some(code) = read_u8(data, p + i) else {
                    break;
                };
                let gid = (i + 1) as u16;
                if let Some(slot) = code_to_gid.get_mut(code as usize) {
                    *slot = gid;
                }
            }
            p += n_codes as usize;
        }
        1 => {
            // format 1: nRanges(u8), Range1[(first u8, nLeft u8)]。
            let Some(n_ranges) = read_u8(data, p) else {
                return;
            };
            p += 1;
            let mut gid = 1u16;
            for _ in 0..n_ranges {
                let (Some(first), Some(n_left)) = (read_u8(data, p), read_u8(data, p + 1)) else {
                    break;
                };
                p += 2;
                for k in 0..=n_left as u16 {
                    let code = first as u16 + k;
                    if code < 256 {
                        if let Some(slot) = code_to_gid.get_mut(code as usize) {
                            *slot = gid;
                        }
                    }
                    gid = gid.saturating_add(1);
                }
            }
        }
        _ => return,
    }
    // supplements（bit7）: nSups(u8), Supplement[(code u8, SID u16)]。
    if format & 0x80 != 0 {
        let Some(n_sups) = read_u8(data, p) else {
            return;
        };
        p += 1;
        for _ in 0..n_sups {
            let (Some(code), Some(sid)) = (read_u8(data, p), read_u16(data, p + 1)) else {
                break;
            };
            p += 3;
            // SID → GID は charset 逆引き。
            if let Some(gid) = charset.iter().position(|&s| s == sid) {
                let _ = strings; // SID 名は不要（charset 逆引きで足りる）。
                if let Some(slot) = code_to_gid.get_mut(code as usize) {
                    *slot = gid as u16;
                }
            }
        }
    }
}

/// Type 2 チャーストリングのバイアスを計算する（subr 数から）。
fn subr_bias(count: usize) -> i32 {
    if count < 1240 {
        107
    } else if count < 33900 {
        1131
    } else {
        32768
    }
}

/// アフィン変換 `[a b c d e f]`（PDF 形式 2x3）をアウトラインの各点へ適用する。
pub fn transform_outline(segs: &[OutlineSegment], m: &[f64; 6]) -> Vec<OutlineSegment> {
    let tf = |x: f64, y: f64| (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5]);
    segs.iter()
        .map(|seg| match *seg {
            OutlineSegment::MoveTo(x, y) => {
                let (nx, ny) = tf(x, y);
                OutlineSegment::MoveTo(nx, ny)
            }
            OutlineSegment::LineTo(x, y) => {
                let (nx, ny) = tf(x, y);
                OutlineSegment::LineTo(nx, ny)
            }
            OutlineSegment::QuadTo(cx, cy, ex, ey) => {
                let (ncx, ncy) = tf(cx, cy);
                let (nex, ney) = tf(ex, ey);
                OutlineSegment::QuadTo(ncx, ncy, nex, ney)
            }
            OutlineSegment::CurveTo(c1x, c1y, c2x, c2y, ex, ey) => {
                let (n1x, n1y) = tf(c1x, c1y);
                let (n2x, n2y) = tf(c2x, c2y);
                let (nex, ney) = tf(ex, ey);
                OutlineSegment::CurveTo(n1x, n1y, n2x, n2y, nex, ney)
            }
            OutlineSegment::Close => OutlineSegment::Close,
        })
        .collect()
}

// --- Type 2 チャーストリング解釈器 ---

/// 暴走ガード（信頼できない入力対策）。
const MAX_OPS: u32 = 100_000;
const MAX_SEGMENTS: usize = 50_000;
const MAX_SUBR_DEPTH: usize = 10;
const STACK_LIMIT: usize = 64;

/// Type 2 チャーストリング解釈の実行状態。
struct Type2Interp<'a> {
    font: &'a CffFont,
    private: PrivateInfo,
    /// オペランドスタック。
    stack: Vec<f64>,
    /// 生成したアウトラインセグメント。
    segments: Vec<OutlineSegment>,
    /// 現在の作図位置。
    x: f64,
    y: f64,
    /// 輪郭が開いているか（最初の moveto 後に true）。
    open: bool,
    /// 確定済みステム数。
    n_stems: usize,
    /// 幅が確定したか。
    width_parsed: bool,
    /// グリフ幅（nominalWidthX 基準、未指定なら defaultWidthX）。
    width: f64,
    /// 演算予算。
    budget: u32,
    /// 実行を打ち切ったか。
    aborted: bool,
    /// transient 配列（put/get 用、32 要素）。
    transient: [f64; 32],
    /// グローバル Subr のバイアス。
    gbias: i32,
    /// ローカル Subr のバイアス。
    lbias: i32,
    /// seac 合成中か（再帰の seac を禁止するため）。
    in_seac: bool,
}

impl<'a> Type2Interp<'a> {
    fn new(font: &'a CffFont, private: PrivateInfo) -> Type2Interp<'a> {
        let width = private.default_width_x;
        let lbias = subr_bias(private.subrs.len());
        Type2Interp {
            font,
            private,
            stack: Vec::with_capacity(STACK_LIMIT),
            segments: Vec::new(),
            x: 0.0,
            y: 0.0,
            open: false,
            n_stems: 0,
            width_parsed: false,
            width,
            budget: MAX_OPS,
            aborted: false,
            transient: [0.0; 32],
            gbias: subr_bias(font.gsubrs.len()),
            lbias,
            in_seac: false,
        }
    }

    /// グリフ幅（advance）。
    fn advance(&self) -> f64 {
        self.width
    }

    /// 開いている輪郭を閉じる。
    fn close_contour(&mut self) {
        if self.open {
            self.segments.push(OutlineSegment::Close);
            self.open = false;
        }
    }

    /// 描き終わりの後処理（開いている輪郭を閉じる）。
    fn finish(&mut self) {
        self.close_contour();
    }

    /// セグメント追加（上限ガード付き）。
    fn push_seg(&mut self, seg: OutlineSegment) {
        if self.segments.len() >= MAX_SEGMENTS {
            self.aborted = true;
            return;
        }
        self.segments.push(seg);
    }

    /// 新しい輪郭を開始（直前の輪郭を閉じてから）。
    fn move_to(&mut self, dx: f64, dy: f64) {
        self.close_contour();
        self.x += dx;
        self.y += dy;
        self.push_seg(OutlineSegment::MoveTo(self.x, self.y));
        self.open = true;
    }

    fn line_to(&mut self, dx: f64, dy: f64) {
        self.x += dx;
        self.y += dy;
        self.push_seg(OutlineSegment::LineTo(self.x, self.y));
    }

    /// 相対 3 次ベジェ（6 デルタ）。
    fn curve_to(&mut self, dx1: f64, dy1: f64, dx2: f64, dy2: f64, dx3: f64, dy3: f64) {
        let c1x = self.x + dx1;
        let c1y = self.y + dy1;
        let c2x = c1x + dx2;
        let c2y = c1y + dy2;
        let ex = c2x + dx3;
        let ey = c2y + dy3;
        self.x = ex;
        self.y = ey;
        self.push_seg(OutlineSegment::CurveTo(c1x, c1y, c2x, c2y, ex, ey));
    }

    /// 幅を確定する（先頭オペランドが余分なら幅とみなす）。
    /// `even` が true なら「引数が偶数のとき幅なし」、false なら個別の判定済み。
    fn maybe_width(&mut self, expected_extra_is_present: bool) {
        if self.width_parsed {
            return;
        }
        self.width_parsed = true;
        if expected_extra_is_present && !self.stack.is_empty() {
            let w = self.stack.remove(0);
            self.width = self.private.nominal_width_x + w;
        }
    }

    /// stem 系演算子: 引数が奇数なら先頭が幅。stem 数を加算してスタッククリア。
    fn stems(&mut self) {
        if !self.width_parsed && self.stack.len() % 2 == 1 {
            let w = self.stack.remove(0);
            self.width = self.private.nominal_width_x + w;
        }
        self.width_parsed = true;
        self.n_stems += self.stack.len() / 2;
        self.stack.clear();
    }

    /// チャーストリングを実行する。`depth` は subr 再帰深さ。
    /// 戻り値は「呼び出し元が継続すべきか」。false で endchar / 打ち切り。
    fn run(&mut self, cs: &[u8], depth: usize) -> bool {
        if depth > MAX_SUBR_DEPTH || self.aborted {
            self.aborted = true;
            return false;
        }
        let mut pos = 0usize;
        while pos < cs.len() {
            if self.budget == 0 {
                self.aborted = true;
                return false;
            }
            self.budget -= 1;
            let b0 = cs[pos];
            pos += 1;
            match b0 {
                // 数値オペランド。
                28 => {
                    let Some(v) = read_u16(cs, pos) else {
                        self.aborted = true;
                        return false;
                    };
                    self.push_num((v as i16) as f64);
                    pos += 2;
                }
                32..=246 => self.push_num(b0 as f64 - 139.0),
                247..=250 => {
                    let Some(b1) = read_u8(cs, pos) else {
                        self.aborted = true;
                        return false;
                    };
                    self.push_num((b0 as f64 - 247.0) * 256.0 + b1 as f64 + 108.0);
                    pos += 1;
                }
                251..=254 => {
                    let Some(b1) = read_u8(cs, pos) else {
                        self.aborted = true;
                        return false;
                    };
                    self.push_num(-(b0 as f64 - 251.0) * 256.0 - b1 as f64 - 108.0);
                    pos += 1;
                }
                255 => {
                    // 16.16 固定小数。
                    let Some(b) = cs.get(pos..pos + 4) else {
                        self.aborted = true;
                        return false;
                    };
                    let v = i32::from_be_bytes([b[0], b[1], b[2], b[3]]);
                    self.push_num(v as f64 / 65536.0);
                    pos += 4;
                }
                // 演算子。
                1 | 3 | 18 | 23 => self.stems(), // hstem/vstem/hstemhm/vstemhm
                19 | 20 => {
                    // hintmask / cntrmask: 直前の暗黙 vstem も数える。
                    self.stems();
                    // (n_stems).div_ceil(8) バイトをスキップ。
                    let skip = self.n_stems.div_ceil(8);
                    pos = pos.saturating_add(skip);
                    if pos > cs.len() {
                        self.aborted = true;
                        return false;
                    }
                }
                21 => {
                    // rmoveto。
                    self.maybe_width(self.stack.len() > 2);
                    let dx = self.arg(0);
                    let dy = self.arg(1);
                    self.move_to(dx, dy);
                    self.stack.clear();
                }
                22 => {
                    // hmoveto。
                    self.maybe_width(self.stack.len() > 1);
                    let dx = self.arg(0);
                    self.move_to(dx, 0.0);
                    self.stack.clear();
                }
                4 => {
                    // vmoveto。
                    self.maybe_width(self.stack.len() > 1);
                    let dy = self.arg(0);
                    self.move_to(0.0, dy);
                    self.stack.clear();
                }
                5 => {
                    // rlineto: (dx dy)+。
                    let n = self.stack.len();
                    let mut i = 0;
                    while i + 2 <= n {
                        let dx = self.arg(i);
                        let dy = self.arg(i + 1);
                        self.line_to(dx, dy);
                        i += 2;
                    }
                    self.stack.clear();
                }
                6 => {
                    // hlineto: 交互（h から開始）。
                    self.alt_lineto(true);
                    self.stack.clear();
                }
                7 => {
                    // vlineto: 交互（v から開始）。
                    self.alt_lineto(false);
                    self.stack.clear();
                }
                8 => {
                    // rrcurveto: (dx1 dy1 dx2 dy2 dx3 dy3)+。
                    let mut i = 0;
                    while i + 6 <= self.stack.len() {
                        let a = self.arg(i);
                        let b = self.arg(i + 1);
                        let c = self.arg(i + 2);
                        let d = self.arg(i + 3);
                        let e = self.arg(i + 4);
                        let f = self.arg(i + 5);
                        self.curve_to(a, b, c, d, e, f);
                        i += 6;
                    }
                    self.stack.clear();
                }
                24 => {
                    // rcurveline: rrcurveto×n + rlineto。
                    let n = self.stack.len();
                    let mut i = 0;
                    while i + 6 <= n.saturating_sub(2) {
                        let a = self.arg(i);
                        let b = self.arg(i + 1);
                        let c = self.arg(i + 2);
                        let d = self.arg(i + 3);
                        let e = self.arg(i + 4);
                        let f = self.arg(i + 5);
                        self.curve_to(a, b, c, d, e, f);
                        i += 6;
                    }
                    if i + 1 < n {
                        let dx = self.arg(i);
                        let dy = self.arg(i + 1);
                        self.line_to(dx, dy);
                    }
                    self.stack.clear();
                }
                25 => {
                    // rlinecurve: rlineto×n + rrcurveto。
                    let n = self.stack.len();
                    let mut i = 0;
                    while i + 2 <= n.saturating_sub(6) {
                        let dx = self.arg(i);
                        let dy = self.arg(i + 1);
                        self.line_to(dx, dy);
                        i += 2;
                    }
                    if i + 6 <= n {
                        let a = self.arg(i);
                        let b = self.arg(i + 1);
                        let c = self.arg(i + 2);
                        let d = self.arg(i + 3);
                        let e = self.arg(i + 4);
                        let f = self.arg(i + 5);
                        self.curve_to(a, b, c, d, e, f);
                    }
                    self.stack.clear();
                }
                26 => {
                    // vvcurveto。
                    self.vv_hh_curveto(true);
                    self.stack.clear();
                }
                27 => {
                    // hhcurveto。
                    self.vv_hh_curveto(false);
                    self.stack.clear();
                }
                30 => {
                    // vhcurveto。
                    self.vh_hv_curveto(false);
                    self.stack.clear();
                }
                31 => {
                    // hvcurveto。
                    self.vh_hv_curveto(true);
                    self.stack.clear();
                }
                10 => {
                    // callsubr。
                    if let Some(idx) = self.stack.pop() {
                        let i = idx as i32 + self.lbias;
                        if let Some(&(s, e)) = self.subr_range(i, false) {
                            if let Some(sub) = self.font.data.get(s..e) {
                                let sub = sub.to_vec();
                                if !self.run(&sub, depth + 1) {
                                    return false;
                                }
                            }
                        }
                    }
                }
                29 => {
                    // callgsubr。
                    if let Some(idx) = self.stack.pop() {
                        let i = idx as i32 + self.gbias;
                        if let Some(&(s, e)) = self.subr_range(i, true) {
                            if let Some(sub) = self.font.data.get(s..e) {
                                let sub = sub.to_vec();
                                if !self.run(&sub, depth + 1) {
                                    return false;
                                }
                            }
                        }
                    }
                }
                11 => {
                    // return。
                    return true;
                }
                14 => {
                    // endchar。
                    self.maybe_width(self.stack.len() == 1 || self.stack.len() == 5);
                    // seac 互換: 残り 4 引数なら合成。
                    if self.stack.len() >= 4 && !self.in_seac {
                        let adx = self.arg(0);
                        let ady = self.arg(1);
                        let bchar = self.arg(2) as i64;
                        let achar = self.arg(3) as i64;
                        self.do_seac(adx, ady, bchar, achar);
                    }
                    self.close_contour();
                    return false;
                }
                12 => {
                    // escape。
                    let Some(b1) = read_u8(cs, pos) else {
                        self.aborted = true;
                        return false;
                    };
                    pos += 1;
                    self.escape_op(b1);
                }
                _ => {
                    // 未知の演算子: スタックをクリアして継続。
                    self.stack.clear();
                }
            }
            if self.aborted {
                return false;
            }
        }
        true
    }

    /// スタックへ数値を積む（上限ガード）。
    fn push_num(&mut self, v: f64) {
        if self.stack.len() >= STACK_LIMIT {
            // オーバーフロー: 古いものを落として継続（壊れ耐性）。
            self.stack.remove(0);
        }
        self.stack.push(v);
    }

    /// スタックの i 番目を取り出す（無ければ 0）。
    fn arg(&self, i: usize) -> f64 {
        self.stack.get(i).copied().unwrap_or(0.0)
    }

    /// subr の範囲を引く。`global` で gsubr / lsubr を選ぶ。
    fn subr_range(&self, i: i32, global: bool) -> Option<&(usize, usize)> {
        if i < 0 {
            return None;
        }
        let list = if global {
            &self.font.gsubrs
        } else {
            &self.private.subrs
        };
        list.get(i as usize)
    }

    /// hlineto/vlineto の交互ライン。`horizontal_first` で開始方向を選ぶ。
    fn alt_lineto(&mut self, horizontal_first: bool) {
        let n = self.stack.len();
        let mut horizontal = horizontal_first;
        for i in 0..n {
            let d = self.arg(i);
            if horizontal {
                self.line_to(d, 0.0);
            } else {
                self.line_to(0.0, d);
            }
            horizontal = !horizontal;
        }
    }

    /// vvcurveto(true)/hhcurveto(false)。
    /// 奇数個なら先頭に dx1（hh）/ dy1（vv）。
    fn vv_hh_curveto(&mut self, vertical: bool) {
        let n = self.stack.len();
        let mut i = 0;
        // 先頭の余り（dx1 or dy1）。
        let mut extra = 0.0;
        if n % 4 == 1 {
            extra = self.arg(0);
            i = 1;
        }
        while i + 4 <= n {
            if vertical {
                // vvcurveto: dx1=extra(初回のみ), dy1, (dx2 dy2), dy3。
                let dx1 = extra;
                let dy1 = self.arg(i);
                let dx2 = self.arg(i + 1);
                let dy2 = self.arg(i + 2);
                let dy3 = self.arg(i + 3);
                self.curve_to(dx1, dy1, dx2, dy2, 0.0, dy3);
            } else {
                // hhcurveto: dy1=extra(初回のみ), dx1, (dx2 dy2), dx3。
                let dy1 = extra;
                let dx1 = self.arg(i);
                let dx2 = self.arg(i + 1);
                let dy2 = self.arg(i + 2);
                let dx3 = self.arg(i + 3);
                self.curve_to(dx1, dy1, dx2, dy2, dx3, 0.0);
            }
            extra = 0.0;
            i += 4;
        }
    }

    /// vhcurveto(false)/hvcurveto(true)。4 個組交互、最後に 5 個目があれば終点の他軸。
    fn vh_hv_curveto(&mut self, mut horizontal_start: bool) {
        let n = self.stack.len();
        let mut i = 0;
        while i + 4 <= n {
            let remaining = n - i;
            // 最後の組で 5 個残っていれば 5 個目を使う。
            let last_extra = if remaining == 5 { self.arg(i + 4) } else { 0.0 };
            if horizontal_start {
                // hv: 開始は水平接線、終了は垂直接線。
                let dx1 = self.arg(i);
                let dx2 = self.arg(i + 1);
                let dy2 = self.arg(i + 2);
                let dy3 = self.arg(i + 3);
                self.curve_to(dx1, 0.0, dx2, dy2, last_extra, dy3);
            } else {
                // vh: 開始は垂直接線、終了は水平接線。
                let dy1 = self.arg(i);
                let dx2 = self.arg(i + 1);
                let dy2 = self.arg(i + 2);
                let dx3 = self.arg(i + 3);
                self.curve_to(0.0, dy1, dx2, dy2, dx3, last_extra);
            }
            horizontal_start = !horizontal_start;
            i += 4;
        }
    }

    /// seac（endchar の合成アクセント）を実行する。
    /// bchar / achar は StandardEncoding 経由でグリフ名 → GID を求める。
    fn do_seac(&mut self, adx: f64, ady: f64, bchar: i64, achar: i64) {
        let resolve = |code: i64| -> Option<u16> {
            if !(0..256).contains(&code) {
                return None;
            }
            let c = crate::encoding::standard_encoding(code as u8)?;
            self.font.gid_by_unicode(c)
        };
        // ベースグリフ。
        if let Some(bgid) = resolve(bchar) {
            self.append_component(bgid, 0.0, 0.0);
        }
        // アクセントグリフ（adx, ady オフセット）。
        if let Some(agid) = resolve(achar) {
            self.append_component(agid, adx, ady);
        }
    }

    /// 合成コンポーネントを seac 再帰禁止で実行し、オフセットを付けて追記する。
    fn append_component(&mut self, gid: u16, dx: f64, dy: f64) {
        let Some(&(s, e)) = self.font.char_strings.get(gid as usize) else {
            return;
        };
        let Some(cs) = self.font.data.get(s..e) else {
            return;
        };
        let cs = cs.to_vec();
        let fd = self.font.fd_select.fd_for(gid);
        let private = self.font.privates.get(fd).cloned().unwrap_or_default();
        let mut sub = Type2Interp::new(self.font, private);
        sub.in_seac = true;
        sub.run(&cs, 0);
        sub.finish();
        // sub.segments を取り出してから追記（借用衝突回避）。
        let segs = std::mem::take(&mut sub.segments);
        for seg in segs {
            let shifted = shift_segment(seg, dx, dy);
            self.push_seg(shifted);
        }
    }

    /// escape（12 b1）演算子の処理。
    fn escape_op(&mut self, b1: u8) {
        match b1 {
            3 => self.bin_op(|a, b| if a != 0.0 && b != 0.0 { 1.0 } else { 0.0 }), // and
            4 => self.bin_op(|a, b| if a != 0.0 || b != 0.0 { 1.0 } else { 0.0 }), // or
            5 => self.un_op(|a| if a == 0.0 { 1.0 } else { 0.0 }),                 // not
            9 => self.un_op(|a| a.abs()),                                          // abs
            10 => self.bin_op(|a, b| a + b),                                       // add
            11 => self.bin_op(|a, b| a - b),                                       // sub
            12 => self.bin_op(|a, b| if b == 0.0 { 0.0 } else { a / b }),          // div
            14 => self.un_op(|a| -a),                                              // neg
            15 => self.bin_op(|a, b| if a == b { 1.0 } else { 0.0 }),              // eq
            18 => {
                self.stack.pop();
            }                  // drop
            20 => {
                // put: val i → transient[i]。
                let i = self.stack.pop().unwrap_or(0.0);
                let v = self.stack.pop().unwrap_or(0.0);
                let idx = i as usize;
                if idx < self.transient.len() {
                    self.transient[idx] = v;
                }
            }
            21 => {
                // get: i → transient[i]。
                let i = self.stack.pop().unwrap_or(0.0);
                let idx = i as usize;
                let v = self.transient.get(idx).copied().unwrap_or(0.0);
                self.push_num(v);
            }
            22 => {
                // ifelse: s1 s2 v1 v2 → (v1<=v2 ? s1 : s2)。
                let v2 = self.stack.pop().unwrap_or(0.0);
                let v1 = self.stack.pop().unwrap_or(0.0);
                let s2 = self.stack.pop().unwrap_or(0.0);
                let s1 = self.stack.pop().unwrap_or(0.0);
                self.push_num(if v1 <= v2 { s1 } else { s2 });
            }
            23 => self.push_num(0.5),        // random（固定 0.5）
            24 => self.bin_op(|a, b| a * b), // mul
            26 => self.un_op(|a| if a >= 0.0 { a.sqrt() } else { 0.0 }), // sqrt
            27 => {
                // dup。
                let v = self.stack.last().copied().unwrap_or(0.0);
                self.push_num(v);
            }
            28 => {
                // exch。
                let n = self.stack.len();
                if n >= 2 {
                    self.stack.swap(n - 1, n - 2);
                }
            }
            29 => {
                // index: i → スタックの i 番目（0 = top）を複製。
                let i = self.stack.pop().unwrap_or(0.0);
                let n = self.stack.len();
                let idx = if i < 0.0 { 0 } else { i as usize };
                let v = if idx < n {
                    self.stack[n - 1 - idx]
                } else {
                    self.stack.last().copied().unwrap_or(0.0)
                };
                self.push_num(v);
            }
            30 => {
                // roll: N J → 上位 N 要素を J だけ回転。
                let j = self.stack.pop().unwrap_or(0.0) as i64;
                let nn = self.stack.pop().unwrap_or(0.0) as i64;
                self.roll(nn, j);
            }
            34 => self.hflex(),
            35 => self.flex(),
            36 => self.hflex1(),
            37 => self.flex1(),
            _ => {
                // 未知 escape: スタッククリアで読み飛ばす。
                self.stack.clear();
            }
        }
    }

    fn bin_op(&mut self, f: impl Fn(f64, f64) -> f64) {
        let b = self.stack.pop().unwrap_or(0.0);
        let a = self.stack.pop().unwrap_or(0.0);
        self.push_num(f(a, b));
    }

    fn un_op(&mut self, f: impl Fn(f64) -> f64) {
        let a = self.stack.pop().unwrap_or(0.0);
        self.push_num(f(a));
    }

    /// roll 演算子（上位 n 要素を j だけ回転）。
    fn roll(&mut self, n: i64, j: i64) {
        let len = self.stack.len();
        if n <= 0 || n as usize > len {
            return;
        }
        let n = n as usize;
        let start = len - n;
        let slice = &mut self.stack[start..];
        let j = ((j % n as i64) + n as i64) % n as i64;
        slice.rotate_right(j as usize);
    }

    /// hflex（escape 34）: 2 本の 3 次ベジェ（y は基準線へ戻る）。
    /// 引数: dx1 dx2 dy2 dx3 dx4 dx5 dx6。
    fn hflex(&mut self) {
        if self.stack.len() < 7 {
            self.stack.clear();
            return;
        }
        let dx1 = self.arg(0);
        let dx2 = self.arg(1);
        let dy2 = self.arg(2);
        let dx3 = self.arg(3);
        let dx4 = self.arg(4);
        let dx5 = self.arg(5);
        let dx6 = self.arg(6);
        self.curve_to(dx1, 0.0, dx2, dy2, dx3, 0.0);
        self.curve_to(dx4, 0.0, dx5, -dy2, dx6, 0.0);
        self.stack.clear();
    }

    /// flex（escape 35）: 13 引数（6+6 デルタ + fd しきい値）。
    fn flex(&mut self) {
        if self.stack.len() < 12 {
            self.stack.clear();
            return;
        }
        let a = self.arg(0);
        let b = self.arg(1);
        let c = self.arg(2);
        let d = self.arg(3);
        let e = self.arg(4);
        let f = self.arg(5);
        self.curve_to(a, b, c, d, e, f);
        let a2 = self.arg(6);
        let b2 = self.arg(7);
        let c2 = self.arg(8);
        let d2 = self.arg(9);
        let e2 = self.arg(10);
        let f2 = self.arg(11);
        self.curve_to(a2, b2, c2, d2, e2, f2);
        self.stack.clear();
    }

    /// hflex1（escape 36）: 引数 dx1 dy1 dx2 dy2 dx3 dx4 dx5 dy5 dx6。
    fn hflex1(&mut self) {
        if self.stack.len() < 9 {
            self.stack.clear();
            return;
        }
        let dx1 = self.arg(0);
        let dy1 = self.arg(1);
        let dx2 = self.arg(2);
        let dy2 = self.arg(3);
        let dx3 = self.arg(4);
        let dx4 = self.arg(5);
        let dx5 = self.arg(6);
        let dy5 = self.arg(7);
        let dx6 = self.arg(8);
        self.curve_to(dx1, dy1, dx2, dy2, dx3, 0.0);
        // 2 本目の終点 y は最初の dy 合計を打ち消して基準線へ戻る。
        let dy6 = -(dy1 + dy2 + dy5);
        self.curve_to(dx4, 0.0, dx5, dy5, dx6, dy6);
        self.stack.clear();
    }

    /// flex1（escape 37）: 引数 dx1 dy1 dx2 dy2 dx3 dy3 dx4 dy4 dx5 dy5 d6。
    /// dx/dy の合計の大小で終点の拘束軸が決まる。
    fn flex1(&mut self) {
        if self.stack.len() < 11 {
            self.stack.clear();
            return;
        }
        let dx1 = self.arg(0);
        let dy1 = self.arg(1);
        let dx2 = self.arg(2);
        let dy2 = self.arg(3);
        let dx3 = self.arg(4);
        let dy3 = self.arg(5);
        let dx4 = self.arg(6);
        let dy4 = self.arg(7);
        let dx5 = self.arg(8);
        let dy5 = self.arg(9);
        let d6 = self.arg(10);
        let dx = dx1 + dx2 + dx3 + dx4 + dx5;
        let dy = dy1 + dy2 + dy3 + dy4 + dy5;
        self.curve_to(dx1, dy1, dx2, dy2, dx3, dy3);
        if dx.abs() > dy.abs() {
            self.curve_to(dx4, dy4, dx5, dy5, d6, -dy);
        } else {
            self.curve_to(dx4, dy4, dx5, dy5, -dx, d6);
        }
        self.stack.clear();
    }
}

/// セグメントをデルタ (dx,dy) だけ平行移動する（seac 合成用）。
fn shift_segment(seg: OutlineSegment, dx: f64, dy: f64) -> OutlineSegment {
    match seg {
        OutlineSegment::MoveTo(x, y) => OutlineSegment::MoveTo(x + dx, y + dy),
        OutlineSegment::LineTo(x, y) => OutlineSegment::LineTo(x + dx, y + dy),
        OutlineSegment::QuadTo(cx, cy, ex, ey) => {
            OutlineSegment::QuadTo(cx + dx, cy + dy, ex + dx, ey + dy)
        }
        OutlineSegment::CurveTo(c1x, c1y, c2x, c2y, ex, ey) => {
            OutlineSegment::CurveTo(c1x + dx, c1y + dy, c2x + dx, c2y + dy, ex + dx, ey + dy)
        }
        OutlineSegment::Close => OutlineSegment::Close,
    }
}

// 標準文字列表は別ファイルから include。
include!("cff_strings.rs");

#[cfg(test)]
mod tests {
    use super::*;

    /// SID 0 = ".notdef" は常に有効でなければならない。
    #[test]
    fn sid_zero_is_notdef() {
        assert_eq!(STANDARD_STRINGS[0], ".notdef");
    }

    /// 391 個（SID 0..=390）でなければならない。
    #[test]
    fn standard_strings_count() {
        assert_eq!(STANDARD_STRINGS.len(), 391);
    }

    /// `subr_bias` の境界値。
    #[test]
    fn subr_bias_boundaries() {
        assert_eq!(subr_bias(0), 107);
        assert_eq!(subr_bias(1239), 107);
        assert_eq!(subr_bias(1240), 1131);
        assert_eq!(subr_bias(33899), 1131);
        assert_eq!(subr_bias(33900), 32768);
    }

    /// 空 INDEX（count=0）は 2 バイトで終端。
    #[test]
    fn parse_index_empty() {
        let data = [0u8, 0u8];
        let (entries, end) = parse_index(&data, 0).expect("空 INDEX");
        assert!(entries.is_empty());
        assert_eq!(end, 2);
    }

    /// 1 エントリの INDEX（offSize=1）。
    #[test]
    fn parse_index_one_entry() {
        // count=1, offSize=1, offsets=[1,4], data=[0xAA, 0xBB, 0xCC]
        let data = [0x00, 0x01, 0x01, 0x01, 0x04, 0xAA, 0xBB, 0xCC];
        let (entries, end) = parse_index(&data, 0).expect("INDEX");
        assert_eq!(entries.len(), 1);
        let (s, e) = entries[0];
        assert_eq!(&data[s..e], &[0xAA, 0xBB, 0xCC]);
        assert_eq!(end, data.len());
    }

    /// DICT の数値オペランド（b0=32..=246）。
    #[test]
    fn parse_dict_small_int() {
        // 139 → 0, 247→108+x, …。32 は値 -107。
        let data = [32u8, 0u8]; // operand=−107, then op=0 (version)
        let d = parse_dict(&data);
        assert_eq!(d.entries.len(), 1);
        assert_eq!(d.entries[0].0, 0);
        assert_eq!(d.entries[0].1, vec![-107.0]);
    }

    /// FdSelect::Single はすべて FD 0。
    #[test]
    fn fdselect_single() {
        let s = FdSelect::Single;
        assert_eq!(s.fd_for(0), 0);
        assert_eq!(s.fd_for(1000), 0);
    }

    /// FdSelect::PerGlyph は GID ごとに FD 番号。
    #[test]
    fn fdselect_per_glyph() {
        let s = FdSelect::PerGlyph(vec![0, 1, 2, 1]);
        assert_eq!(s.fd_for(0), 0);
        assert_eq!(s.fd_for(2), 2);
        // 範囲外は 0 で fallback。
        assert_eq!(s.fd_for(100), 0);
    }

    /// FdSelect::Ranges（format 3）の引き当て。
    #[test]
    fn fdselect_ranges() {
        let s = FdSelect::Ranges(vec![(0, 0), (5, 1), (10, 2)], 20);
        assert_eq!(s.fd_for(0), 0);
        assert_eq!(s.fd_for(4), 0);
        assert_eq!(s.fd_for(5), 1);
        assert_eq!(s.fd_for(9), 1);
        assert_eq!(s.fd_for(10), 2);
        assert_eq!(s.fd_for(19), 2);
        // sentinel 以降は無効 → 0 fallback。
        assert_eq!(s.fd_for(20), 0);
    }

    /// 壊れた CFF（短すぎ）でも panic せず Err を返す。
    #[test]
    fn parse_garbage_no_panic() {
        let _ = CffFont::parse(vec![]);
        let _ = CffFont::parse(vec![1, 0, 4, 1]);
        let _ = CffFont::parse(vec![0, 0, 4, 1]); // major=0 → CFF2 扱いではないが拒否
    }

    /// transform_outline は各点に行列を適用する。
    #[test]
    fn transform_outline_scales() {
        let segs = vec![
            OutlineSegment::MoveTo(10.0, 20.0),
            OutlineSegment::LineTo(30.0, 40.0),
            OutlineSegment::Close,
        ];
        // スケール 2 倍 + 平行移動 (5, 7)。
        let m = [2.0, 0.0, 0.0, 2.0, 5.0, 7.0];
        let out = transform_outline(&segs, &m);
        assert_eq!(out[0], OutlineSegment::MoveTo(25.0, 47.0));
        assert_eq!(out[1], OutlineSegment::LineTo(65.0, 87.0));
        assert_eq!(out[2], OutlineSegment::Close);
    }
}
