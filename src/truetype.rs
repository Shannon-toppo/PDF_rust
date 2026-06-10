//! TrueType / TrueType Collection (TTF/TTC) フォントの解析。
//!
//! PDF へのフォント埋め込み（CIDFontType2 / Identity-H）に必要な情報を
//! 取り出すための最小限のパーサ。対応テーブル:
//!
//! - sfnt テーブルディレクトリ（TTC ヘッダ `ttcf` 含む）
//! - `cmap` format 12（優先）/ format 4 — Unicode → グリフ ID
//! - `head` — unitsPerEm, indexToLocFormat, FontBBox
//! - `hhea` / `hmtx` — アセント/ディセント、グリフ advance 幅
//! - `maxp` — グリフ数
//! - `OS/2`（任意）— CapHeight
//! - `post` — italicAngle
//! - `name` — PostScript 名（nameID 6）
//! - `loca` / `glyf` — グリフデータ（サブセット化用）
//!
//! 入力は信頼できないデータとして扱い、不正なファイルでも
//! panic せず [`PdfError::Font`] を返すこと（すべて境界検査する）。

use crate::error::{PdfError, Result};

/// 解析済みの TrueType フォント 1 書体分。
///
/// TTC の場合もファイル全体のバイト列を保持し、テーブルディレクトリだけ
/// 選択した書体のものを使う（テーブルオフセットはファイル先頭基準）。
pub struct TrueTypeFont {
    /// ファイル全体のバイト列。
    data: Vec<u8>,
    /// タグ → (オフセット, 長さ)。選択した書体のテーブルディレクトリ。
    tables: std::collections::HashMap<[u8; 4], (usize, usize)>,
    units_per_em: u16,
    /// indexToLocFormat（解析時に loca 正規化へ使用。フィールドとして保持）。
    #[allow(dead_code)]
    index_to_loc_format: i16,
    num_glyphs: u16,
    font_bbox: [i32; 4],
    ascent: i32,
    descent: i32,
    cap_height: i32,
    italic_angle: f64,
    num_h_metrics: u16,
    post_script_name: String,
    /// `loca` テーブルを u32 オフセット（glyf 先頭基準）に正規化したもの。
    /// 長さは `num_glyphs + 1`。glyf/loca が無い（CFF 等）場合は空。
    loca: Vec<u32>,
    /// cmap の解析結果（実装の都合で内部表現は自由に変えてよい）。
    cmap: Cmap,
}

/// cmap サブテーブルの内部表現。
enum Cmap {
    /// format 4: BMP のセグメント配列（binary search で引く）。
    Format4 {
        /// (end_code, start_code, id_delta, id_range_offset, range_offset_pos)
        /// など、実装が引きやすい形でよい。
        segments: Vec<Format4Segment>,
        /// glyphIdArray を含む format 4 サブテーブル全体のコピー
        /// （idRangeOffset 経由の参照に使う）。
        subtable: Vec<u8>,
    },
    /// format 12: SequentialMapGroup の配列。
    Format12 { groups: Vec<(u32, u32, u32)> }, // (startChar, endChar, startGID)
    /// 使える cmap が無い。
    None,
}

struct Format4Segment {
    end_code: u16,
    start_code: u16,
    id_delta: i16,
    id_range_offset: u16,
    /// この segment の idRangeOffset フィールド自身の subtable 内オフセット
    /// （spec の「idRangeOffset の位置からの相対参照」を解決するため）。
    range_offset_pos: usize,
}

// --- バイト列読み出しの補助（すべて境界検査・big-endian） ---

/// `data[pos..pos+2]` を big-endian u16 として読む。範囲外は `None`。
fn read_u16(data: &[u8], pos: usize) -> Option<u16> {
    let b = data.get(pos..pos + 2)?;
    Some(u16::from_be_bytes([b[0], b[1]]))
}

/// `data[pos..pos+2]` を big-endian i16 として読む。範囲外は `None`。
fn read_i16(data: &[u8], pos: usize) -> Option<i16> {
    read_u16(data, pos).map(|v| v as i16)
}

