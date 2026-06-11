//! ビューワー機能（描画以外）: しおり・リンク注釈・宛先解決・ページラベル。
//!
//! PDF の「インタラクティブ機能」（仕様 §12）のうち、ビューワーのナビゲーション
//! に必要な読み取り API を提供する。
//!
//! - しおり（/Outlines ツリー）: [`Document::outlines`]
//! - リンク注釈（/Annots の Link）: [`Document::page_links`]
//! - ページラベル（/PageLabels）: [`Document::page_label`]
//!
//! 宛先（Destination）は明示配列・名前付き宛先（古典 /Dests 辞書と
//! /Names /Dests 名前ツリーの両方）を解決し、ページ参照は 0 始まりの
//! ページ番号へ変換する。壊れた構造は読み飛ばして得られた分だけ返す
//! （ライブラリ全体の耐故障性方針と同じ）。

use std::collections::{HashMap, HashSet};

use crate::document::{decode_text_string, Document};
use crate::error::Result;
use crate::object::{Dictionary, Object};

/// 文書内の移動先（宛先）。
///
/// 明示宛先配列 `[page /XYZ left top zoom]` 等を解決した結果。
/// `/Fit` 系など座標を持たない宛先では `x`/`y`/`zoom` は `None` になる。
#[derive(Debug, Clone, PartialEq)]
pub struct Destination {
    /// 移動先ページ（0 始まり）。ページ参照が解決できなければ `None`。
    pub page_index: Option<usize>,
    /// 表示位置の X 座標（ユーザー空間。`/XYZ` `/FitV` `/FitR` で設定）。
    pub x: Option<f64>,
    /// 表示位置の Y 座標（ユーザー空間。`/XYZ` `/FitH` `/FitR` で設定）。
    pub y: Option<f64>,
    /// ズーム率（`/XYZ` の第 3 値。0 や null は「現状維持」なので `None`）。
    pub zoom: Option<f64>,
}

/// リンク・しおりの移動先の種別。
#[derive(Debug, Clone, PartialEq)]
pub enum LinkTarget {
    /// 文書内の宛先への移動（`/Dest` または `/GoTo` アクション）。
    Goto(Destination),
    /// 外部 URI（`/URI` アクション）。
    Uri(String),
}

/// しおり（アウトライン）の 1 項目。
#[derive(Debug, Clone)]
pub struct OutlineItem {
    /// 表題（`/Title`。テキスト文字列としてデコード済み）。
    pub title: String,
    /// 移動先。`/Dest` か `/A`（GoTo・URI）から解決する。無ければ `None`。
    pub target: Option<LinkTarget>,
    /// 子項目。
    pub children: Vec<OutlineItem>,
}

/// ページ上のリンク注釈 1 件。
#[derive(Debug, Clone)]
pub struct Link {
    /// クリック領域 `[x0, y0, x1, y1]`（ユーザー空間、正規化済み）。
    pub rect: [f64; 4],
    /// 移動先。
    pub target: LinkTarget,
}

// ---------------------------------------------------------------------------
// Document への API 実装
// ---------------------------------------------------------------------------

impl Document {
    /// しおり（/Outlines ツリー）を読み取る。
    ///
    /// しおりが無い・壊れている場合は空の `Vec` を返す。循環参照・過深は
    /// 打ち切って読めた分だけ返す。
    pub fn outlines(&self) -> Vec<OutlineItem> {
        let root = match self
            .catalog()
            .ok()
            .and_then(|c| self.dict_get(c, "Outlines"))
            .and_then(|o| o.as_dict().ok())
        {
            Some(d) => d,
            None => return Vec::new(),
        };
        let pages = self.page_number_map();
        let mut visited = HashSet::new();
        self.outline_children(root, &pages, &mut visited, 0)
    }

