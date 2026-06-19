//! ストリームフィルタ（PDF 32000-1:2008 §7.4）。
//!
//! 対応フィルタ:
//! - `FlateDecode`（zlib/DEFLATE。[`flate`] モジュール。PNG/TIFF predictor 対応）
//! - `ASCIIHexDecode`
//! - `ASCII85Decode`
//! - `RunLengthDecode`
//! - `LZWDecode`（predictor 対応）
//! - `CCITTFaxDecode`（T.4 1D / T.4 2D / T.6。[`ccitt`] モジュール）
//!
//! `DCTDecode`（JPEG）や `JPXDecode`（JPEG2000）は画像コーデックであり、
//! データはそのまま画像ファイルとして扱えるためデコードせずにエラーを返す
//! （描画パスでは画像 XObject 側で個別にデコードする）。`JBIG2Decode` は
//! 未対応。

pub mod ccitt;
pub mod dct;
pub mod flate;

use crate::error::{PdfError, Result};
use crate::object::{Dictionary, Object};

fn err(msg: impl Into<String>) -> PdfError {
    PdfError::Filter(msg.into())
}

/// 間接参照を解決するためのコールバック。
pub type Resolver<'a> = &'a dyn Fn(&Object) -> Object;

/// ストリーム辞書の `/Filter` チェーンに従ってデータを伸長する。
///
/// `resolve` を渡すと `/Filter` や `/DecodeParms` 内の間接参照を解決できる。
pub fn decode_stream(dict: &Dictionary, data: &[u8], resolve: Option<Resolver>) -> Result<Vec<u8>> {
    let deref = |o: &Object| -> Object {
        match (resolve, o) {
            (Some(f), Object::Reference(_)) => f(o),
            _ => o.clone(),
        }
    };

    // /Filter は名前単体または名前の配列（/F は省略名）
    let filter_obj = dict.get("Filter").or_else(|| dict.get("F")).map(&deref);
    let filters: Vec<String> = match &filter_obj {
        None => return Ok(data.to_vec()),
        Some(Object::Name(n)) => vec![n.clone()],
        Some(Object::Array(a)) => {
            let mut v = Vec::new();
            for o in a {
                v.push(
                    deref(o)
                        .as_name()
                        .map_err(|_| err("non-name in /Filter array"))?
                        .to_string(),
                );
            }
            v
        }
        Some(Object::Null) => return Ok(data.to_vec()),
        Some(o) => return Err(err(format!("invalid /Filter type: {}", o.type_name()))),
    };

    // /DecodeParms はフィルタごとの辞書（または辞書の配列）
    let parms_obj = dict
        .get("DecodeParms")
        .or_else(|| dict.get("DP"))
        .map(&deref);
    let parms_for = |i: usize| -> Option<Dictionary> {
        match &parms_obj {
            Some(Object::Dictionary(d)) if i == 0 => Some(d.clone()),
            Some(Object::Array(a)) => match a.get(i).map(&deref) {
                Some(Object::Dictionary(d)) => Some(d),
                _ => None,
            },
            _ => None,
        }
    };

    let mut current = data.to_vec();
    for (i, name) in filters.iter().enumerate() {
        let parms = parms_for(i);
        current = apply_filter(name, &current, parms.as_ref(), resolve)?;
    }
    Ok(current)
}

/// 単一フィルタを適用する。
fn apply_filter(
    name: &str,
    data: &[u8],
    parms: Option<&Dictionary>,
    resolve: Option<Resolver>,
) -> Result<Vec<u8>> {
    match name {
        "FlateDecode" | "Fl" => {
            let decoded = flate::decompress(data)?;
            apply_predictor(decoded, parms, resolve)
        }
        "LZWDecode" | "LZW" => {
            let early = get_int_parm(parms, "EarlyChange", resolve).unwrap_or(1);
            let decoded = lzw_decode(data, early != 0)?;
            apply_predictor(decoded, parms, resolve)
        }
        "ASCIIHexDecode" | "AHx" => ascii_hex_decode(data),
        "ASCII85Decode" | "A85" => ascii85_decode(data),
        "RunLengthDecode" | "RL" => run_length_decode(data),
        // CCITTFaxDecode: T.4 / T.6 を /DecodeParms に従って復号する
        "CCITTFaxDecode" | "CCF" => {
            let params = parms.map(ccitt::params_from_dict).unwrap_or_default();
            ccitt::decode(data, &params)
        }
        // 画像コーデック: ストリーム単位ではデコードしない（描画側で個別処理）
        "DCTDecode" | "DCT" | "JPXDecode" | "JBIG2Decode" => Err(err(format!(
            "image codec filter /{name} is not decoded (use raw data)"
        ))),
        // /Crypt フィルタは「このストリームは別途指定された CFM で復号する」
        // という指示。本ライブラリはストリーム本体の復号を読み込み時に行うため、
        // ここに辿り着く時点で平文。/Identity 扱いで通す。
        "Crypt" => Ok(data.to_vec()),
        other => Err(err(format!("unknown filter /{other}"))),
    }
}