/// `data[pos..pos+4]` を big-endian u32 として読む。範囲外は `None`。
fn read_u32(data: &[u8], pos: usize) -> Option<u32> {
    let b = data.get(pos..pos + 4)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// `data[pos..pos+4]` を big-endian i32 として読む。範囲外は `None`。
fn read_i32(data: &[u8], pos: usize) -> Option<i32> {
    read_u32(data, pos).map(|v| v as i32)
}

/// sfnt のマジック値として妥当か。
fn is_sfnt_magic(tag: u32) -> bool {
    // 0x00010000 = TrueType outlines, "true" = Apple TrueType, "OTTO" = CFF.
    tag == 0x0001_0000 || tag == u32::from_be_bytes(*b"true") || tag == u32::from_be_bytes(*b"OTTO")
}

impl TrueTypeFont {
    /// ファイルに含まれる書体数を返す（TTC ヘッダ `ttcf` なら numFonts、
    /// 通常の TTF なら 1、どちらでもなければ 0）。
    pub fn num_fonts(data: &[u8]) -> usize {
        let Some(tag) = read_u32(data, 0) else {
            return 0;
        };
        if tag == u32::from_be_bytes(*b"ttcf") {
            // ttcf: u32 version, u32 numFonts。
            match read_u32(data, 8) {
                Some(n) => n as usize,
                None => 0,
            }
        } else if is_sfnt_magic(tag) {
            1
        } else {
            0
        }
    }

    /// フォントを解析する。`ttc_index` は TTC 内の書体番号（TTF なら 0）。
    ///
    /// 実装手順:
    /// 1. 先頭 4 バイトで判別: `ttcf` → TTC ヘッダ（u32 numFonts,
    ///    u32 offsetTable[numFonts]）から `ttc_index` 番目のオフセットへ。
    ///    `0x00010000` または `true` → そのまま sfnt。`OTTO` → CFF 系
    ///    （テーブルは読めるので glyf 関連だけ空にし、`is_cff()` で判定可能に）。
    /// 2. sfnt ヘッダ: u32 version, u16 numTables, u16x3 (無視)。
    ///    続いてテーブルレコード 16 バイト × numTables
    ///    （tag[4], checkSum u32, offset u32, length u32）。
    ///    offset+length がファイル範囲内か検査して `tables` へ。
    /// 3. head(54 バイト以上): unitsPerEm @18 (u16), xMin..yMax @36..44 (i16 x4),
    ///    indexToLocFormat @50 (i16)。
    /// 4. maxp: numGlyphs @4 (u16)。
    /// 5. hhea: ascender @4 (i16), descender @6 (i16), numberOfHMetrics @34 (u16)。
    /// 6. OS/2（あれば）: sCapHeight @88 (i16, version>=2 のときのみ)。
    ///    無ければ cap_height = ascent * 7 / 10 で近似。
    /// 7. post: italicAngle @4 (Fixed 16.16)。無ければ 0.0。
    /// 8. name: nameID 6 (PostScript name) を探す。platform 3 (UTF-16BE) を優先、
    ///    platform 1 (バイトをそのまま ASCII 扱い) でも可。
    ///    見つからなければ "Embedded" + 適当な代替。
    ///    非 ASCII や空白は除去/置換して PDF 名として安全にすること。
    /// 9. loca: indexToLocFormat=0 なら u16×(numGlyphs+1) を 2 倍して u32 に、
    ///    1 なら u32×(numGlyphs+1)。glyf か loca が無ければ空のままにする。
    /// 10. cmap: 「platformID=3, encodingID=10 の format 12」→
    ///     「platformID=0 で format 12」→「(3,1) の format 4」→
    ///     「(0,*) の format 4」の優先順で 1 つ選ぶ。
    pub fn parse(data: Vec<u8>, ttc_index: u32) -> Result<TrueTypeFont> {
        // 1. sfnt オフセットテーブルの開始位置を決める。
        let head_tag = read_u32(&data, 0).ok_or_else(|| font_err("ファイルが短すぎます"))?;
        let sfnt_offset = if head_tag == u32::from_be_bytes(*b"ttcf") {
            let num_fonts = read_u32(&data, 8).ok_or_else(|| font_err("TTC ヘッダが不正"))?;
            if ttc_index >= num_fonts {
                return Err(font_err("TTC インデックスが範囲外"));
            }
            // offsetTable[ttc_index] @ 12 + 4*index。
            let pos = 12usize
                .checked_add((ttc_index as usize).saturating_mul(4))
                .ok_or_else(|| font_err("TTC オフセット計算オーバーフロー"))?;
            read_u32(&data, pos).ok_or_else(|| font_err("TTC オフセットテーブルが不正"))? as usize
        } else {
            0
        };

        // 2. sfnt ヘッダ + テーブルディレクトリ。
        let sfnt_version =
            read_u32(&data, sfnt_offset).ok_or_else(|| font_err("sfnt ヘッダが読めません"))?;
        if !is_sfnt_magic(sfnt_version) {
            return Err(font_err("不明な sfnt バージョン"));
        }
        let num_tables = read_u16(&data, sfnt_offset + 4)
            .ok_or_else(|| font_err("sfnt numTables が読めません"))?;

        let mut tables: std::collections::HashMap<[u8; 4], (usize, usize)> =
            std::collections::HashMap::new();
        // レコードは sfnt_offset + 12 から 16 バイトずつ。
        let record_base = sfnt_offset
            .checked_add(12)
            .ok_or_else(|| font_err("テーブルレコード位置オーバーフロー"))?;
        for i in 0..num_tables as usize {
            let rec = record_base
                .checked_add(
                    i.checked_mul(16)
                        .ok_or_else(|| font_err("レコードオーバーフロー"))?,
                )
                .ok_or_else(|| font_err("レコードオーバーフロー"))?;
            let tag_bytes = data
                .get(rec..rec + 4)
                .ok_or_else(|| font_err("テーブルタグが読めません"))?;
            let mut tag = [0u8; 4];
            tag.copy_from_slice(tag_bytes);
            let offset = read_u32(&data, rec + 8)
                .ok_or_else(|| font_err("テーブルオフセットが読めません"))?
                as usize;
            let length = read_u32(&data, rec + 12)
                .ok_or_else(|| font_err("テーブル長が読めません"))?
                as usize;
            // offset+length がファイル範囲内かを検査。範囲外のレコードは無視する。
            if let Some(end) = offset.checked_add(length) {
                if end <= data.len() {
                    tables.insert(tag, (offset, length));
                }
            }
        }

        // 各テーブルのスライスを取り出す補助クロージャ。
        let table_slice = |tbls: &std::collections::HashMap<[u8; 4], (usize, usize)>,
                           tag: &[u8; 4]|
         -> Option<(usize, usize)> { tbls.get(tag).copied() };

        // 3. head。
        let (head_off, head_len) =
            table_slice(&tables, b"head").ok_or_else(|| font_err("head テーブルがありません"))?;
        let head = data
            .get(head_off..head_off + head_len)
            .ok_or_else(|| font_err("head テーブルが範囲外"))?;
        let units_per_em =
            read_u16(head, 18).ok_or_else(|| font_err("head.unitsPerEm が読めません"))?;
        if units_per_em == 0 {
            return Err(font_err("unitsPerEm が 0"));
        }
        let x_min = read_i16(head, 36).ok_or_else(|| font_err("head.xMin が読めません"))? as i32;
        let y_min = read_i16(head, 38).ok_or_else(|| font_err("head.yMin が読めません"))? as i32;
        let x_max = read_i16(head, 40).ok_or_else(|| font_err("head.xMax が読めません"))? as i32;
        let y_max = read_i16(head, 42).ok_or_else(|| font_err("head.yMax が読めません"))? as i32;
        let font_bbox = [x_min, y_min, x_max, y_max];
        let index_to_loc_format =
            read_i16(head, 50).ok_or_else(|| font_err("head.indexToLocFormat が読めません"))?;
        if index_to_loc_format != 0 && index_to_loc_format != 1 {
            return Err(font_err("indexToLocFormat が不正"));
        }

        // 4. maxp。
        let (maxp_off, maxp_len) =
            table_slice(&tables, b"maxp").ok_or_else(|| font_err("maxp テーブルがありません"))?;
        let maxp = data
            .get(maxp_off..maxp_off + maxp_len)
            .ok_or_else(|| font_err("maxp テーブルが範囲外"))?;
        let num_glyphs =
            read_u16(maxp, 4).ok_or_else(|| font_err("maxp.numGlyphs が読めません"))?;

        // 5. hhea。
        let (hhea_off, hhea_len) =
            table_slice(&tables, b"hhea").ok_or_else(|| font_err("hhea テーブルがありません"))?;
        let hhea = data
            .get(hhea_off..hhea_off + hhea_len)
            .ok_or_else(|| font_err("hhea テーブルが範囲外"))?;
        let ascent =
            read_i16(hhea, 4).ok_or_else(|| font_err("hhea.ascender が読めません"))? as i32;
        let descent =
            read_i16(hhea, 6).ok_or_else(|| font_err("hhea.descender が読めません"))? as i32;
        let num_h_metrics =
            read_u16(hhea, 34).ok_or_else(|| font_err("hhea.numberOfHMetrics が読めません"))?;

        // 6. OS/2（任意）。
        let cap_height = {
            let mut ch: Option<i32> = None;
            if let Some((os2_off, os2_len)) = table_slice(&tables, b"OS/2") {
                if let Some(os2) = data.get(os2_off..os2_off + os2_len) {
                    let version = read_u16(os2, 0).unwrap_or(0);
                    // sCapHeight @88 は version >= 2 のときのみ存在。
                    if version >= 2 {
                        if let Some(v) = read_i16(os2, 88) {
                            ch = Some(v as i32);
                        }
                    }
                }
            }
            // 近似値: ascent * 7 / 10。
            ch.unwrap_or(ascent * 7 / 10)
        };

        // 7. post（任意）。
        let italic_angle = {
            let mut angle = 0.0_f64;
            if let Some((post_off, post_len)) = table_slice(&tables, b"post") {
                if let Some(post) = data.get(post_off..post_off + post_len) {
                    if let Some(fixed) = read_i32(post, 4) {
                        angle = fixed as f64 / 65536.0;
                    }
                }
            }
            angle
        };

        // 8. name（PostScript 名 nameID 6）。
        let post_script_name = parse_post_script_name(&data, &tables);

        // 9. loca（glyf があるときのみ）。
        let loca = parse_loca(&data, &tables, num_glyphs, index_to_loc_format);

        // 10. cmap。
        let cmap = parse_cmap(&data, &tables);

        Ok(TrueTypeFont {
            data,
            tables,
            units_per_em,
            index_to_loc_format,
            num_glyphs,
            font_bbox,
            ascent,
            descent,
            cap_height,
            italic_angle,
            num_h_metrics,
            post_script_name,
            loca,
            cmap,
        })
    }

    /// 生のテーブルを取得する（タグは `b"glyf"` など）。
    pub fn table(&self, tag: &[u8; 4]) -> Option<&[u8]> {
        let (off, len) = *self.tables.get(tag)?;
        self.data.get(off..off + len)
    }

    /// CFF アウトライン（OpenType/CFF）か。true なら FontFile2 埋め込み不可。
    pub fn is_cff(&self) -> bool {
        self.tables.contains_key(b"CFF ") && !self.tables.contains_key(b"glyf")
    }

    /// グリフ数。
    pub fn num_glyphs(&self) -> u16 {
        self.num_glyphs
    }

    /// unitsPerEm（通常 1000 または 2048）。
    pub fn units_per_em(&self) -> u16 {
        self.units_per_em
    }

    /// FontBBox `[xMin, yMin, xMax, yMax]`（フォント単位）。
    pub fn font_bbox(&self) -> [i32; 4] {
        self.font_bbox
    }

    /// アセント（フォント単位、hhea.ascender）。
    pub fn ascent(&self) -> i32 {
        self.ascent
    }

    /// ディセント（フォント単位、hhea.descender、通常負値）。
    pub fn descent(&self) -> i32 {
        self.descent
    }

    /// 大文字の高さ（フォント単位）。OS/2 に無ければ近似値。
    pub fn cap_height(&self) -> i32 {
        self.cap_height
    }

    /// イタリック角（度）。
    pub fn italic_angle(&self) -> f64 {
        self.italic_angle
    }

    /// PostScript 名（PDF の BaseFont に使える形に正規化済み）。
    pub fn post_script_name(&self) -> &str {
        &self.post_script_name
    }

    /// Unicode 文字 → グリフ ID。マップに無ければ `None`。
    pub fn glyph_id(&self, c: char) -> Option<u16> {
        let code = c as u32;
        match &self.cmap {
            Cmap::Format12 { groups } => {
                // groups は startCharCode 昇順前提。binary search。
                let mut lo = 0usize;
                let mut hi = groups.len();
                while lo < hi {
                    let mid = lo + (hi - lo) / 2;
                    let (start, end, start_gid) = groups[mid];
                    if code < start {
                        hi = mid;
                    } else if code > end {
                        lo = mid + 1;
                    } else {
                        // gid = startGlyphID + (c - startCharCode)。
                        let gid = start_gid.wrapping_add(code - start);
                        // u16 範囲超過 or 0 は None。
                        if gid == 0 || gid > u16::MAX as u32 {
                            return None;
                        }
                        return Some(gid as u16);
                    }
                }
                None
            }
            Cmap::Format4 { segments, subtable } => {
                if code > 0xFFFF {
                    return None;
                }
                let c16 = code as u16;
                // endCode >= c の最初の segment を探す（segments は endCode 昇順）。
                let mut lo = 0usize;
                let mut hi = segments.len();
                while lo < hi {
                    let mid = lo + (hi - lo) / 2;
                    if segments[mid].end_code < c16 {
                        lo = mid + 1;
                    } else {
                        hi = mid;
                    }
                }
                let seg = segments.get(lo)?;
                if c16 < seg.start_code {
                    return None;
                }
                let gid: u16 = if seg.id_range_offset == 0 {
                    ((c16 as i32 + seg.id_delta as i32) & 0xFFFF) as u16
                } else {
                    // address = range_offset_pos + idRangeOffset + 2*(c - startCode)。
                    let addr = seg
                        .range_offset_pos
                        .checked_add(seg.id_range_offset as usize)?
                        .checked_add(2 * (c16 - seg.start_code) as usize)?;
                    let glyph = read_u16(subtable, addr)?;
                    if glyph == 0 {
                        return None;
                    }
                    ((glyph as i32 + seg.id_delta as i32) & 0xFFFF) as u16
                };
                if gid == 0 {
                    None
                } else {
                    Some(gid)
                }
            }
            Cmap::None => None,
        }
    }

    /// グリフの advance 幅（フォント単位）。
    /// `gid >= numberOfHMetrics` のときは最後のエントリの幅を使う（spec どおり）。
    pub fn advance_width(&self, gid: u16) -> u16 {
        let default = self.units_per_em / 2;
        if self.num_h_metrics == 0 {
            return default;
        }
        let Some((hmtx_off, hmtx_len)) = self.tables.get(b"hmtx").copied() else {
            return default;
        };
        let Some(hmtx) = self.data.get(hmtx_off..hmtx_off + hmtx_len) else {
            return default;
        };
        // gid < numberOfHMetrics ならエントリ gid、そうでなければ最後の advance。
        let index = if gid < self.num_h_metrics {
            gid as usize
        } else {
            (self.num_h_metrics - 1) as usize
        };
        // 各エントリ {advanceWidth u16, lsb i16} = 4 バイト。
        match index.checked_mul(4).and_then(|pos| read_u16(hmtx, pos)) {
            Some(w) => w,
            None => default,
        }
    }

    /// グリフのアウトラインデータ（`glyf` テーブル内のバイト列）。
    /// 空グリフ（スペース等）は `Some(&[])`。gid 範囲外や glyf 無しは `None`。
    pub fn glyph_data(&self, gid: u16) -> Option<&[u8]> {
        if self.loca.is_empty() {
            return None;
        }
        let gid = gid as usize;
        // loca[gid]..loca[gid+1]。
        let start = *self.loca.get(gid)? as usize;
        let end = *self.loca.get(gid + 1)? as usize;
        let (glyf_off, glyf_len) = self.tables.get(b"glyf").copied()?;
        let glyf = self.data.get(glyf_off..glyf_off + glyf_len)?;
        // start > glyf 長 or start > end は None。end は glyf 範囲にクランプ。
        if start > glyf.len() || start > end {
            return None;
        }
        let end = end.min(glyf.len());
        glyf.get(start..end)
    }
}

/// `name` テーブルから nameID 6（PostScript 名）を取り出して正規化する。
fn parse_post_script_name(
    data: &[u8],
    tables: &std::collections::HashMap<[u8; 4], (usize, usize)>,
) -> String {
    let fallback = || "EmbeddedFont".to_string();
    let Some((name_off, name_len)) = tables.get(b"name").copied() else {
        return fallback();
    };
    let Some(name) = data.get(name_off..name_off + name_len) else {
        return fallback();
    };
    // header: u16 format, u16 count, u16 stringOffset。
    let Some(count) = read_u16(name, 2) else {
        return fallback();
    };
    let Some(string_offset) = read_u16(name, 4) else {
        return fallback();
    };
    let string_offset = string_offset as usize;

    // platform 3 (UTF-16BE) を優先、次点で platform 1。
    let mut best_p3: Option<String> = None;
    let mut best_p1: Option<String> = None;

    for i in 0..count as usize {
        // 各レコード 12 バイト、header 6 バイトの後。
        let rec = match 6usize.checked_add(i.saturating_mul(12)) {
            Some(v) => v,
            None => break,
        };
        let Some(platform_id) = read_u16(name, rec) else {
            break;
        };
        let Some(name_id) = read_u16(name, rec + 6) else {
            break;
        };
        if name_id != 6 {
            continue;
        }
        let Some(length) = read_u16(name, rec + 8) else {
            continue;
        };
        let Some(offset) = read_u16(name, rec + 10) else {
            continue;
        };
        // 文字列は name 先頭 + stringOffset + offset。
        let str_start = match string_offset.checked_add(offset as usize) {
            Some(v) => v,
            None => continue,
        };
        let str_end = match str_start.checked_add(length as usize) {
            Some(v) => v,
            None => continue,
        };
        let Some(bytes) = name.get(str_start..str_end) else {
            continue;
        };
        if platform_id == 3 {
            // UTF-16BE。ASCII のみ取り出す。
            let mut s = String::new();
            let mut j = 0;
            while j + 1 < bytes.len() {
                let cp = u16::from_be_bytes([bytes[j], bytes[j + 1]]);
                if cp < 0x80 {
                    s.push(cp as u8 as char);
                }
                j += 2;
            }
            if best_p3.is_none() {
                best_p3 = Some(s);
            }
        } else if platform_id == 1 {
            // バイトをそのまま ASCII 扱い。
            let s: String = bytes.iter().map(|&b| b as char).collect();
            if best_p1.is_none() {
                best_p1 = Some(s);
            }
        }
    }

    let raw = best_p3.or(best_p1);
    match raw {
        Some(s) => {
            let sanitized = sanitize_pdf_name(&s);
            if sanitized.is_empty() {
                fallback()
            } else {
                sanitized
            }
        }
        None => fallback(),
    }
}

/// PDF 名として安全な文字列に正規化する。
/// ASCII 0x21..=0x7E のうち PDF デリミタ `()<>[]{}/%#` と空白を除去。
fn sanitize_pdf_name(s: &str) -> String {
    let mut out = String::new();
    for ch in s.chars() {
        let c = ch as u32;
        if (0x21..=0x7E).contains(&c) {
            // PDF デリミタや特殊文字は除外。
            if !matches!(
                ch,
                '(' | ')' | '<' | '>' | '[' | ']' | '{' | '}' | '/' | '%' | '#'
            ) {
                out.push(ch);
            }
        }
    }
    out
}

/// `loca`/`glyf` から正規化済み loca（u32, glyf 先頭基準）を作る。
/// glyf か loca が無ければ空 Vec。
fn parse_loca(
    data: &[u8],
    tables: &std::collections::HashMap<[u8; 4], (usize, usize)>,
    num_glyphs: u16,
    index_to_loc_format: i16,
) -> Vec<u32> {
    if !tables.contains_key(b"glyf") {
        return Vec::new();
    }
    let Some((loca_off, loca_len)) = tables.get(b"loca").copied() else {
        return Vec::new();
    };
    let Some(loca) = data.get(loca_off..loca_off + loca_len) else {
        return Vec::new();
    };
    let count = num_glyphs as usize + 1;
    let mut out = Vec::with_capacity(count);
    if index_to_loc_format == 0 {
        // short: u16 × count、値は 2 倍。
        for i in 0..count {
            match read_u16(loca, i * 2) {
                Some(v) => out.push(v as u32 * 2),
                None => return Vec::new(),
            }
        }
    } else {
        // long: u32 × count。
        for i in 0..count {
            match read_u32(loca, i * 4) {
                Some(v) => out.push(v),
                None => return Vec::new(),
            }
        }
    }
    out
}

/// `cmap` テーブルを解析し、優先順に 1 つのサブテーブルを選ぶ。
fn parse_cmap(data: &[u8], tables: &std::collections::HashMap<[u8; 4], (usize, usize)>) -> Cmap {
    let Some((cmap_off, cmap_len)) = tables.get(b"cmap").copied() else {
        return Cmap::None;
    };
    let Some(cmap) = data.get(cmap_off..cmap_off + cmap_len) else {
        return Cmap::None;
    };
    // header: u16 version, u16 numTables。
    let Some(num_sub) = read_u16(cmap, 2) else {
        return Cmap::None;
    };

    // 各 encoding record: {platformID u16, encodingID u16, offset u32}。
    // 優先度を割り当てて最良のものを選ぶ（小さいほど優先）。
    // 0: (3,10) f12, 1: (0,*) f12, 2: (3,1) f4, 3: (0,*) f4, 4: any f4。
    let mut best: Option<(u8, usize, u16)> = None; // (priority, sub_offset, format)

    for i in 0..num_sub as usize {
        let rec = match 4usize.checked_add(i.saturating_mul(8)) {
            Some(v) => v,
            None => break,
        };
        let Some(platform_id) = read_u16(cmap, rec) else {
            break;
        };
        let Some(encoding_id) = read_u16(cmap, rec + 2) else {
            break;
        };
        let Some(sub_off) = read_u32(cmap, rec + 4) else {
            break;
        };
        let sub_off = sub_off as usize;
        // サブテーブル format を覗く。
        let Some(format) = read_u16(cmap, sub_off) else {
            continue;
        };

        let priority: Option<u8> = match format {
            12 => {
                if platform_id == 3 && encoding_id == 10 {
                    Some(0)
                } else if platform_id == 0 {
                    Some(1)
                } else {
                    None
                }
            }
            4 => {
                if platform_id == 3 && encoding_id == 1 {
                    Some(2)
                } else if platform_id == 0 {
                    Some(3)
                } else {
                    Some(4)
                }
            }
            _ => None,
        };

        if let Some(p) = priority {
            let better = match best {
                Some((bp, _, _)) => p < bp,
                None => true,
            };
            if better {
                best = Some((p, sub_off, format));
            }
        }
    }

    let Some((_, sub_off, format)) = best else {
        return Cmap::None;
    };

    match format {
        12 => parse_cmap_format12(cmap, sub_off),
        4 => parse_cmap_format4(cmap, sub_off),
        _ => Cmap::None,
    }
}

/// cmap format 12 を解析。
fn parse_cmap_format12(cmap: &[u8], sub_off: usize) -> Cmap {
    // u16 format=12, u16 reserved, u32 length, u32 language, u32 numGroups。
    let Some(num_groups) = read_u32(cmap, sub_off + 12) else {
        return Cmap::None;
    };
    let mut groups = Vec::with_capacity(num_groups as usize);
    // groups は sub_off + 16 から {start u32, end u32, startGID u32} = 12 バイト。
    let base = match sub_off.checked_add(16) {
        Some(v) => v,
        None => return Cmap::None,
    };
    for i in 0..num_groups as usize {
        let g = match base.checked_add(i.saturating_mul(12)) {
            Some(v) => v,
            None => break,
        };
        let (Some(start), Some(end), Some(start_gid)) = (
            read_u32(cmap, g),
            read_u32(cmap, g + 4),
            read_u32(cmap, g + 8),
        ) else {
            break;
        };
        groups.push((start, end, start_gid));
    }
    if groups.is_empty() {
        Cmap::None
    } else {
        // binary search のため startChar 昇順にソート（通常は既に昇順）。
        groups.sort_by_key(|g| g.0);
        Cmap::Format12 { groups }
    }
}

/// cmap format 4 を解析。
fn parse_cmap_format4(cmap: &[u8], sub_off: usize) -> Cmap {
    // u16 format=4, u16 length。
    let Some(length) = read_u16(cmap, sub_off + 2) else {
        return Cmap::None;
    };
    let length = length as usize;
    // サブテーブル全体（length 分、テーブル境界にクランプ）をコピー。
    let sub_end = match sub_off.checked_add(length) {
        Some(v) => v.min(cmap.len()),
        None => cmap.len(),
    };
    let Some(subtable) = cmap.get(sub_off..sub_end) else {
        return Cmap::None;
    };
    let subtable = subtable.to_vec();

    // segCountX2 @6。
    let Some(seg_count_x2) = read_u16(&subtable, 6) else {
        return Cmap::None;
    };
    let seg_count = seg_count_x2 as usize / 2;
    if seg_count == 0 {
        return Cmap::None;
    }

    // 配列のオフセット（subtable 内）。
    // endCode @14, reservedPad @14+2*segCount, startCode の後 idDelta, idRangeOffset。
    let end_base = 14usize;
    let start_base = end_base + 2 * seg_count + 2; // +2 は reservedPad。
    let delta_base = start_base + 2 * seg_count;
    let range_base = delta_base + 2 * seg_count;

    let mut segments = Vec::with_capacity(seg_count);
    for i in 0..seg_count {
        let (Some(end_code), Some(start_code), Some(id_delta), Some(id_range_offset)) = (
            read_u16(&subtable, end_base + 2 * i),
            read_u16(&subtable, start_base + 2 * i),
            read_i16(&subtable, delta_base + 2 * i),
            read_u16(&subtable, range_base + 2 * i),
        ) else {
            return Cmap::None;
        };
        // この segment の idRangeOffset フィールド自身の subtable 内オフセット。
        let range_offset_pos = range_base + 2 * i;
        segments.push(Format4Segment {
            end_code,
            start_code,
            id_delta,
            id_range_offset,
            range_offset_pos,
        });
    }

    // endCode 昇順前提（spec で保証）。念のためソート。
    segments.sort_by_key(|s| s.end_code);
    Cmap::Format4 { segments, subtable }
}

impl std::fmt::Debug for TrueTypeFont {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrueTypeFont")
            .field("post_script_name", &self.post_script_name)
            .field("num_glyphs", &self.num_glyphs)
            .field("units_per_em", &self.units_per_em)
            .finish()
    }
}

