//! TrueType フォントのサブセット化。
//!
//! PDF 埋め込み用に、使用グリフだけを残した sfnt バイナリを生成する。
//! **グリフ ID は元のまま維持**し（renumbering しない）、未使用グリフの
//! アウトラインデータを空にする「sparse glyf」方式を採る。
//! これにより composite グリフの参照先 ID 書き換えが不要になり、
//! `/CIDToGIDMap /Identity` がそのまま成立する。
//!
//! 出力テーブル:
//! - `head`（indexToLocFormat=1 に固定、checkSumAdjustment 再計算）
//! - `hhea` / `maxp` / `hmtx`（元のままコピー。グリフ数・メトリクス数は不変）
//! - `loca`（long 形式で再構築）/ `glyf`（使用グリフのみ）
//! - `cvt ` / `fpgm` / `prep`（ヒンティング用。元にあればコピー）
//!
//! `cmap` は含めない（PDF では `/CIDToGIDMap /Identity` により
//! 文字コード→グリフの対応が完結するため不要）。

use std::collections::BTreeSet;

use crate::error::Result;
use crate::truetype::{font_err, TrueTypeFont};

/// 使用グリフ集合 `used_gids` でフォントをサブセット化し、
/// FontFile2 に埋め込める sfnt バイナリを返す。
///
/// - GID 0（.notdef）と composite グリフの構成要素は自動的に含める
/// - グリフ ID は変更されない（未使用グリフは長さ 0 になる）
pub fn subset_font(font: &TrueTypeFont, used_gids: &BTreeSet<u16>) -> Result<Vec<u8>> {
    if font.table(b"glyf").is_none() || font.num_glyphs() == 0 {
        return Err(font_err(
            "font has no glyf outlines (CFF fonts cannot be subset)",
        ));
    }
    let num_glyphs = font.num_glyphs();

    // --- 1. グリフ閉包: 使用グリフ + composite 構成要素 + .notdef ---
    let mut keep: BTreeSet<u16> = BTreeSet::new();
    let mut stack: Vec<u16> = used_gids
        .iter()
        .copied()
        .filter(|&g| g < num_glyphs)
        .collect();
    stack.push(0); // .notdef は常に含める
    while let Some(gid) = stack.pop() {
        if !keep.insert(gid) {
            continue;
        }
        if let Some(data) = font.glyph_data(gid) {
            for comp in composite_components(data) {
                if comp < num_glyphs && !keep.contains(&comp) {
                    stack.push(comp);
                }
            }
        }
    }

    // --- 2. glyf / loca の再構築 ---
    // loca[i] .. loca[i+1] がグリフ i のデータ範囲。未使用グリフは
    // 同一オフセット（長さ 0）にする。各グリフは 4 バイト境界に
    // 揃える（パディングはそのグリフの範囲内に含まれるが、グリフの
    // 構造解析はアウトライン末尾で止まるため無害）。
    let mut glyf: Vec<u8> = Vec::new();
    let mut loca: Vec<u32> = Vec::with_capacity(num_glyphs as usize + 1);
    for gid in 0..num_glyphs {
        loca.push(glyf.len() as u32);
        if keep.contains(&gid) {
            if let Some(data) = font.glyph_data(gid) {
                glyf.extend_from_slice(data);
                glyf.resize(padded_len(glyf.len()), 0);
            }
        }
    }
    loca.push(glyf.len() as u32);
    let mut loca_bytes: Vec<u8> = Vec::with_capacity(loca.len() * 4);
    for off in &loca {
        loca_bytes.extend_from_slice(&off.to_be_bytes());
    }

    // --- 3. head の複製と修正 ---
    let head_src = font
        .table(b"head")
        .ok_or_else(|| font_err("missing head table"))?;
    if head_src.len() < 54 {
        return Err(font_err("head table too short"));
    }
    let mut head = head_src.to_vec();
    head[8..12].copy_from_slice(&[0, 0, 0, 0]); // checkSumAdjustment は後で計算
    head[50..52].copy_from_slice(&1i16.to_be_bytes()); // indexToLocFormat = long

    // --- 4. 出力テーブルの収集 ---
    let mut tables: Vec<([u8; 4], Vec<u8>)> =
        vec![(*b"head", head), (*b"loca", loca_bytes), (*b"glyf", glyf)];
    for tag in [b"hhea", b"maxp", b"hmtx", b"cvt ", b"fpgm", b"prep"] {
        if let Some(data) = font.table(tag) {
            tables.push((*tag, data.to_vec()));
        }
    }
    if !tables.iter().any(|(t, _)| t == b"hhea")
        || !tables.iter().any(|(t, _)| t == b"maxp")
        || !tables.iter().any(|(t, _)| t == b"hmtx")
    {
        return Err(font_err("missing required metric tables (hhea/maxp/hmtx)"));
    }
    // テーブルディレクトリはタグ昇順が必須（sfnt 仕様）
    tables.sort_by_key(|(tag, _)| *tag);

    // --- 5. sfnt の組み立て ---
    let n = tables.len() as u16;
    // searchRange = 2^floor(log2(n)) * 16
    let mut entry_selector: u16 = 0;
    while (2u32 << entry_selector) <= n as u32 {
        entry_selector += 1;
    }
    let search_range: u16 = (1u16 << entry_selector) * 16;
    let range_shift: u16 = n * 16 - search_range;

    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(&0x00010000u32.to_be_bytes()); // sfntVersion
    out.extend_from_slice(&n.to_be_bytes());
    out.extend_from_slice(&search_range.to_be_bytes());
    out.extend_from_slice(&entry_selector.to_be_bytes());
    out.extend_from_slice(&range_shift.to_be_bytes());

    // ディレクトリ（オフセットは後決め）: 12 + 16n がテーブル領域の先頭
    let mut offset = 12 + 16 * n as usize;
    let mut head_offset = 0usize;
    for (tag, data) in &tables {
        let len = data.len();
        if tag == b"head" {
            head_offset = offset;
        }
        out.extend_from_slice(tag);
        out.extend_from_slice(&table_checksum(data).to_be_bytes());
        out.extend_from_slice(&(offset as u32).to_be_bytes());
        out.extend_from_slice(&(len as u32).to_be_bytes());
        offset += padded_len(len);
    }
    // テーブル本体（4 バイト境界へゼロ詰め）
    for (_, data) in &tables {
        out.extend_from_slice(data);
        out.resize(out.len() + padded_len(data.len()) - data.len(), 0);
    }

    // --- 6. checkSumAdjustment（フォント全体のチェックサム補正、head @8）---
    // 仕様: adjustment = 0xB1B0AFBA - (ファイル全体の u32 和)
    let whole = table_checksum(&out);
    let adjustment = 0xB1B0AFBAu32.wrapping_sub(whole);
    out[head_offset + 8..head_offset + 12].copy_from_slice(&adjustment.to_be_bytes());

    Ok(out)
}