    /// `node` の `/First` → `/Next` チェーンを辿って子項目列を作る。
    fn outline_children(
        &self,
        node: &Dictionary,
        pages: &HashMap<u32, usize>,
        visited: &mut HashSet<u32>,
        depth: usize,
    ) -> Vec<OutlineItem> {
        let mut out = Vec::new();
        if depth > 32 {
            return out; // 過深は打ち切り
        }
        let mut cur = node.get("First").and_then(|o| o.as_reference().ok());
        while let Some(id) = cur {
            // 同じ項目を二度訪れたら循環なので打ち切る。
            if !visited.insert(id.0) {
                break;
            }
            let dict = match self.get_object(id).ok().and_then(|o| o.as_dict().ok()) {
                Some(d) => d,
                None => break,
            };
            let title = match self.dict_get(dict, "Title") {
                Some(Object::String(s, _)) => decode_text_string(s),
                _ => String::new(),
            };
            let target = self.item_target(dict, pages);
            let children = self.outline_children(dict, pages, visited, depth + 1);
            out.push(OutlineItem {
                title,
                target,
                children,
            });
            cur = dict.get("Next").and_then(|o| o.as_reference().ok());
        }
        out
    }

    /// ページのリンク注釈を読み取る（`index` は 0 始まり）。
    ///
    /// `/Annots` のうち `/Subtype /Link` で移動先が解決できたものだけ返す。
    pub fn page_links(&self, index: usize) -> Result<Vec<Link>> {
        let page_id = self.page_id(index)?;
        let dict = self.get_object(page_id)?.as_dict()?;
        let annots = match self.dict_get(dict, "Annots") {
            Some(Object::Array(a)) => a.clone(),
            _ => return Ok(Vec::new()),
        };
        let pages = self.page_number_map();
        let mut out = Vec::new();
        for a in &annots {
            let ad = match self.resolve(a).as_dict() {
                Ok(d) => d,
                Err(_) => continue,
            };
            if ad.get("Subtype").and_then(|o| o.as_name().ok()) != Some("Link") {
                continue;
            }
            let rect = match self.rect_from(ad.get("Rect")) {
                Some(r) => r,
                None => continue,
            };
            if let Some(target) = self.item_target(ad, &pages) {
                out.push(Link { rect, target });
            }
        }
        Ok(out)
    }

    /// ページラベル（/PageLabels）を解決して返す（`index` は 0 始まり）。
    ///
    /// `/PageLabels` が無い場合は仕様の既定どおり 1 始まりの 10 進数字列
    /// （`"1"` `"2"` …）を返す。
    pub fn page_label(&self, index: usize) -> String {
        let ranges = self.page_label_ranges();
        // index 以下で最大の開始番号を持つ範囲を探す。
        let mut best: Option<&(usize, Dictionary)> = None;
        for r in &ranges {
            if r.0 <= index && best.map(|b| b.0 <= r.0).unwrap_or(true) {
                best = Some(r);
            }
        }
        match best {
            Some((start, dict)) => self.format_page_label(dict, index - start),
            None => (index + 1).to_string(),
        }
    }

    /// 全ページのラベルを文書順で返す（[`Document::page_label`] の一括版）。
    pub fn page_labels(&self) -> Vec<String> {
        (0..self.page_count()).map(|i| self.page_label(i)).collect()
    }

    // --- 内部ヘルパ ---------------------------------------------------------

    /// オブジェクト番号 → ページ番号（0 始まり）の対応表を作る。
    ///
    /// 宛先配列のページ参照は世代番号が揺れることがあるため番号のみで引く。
    fn page_number_map(&self) -> HashMap<u32, usize> {
        self.pages()
            .iter()
            .enumerate()
            .map(|(i, id)| (id.0, i))
            .collect()
    }

    /// しおり項目・注釈辞書から移動先を解決する（`/Dest` 優先、次に `/A`）。
    fn item_target(&self, dict: &Dictionary, pages: &HashMap<u32, usize>) -> Option<LinkTarget> {
        if let Some(dest) = dict.get("Dest") {
            if let Some(d) = self.resolve_destination(dest, pages) {
                return Some(LinkTarget::Goto(d));
            }
        }
        let action = self.dict_get(dict, "A")?.as_dict().ok()?;
        self.action_target(action, pages)
    }

    /// アクション辞書（`/A`）から移動先を解決する。GoTo と URI のみ対応。
    fn action_target(
        &self,
        action: &Dictionary,
        pages: &HashMap<u32, usize>,
    ) -> Option<LinkTarget> {
        match action.get("S").and_then(|o| o.as_name().ok()) {
            Some("GoTo") => {
                let d = action.get("D")?;
                self.resolve_destination(d, pages).map(LinkTarget::Goto)
            }
            Some("URI") => match self.dict_get(action, "URI") {
                Some(Object::String(s, _)) => {
                    Some(LinkTarget::Uri(String::from_utf8_lossy(s).into_owned()))
                }
                _ => None,
            },
            // GoToR・Launch・JavaScript 等は未対応（無視）。
            _ => None,
        }
    }