/// `PdfError::Font` を作る補助。
pub(crate) fn font_err(msg: impl Into<String>) -> PdfError {
    PdfError::Font(msg.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// big-endian u16/u32 を Vec へ追記する補助。
    fn push_u16(v: &mut Vec<u8>, x: u16) {
        v.extend_from_slice(&x.to_be_bytes());
    }
    fn push_u32(v: &mut Vec<u8>, x: u32) {
        v.extend_from_slice(&x.to_be_bytes());
    }
    fn push_i16(v: &mut Vec<u8>, x: i16) {
        v.extend_from_slice(&x.to_be_bytes());
    }

    /// 最小限の有効な sfnt を組み立てる。
    /// cmap は format 4 で 'A'(0x41) → GID 1 をマップ。
    fn build_synthetic_font() -> Vec<u8> {
        // 各テーブルのバイト列を先に作る。
        // --- head (54 バイト) ---
        let mut head = vec![0u8; 54];
        // unitsPerEm @18 = 1000。
        head[18..20].copy_from_slice(&1000u16.to_be_bytes());
        // bbox @36..44。
        head[36..38].copy_from_slice(&(-100i16).to_be_bytes()); // xMin
        head[38..40].copy_from_slice(&(-200i16).to_be_bytes()); // yMin
        head[40..42].copy_from_slice(&(800i16).to_be_bytes()); // xMax
        head[42..44].copy_from_slice(&(900i16).to_be_bytes()); // yMax
                                                               // indexToLocFormat @50 = 0 (short)。
        head[50..52].copy_from_slice(&0i16.to_be_bytes());

        // --- maxp (32 バイト) numGlyphs @4 = 3 ---
        let mut maxp = vec![0u8; 32];
        maxp[0..4].copy_from_slice(&0x0001_0000u32.to_be_bytes()); // version
        maxp[4..6].copy_from_slice(&3u16.to_be_bytes()); // numGlyphs

        // --- hhea (36 バイト) ---
        let mut hhea = vec![0u8; 36];
        hhea[4..6].copy_from_slice(&800i16.to_be_bytes()); // ascender
        hhea[6..8].copy_from_slice(&(-200i16).to_be_bytes()); // descender
        hhea[34..36].copy_from_slice(&2u16.to_be_bytes()); // numberOfHMetrics = 2

        // --- hmtx: numberOfHMetrics(2) longHorMetric + (3-2)=1 lsb ---
        // gid0: aw=500, gid1: aw=600。 gid2 は最後の aw(600) を使う。
        let mut hmtx = Vec::new();
        push_u16(&mut hmtx, 500);
        push_i16(&mut hmtx, 10);
        push_u16(&mut hmtx, 600);
        push_i16(&mut hmtx, 20);
        push_i16(&mut hmtx, 30); // gid2 の lsb のみ。

        // --- glyf: gid0 空, gid1 に 4 バイト, gid2 空 ---
        let glyf: Vec<u8> = vec![0xDE, 0xAD, 0xBE, 0xEF];
        // loca (short, numGlyphs+1 = 4 エントリ、値は実バイト/2)。
        // offsets: gid0=0, gid1=0(空), gid2=4, gid3=4。
        // 実バイト: 0,0,4,4 → /2 = 0,0,2,2。
        let mut loca = Vec::new();
        push_u16(&mut loca, 0); // gid0 start
        push_u16(&mut loca, 0); // gid1 start (gid0 empty)
        push_u16(&mut loca, 2); // gid2 start (gid1 = 4 bytes)
        push_u16(&mut loca, 2); // end (gid2 empty)

        // --- cmap: format 4, 'A'(0x41)→GID1 ---
        // 2 segments: [0x41..0x41] と終端 [0xFFFF..0xFFFF]。
        let mut cmap = Vec::new();
        push_u16(&mut cmap, 0); // version
        push_u16(&mut cmap, 1); // numTables
        push_u16(&mut cmap, 3); // platformID = 3
        push_u16(&mut cmap, 1); // encodingID = 1
        push_u32(&mut cmap, 12); // offset (header 4 + record 8 = 12)
        let sub_start = cmap.len();
        // format 4 subtable。
        let seg_count = 2u16;
        let seg_count_x2 = seg_count * 2;
        // length は後で埋める。
        push_u16(&mut cmap, 4); // format
        let length_pos = cmap.len();
        push_u16(&mut cmap, 0); // length placeholder
        push_u16(&mut cmap, 0); // language
        push_u16(&mut cmap, seg_count_x2); // segCountX2
        push_u16(&mut cmap, 0); // searchRange
        push_u16(&mut cmap, 0); // entrySelector
        push_u16(&mut cmap, 0); // rangeShift
                                // endCode[2]。
        push_u16(&mut cmap, 0x41);
        push_u16(&mut cmap, 0xFFFF);
        // reservedPad。
        push_u16(&mut cmap, 0);
        // startCode[2]。
        push_u16(&mut cmap, 0x41);
        push_u16(&mut cmap, 0xFFFF);
        // idDelta[2]: 'A'(0x41) → GID1 なので delta = 1 - 0x41 = -0x40。
        push_i16(&mut cmap, -0x40);
        push_i16(&mut cmap, 1); // 終端セグメントの delta（0xFFFF+1 = 0 を回避; missingGlyph）
                                // idRangeOffset[2] = 0,0。
        push_u16(&mut cmap, 0);
        push_u16(&mut cmap, 0);
        // length を埋める。
        let sub_len = (cmap.len() - sub_start) as u16;
        cmap[length_pos..length_pos + 2].copy_from_slice(&sub_len.to_be_bytes());

        // --- name: nameID 6 = "TestFont" (platform 3, UTF-16BE) ---
        let mut name = Vec::new();
        let ps = "TestFont";
        // UTF-16BE バイト列。
        let mut ps_utf16 = Vec::new();
        for ch in ps.chars() {
            push_u16(&mut ps_utf16, ch as u16);
        }
        push_u16(&mut name, 0); // format
        push_u16(&mut name, 1); // count
        let string_offset_pos = name.len();
        push_u16(&mut name, 0); // stringOffset placeholder
                                // record。
        push_u16(&mut name, 3); // platformID
        push_u16(&mut name, 1); // encodingID
        push_u16(&mut name, 0); // languageID
        push_u16(&mut name, 6); // nameID
        push_u16(&mut name, ps_utf16.len() as u16); // length
        push_u16(&mut name, 0); // offset (string storage 先頭から)
                                // string storage 開始位置 = 現在の name.len()。
        let string_offset = name.len() as u16;
        name[string_offset_pos..string_offset_pos + 2]
            .copy_from_slice(&string_offset.to_be_bytes());
        name.extend_from_slice(&ps_utf16);

        // --- テーブルディレクトリを組み立てる ---
        // タグ昇順は必須ではないが揃える。
        let entries: Vec<(&[u8; 4], Vec<u8>)> = vec![
            (b"cmap", cmap),
            (b"glyf", glyf),
            (b"head", head),
            (b"hhea", hhea),
            (b"hmtx", hmtx),
            (b"loca", loca),
            (b"maxp", maxp),
            (b"name", name),
        ];
        let num_tables = entries.len() as u16;

        // sfnt ヘッダ 12 バイト + レコード 16 * numTables。
        let mut out = Vec::new();
        push_u32(&mut out, 0x0001_0000); // sfntVersion
        push_u16(&mut out, num_tables);
        push_u16(&mut out, 0); // searchRange
        push_u16(&mut out, 0); // entrySelector
        push_u16(&mut out, 0); // rangeShift

        let header_size = 12 + 16 * entries.len();
        // 各テーブルのデータ開始オフセット（4 バイト境界に揃える）。
        let mut data_offset = header_size;
        let mut offsets = Vec::new();
        for (_, body) in &entries {
            offsets.push(data_offset);
            let mut len = body.len();
            // 4 バイト境界。
            if !len.is_multiple_of(4) {
                len += 4 - (len % 4);
            }
            data_offset += len;
        }

        // レコードを書き込む。
        for (i, (tag, body)) in entries.iter().enumerate() {
            out.extend_from_slice(*tag);
            push_u32(&mut out, 0); // checkSum (無視される)
            push_u32(&mut out, offsets[i] as u32);
            push_u32(&mut out, body.len() as u32);
        }
        // テーブル本体（パディング込み）。
        for (_, body) in &entries {
            out.extend_from_slice(body);
            let pad = (4 - (body.len() % 4)) % 4;
            out.resize(out.len() + pad, 0);
        }
        out
    }

    #[test]
    fn test_synthetic_font() {
        let data = build_synthetic_font();
        assert_eq!(TrueTypeFont::num_fonts(&data), 1);

        let font = TrueTypeFont::parse(data, 0).expect("parse should succeed");
        assert_eq!(font.units_per_em(), 1000);
        assert_eq!(font.num_glyphs(), 3);
        assert_eq!(font.font_bbox(), [-100, -200, 800, 900]);
        assert_eq!(font.ascent(), 800);
        assert_eq!(font.descent(), -200);
        assert!(!font.is_cff());
        assert_eq!(font.post_script_name(), "TestFont");

        // glyph_id。
        assert_eq!(font.glyph_id('A'), Some(1));
        assert_eq!(font.glyph_id('Z'), None);

        // advance_width: gid0=500, gid1=600, gid2(>=numHMetrics)→最後 600。
        assert_eq!(font.advance_width(0), 500);
        assert_eq!(font.advance_width(1), 600);
        assert_eq!(font.advance_width(2), 600);

        // glyph_data: gid0 空, gid1 = 4 バイト。
        assert_eq!(font.glyph_data(0), Some(&[][..]));
        assert_eq!(font.glyph_data(1), Some(&[0xDE, 0xAD, 0xBE, 0xEF][..]));
        // gid2 空。
        assert_eq!(font.glyph_data(2), Some(&[][..]));
        // 範囲外。
        assert_eq!(font.glyph_data(99), None);
    }

    #[test]
    fn test_malformed_inputs() {
        // 全部ゼロの 10 バイト。
        assert!(TrueTypeFont::parse(vec![0u8; 10], 0).is_err());
        // 妥当なマジックだけで残りが無い。
        let mut trunc = Vec::new();
        trunc.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        trunc.extend_from_slice(&50u16.to_be_bytes()); // numTables = 50 だが本体無し
        assert!(TrueTypeFont::parse(trunc, 0).is_err());

        // num_fonts。
        assert_eq!(TrueTypeFont::num_fonts(b"junk"), 0);
        assert_eq!(TrueTypeFont::num_fonts(&[]), 0);
        assert_eq!(TrueTypeFont::num_fonts(&[0, 1, 0]), 0);

        // ランダム/切り詰めバッファでも panic しない。
        for seed in 0..32u32 {
            let len = (seed as usize * 7) % 200;
            let buf: Vec<u8> = (0..len)
                .map(|i| ((i as u32).wrapping_mul(seed)) as u8)
                .collect();
            let _ = TrueTypeFont::num_fonts(&buf);
            let _ = TrueTypeFont::parse(buf, 0); // Err か Ok、いずれも panic しなければよい
        }

        // ttcf ヘッダだが offset が範囲外。
        let mut ttc = Vec::new();
        ttc.extend_from_slice(b"ttcf");
        ttc.extend_from_slice(&0x0002_0000u32.to_be_bytes()); // version
        ttc.extend_from_slice(&1u32.to_be_bytes()); // numFonts
        ttc.extend_from_slice(&0xFFFF_FFFFu32.to_be_bytes()); // offset[0] 範囲外
        assert!(TrueTypeFont::parse(ttc, 0).is_err());
    }

    #[test]
    fn test_arial() {
        let path = "C:\\Windows\\Fonts\\arial.ttf";
        let Ok(data) = std::fs::read(path) else {
            eprintln!("skip test_arial: {path} not found");
            return;
        };
        let font = TrueTypeFont::parse(data, 0).expect("arial parse");
        assert!(font.units_per_em() > 0);
        let gid_a = font.glyph_id('A');
        assert!(gid_a.is_some());
        assert!(font.advance_width(gid_a.unwrap()) > 0);
        assert!(font.post_script_name().to_lowercase().contains("arial"));
        assert!(!font.is_cff());
    }

    #[test]
    fn test_msgothic_ttc() {
        let path = "C:\\Windows\\Fonts\\msgothic.ttc";
        let Ok(data) = std::fs::read(path) else {
            eprintln!("skip test_msgothic_ttc: {path} not found");
            return;
        };
        assert!(TrueTypeFont::num_fonts(&data) >= 2);
        let font = TrueTypeFont::parse(data, 0).expect("msgothic parse");
        let gid_a = font.glyph_id('あ');
        assert!(gid_a.is_some());
        assert!(font.glyph_id('漢').is_some());
        // あ のグリフデータは Some であるべき。
        assert!(font.glyph_data(gid_a.unwrap()).is_some());
    }
}