/// 4 バイト境界へ切り上げた長さ。
fn padded_len(len: usize) -> usize {
    (len + 3) & !3
}

/// sfnt テーブルチェックサム: ビッグエンディアン u32 の和（不足分は 0 詰め）。
fn table_checksum(data: &[u8]) -> u32 {
    let mut sum = 0u32;
    for chunk in data.chunks(4) {
        let mut word = [0u8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        sum = sum.wrapping_add(u32::from_be_bytes(word));
    }
    sum
}

/// composite グリフの構成要素 GID を列挙する（単純グリフなら空）。
///
/// glyf エントリ構造: i16 numberOfContours（負なら composite）, FontBBox 8 バイト、
/// 以降 composite なら { u16 flags, u16 glyphIndex, 引数, 変形 } の繰り返し。
fn composite_components(data: &[u8]) -> Vec<u16> {
    let mut out = Vec::new();
    if data.len() < 10 {
        return out; // 空グリフまたは不正
    }
    let num_contours = i16::from_be_bytes([data[0], data[1]]);
    if num_contours >= 0 {
        return out; // 単純グリフ
    }
    let mut p = 10;
    loop {
        if p + 4 > data.len() {
            break; // 不正データ: 読めた分だけ返す
        }
        let flags = u16::from_be_bytes([data[p], data[p + 1]]);
        out.push(u16::from_be_bytes([data[p + 2], data[p + 3]]));
        p += 4;
        // ARG_1_AND_2_ARE_WORDS (bit 0): 引数が各 2 バイト、でなければ各 1 バイト
        p += if flags & 0x0001 != 0 { 4 } else { 2 };
        // 変形: WE_HAVE_A_SCALE (bit 3) / X_AND_Y_SCALE (bit 6) / TWO_BY_TWO (bit 7)
        if flags & 0x0008 != 0 {
            p += 2;
        } else if flags & 0x0040 != 0 {
            p += 4;
        } else if flags & 0x0080 != 0 {
            p += 8;
        }
        // MORE_COMPONENTS (bit 5)
        if flags & 0x0020 == 0 {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padded_and_checksum_helpers() {
        assert_eq!(padded_len(0), 0);
        assert_eq!(padded_len(1), 4);
        assert_eq!(padded_len(4), 4);
        assert_eq!(padded_len(5), 8);
        // 0x01020304 + 0x05000000（パディング 0 詰め）
        assert_eq!(
            table_checksum(&[1, 2, 3, 4, 5]),
            0x01020304u32.wrapping_add(0x05000000)
        );
    }

    #[test]
    fn composite_parsing() {
        // numberOfContours = -1, bbox(8), 2 コンポーネント:
        //   flags=0x0021 (WORDS|MORE), gid=5, args 4B
        //   flags=0x0008 (SCALE),      gid=7, args 2B, scale 2B
        let mut g = Vec::new();
        g.extend_from_slice(&(-1i16).to_be_bytes());
        g.extend_from_slice(&[0u8; 8]); // bbox
        g.extend_from_slice(&0x0021u16.to_be_bytes());
        g.extend_from_slice(&5u16.to_be_bytes());
        g.extend_from_slice(&[0u8; 4]); // word args
        g.extend_from_slice(&0x0008u16.to_be_bytes());
        g.extend_from_slice(&7u16.to_be_bytes());
        g.extend_from_slice(&[0u8; 2]); // byte args
        g.extend_from_slice(&[0u8; 2]); // scale
        assert_eq!(composite_components(&g), vec![5, 7]);
        // 単純グリフ
        let simple = [0u8, 2, 0, 0, 0, 0, 0, 0, 0, 0];
        assert!(composite_components(&simple).is_empty());
        // 壊れた composite（途中で切れている）でも panic しない
        assert_eq!(composite_components(&g[..14]), vec![5]);
    }

    /// システムフォントを使った構造検証（フォントが無ければスキップ）。
    fn load_system_font(path: &str, index: u32) -> Option<TrueTypeFont> {
        let data = std::fs::read(path).ok()?;
        TrueTypeFont::parse(data, index).ok()
    }

    #[test]
    fn subset_arial_structure() {
        let Some(font) = load_system_font(r"C:\Windows\Fonts\arial.ttf", 0) else {
            eprintln!("skip: arial.ttf not found");
            return;
        };
        let gid_a = font.glyph_id('A').unwrap();
        let gid_b = font.glyph_id('B').unwrap();
        let used: BTreeSet<u16> = [gid_a, gid_b].into_iter().collect();
        let out = subset_font(&font, &used).unwrap();
        assert!(
            out.len()
                < std::fs::metadata(r"C:\Windows\Fonts\arial.ttf")
                    .unwrap()
                    .len() as usize
                    / 2
        );

        // チェックサム検証: 全体の u32 和が魔法数になること
        assert_eq!(table_checksum(&out), 0xB1B0AFBA);

        // サブセットを自前パーサで再解析して構造を検証
        let sub = TrueTypeFont::parse(out, 0).unwrap();
        assert_eq!(sub.num_glyphs(), font.num_glyphs());
        assert_eq!(sub.units_per_em(), font.units_per_em());
        // 残したグリフ: データが元と一致（パディング分は前方一致で比較）
        for gid in [0u16, gid_a, gid_b] {
            let orig = font.glyph_data(gid).unwrap();
            let kept = sub.glyph_data(gid).unwrap();
            assert!(
                kept.len() >= orig.len() && &kept[..orig.len()] == orig,
                "gid {gid}"
            );
        }
        // 落としたグリフ: 空になっている
        let gid_z = font.glyph_id('z').unwrap();
        assert_eq!(sub.glyph_data(gid_z), Some(&[][..]));
        // メトリクスは保持
        assert_eq!(sub.advance_width(gid_a), font.advance_width(gid_a));
        assert_eq!(sub.advance_width(gid_z), font.advance_width(gid_z));
    }

    #[test]
    fn subset_includes_composite_components() {
        let Some(font) = load_system_font(r"C:\Windows\Fonts\arial.ttf", 0) else {
            eprintln!("skip: arial.ttf not found");
            return;
        };
        // 'Á' は通常 'A' + アクセントの composite
        let Some(gid) = font.glyph_id('Á') else {
            eprintln!("skip: no Á in font");
            return;
        };
        let used: BTreeSet<u16> = [gid].into_iter().collect();
        let out = subset_font(&font, &used).unwrap();
        let sub = TrueTypeFont::parse(out, 0).unwrap();
        let data = sub.glyph_data(gid).unwrap().to_vec();
        for comp in composite_components(&data) {
            let comp_orig = font.glyph_data(comp).unwrap();
            let comp_sub = sub.glyph_data(comp).unwrap();
            assert!(
                comp_sub.len() >= comp_orig.len(),
                "composite component {comp} was dropped"
            );
        }
    }

    #[test]
    fn subset_japanese_ttc() {
        let path = r"C:\Windows\Fonts\msgothic.ttc";
        let Some(font) = load_system_font(path, 0) else {
            eprintln!("skip: msgothic.ttc not found");
            return;
        };
        let chars = "こんにちは世界日本語描画";
        let used: BTreeSet<u16> = chars.chars().filter_map(|c| font.glyph_id(c)).collect();
        assert!(!used.is_empty());
        let out = subset_font(&font, &used).unwrap();
        // 数 MB のフォントが大幅に縮むこと
        assert!(
            out.len() < 1_000_000,
            "subset too large: {} bytes",
            out.len()
        );
        let sub = TrueTypeFont::parse(out, 0).unwrap();
        for gid in &used {
            assert!(sub.glyph_data(*gid).is_some());
            assert_eq!(sub.advance_width(*gid), font.advance_width(*gid));
        }
    }
}