    /// 宛先オブジェクト（明示配列・名前・文字列・/D 付き辞書）を解決する。
    fn resolve_destination(
        &self,
        obj: &Object,
        pages: &HashMap<u32, usize>,
    ) -> Option<Destination> {
        self.resolve_destination_inner(obj, pages, 0)
    }

    fn resolve_destination_inner(
        &self,
        obj: &Object,
        pages: &HashMap<u32, usize>,
        depth: usize,
    ) -> Option<Destination> {
        if depth > 4 {
            return None; // 名前 → 辞書 → 名前 … の循環防止
        }
        match self.resolve(obj) {
            Object::Array(arr) => self.parse_explicit_dest(arr, pages),
            // 名前付き宛先の値が <</D [...]>> 形式の場合。
            Object::Dictionary(d) => {
                let inner = d.get("D")?;
                self.resolve_destination_inner(inner, pages, depth + 1)
            }
            Object::Name(n) => {
                let target = self.lookup_named_dest(n.as_bytes())?;
                self.resolve_destination_inner(&target, pages, depth + 1)
            }
            Object::String(s, _) => {
                let s = s.clone();
                let target = self.lookup_named_dest(&s)?;
                self.resolve_destination_inner(&target, pages, depth + 1)
            }
            _ => None,
        }
    }

    /// 明示宛先配列 `[page /XYZ left top zoom]` 等を解釈する。
    fn parse_explicit_dest(
        &self,
        arr: &[Object],
        pages: &HashMap<u32, usize>,
    ) -> Option<Destination> {
        let page_index = match arr.first() {
            // 通常はページオブジェクトへの参照。
            Some(Object::Reference(id)) => pages.get(&id.0).copied(),
            // リモート宛先由来などでページ番号（0 始まり）の場合もある。
            Some(Object::Integer(n)) if *n >= 0 => Some(*n as usize),
            _ => None,
        };
        let num = |i: usize| -> Option<f64> {
            arr.get(i)
                .map(|o| self.resolve(o))
                .and_then(|o| o.as_number().ok())
                .filter(|v| v.is_finite())
        };
        let kind = arr.get(1).and_then(|o| o.as_name().ok()).unwrap_or("");
        let (x, y, zoom) = match kind {
            "XYZ" => (num(2), num(3), num(4).filter(|&z| z > 0.0)),
            "FitH" | "FitBH" => (None, num(2), None),
            "FitV" | "FitBV" => (num(2), None, None),
            // FitR [left bottom right top]: 左上隅を表示位置とする。
            "FitR" => (num(2), num(5), None),
            // Fit・FitB・未知の種別は座標なし。
            _ => (None, None, None),
        };
        Some(Destination {
            page_index,
            x,
            y,
            zoom,
        })
    }

    /// 名前付き宛先を引く。古典 `/Dests` 辞書 → `/Names /Dests` 名前ツリーの順。
    fn lookup_named_dest(&self, key: &[u8]) -> Option<Object> {
        let catalog = self.catalog().ok()?;
        // PDF 1.1 形式: カタログ /Dests 辞書（キーは名前）。
        if let Some(dests) = self
            .dict_get(catalog, "Dests")
            .and_then(|o| o.as_dict().ok())
        {
            if let Ok(name) = std::str::from_utf8(key) {
                if let Some(v) = dests.get(name) {
                    return Some(v.clone());
                }
            }
        }
        // PDF 1.2 以降: /Names /Dests 名前ツリー（キーは文字列）。
        let tree = self
            .dict_get(catalog, "Names")
            .and_then(|o| o.as_dict().ok())
            .and_then(|n| self.dict_get(n, "Dests"))
            .and_then(|o| o.as_dict().ok())?;
        let mut visited = HashSet::new();
        self.name_tree_lookup(tree, key, &mut visited, 0)
    }