fn get_int_parm(parms: Option<&Dictionary>, key: &str, resolve: Option<Resolver>) -> Option<i64> {
    let o = parms?.get(key)?;
    let o = match (resolve, o) {
        (Some(f), Object::Reference(_)) => f(o),
        _ => o.clone(),
    };
    o.as_int().ok()
}

// ---------------------------------------------------------------------------
// Predictor（§7.4.4.4, PNG: RFC 2083）
// ---------------------------------------------------------------------------

/// `/Predictor` パラメータを適用して予測フィルタを解除する。
fn apply_predictor(
    data: Vec<u8>,
    parms: Option<&Dictionary>,
    resolve: Option<Resolver>,
) -> Result<Vec<u8>> {
    let predictor = get_int_parm(parms, "Predictor", resolve).unwrap_or(1);
    if predictor <= 1 {
        return Ok(data);
    }
    let colors = get_int_parm(parms, "Colors", resolve).unwrap_or(1) as usize;
    let bpc = get_int_parm(parms, "BitsPerComponent", resolve).unwrap_or(8) as usize;
    let columns = get_int_parm(parms, "Columns", resolve).unwrap_or(1) as usize;
    let bytes_per_pixel = (colors * bpc).div_ceil(8);
    let row_len = (colors * bpc * columns).div_ceil(8);

    if predictor == 2 {
        // TIFF predictor（bpc=8 のみ対応）
        if bpc != 8 {
            return Err(err(
                "TIFF predictor with BitsPerComponent != 8 is not supported",
            ));
        }
        let mut out = data;
        for row in out.chunks_mut(row_len) {
            for i in bytes_per_pixel..row.len() {
                row[i] = row[i].wrapping_add(row[i - bytes_per_pixel]);
            }
        }
        return Ok(out);
    }

    // PNG predictor: 各行の先頭にフィルタタイプ 1 バイト
    let stride = row_len + 1;
    let rows = data.len() / stride;
    let mut out = Vec::with_capacity(rows * row_len);
    let mut prev_row = vec![0u8; row_len];
    for r in 0..rows {
        let src = &data[r * stride..(r + 1) * stride];
        let ftype = src[0];
        let mut row = src[1..].to_vec();
        match ftype {
            0 => {} // None
            1 => {
                // Sub
                for i in bytes_per_pixel..row_len {
                    row[i] = row[i].wrapping_add(row[i - bytes_per_pixel]);
                }
            }
            2 => {
                // Up
                for i in 0..row_len {
                    row[i] = row[i].wrapping_add(prev_row[i]);
                }
            }
            3 => {
                // Average
                for i in 0..row_len {
                    let left = if i >= bytes_per_pixel {
                        row[i - bytes_per_pixel] as u16
                    } else {
                        0
                    };
                    let up = prev_row[i] as u16;
                    row[i] = row[i].wrapping_add(((left + up) / 2) as u8);
                }
            }
            4 => {
                // Paeth
                for i in 0..row_len {
                    let a = if i >= bytes_per_pixel {
                        row[i - bytes_per_pixel] as i16
                    } else {
                        0
                    };
                    let b = prev_row[i] as i16;
                    let c = if i >= bytes_per_pixel {
                        prev_row[i - bytes_per_pixel] as i16
                    } else {
                        0
                    };
                    let p = a + b - c;
                    let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
                    let pred = if pa <= pb && pa <= pc {
                        a
                    } else if pb <= pc {
                        b
                    } else {
                        c
                    };
                    row[i] = row[i].wrapping_add(pred as u8);
                }
            }
            t => return Err(err(format!("unknown PNG filter type {t}"))),
        }
        out.extend_from_slice(&row);
        prev_row = row;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// ASCIIHexDecode（§7.4.2）
// ---------------------------------------------------------------------------

fn ascii_hex_decode(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() / 2);
    let mut hi: Option<u8> = None;
    for &b in data {
        match b {
            b'>' => break,
            b if crate::lexer::is_whitespace(b) => {}
            b => {
                let v = match b {
                    b'0'..=b'9' => b - b'0',
                    b'a'..=b'f' => b - b'a' + 10,
                    b'A'..=b'F' => b - b'A' + 10,
                    _ => return Err(err("invalid hex digit in ASCIIHexDecode")),
                };
                match hi.take() {
                    Some(h) => out.push((h << 4) | v),
                    None => hi = Some(v),
                }
            }
        }
    }
    if let Some(h) = hi {
        out.push(h << 4);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// ASCII85Decode（§7.4.3）
// ---------------------------------------------------------------------------

fn ascii85_decode(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() * 4 / 5);
    let mut group = [0u8; 5];
    let mut n = 0usize;
    let mut i = 0usize;
    // 先頭の <~ は任意（Adobe 拡張）
    let start = if data.starts_with(b"<~") { 2 } else { 0 };
    let bytes = &data[start..];
    while i < bytes.len() {
        let b = bytes[i];
        i += 1;
        match b {
            b'~' => break, // ~> 終端
            b'z' if n == 0 => out.extend_from_slice(&[0, 0, 0, 0]),
            b if crate::lexer::is_whitespace(b) => {}
            b'!'..=b'u' => {
                group[n] = b - b'!';
                n += 1;
                if n == 5 {
                    let mut v: u32 = 0;
                    for &g in &group {
                        v = v
                            .checked_mul(85)
                            .and_then(|x| x.checked_add(g as u32))
                            .ok_or_else(|| err("ASCII85 group overflow"))?;
                    }
                    out.extend_from_slice(&v.to_be_bytes());
                    n = 0;
                }
            }
            _ => return Err(err("invalid character in ASCII85Decode")),
        }
    }
    // 端数グループ: n 文字 (2<=n<=4) → n-1 バイト
    if n == 1 {
        return Err(err("invalid trailing ASCII85 group of length 1"));
    }
    if n > 1 {
        for slot in group.iter_mut().skip(n) {
            *slot = 84; // 'u'
        }
        let mut v: u32 = 0;
        for &g in &group {
            v = v.wrapping_mul(85).wrapping_add(g as u32);
        }
        let bytes4 = v.to_be_bytes();
        out.extend_from_slice(&bytes4[..n - 1]);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// RunLengthDecode（§7.4.5）
// ---------------------------------------------------------------------------

fn run_length_decode(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < data.len() {
        let l = data[i];
        i += 1;
        match l {
            128 => break, // EOD
            0..=127 => {
                let n = l as usize + 1;
                if i + n > data.len() {
                    return Err(err("RunLengthDecode: literal run past end"));
                }
                out.extend_from_slice(&data[i..i + n]);
                i += n;
            }
            129..=255 => {
                let n = 257 - l as usize;
                let b = *data
                    .get(i)
                    .ok_or_else(|| err("RunLengthDecode: missing repeat byte"))?;
                i += 1;
                out.extend(std::iter::repeat_n(b, n));
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// LZWDecode（§7.4.4.2, TIFF 系 LZW）
// ---------------------------------------------------------------------------

fn lzw_decode(data: &[u8], early_change: bool) -> Result<Vec<u8>> {
    const CLEAR: u16 = 256;
    const EOD: u16 = 257;
    let mut out = Vec::new();
    // 辞書: 各エントリは (前のエントリ index または None, 末尾バイト)
    let mut dict: Vec<(Option<u16>, u8)> = Vec::new();
    let reset_dict = |d: &mut Vec<(Option<u16>, u8)>| {
        d.clear();
        for b in 0..=255u16 {
            d.push((None, b as u8));
        }
        d.push((None, 0)); // 256 CLEAR
        d.push((None, 0)); // 257 EOD
    };
    reset_dict(&mut dict);

    let expand = |dict: &Vec<(Option<u16>, u8)>, mut code: u16, out: &mut Vec<u8>| -> Result<()> {
        let mut stack = Vec::new();
        loop {
            let (prev, byte) = *dict
                .get(code as usize)
                .ok_or_else(|| err("LZW: code out of range"))?;
            stack.push(byte);
            match prev {
                Some(p) => code = p,
                None => break,
            }
        }
        out.extend(stack.iter().rev());
        Ok(())
    };

    let mut bit_pos = 0usize;
    let total_bits = data.len() * 8;
    let mut code_width = 9usize;
    let mut prev_code: Option<u16> = None;

    // MSB ファーストでビットを読む
    let read_code = |bit_pos: &mut usize, width: usize| -> Option<u16> {
        if *bit_pos + width > total_bits {
            return None;
        }
        let mut v: u16 = 0;
        for _ in 0..width {
            let byte = data[*bit_pos / 8];
            let bit = (byte >> (7 - (*bit_pos % 8))) & 1;
            v = (v << 1) | bit as u16;
            *bit_pos += 1;
        }
        Some(v)
    };

    while let Some(code) = read_code(&mut bit_pos, code_width) {
        match code {
            CLEAR => {
                reset_dict(&mut dict);
                code_width = 9;
                prev_code = None;
            }
            EOD => break,
            code => {
                if let Some(prev) = prev_code {
                    // 新エントリ: prev + (今回展開する系列の先頭バイト)
                    let first_byte_code = if (code as usize) < dict.len() {
                        code
                    } else {
                        prev
                    };
                    // 先頭バイトを求める
                    let mut c = first_byte_code;
                    let first = loop {
                        let (p, b) = dict[c as usize];
                        match p {
                            Some(pp) => c = pp,
                            None => break b,
                        }
                    };
                    dict.push((Some(prev), first));
                }
                if (code as usize) >= dict.len() {
                    return Err(err("LZW: invalid code"));
                }
                expand(&dict, code, &mut out)?;
                prev_code = Some(code);
                // 符号幅の拡張（EarlyChange=1 なら 1 早く広げる）
                let limit = dict.len() + if early_change { 1 } else { 0 };
                code_width = match limit {
                    0..=511 => 9,
                    512..=1023 => 10,
                    1024..=2047 => 11,
                    _ => 12,
                };
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_hex() {
        assert_eq!(ascii_hex_decode(b"48 65 6C 6C 6F>").unwrap(), b"Hello");
        assert_eq!(ascii_hex_decode(b"414>").unwrap(), vec![0x41, 0x40]);
    }

    #[test]
    fn ascii85() {
        // 既知の例: "Man " -> 9jqo^（Wikipedia の例の先頭グループ）
        assert_eq!(ascii85_decode(b"9jqo^~>").unwrap(), b"Man ");
        assert_eq!(ascii85_decode(b"7:C7_~>").unwrap(), b"Easy");
        assert_eq!(ascii85_decode(b"z~>").unwrap(), vec![0, 0, 0, 0]);
        // 端数グループ: "ab" (2 bytes) は 3 文字
        assert_eq!(ascii85_decode(b"@:B~>").unwrap(), b"ab");
    }

    #[test]
    fn run_length() {
        // リテラル 3 バイト "abc" + 反復 4 x 'z' + EOD
        let data = [2u8, b'a', b'b', b'c', 253, b'z', 128];
        assert_eq!(run_length_decode(&data).unwrap(), b"abczzzz");
    }

    #[test]
    fn png_predictor_up() {
        // 2 列 x 3 行, Up フィルタ
        let raw = [10u8, 20, 5, 5, 0, 0];
        // 各行: filter type 2 (Up), 差分
        let filtered = vec![2u8, 10, 20, 2, 251, 241, 2, 251, 251];
        let mut parms = Dictionary::new();
        parms.set("Predictor", 12);
        parms.set("Columns", 2);
        let out = apply_predictor(filtered, Some(&parms), None).unwrap();
        // row0 = 10,20 / row1 = 10+(-5)=5, 20+(-15)=5 / row2 = 0,0
        assert_eq!(out, raw);
    }

    #[test]
    fn decode_stream_flate_chain() {
        let plain = b"some stream content".to_vec();
        let mut dict = Dictionary::new();
        dict.set("Filter", Object::Name("FlateDecode".into()));
        let data = flate::compress(&plain);
        assert_eq!(decode_stream(&dict, &data, None).unwrap(), plain);
    }
}