    /// 名前ツリーからキーを検索する。`/Limits` は使わず全ノードを走査する
    /// （壊れた Limits への耐性優先。ノード数は visited で抑制）。
    fn name_tree_lookup(
        &self,
        node: &Dictionary,
        key: &[u8],
        visited: &mut HashSet<u32>,
        depth: usize,
    ) -> Option<Object> {
        if depth > 32 {
            return None;
        }
        // 葉ノード: /Names [key1 val1 key2 val2 ...]
        if let Some(Object::Array(names)) = self.dict_get(node, "Names") {
            for pair in names.chunks(2) {
                if let [k, v] = pair {
                    if let Object::String(s, _) = self.resolve(k) {
                        if s.as_slice() == key {
                            return Some(v.clone());
                        }
                    }
                }
            }
        }
        // 中間ノード: /Kids
        if let Some(Object::Array(kids)) = self.dict_get(node, "Kids") {
            for kid in kids {
                if let Object::Reference(id) = kid {
                    if !visited.insert(id.0) {
                        continue; // 循環防止
                    }
                }
                if let Ok(kd) = self.resolve(kid).as_dict() {
                    if let Some(v) = self.name_tree_lookup(kd, key, visited, depth + 1) {
                        return Some(v);
                    }
                }
            }
        }
        None
    }

    /// `/Rect` 配列を `[x0, y0, x1, y1]`（x0<x1, y0<y1）へ正規化する。
    fn rect_from(&self, obj: Option<&Object>) -> Option<[f64; 4]> {
        let arr = match obj.map(|o| self.resolve(o)) {
            Some(Object::Array(a)) if a.len() == 4 => a,
            _ => return None,
        };
        let mut v = [0.0f64; 4];
        for (i, o) in arr.iter().enumerate() {
            v[i] = self.resolve(o).as_number().ok().filter(|x| x.is_finite())?;
        }
        Some([
            v[0].min(v[2]),
            v[1].min(v[3]),
            v[0].max(v[2]),
            v[1].max(v[3]),
        ])
    }

    /// `/PageLabels` 数値ツリーを `(開始ページ番号, ラベル辞書)` の列に展開する。
    fn page_label_ranges(&self) -> Vec<(usize, Dictionary)> {
        let mut out = Vec::new();
        let root = match self
            .catalog()
            .ok()
            .and_then(|c| self.dict_get(c, "PageLabels"))
            .and_then(|o| o.as_dict().ok())
        {
            Some(d) => d,
            None => return out,
        };
        let mut visited = HashSet::new();
        self.collect_number_tree(root, &mut out, &mut visited, 0);
        out
    }

    /// 数値ツリー（/Nums と /Kids）を再帰的に集める。
    fn collect_number_tree(
        &self,
        node: &Dictionary,
        out: &mut Vec<(usize, Dictionary)>,
        visited: &mut HashSet<u32>,
        depth: usize,
    ) {
        if depth > 32 {
            return;
        }
        if let Some(Object::Array(nums)) = self.dict_get(node, "Nums") {
            for pair in nums.chunks(2) {
                if let [k, v] = pair {
                    let key = match self.resolve(k).as_int() {
                        Ok(n) if n >= 0 => n as usize,
                        _ => continue,
                    };
                    if let Ok(d) = self.resolve(v).as_dict() {
                        out.push((key, d.clone()));
                    }
                }
            }
        }
        if let Some(Object::Array(kids)) = self.dict_get(node, "Kids") {
            for kid in kids {
                if let Object::Reference(id) = kid {
                    if !visited.insert(id.0) {
                        continue;
                    }
                }
                if let Ok(kd) = self.resolve(kid).as_dict() {
                    self.collect_number_tree(kd, out, visited, depth + 1);
                }
            }
        }
    }

    /// ラベル辞書 1 範囲分から、範囲先頭 `offset` 番目のラベル文字列を作る。
    fn format_page_label(&self, dict: &Dictionary, offset: usize) -> String {
        let prefix = match self.dict_get(dict, "P") {
            Some(Object::String(s, _)) => decode_text_string(s),
            _ => String::new(),
        };
        let start = self
            .dict_get(dict, "St")
            .and_then(|o| o.as_int().ok())
            .filter(|&n| n >= 1)
            .unwrap_or(1) as usize;
        let n = start.saturating_add(offset);
        let numeral = match dict.get("S").and_then(|o| o.as_name().ok()) {
            Some("D") => n.to_string(),
            Some("R") => to_roman(n),
            Some("r") => to_roman(n).to_lowercase(),
            Some("A") => to_letters(n),
            Some("a") => to_letters(n).to_lowercase(),
            // /S 無し: 接頭辞のみ（仕様 §12.4.2）。
            _ => String::new(),
        };
        prefix + &numeral
    }
}

// ---------------------------------------------------------------------------
// 数字スタイル変換
// ---------------------------------------------------------------------------

/// 1 以上の整数を大文字ローマ数字へ変換する（4000 以上は M を並べる）。
fn to_roman(mut n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    const TABLE: [(usize, &str); 13] = [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    let mut out = String::new();
    for &(v, s) in &TABLE {
        while n >= v {
            out.push_str(s);
            n -= v;
            if out.len() > 256 {
                return out; // 異常に大きい値の暴走防止
            }
        }
    }
    out
}

/// 1 以上の整数を A, B, …, Z, AA, BB, …（仕様 §12.4.2 の繰り返し方式）へ変換する。
fn to_letters(n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    let letter = (b'A' + ((n - 1) % 26) as u8) as char;
    let repeat = ((n - 1) / 26 + 1).min(64); // 異常値の暴走防止
    std::iter::repeat_n(letter, repeat).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::StringFormat;

    #[test]
    fn roman_numerals() {
        assert_eq!(to_roman(1), "I");
        assert_eq!(to_roman(4), "IV");
        assert_eq!(to_roman(9), "IX");
        assert_eq!(to_roman(14), "XIV");
        assert_eq!(to_roman(1994), "MCMXCIV");
    }

    #[test]
    fn letter_labels() {
        assert_eq!(to_letters(1), "A");
        assert_eq!(to_letters(26), "Z");
        assert_eq!(to_letters(27), "AA");
        assert_eq!(to_letters(53), "AAA");
    }

    /// カタログを可変で取得するテスト用ヘルパ。
    fn catalog_mut(doc: &mut Document) -> &mut Dictionary {
        let root = doc
            .trailer
            .get("Root")
            .and_then(|o| o.as_reference().ok())
            .unwrap();
        doc.get_object_mut(root).unwrap().as_dict_mut().unwrap()
    }

    #[test]
    fn page_labels_with_ranges() {
        let mut doc = Document::new();
        for _ in 0..5 {
            doc.add_page(100.0, 100.0).unwrap();
        }
        // 0-1: 小文字ローマ数字、2-: "A-" 接頭辞 + 10 始まり 10 進。
        let mut r1 = Dictionary::new();
        r1.set("S", Object::name("r"));
        let mut r2 = Dictionary::new();
        r2.set("S", Object::name("D"));
        r2.set("St", 10);
        r2.set("P", Object::String(b"A-".to_vec(), StringFormat::Literal));
        let mut labels = Dictionary::new();
        labels.set(
            "Nums",
            Object::Array(vec![
                0.into(),
                Object::Dictionary(r1),
                2.into(),
                Object::Dictionary(r2),
            ]),
        );
        catalog_mut(&mut doc).set("PageLabels", labels);

        assert_eq!(doc.page_labels(), vec!["i", "ii", "A-10", "A-11", "A-12"]);
    }

    #[test]
    fn page_label_default_is_decimal() {
        let mut doc = Document::new();
        doc.add_page(100.0, 100.0).unwrap();
        doc.add_page(100.0, 100.0).unwrap();
        assert_eq!(doc.page_label(0), "1");
        assert_eq!(doc.page_label(1), "2");
    }

    #[test]
    fn outlines_tree_with_children() {
        let mut doc = Document::new();
        let p0 = doc.add_page(100.0, 100.0).unwrap();
        let p1 = doc.add_page(100.0, 100.0).unwrap();

        // 子: "1.1" → page 1 (Fit)
        let mut child = Dictionary::new();
        child.set("Title", Object::string_literal("1.1"));
        child.set(
            "Dest",
            Object::Array(vec![Object::Reference(p1), Object::name("Fit")]),
        );
        let child_id = doc.add_object(child);

        // 項目 1: "Chapter 1" → page 0 /XYZ
        let mut item1 = Dictionary::new();
        item1.set("Title", Object::string_literal("Chapter 1"));
        item1.set(
            "Dest",
            Object::Array(vec![
                Object::Reference(p0),
                Object::name("XYZ"),
                Object::Null,
                792.into(),
                Object::Null,
            ]),
        );
        item1.set("First", Object::Reference(child_id));
        item1.set("Last", Object::Reference(child_id));
        let item1_id = doc.add_object(item1);

        // 項目 2: URI アクション
        let mut action = Dictionary::new();
        action.set("S", Object::name("URI"));
        action.set("URI", Object::string_literal("https://example.com/"));
        let mut item2 = Dictionary::new();
        item2.set("Title", Object::string_literal("Site"));
        item2.set("A", Object::Dictionary(action));
        let item2_id = doc.add_object(item2);

        // チェーン接続とルート登録。
        if let Ok(d) = doc.get_object_mut(item1_id).and_then(|o| o.as_dict_mut()) {
            d.set("Next", Object::Reference(item2_id));
        }
        let mut root = Dictionary::new();
        root.set("Type", Object::name("Outlines"));
        root.set("First", Object::Reference(item1_id));
        root.set("Last", Object::Reference(item2_id));
        let root_id = doc.add_object(root);
        catalog_mut(&mut doc).set("Outlines", Object::Reference(root_id));

        let outlines = doc.outlines();
        assert_eq!(outlines.len(), 2);
        assert_eq!(outlines[0].title, "Chapter 1");
        match &outlines[0].target {
            Some(LinkTarget::Goto(d)) => {
                assert_eq!(d.page_index, Some(0));
                assert_eq!(d.y, Some(792.0));
                assert_eq!(d.x, None);
            }
            other => panic!("unexpected target: {other:?}"),
        }
        assert_eq!(outlines[0].children.len(), 1);
        assert_eq!(outlines[0].children[0].title, "1.1");
        match &outlines[0].children[0].target {
            Some(LinkTarget::Goto(d)) => assert_eq!(d.page_index, Some(1)),
            other => panic!("unexpected target: {other:?}"),
        }
        assert_eq!(
            outlines[1].target,
            Some(LinkTarget::Uri("https://example.com/".into()))
        );
    }

    /// 自己参照するしおりで無限ループしない。
    #[test]
    fn outline_cycle_terminates() {
        let mut doc = Document::new();
        let mut item = Dictionary::new();
        item.set("Title", Object::string_literal("loop"));
        let item_id = doc.add_object(item);
        // First も Next も自分自身を指す。
        if let Ok(d) = doc.get_object_mut(item_id).and_then(|o| o.as_dict_mut()) {
            d.set("First", Object::Reference(item_id));
            d.set("Next", Object::Reference(item_id));
        }
        let mut root = Dictionary::new();
        root.set("First", Object::Reference(item_id));
        let root_id = doc.add_object(root);
        catalog_mut(&mut doc).set("Outlines", Object::Reference(root_id));

        let outlines = doc.outlines();
        assert_eq!(outlines.len(), 1);
        assert_eq!(outlines[0].title, "loop");
    }

    #[test]
    fn link_annotations_goto_and_uri() {
        let mut doc = Document::new();
        let p0 = doc.add_page(612.0, 792.0).unwrap();
        let p1 = doc.add_page(612.0, 792.0).unwrap();

        // GoTo アクション付きリンク。
        let mut action = Dictionary::new();
        action.set("S", Object::name("GoTo"));
        action.set(
            "D",
            Object::Array(vec![
                Object::Reference(p1),
                Object::name("FitH"),
                700.into(),
            ]),
        );
        let mut link1 = Dictionary::new();
        link1.set("Subtype", Object::name("Link"));
        link1.set(
            "Rect",
            Object::Array(vec![72.into(), 700.into(), 200.into(), 720.into()]),
        );
        link1.set("A", Object::Dictionary(action));
        let link1_id = doc.add_object(link1);

        // URI リンク（Rect が逆順でも正規化される）。
        let mut action2 = Dictionary::new();
        action2.set("S", Object::name("URI"));
        action2.set("URI", Object::string_literal("https://example.com/"));
        let mut link2 = Dictionary::new();
        link2.set("Subtype", Object::name("Link"));
        link2.set(
            "Rect",
            Object::Array(vec![300.into(), 720.into(), 200.into(), 700.into()]),
        );
        link2.set("A", Object::Dictionary(action2));
        let link2_id = doc.add_object(link2);

        let page = doc.get_object_mut(p0).unwrap().as_dict_mut().unwrap();
        page.set(
            "Annots",
            Object::Array(vec![
                Object::Reference(link1_id),
                Object::Reference(link2_id),
            ]),
        );

        let links = doc.page_links(0).unwrap();
        assert_eq!(links.len(), 2);
        match &links[0].target {
            LinkTarget::Goto(d) => {
                assert_eq!(d.page_index, Some(1));
                assert_eq!(d.y, Some(700.0));
            }
            other => panic!("unexpected target: {other:?}"),
        }
        assert_eq!(links[0].rect, [72.0, 700.0, 200.0, 720.0]);
        assert_eq!(links[1].rect, [200.0, 700.0, 300.0, 720.0]);
        assert_eq!(
            links[1].target,
            LinkTarget::Uri("https://example.com/".into())
        );
        // リンクの無いページは空。
        assert!(doc.page_links(1).unwrap().is_empty());
    }

    #[test]
    fn named_destination_via_name_tree() {
        let mut doc = Document::new();
        let p0 = doc.add_page(612.0, 792.0).unwrap();

        // /Names /Dests 名前ツリー（葉 1 段）。
        let mut leaf = Dictionary::new();
        leaf.set(
            "Names",
            Object::Array(vec![
                Object::string_literal("sec1"),
                Object::Array(vec![
                    Object::Reference(p0),
                    Object::name("XYZ"),
                    10.into(),
                    20.into(),
                    Object::Real(1.5),
                ]),
            ]),
        );
        let leaf_id = doc.add_object(leaf);
        let mut tree = Dictionary::new();
        tree.set("Kids", Object::Array(vec![Object::Reference(leaf_id)]));
        let tree_id = doc.add_object(tree);
        let mut names = Dictionary::new();
        names.set("Dests", Object::Reference(tree_id));
        catalog_mut(&mut doc).set("Names", names);

        // 文字列宛先のリンク。
        let mut link = Dictionary::new();
        link.set("Subtype", Object::name("Link"));
        link.set(
            "Rect",
            Object::Array(vec![0.into(), 0.into(), 10.into(), 10.into()]),
        );
        link.set("Dest", Object::string_literal("sec1"));
        let link_id = doc.add_object(link);
        let page = doc.get_object_mut(p0).unwrap().as_dict_mut().unwrap();
        page.set("Annots", Object::Array(vec![Object::Reference(link_id)]));

        let links = doc.page_links(0).unwrap();
        assert_eq!(links.len(), 1);
        match &links[0].target {
            LinkTarget::Goto(d) => {
                assert_eq!(d.page_index, Some(0));
                assert_eq!(d.x, Some(10.0));
                assert_eq!(d.y, Some(20.0));
                assert_eq!(d.zoom, Some(1.5));
            }
            other => panic!("unexpected target: {other:?}"),
        }
    }

    /// 古典 /Dests 辞書（PDF 1.1）経由の名前付き宛先。
    #[test]
    fn named_destination_via_classic_dests() {
        let mut doc = Document::new();
        let p0 = doc.add_page(612.0, 792.0).unwrap();
        let mut dests = Dictionary::new();
        // 値が <</D [...]>> 形式。
        let mut val = Dictionary::new();
        val.set(
            "D",
            Object::Array(vec![Object::Reference(p0), Object::name("Fit")]),
        );
        dests.set("intro", Object::Dictionary(val));
        catalog_mut(&mut doc).set("Dests", dests);

        let mut item = Dictionary::new();
        item.set("Title", Object::string_literal("Intro"));
        item.set("Dest", Object::name("intro"));
        let item_id = doc.add_object(item);
        let mut root = Dictionary::new();
        root.set("First", Object::Reference(item_id));
        let root_id = doc.add_object(root);
        catalog_mut(&mut doc).set("Outlines", Object::Reference(root_id));

        let outlines = doc.outlines();
        assert_eq!(outlines.len(), 1);
        match &outlines[0].target {
            Some(LinkTarget::Goto(d)) => assert_eq!(d.page_index, Some(0)),
            other => panic!("unexpected target: {other:?}"),
        }
    }
}
