//! PDF ドキュメントの読み込み・編集・保存。
//!
//! [`Document`] が本ライブラリの中心 API。ファイル全体をメモリに読み込み、
//! すべての間接オブジェクトを `(番号, 世代)` → [`Object`] のマップとして
//! 保持する。編集後は [`Document::save`] で完全書き直し（非増分更新）の
//! PDF を出力する。

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

use crate::content::{write_content, Operation};
use crate::error::{PdfError, Result};
use crate::font::StandardFont;
use crate::object::{Dictionary, Object, ObjectId, Stream, StringFormat};
use crate::parser::Parser;
use crate::xref::{self, Xref, XrefEntry};

static NULL_OBJ: Object = Object::Null;

/// 文書に埋め込まれた TrueType フォントのハンドル。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbeddedFontId(pub(crate) usize);

/// 埋め込みフォント 1 書体分の状態。
#[derive(Debug)]
struct EmbeddedFont {
    font: crate::truetype::TrueTypeFont,
    /// ページから参照される /Font オブジェクト（Type0）。
    type0_id: ObjectId,
    descendant_id: ObjectId,
    descriptor_id: ObjectId,
    fontfile_id: ObjectId,
    tounicode_id: ObjectId,
    /// gid → 代表文字（描画に使われたグリフの記録）。
    used: BTreeMap<u16, char>,
}

/// メモリ上の PDF ドキュメント。
#[derive(Debug)]
pub struct Document {
    /// PDF バージョン（`"1.7"` など）。
    pub version: String,
    /// すべての間接オブジェクト。
    pub objects: BTreeMap<ObjectId, Object>,
    /// トレーラ辞書（`/Root` `/Info` などを保持。`/Size` 等は保存時に再生成）。
    pub trailer: Dictionary,
    next_id: u32,
    /// 埋め込み TrueType フォントのリスト。
    embedded_fonts: Vec<EmbeddedFont>,
}

// ---------------------------------------------------------------------------
// 生成・読み込み
// ---------------------------------------------------------------------------

impl Document {
    /// 空のドキュメント（カタログ + 空のページツリー）を作る。
    pub fn new() -> Document {
        let mut doc = Document {
            version: "1.7".into(),
            objects: BTreeMap::new(),
            trailer: Dictionary::new(),
            next_id: 1,
            embedded_fonts: Vec::new(),
        };
        let mut pages = Dictionary::new();
        pages.set("Type", Object::name("Pages"));
        pages.set("Kids", Object::Array(vec![]));
        pages.set("Count", 0);
        let pages_id = doc.add_object(pages);

        let mut catalog = Dictionary::new();
        catalog.set("Type", Object::name("Catalog"));
        catalog.set("Pages", Object::Reference(pages_id));
        let catalog_id = doc.add_object(catalog);

        doc.trailer.set("Root", Object::Reference(catalog_id));
        doc
    }

    /// ファイルから読み込む。
    pub fn load(path: impl AsRef<Path>) -> Result<Document> {
        let data = std::fs::read(path)?;
        Self::from_bytes(&data)
    }

    /// バイト列から読み込む。
    ///
    /// xref が壊れている場合は全ファイル走査による再構築を試みる。
    pub fn from_bytes(data: &[u8]) -> Result<Document> {
        let version = parse_header_version(data)?;
        let primary = Xref::load(data).and_then(|x| Self::build(data, x, version.clone()));
        match primary {
            Ok(doc) => Ok(doc),
            Err(PdfError::EncryptionNotSupported) => Err(PdfError::EncryptionNotSupported),
            Err(_) => {
                let x = xref::reconstruct(data)?;
                Self::build(data, x, version)
            }
        }
    }

    /// xref に従ってすべてのオブジェクトを読み込む。
    fn build(data: &[u8], xref: Xref, version: String) -> Result<Document> {
        if xref.trailer.get("Encrypt").is_some() {
            return Err(PdfError::EncryptionNotSupported);
        }

        // /Length 間接参照の解決用: xref から番号を引いて単純オブジェクトを読む
        let length_resolver = |id: ObjectId| -> Option<i64> {
            match xref.entries.get(&id.0)? {
                XrefEntry::InFile { offset, .. } => {
                    let mut p = Parser::new_at(data, *offset);
                    let (_, obj) = p.parse_indirect_object().ok()?;
                    obj.as_int().ok()
                }
                _ => None,
            }
        };

        // 第 1 段階: ファイル直置きのオブジェクト
        let mut objects: BTreeMap<ObjectId, Object> = BTreeMap::new();
        for (&num, &entry) in &xref.entries {
            if let XrefEntry::InFile { offset, .. } = entry {
                if offset >= data.len() {
                    continue; // 範囲外エントリは無視（耐性優先）
                }
                let mut p = Parser::new_at(data, offset);
                p.length_resolver = Some(&length_resolver);
                match p.parse_indirect_object() {
                    Ok(((n, g), obj)) if n == num => {
                        objects.insert((n, g), obj);
                    }
                    // 番号不一致・解析失敗のエントリは無視（後段の再構築に任せる）
                    _ => {}
                }
            }
        }

        // 第 2 段階: オブジェクトストリーム内の圧縮オブジェクト
        let stream_nums: HashSet<u32> = xref
            .entries
            .values()
            .filter_map(|e| match e {
                XrefEntry::InStream { stream_num, .. } => Some(*stream_num),
                _ => None,
            })
            .collect();
        let mut extracted: Vec<(ObjectId, Object)> = Vec::new();
        for snum in stream_nums {
            let stream = match objects.get(&(snum, 0)) {
                Some(Object::Stream(s)) => s.clone(),
                _ => continue,
            };
            let resolve = |o: &Object| -> Object {
                match o {
                    Object::Reference(id) => objects.get(id).cloned().unwrap_or(Object::Null),
                    other => other.clone(),
                }
            };
            let decoded =
                match crate::filters::decode_stream(&stream.dict, &stream.data, Some(&resolve)) {
                    Ok(d) => d,
                    Err(_) => continue,
                };
            let n = match stream.dict.get("N").map(&resolve) {
                Some(o) => o.as_int().unwrap_or(0) as usize,
                None => 0,
            };
            let first = match stream.dict.get("First").map(resolve) {
                Some(o) => o.as_int().unwrap_or(0) as usize,
                None => 0,
            };
            // ヘッダ部: 「オブジェクト番号 オフセット」の対が N 個
            let mut header = Parser::new_at(&decoded, 0);
            let mut pairs = Vec::with_capacity(n);
            for _ in 0..n {
                let onum = match header.lexer.next_token() {
                    Ok(crate::lexer::Token::Integer(v)) if v >= 0 => v as u32,
                    _ => break,
                };
                let ooff = match header.lexer.next_token() {
                    Ok(crate::lexer::Token::Integer(v)) if v >= 0 => v as usize,
                    _ => break,
                };
                pairs.push((onum, ooff));
            }
            for (onum, ooff) in pairs {
                // この番号が確かにこのストリーム内を指しているときだけ採用
                match xref.entries.get(&onum) {
                    Some(XrefEntry::InStream { stream_num, .. }) if *stream_num == snum => {}
                    _ => continue,
                }
                if first + ooff >= decoded.len() {
                    continue;
                }
                if let Ok(obj) = Parser::new_at(&decoded, first + ooff).parse_object() {
                    extracted.push(((onum, 0), obj));
                }
            }
        }
        for (id, obj) in extracted {
            objects.insert(id, obj);
        }

        // ObjStm / XRef ストリームは保存時に再生成されるため取り除く
        objects.retain(|_, obj| {
            if let Object::Stream(s) = obj {
                if let Some(Object::Name(t)) = s.dict.get("Type") {
                    return t != "ObjStm" && t != "XRef";
                }
            }
            true
        });

        if objects.is_empty() {
            return Err(PdfError::BrokenXref("no objects could be loaded".into()));
        }

        let mut trailer = xref.trailer;
        for k in [
            "Prev",
            "XRefStm",
            "Size",
            "Type",
            "W",
            "Index",
            "Length",
            "Filter",
            "DecodeParms",
        ] {
            trailer.remove(k);
        }
        let next_id = objects.keys().map(|(n, _)| *n).max().unwrap_or(0) + 1;
        let doc = Document {
            version,
            objects,
            trailer,
            next_id,
            embedded_fonts: Vec::new(),
        };
        // 最低限の検証: カタログが引けること
        doc.catalog()?;
        Ok(doc)
    }
}

impl Default for Document {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// オブジェクトアクセス
// ---------------------------------------------------------------------------

impl Document {
    /// 新しい間接オブジェクトを登録し、その ID を返す。
    pub fn add_object(&mut self, obj: impl Into<Object>) -> ObjectId {
        let id = (self.next_id, 0);
        self.next_id += 1;
        self.objects.insert(id, obj.into());
        id
    }

    /// メモリ上の埋め込みフォント（[`load_font`](Self::load_font) で読み込んだもの）の
    /// パース済み TrueType プログラムを、その Type0 オブジェクト ID から引く。
    ///
    /// `to_bytes`/`save` 前は `/FontFile2` がまだ生成されていない（プレースホルダ
    /// が Null）ため、レンダラはこの経路でメモリ上のフォントを直接使ってグリフを
    /// 描画する。保存後に再読み込みした文書では `embedded_fonts` が空になるので
    /// `None` を返し、レンダラは `/FontFile2` から復元する。
    pub(crate) fn embedded_program_by_type0_id(
        &self,
        id: ObjectId,
    ) -> Option<&crate::truetype::TrueTypeFont> {
        self.embedded_fonts
            .iter()
            .find(|ef| ef.type0_id == id)
            .map(|ef| &ef.font)
    }

    /// ID でオブジェクトを取得する。世代番号が一致しない場合は
    /// 同じ番号の別世代も探す（壊れた PDF への耐性）。
    pub fn get_object(&self, id: ObjectId) -> Result<&Object> {
        if let Some(o) = self.objects.get(&id) {
            return Ok(o);
        }
        self.objects
            .range((id.0, 0)..=(id.0, u16::MAX))
            .next()
            .map(|(_, o)| o)
            .ok_or(PdfError::MissingObject(id.0, id.1))
    }

    /// ID でオブジェクトを可変取得する。
    pub fn get_object_mut(&mut self, id: ObjectId) -> Result<&mut Object> {
        let key = if self.objects.contains_key(&id) {
            id
        } else {
            *self
                .objects
                .range((id.0, 0)..=(id.0, u16::MAX))
                .next()
                .map(|(k, _)| k)
                .ok_or(PdfError::MissingObject(id.0, id.1))?
        };
        Ok(self.objects.get_mut(&key).unwrap())
    }

    /// 間接参照を辿って実体を返す（参照でなければそのまま返す）。
    /// 参照切れは `Object::Null` になる。
    pub fn resolve<'a>(&'a self, mut obj: &'a Object) -> &'a Object {
        for _ in 0..64 {
            match obj {
                Object::Reference(id) => match self.get_object(*id) {
                    Ok(o) => obj = o,
                    Err(_) => return &NULL_OBJ,
                },
                other => return other,
            }
        }
        &NULL_OBJ
    }

    /// 辞書 `dict` のキー `key` を取得し、間接参照も解決して返す。
    pub fn dict_get<'a>(&'a self, dict: &'a Dictionary, key: &str) -> Option<&'a Object> {
        dict.get(key)
            .map(|o| self.resolve(o))
            .filter(|o| !matches!(o, Object::Null))
    }

    /// カタログ（ルート辞書）を返す。
    pub fn catalog(&self) -> Result<&Dictionary> {
        let root = self.trailer.require("Root")?;
        self.resolve(root).as_dict()
    }

    /// ストリームの中身を `/Filter` を適用して伸長する。
    /// `/Length` や `/Filter` 内の間接参照も解決される。
    pub fn get_stream_data(&self, stream: &Stream) -> Result<Vec<u8>> {
        let resolve = |o: &Object| self.resolve(o).clone();
        crate::filters::decode_stream(&stream.dict, &stream.data, Some(&resolve))
    }
}

// ---------------------------------------------------------------------------
// ページツリー
// ---------------------------------------------------------------------------

impl Document {
    /// 全ページの ID を文書順で返す。
    pub fn pages(&self) -> Vec<ObjectId> {
        let mut out = Vec::new();
        let pages_ref = self
            .catalog()
            .ok()
            .and_then(|c| c.get("Pages").cloned())
            .and_then(|o| o.as_reference().ok());
        if let Some(root) = pages_ref {
            let mut visited = HashSet::new();
            self.walk_page_tree(root, &mut out, &mut visited, 0);
        }
        out
    }

    fn walk_page_tree(
        &self,
        node_id: ObjectId,
        out: &mut Vec<ObjectId>,
        visited: &mut HashSet<u32>,
        depth: usize,
    ) {
        if depth > 64 || !visited.insert(node_id.0) {
            return; // 循環・過深防止
        }
        let dict = match self.get_object(node_id).ok().and_then(|o| o.as_dict().ok()) {
            Some(d) => d,
            None => return,
        };
        let node_type = dict.get("Type").and_then(|o| o.as_name().ok());
        let is_pages =
            node_type == Some("Pages") || (node_type.is_none() && dict.contains_key("Kids"));
        if is_pages {
            if let Some(Object::Array(kids)) = self.dict_get(dict, "Kids") {
                let kid_ids: Vec<ObjectId> =
                    kids.iter().filter_map(|k| k.as_reference().ok()).collect();
                for kid in kid_ids {
                    self.walk_page_tree(kid, out, visited, depth + 1);
                }
            }
        } else {
            out.push(node_id);
        }
    }

    /// ページ数。
    pub fn page_count(&self) -> usize {
        self.pages().len()
    }

    /// `index`（0 始まり）番目のページ ID。
    pub fn page_id(&self, index: usize) -> Result<ObjectId> {
        let pages = self.pages();
        let count = pages.len();
        pages
            .into_iter()
            .nth(index)
            .ok_or(PdfError::PageOutOfRange { index, count })
    }

    /// ページの継承属性（`/Resources` `/MediaBox` `/Rotate` など）を、
    /// ページ自身 → 祖先の順で探して返す。
    pub fn page_attr<'a>(&'a self, page_id: ObjectId, key: &str) -> Option<&'a Object> {
        let mut current = page_id;
        for _ in 0..64 {
            let dict = self.get_object(current).ok()?.as_dict().ok()?;
            if let Some(v) = self.dict_get(dict, key) {
                return Some(v);
            }
            current = dict.get("Parent")?.as_reference().ok()?;
        }
        None
    }

    /// ページの MediaBox `[x0 y0 x1 y1]` を返す（継承解決込み。既定は Letter）。
    pub fn page_media_box(&self, page_id: ObjectId) -> [f64; 4] {
        if let Some(Object::Array(a)) = self.page_attr(page_id, "MediaBox") {
            if a.len() == 4 {
                let mut v = [0.0; 4];
                for (i, o) in a.iter().enumerate() {
                    v[i] = self.resolve(o).as_number().unwrap_or(0.0);
                }
                return v;
            }
        }
        [0.0, 0.0, 612.0, 792.0]
    }

    /// ページのコンテントストリームをすべて連結して伸長したバイト列を返す。
    pub fn page_content_bytes(&self, page_id: ObjectId) -> Result<Vec<u8>> {
        let dict = self.get_object(page_id)?.as_dict()?;
        let contents = match self.dict_get(dict, "Contents") {
            Some(c) => c,
            None => return Ok(Vec::new()),
        };
        let mut out = Vec::new();
        match contents {
            Object::Stream(s) => out = self.get_stream_data(s)?,
            Object::Array(items) => {
                for item in items {
                    if let Object::Stream(s) = self.resolve(item) {
                        out.extend_from_slice(&self.get_stream_data(s)?);
                        out.push(b'\n'); // ストリーム境界はトークン境界（§7.8.2）
                    }
                }
            }
            _ => {}
        }
        Ok(out)
    }

    /// ページの実効 `/Resources` 辞書（継承解決込み）を返す。なければ空辞書。
    pub fn page_resources(&self, page_id: ObjectId) -> Dictionary {
        match self.page_attr(page_id, "Resources") {
            Some(Object::Dictionary(d)) => d.clone(),
            _ => Dictionary::new(),
        }
    }

    /// ページのテキストを抽出する（`index` は 0 始まり）。
    pub fn extract_text(&self, index: usize) -> Result<String> {
        let page_id = self.page_id(index)?;
        crate::text::extract_page_text(self, page_id)
    }

    /// ページの位置付きテキストスパンを抽出する（`index` は 0 始まり）。
    ///
    /// テキスト選択・検索ハイライト用。1 つの表示演算（`Tj`/`TJ` 等）が
    /// 1 つの [`crate::text::TextSpan`] になり、境界箱はページのユーザー空間
    /// （原点左下・ポイント単位）で返る。改行/空白の復元は行わない
    /// （文字列としての抽出は [`extract_text`](Self::extract_text) を使う）。
    pub fn extract_text_spans(&self, index: usize) -> Result<Vec<crate::text::TextSpan>> {
        let page_id = self.page_id(index)?;
        crate::text::extract_page_text_spans(self, page_id)
    }

    /// ページの論理サイズ（ポイント単位、`/Rotate` 反映済み）を `(幅, 高さ)` で返す。
    ///
    /// `/MediaBox` を正規化したサイズで、回転が 90/270 度なら幅と高さを
    /// 入れ替えて返す。[`render_page`](Self::render_page) を `scale` 倍で
    /// 呼んだときのデバイスピクセルサイズは、この値の `scale` 倍（切り上げ）
    /// になる。DPI 指定でレンダリングしたい場合は `scale = dpi / 72.0`
    /// （例: 144dpi → 2.0）。
    pub fn page_size(&self, index: usize) -> Result<(f64, f64)> {
        let page_id = self.page_id(index)?;
        let mb = self.page_media_box(page_id);
        let page_w = (mb[2] - mb[0]).abs().max(1.0);
        let page_h = (mb[3] - mb[1]).abs().max(1.0);
        let rotate = self.page_rotation(page_id);
        if rotate == 90 || rotate == 270 {
            Ok((page_h, page_w))
        } else {
            Ok((page_w, page_h))
        }
    }

    /// ページの `/Rotate`（継承込み）を 0/90/180/270 へ正規化して返す。
    fn page_rotation(&self, page_id: ObjectId) -> i64 {
        let rotate = self
            .page_attr(page_id, "Rotate")
            .and_then(|o| o.as_int().ok())
            .unwrap_or(0)
            .rem_euclid(360);
        (rotate / 90 * 90).rem_euclid(360) // 90 の倍数でない値は切り捨て
    }

    /// ページをラスタライズして RGBA ピクセルバッファを返す。
    ///
    /// `scale` は 72dpi を 1.0 とする拡大率（`dpi / 72.0` で換算。2.0 ≒ 144dpi）。
    /// ページの `/MediaBox` と `/Rotate`（継承込み）を反映する。塗り・線・
    /// クリップ・Form XObject・画像（XObject / インライン）・テキスト
    /// （TrueType グリフ）・注釈の外観ストリーム（`/AP` `/N`）を解釈する。
    /// テキストは埋め込み TrueType（`/FontFile2`）と非埋め込みのシステム
    /// フォント代替（`C:\Windows\Fonts`）に対応し、CFF（`.otf`）・Type1
    /// フォントは描画しない（字送りのみ）。
    ///
    /// 壊れたコンテントや未対応機能は読み飛ばして「描けるだけ描く」。
    /// コンテントの解析に失敗しても空ページ（背景白）を返し、`Err` にしない。
    ///
    /// 領域（タイル）指定・協調キャンセル・品質切替などの制御は
    /// [`render_page_with`](Self::render_page_with) を使う（本メソッドは
    /// その薄いラッパ）。
    pub fn render_page(&self, index: usize, scale: f64) -> Result<crate::render::Pixmap> {
        self.render_page_with(
            index,
            &crate::render::RenderOptions {
                scale,
                ..Default::default()
            },
        )
    }

    /// オプション指定でページをラスタライズして RGBA ピクセルバッファを返す。
    ///
    /// 描画内容と耐故障性は [`render_page`](Self::render_page) と同じ。
    /// [`crate::render::RenderOptions`] で領域（タイル）レンダリング・協調
    /// キャンセル・注釈の ON/OFF・品質切替を制御できる。キャンセルされた
    /// 場合は部分結果を返さず [`PdfError::Cancelled`] を返す。
    pub fn render_page_with(
        &self,
        index: usize,
        options: &crate::render::RenderOptions,
    ) -> Result<crate::render::Pixmap> {
        let mut pm = crate::render::Pixmap::new(1, 1);
        self.render_page_into(index, options, &mut pm)?;
        Ok(pm)
    }

    /// 既存の [`Pixmap`](crate::render::Pixmap) を出力先に再利用してページを
    /// ラスタライズする。
    ///
    /// 連続レンダリング（ズーム・スクロール時の再描画）でバッファの再確保を
    /// 避けるための [`render_page_with`](Self::render_page_with) の変種。
    /// `pm` は内部でサイズ変更（白で初期化）されるため、呼び出し前のサイズは
    /// 一致していなくてよい。`Err` を返した場合（キャンセル含む）の `pm` の
    /// 内容は未定義（部分描画が残りうる）。
    pub fn render_page_into(
        &self,
        index: usize,
        options: &crate::render::RenderOptions,
        pm: &mut crate::render::Pixmap,
    ) -> Result<()> {
        use crate::render::{Matrix, Renderer};

        // 開始前のキャンセル確認（フラグが立っていれば何も描かない）。
        if let Some(c) = &options.cancel {
            if c.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(PdfError::Cancelled);
            }
        }

        let page_id = self.page_id(index)?;

        // MediaBox を取得して x0<x1・y0<y1 に正規化。
        let mb = self.page_media_box(page_id);
        let x0 = mb[0].min(mb[2]);
        let y0 = mb[1].min(mb[3]);
        let page_w = (mb[2] - mb[0]).abs().max(1.0);
        let page_h = (mb[3] - mb[1]).abs().max(1.0);

        let rotate = self.page_rotation(page_id);

        // スケールの補正。
        let mut scale = if options.scale.is_finite() && options.scale > 0.0 {
            options.scale
        } else {
            1.0
        };

        // ピクセル数の上限ガード（長辺 10000・総面積 1 億）。
        const MAX_SIDE: f64 = 10000.0;
        const MAX_AREA: f64 = 100_000_000.0;

        // 出力ピクセルサイズと、デバイス空間での平行移動（タイルの左上）。
        let (dev_w, dev_h, tile_tx, tile_ty) = match options.region {
            None => {
                // 全面: ガードはスケールを縮めて適用する（従来挙動）。
                let (dev_w_f, dev_h_f) = if rotate == 90 || rotate == 270 {
                    (page_h * scale, page_w * scale)
                } else {
                    (page_w * scale, page_h * scale)
                };
                let longest = dev_w_f.max(dev_h_f);
                if longest > MAX_SIDE {
                    scale *= MAX_SIDE / longest;
                }
                let area = (page_w * scale) * (page_h * scale);
                if area > MAX_AREA {
                    scale *= (MAX_AREA / area).sqrt();
                }
                let (w, h) = if rotate == 90 || rotate == 270 {
                    (
                        (page_h * scale).ceil().max(1.0) as u32,
                        (page_w * scale).ceil().max(1.0) as u32,
                    )
                } else {
                    (
                        (page_w * scale).ceil().max(1.0) as u32,
                        (page_h * scale).ceil().max(1.0) as u32,
                    )
                };
                (w, h, 0.0, 0.0)
            }
            Some([rx, ry, rw, rh]) => {
                // タイル: スケールは縮めず（深いズームが目的）、タイル自体の
                // 大きさにのみガードをかける。
                if !(rx.is_finite() && ry.is_finite() && rw.is_finite() && rh.is_finite()) {
                    return Err(PdfError::Invalid("render region must be finite".into()));
                }
                if rw <= 0.0 || rh <= 0.0 {
                    return Err(PdfError::Invalid(
                        "render region must have positive size".into(),
                    ));
                }
                let mut rw = rw.min(MAX_SIDE);
                let mut rh = rh.min(MAX_SIDE);
                if rw * rh > MAX_AREA {
                    let shrink = (MAX_AREA / (rw * rh)).sqrt();
                    rw *= shrink;
                    rh *= shrink;
                }
                (rw.ceil().max(1.0) as u32, rh.ceil().max(1.0) as u32, rx, ry)
            }
        };

        pm.reset(dev_w, dev_h);

        // 基底 CTM の構成。
        //
        // 手順（点 p に作用する順）:
        //   1. MediaBox 原点を引いて左下を原点へ: translate(-x0, -y0)
        //   2. scale 倍
        //   3. y 軸反転 + 回転（左下原点 y 上向き → 左上原点 y 下向き）
        //
        // 回転は「ページを時計回りに表示」する一般的なビューワー挙動に合わせ、
        // 各回転で結果が dev_w × dev_h の第 1 象限に収まるよう平行移動を足す。
        let sw = page_w * scale; // 回転前スケール後の幅
        let sh = page_h * scale; // 回転前スケール後の高さ
        let origin = Matrix::translate(-x0, -y0).then(&Matrix::scale(scale, scale));
        let rot = match rotate {
            90 => {
                // (sx, sy) → (sh? ...) 時計回り 90。
                // x' = sh - sy 相当を行列で: a=0 b=1 c=-1 d=0 ... ではなく
                // 下記で第1象限へ収める。
                Matrix {
                    a: 0.0,
                    b: 1.0,
                    c: 1.0,
                    d: 0.0,
                    e: 0.0,
                    f: 0.0,
                }
            }
            180 => Matrix {
                a: -1.0,
                b: 0.0,
                c: 0.0,
                d: 1.0,
                e: sw,
                f: 0.0,
            },
            270 => Matrix {
                a: 0.0,
                b: -1.0,
                c: -1.0,
                d: 0.0,
                e: sh,
                f: sw,
            },
            _ => Matrix {
                // 回転 0: y 反転のみ（x はそのまま、y は高さから引く）。
                a: 1.0,
                b: 0.0,
                c: 0.0,
                d: -1.0,
                e: 0.0,
                f: sh,
            },
        };
        let base_ctm = origin.then(&rot);

        // タイル指定なら基底 CTM の後段にタイル左上への平行移動を合成する。
        // 全面レンダ結果から同領域を切り出したものとピクセル一致する。
        let base_ctm = if options.region.is_some() {
            base_ctm.then(&Matrix::translate(-tile_tx, -tile_ty))
        } else {
            base_ctm
        };

        // コンテントを解析して描画。解析失敗時も注釈は描く（描けるだけ描く）。
        let resources = self.page_resources(page_id);
        let mut renderer = Renderer::new(self, pm, base_ctm);
        renderer.set_cancel_flag(options.cancel.clone());
        renderer.set_quality(options.quality);
        if let Ok(bytes) = self.page_content_bytes(page_id) {
            if let Ok(ops) = crate::content::parse_content(&bytes) {
                renderer.run(&ops, &resources);
            }
        }
        if renderer.is_cancelled() {
            return Err(PdfError::Cancelled);
        }

        // 注釈の外観ストリーム（/AP /N）をページ内容の上に描画する。
        if options.annotations {
            let annots: Vec<Dictionary> = match self
                .get_object(page_id)
                .ok()
                .and_then(|o| o.as_dict().ok())
                .and_then(|d| self.dict_get(d, "Annots"))
            {
                Some(Object::Array(a)) => a
                    .iter()
                    .filter_map(|o| self.resolve(o).as_dict().ok().cloned())
                    .collect(),
                _ => Vec::new(),
            };
            for annot in &annots {
                if renderer.is_cancelled() {
                    break;
                }
                renderer.draw_annotation(annot, &resources);
            }
        }
        let cancelled = renderer.is_cancelled();
        drop(renderer);
        if cancelled {
            return Err(PdfError::Cancelled);
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// ページ編集
// ---------------------------------------------------------------------------

impl Document {
    /// 新しい空ページを末尾に追加する。`width` x `height` はポイント単位
    /// （A4 = 595 x 842, Letter = 612 x 792）。追加したページの ID を返す。
    pub fn add_page(&mut self, width: f64, height: f64) -> Result<ObjectId> {
        let pages_id = self.catalog()?.require("Pages")?.as_reference()?;
        let mut page = Dictionary::new();
        page.set("Type", Object::name("Page"));
        page.set("Parent", Object::Reference(pages_id));
        page.set(
            "MediaBox",
            Object::Array(vec![0.into(), 0.into(), width.into(), height.into()]),
        );
        page.set("Resources", Dictionary::new());
        let page_id = self.add_object(page);

        let pages_node = self.get_object_mut(pages_id)?.as_dict_mut()?;
        match pages_node.get_mut("Kids") {
            Some(Object::Array(kids)) => kids.push(Object::Reference(page_id)),
            _ => pages_node.set("Kids", Object::Array(vec![Object::Reference(page_id)])),
        }
        let count = pages_node
            .get("Count")
            .and_then(|o| o.as_int().ok())
            .unwrap_or(0);
        pages_node.set("Count", count + 1);
        Ok(page_id)
    }

    /// ページを削除する（`index` は 0 始まり）。
    pub fn remove_page(&mut self, index: usize) -> Result<()> {
        let page_id = self.page_id(index)?;
        let parent_id = self
            .get_object(page_id)?
            .as_dict()?
            .get("Parent")
            .and_then(|o| o.as_reference().ok())
            .ok_or(PdfError::Invalid("page has no /Parent".into()))?;

        // 親の Kids から取り除く
        let parent = self.get_object_mut(parent_id)?.as_dict_mut()?;
        if let Some(Object::Array(kids)) = parent.get_mut("Kids") {
            kids.retain(|k| k.as_reference().ok() != Some(page_id));
        }
        // 祖先すべての Count を 1 減らす
        let mut current = Some(parent_id);
        let mut guard = 0;
        while let Some(node_id) = current {
            guard += 1;
            if guard > 64 {
                break;
            }
            let node = self.get_object_mut(node_id)?.as_dict_mut()?;
            let count = node.get("Count").and_then(|o| o.as_int().ok()).unwrap_or(1);
            node.set("Count", (count - 1).max(0));
            current = node.get("Parent").and_then(|o| o.as_reference().ok());
        }
        self.objects.remove(&page_id);
        Ok(())
    }

    /// ページを回転する。`degrees` は 90 の倍数（時計回り）。既存の回転に加算される。
    pub fn rotate_page(&mut self, index: usize, degrees: i64) -> Result<()> {
        if degrees % 90 != 0 {
            return Err(PdfError::Invalid(
                "rotation must be a multiple of 90".into(),
            ));
        }
        let page_id = self.page_id(index)?;
        let current = self
            .page_attr(page_id, "Rotate")
            .and_then(|o| o.as_int().ok())
            .unwrap_or(0);
        let dict = self.get_object_mut(page_id)?.as_dict_mut()?;
        dict.set("Rotate", (current + degrees).rem_euclid(360));
        Ok(())
    }

    /// ページ末尾にコンテントストリームを 1 本追加する。
    ///
    /// 既存コンテンツのグラフィックス状態の影響を避けたい場合は
    /// 呼び出し側で `q`/`Q` や `BT`/`ET` で囲むこと（本ライブラリの
    /// 高レベル API は囲み済みの演算列を生成する）。
    pub fn append_content(&mut self, index: usize, ops: &[Operation]) -> Result<()> {
        let bytes = write_content(ops);
        self.append_content_bytes(index, bytes)
    }

    /// ページ末尾に生のコンテントストリームを追加する（低レベル API）。
    pub fn append_content_bytes(&mut self, index: usize, bytes: Vec<u8>) -> Result<()> {
        let page_id = self.page_id(index)?;
        let stream_id = self.add_object(Object::Stream(Stream::new(Dictionary::new(), bytes)));

        // 既存の /Contents を配列に正規化して追記
        let existing = {
            let dict = self.get_object(page_id)?.as_dict()?;
            dict.get("Contents").cloned()
        };
        let new_contents = match existing {
            None | Some(Object::Null) => Object::Array(vec![Object::Reference(stream_id)]),
            Some(Object::Array(mut a)) => {
                a.push(Object::Reference(stream_id));
                Object::Array(a)
            }
            Some(Object::Reference(rid)) => {
                // 参照先が配列か単一ストリームかで分岐
                match self.get_object(rid) {
                    Ok(Object::Array(a)) => {
                        let mut a = a.clone();
                        a.push(Object::Reference(stream_id));
                        Object::Array(a)
                    }
                    _ => Object::Array(vec![Object::Reference(rid), Object::Reference(stream_id)]),
                }
            }
            Some(other) => other, // 不正な形は触らない
        };
        let dict = self.get_object_mut(page_id)?.as_dict_mut()?;
        dict.set("Contents", new_contents);
        Ok(())
    }

    /// ページに標準 14 フォントのリソースを登録し、リソース名（`F1` など）を返す。
    /// 同じフォントが登録済みならその名前を返す。
    pub fn ensure_standard_font(&mut self, index: usize, font: StandardFont) -> Result<String> {
        let page_id = self.page_id(index)?;
        // 実効リソースを直接辞書としてページに持たせる（継承・間接参照を解消）
        let mut resources = self.page_resources(page_id);
        let mut fonts = match resources.get("Font").map(|o| self.resolve(o)) {
            Some(Object::Dictionary(d)) => d.clone(),
            _ => Dictionary::new(),
        };

        // 既存エントリの再利用
        for (name, value) in fonts.iter() {
            if let Ok(fd) = self.resolve(value).as_dict() {
                if fd.get("BaseFont").and_then(|o| o.as_name().ok()) == Some(font.base_font()) {
                    return Ok(name.to_string());
                }
            }
        }

        // 新規登録
        let mut fdict = Dictionary::new();
        fdict.set("Type", Object::name("Font"));
        fdict.set("Subtype", Object::name("Type1"));
        fdict.set("BaseFont", Object::name(font.base_font()));
        fdict.set("Encoding", Object::name("WinAnsiEncoding"));
        let fid = self.add_object(fdict);

        let mut n = 1;
        let name = loop {
            let candidate = format!("F{n}");
            if !fonts.contains_key(&candidate) {
                break candidate;
            }
            n += 1;
        };
        fonts.set(name.clone(), Object::Reference(fid));
        resources.set("Font", fonts);
        let dict = self.get_object_mut(page_id)?.as_dict_mut()?;
        dict.set("Resources", resources);
        Ok(name)
    }

    /// TrueType フォントファイル（`.ttf` / `.ttc`）を読み込み、埋め込みフォントを登録する。
    ///
    /// TTC の場合は `ttc_index = 0` で最初の書体を選ぶ。
    /// 詳細は [`load_font_from_bytes`](Self::load_font_from_bytes) を参照。
    pub fn load_font(&mut self, path: impl AsRef<Path>) -> Result<EmbeddedFontId> {
        let data = std::fs::read(path)?;
        self.load_font_from_bytes(data, 0)
    }

    /// バイト列からフォントを読み込み、埋め込みフォントを登録する。
    ///
    /// CFF アウトライン（OpenType/CFF、`.otf`）は非対応。TrueType（glyf）専用。
    /// 返した [`EmbeddedFontId`] を [`add_text_with_font`](Self::add_text_with_font) に渡す。
    pub fn load_font_from_bytes(
        &mut self,
        data: Vec<u8>,
        ttc_index: u32,
    ) -> Result<EmbeddedFontId> {
        let font = crate::truetype::TrueTypeFont::parse(data, ttc_index)?;
        if font.is_cff() {
            return Err(PdfError::Font(
                "CFF (OpenType/CFF) outlines are not supported; use a TrueType (glyf) font".into(),
            ));
        }
        // 5 つのプレースホルダオブジェクトを確保（保存時に上書きされる）
        let type0_id = self.add_object(Object::Null);
        let descendant_id = self.add_object(Object::Null);
        let descriptor_id = self.add_object(Object::Null);
        let fontfile_id = self.add_object(Object::Null);
        let tounicode_id = self.add_object(Object::Null);
        let id = EmbeddedFontId(self.embedded_fonts.len());
        self.embedded_fonts.push(EmbeddedFont {
            font,
            type0_id,
            descendant_id,
            descriptor_id,
            fontfile_id,
            tounicode_id,
            used: BTreeMap::new(),
        });
        Ok(id)
    }

    /// 埋め込み TrueType フォントでページにテキストを描画する。複数行（`\n` 区切り）対応。
    ///
    /// `opts.font`（StandardFont）は無視され、`font` 引数で指定した埋め込みフォントが使われる。
    /// 座標系は PDF 標準（原点は左下、y 軸は上向き、単位はポイント）。
    pub fn add_text_with_font(
        &mut self,
        index: usize,
        text: &str,
        font: EmbeddedFontId,
        opts: &TextOptions,
    ) -> Result<()> {
        if font.0 >= self.embedded_fonts.len() {
            return Err(PdfError::Invalid("invalid font handle".into()));
        }
        // used マップを汚す前にページ番号を検証する
        self.page_id(index)?;

        // ボローチェッカー対策: 必要な情報を先に収集してから self を可変で使う。
        let type0_id = self.embedded_fonts[font.0].type0_id;

        // 各行のエンコード済みバイト列を生成し、used マップも更新する
        let lines: Vec<Vec<u8>> = {
            let ef = &mut self.embedded_fonts[font.0];
            text.split('\n')
                .map(|line| {
                    let mut bytes = Vec::with_capacity(line.len() * 2);
                    for c in line.chars() {
                        let gid = ef.font.glyph_id(c).unwrap_or(0);
                        if gid != 0 {
                            ef.used.insert(gid, c);
                        }
                        bytes.push((gid >> 8) as u8);
                        bytes.push((gid & 0xFF) as u8);
                    }
                    bytes
                })
                .collect()
        };

        // リソース辞書にフォントを登録し、フォント名を得る
        let font_name = self.ensure_embedded_font_resource(index, type0_id)?;

        let leading = opts.leading.unwrap_or(opts.size * 1.2);
        let (r, g, b) = opts.color;

        let mut ops = vec![
            Operation::new("q", vec![]),
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec![Object::name(font_name), opts.size.into()]),
            Operation::new("rg", vec![r.into(), g.into(), b.into()]),
            Operation::new("TL", vec![leading.into()]),
            Operation::new("Td", vec![opts.x.into(), opts.y.into()]),
        ];
        for (i, line_bytes) in lines.into_iter().enumerate() {
            if i > 0 {
                ops.push(Operation::new("T*", vec![]));
            }
            ops.push(Operation::new(
                "Tj",
                vec![Object::String(line_bytes, StringFormat::Hexadecimal)],
            ));
        }
        ops.push(Operation::new("ET", vec![]));
        ops.push(Operation::new("Q", vec![]));
        self.append_content(index, &ops)
    }

    /// 埋め込みフォントで文字列の描画幅を計算する（ポイント単位）。
    ///
    /// `\n` は幅計算上スキップされる。
    pub fn text_width(&self, font: EmbeddedFontId, text: &str, size: f64) -> f64 {
        if font.0 >= self.embedded_fonts.len() {
            return 0.0;
        }
        let ef = &self.embedded_fonts[font.0];
        let upm = ef.font.units_per_em() as f64;
        text.chars()
            .filter(|&c| c != '\n')
            .map(|c| {
                let gid = ef.font.glyph_id(c).unwrap_or(0);
                let aw = ef.font.advance_width(gid) as f64;
                aw / upm * size
            })
            .sum()
    }

    /// 埋め込みフォントをページのリソース辞書に登録し、リソース名（`F{n}`）を返す。
    ///
    /// 既に登録済みの場合はその名前を再利用する。
    fn ensure_embedded_font_resource(
        &mut self,
        index: usize,
        type0_id: ObjectId,
    ) -> Result<String> {
        let page_id = self.page_id(index)?;
        let mut resources = self.page_resources(page_id);
        let mut fonts = match resources.get("Font").map(|o| self.resolve(o)) {
            Some(Object::Dictionary(d)) => d.clone(),
            _ => Dictionary::new(),
        };

        // 既存エントリで同じ type0_id への参照があれば再利用
        for (name, value) in fonts.iter() {
            if let Object::Reference(id) = value {
                if *id == type0_id {
                    return Ok(name.to_string());
                }
            }
        }

        // 新規登録: 空き番号を探す
        let mut n = 1;
        let name = loop {
            let candidate = format!("F{n}");
            if !fonts.contains_key(&candidate) {
                break candidate;
            }
            n += 1;
        };
        fonts.set(name.clone(), Object::Reference(type0_id));
        resources.set("Font", fonts);
        let dict = self.get_object_mut(page_id)?.as_dict_mut()?;
        dict.set("Resources", resources);
        Ok(name)
    }

    /// ページにテキストを描画する。複数行（`\n` 区切り）に対応。
    ///
    /// 座標系は PDF 標準（原点は左下、y 軸は上向き、単位はポイント）。
    /// `(x, y)` は 1 行目のベースライン左端。
    pub fn add_text(&mut self, index: usize, text: &str, opts: &TextOptions) -> Result<()> {
        let font_name = self.ensure_standard_font(index, opts.font)?;
        let leading = opts.leading.unwrap_or(opts.size * 1.2);
        let (r, g, b) = opts.color;

        let mut ops = vec![
            Operation::new("q", vec![]),
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec![Object::name(font_name), opts.size.into()]),
            Operation::new("rg", vec![r.into(), g.into(), b.into()]),
            Operation::new("TL", vec![leading.into()]),
            Operation::new("Td", vec![opts.x.into(), opts.y.into()]),
        ];
        for (i, line) in text.split('\n').enumerate() {
            if i > 0 {
                ops.push(Operation::new("T*", vec![]));
            }
            let encoded: Vec<u8> = line
                .chars()
                .map(|c| crate::font::char_to_winansi(c).unwrap_or(b'?'))
                .collect();
            ops.push(Operation::new("Tj", vec![Object::string_literal(encoded)]));
        }
        ops.push(Operation::new("ET", vec![]));
        ops.push(Operation::new("Q", vec![]));
        self.append_content(index, &ops)
    }

    /// ページに直線を描画する。
    pub fn draw_line(
        &mut self,
        index: usize,
        from: (f64, f64),
        to: (f64, f64),
        opts: &DrawOptions,
    ) -> Result<()> {
        let (r, g, b) = opts.stroke_color;
        let ops = vec![
            Operation::new("q", vec![]),
            Operation::new("w", vec![opts.line_width.into()]),
            Operation::new("RG", vec![r.into(), g.into(), b.into()]),
            Operation::new("m", vec![from.0.into(), from.1.into()]),
            Operation::new("l", vec![to.0.into(), to.1.into()]),
            Operation::new("S", vec![]),
            Operation::new("Q", vec![]),
        ];
        self.append_content(index, &ops)
    }

    /// ページに矩形を描画する。`fill_color` が `Some` なら塗り潰し。
    pub fn draw_rect(
        &mut self,
        index: usize,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        opts: &DrawOptions,
    ) -> Result<()> {
        let (r, g, b) = opts.stroke_color;
        let mut ops = vec![
            Operation::new("q", vec![]),
            Operation::new("w", vec![opts.line_width.into()]),
            Operation::new("RG", vec![r.into(), g.into(), b.into()]),
        ];
        let paint = if let Some((fr, fg, fb)) = opts.fill_color {
            ops.push(Operation::new("rg", vec![fr.into(), fg.into(), fb.into()]));
            "B" // 塗り + 線
        } else {
            "S" // 線のみ
        };
        ops.push(Operation::new(
            "re",
            vec![x.into(), y.into(), width.into(), height.into()],
        ));
        ops.push(Operation::new(paint, vec![]));
        ops.push(Operation::new("Q", vec![]));
        self.append_content(index, &ops)
    }
}

/// [`Document::add_text`] のオプション。
#[derive(Debug, Clone)]
pub struct TextOptions {
    /// 使用フォント（標準 14 フォント）。
    pub font: StandardFont,
    /// フォントサイズ（ポイント）。
    pub size: f64,
    /// 1 行目ベースラインの X 座標。
    pub x: f64,
    /// 1 行目ベースラインの Y 座標。
    pub y: f64,
    /// 文字色 RGB（各 0.0–1.0）。
    pub color: (f64, f64, f64),
    /// 行送り（ポイント）。`None` なら `size * 1.2`。
    pub leading: Option<f64>,
}

impl Default for TextOptions {
    fn default() -> Self {
        TextOptions {
            font: StandardFont::Helvetica,
            size: 12.0,
            x: 72.0,
            y: 720.0,
            color: (0.0, 0.0, 0.0),
            leading: None,
        }
    }
}

/// 図形描画のオプション。
#[derive(Debug, Clone)]
pub struct DrawOptions {
    /// 線色 RGB（各 0.0–1.0）。
    pub stroke_color: (f64, f64, f64),
    /// 塗り色。`None` なら塗らない。
    pub fill_color: Option<(f64, f64, f64)>,
    /// 線幅（ポイント）。
    pub line_width: f64,
}

impl Default for DrawOptions {
    fn default() -> Self {
        DrawOptions {
            stroke_color: (0.0, 0.0, 0.0),
            fill_color: None,
            line_width: 1.0,
        }
    }
}

// ---------------------------------------------------------------------------
// メタデータ（/Info）
// ---------------------------------------------------------------------------

impl Document {
    /// 文書情報辞書（`/Info`）を解決して返す。
    pub fn info(&self) -> Option<&Dictionary> {
        let info = self.trailer.get("Info")?;
        self.resolve(info).as_dict().ok()
    }

    /// 文書情報のテキスト値（`Title` `Author` `Subject` など）を取得する。
    pub fn info_text(&self, key: &str) -> Option<String> {
        let obj = self.info()?.get(key)?;
        match self.resolve(obj) {
            Object::String(s, _) => Some(decode_text_string(s)),
            _ => None,
        }
    }

    /// 文書情報のテキスト値を設定する。`/Info` 辞書が無ければ作る。
    pub fn set_info_text(&mut self, key: impl Into<String>, value: &str) -> Result<()> {
        let encoded = encode_text_string(value);
        let info_id = match self.trailer.get("Info").and_then(|o| o.as_reference().ok()) {
            Some(id) if self.objects.contains_key(&id) => id,
            _ => {
                let id = self.add_object(Dictionary::new());
                self.trailer.set("Info", Object::Reference(id));
                id
            }
        };
        let info = self.get_object_mut(info_id)?.as_dict_mut()?;
        info.set(
            key,
            Object::String(encoded, crate::object::StringFormat::Literal),
        );
        Ok(())
    }

    /// タイトルを取得する。
    pub fn title(&self) -> Option<String> {
        self.info_text("Title")
    }

    /// タイトルを設定する。
    pub fn set_title(&mut self, title: &str) -> Result<()> {
        self.set_info_text("Title", title)
    }
}

// ---------------------------------------------------------------------------
// 保存
// ---------------------------------------------------------------------------

impl Document {
    /// ドキュメント全体を PDF バイト列にシリアライズする。
    ///
    /// 保存時に埋め込みフォントのサブセット化と関連オブジェクト生成が行われる。
    /// 複数回呼んでも冪等（毎回オブジェクトを上書き再構築）。
    pub fn to_bytes(&mut self) -> Result<Vec<u8>> {
        self.finalize_embedded_fonts()?;
        crate::writer::write_document(&self.version, &self.objects, &self.trailer)
    }

    /// ファイルに保存する（完全書き直し。増分更新ではない）。
    ///
    /// 保存時に埋め込みフォントのサブセット化と関連オブジェクト生成が行われる。
    pub fn save(&mut self, path: impl AsRef<Path>) -> Result<()> {
        std::fs::write(path, self.to_bytes()?)?;
        Ok(())
    }

    /// 埋め込みフォントごとにサブセット化を行い、関連オブジェクトを生成する。
    ///
    /// 冪等: 複数回呼ばれても既存のオブジェクトを上書きするだけで副作用はない。
    fn finalize_embedded_fonts(&mut self) -> Result<()> {
        for i in 0..self.embedded_fonts.len() {
            // 必要な情報を先に収集（ボローチェッカー対策）
            let gids: BTreeSet<u16> = self.embedded_fonts[i].used.keys().copied().collect();
            let subset = crate::subset::subset_font(&self.embedded_fonts[i].font, &gids)?;

            let ef = &self.embedded_fonts[i];
            let upm = ef.font.units_per_em() as f64;
            let s = 1000.0 / upm;

            // サブセットタグ（6 文字の大文字 ASCII）を決定論的に生成
            let tag = make_subset_tag(ef.font.post_script_name(), &gids);
            let base = format!("{tag}+{}", ef.font.post_script_name());

            let bbox = ef.font.font_bbox();
            let ascent = (ef.font.ascent() as f64 * s).round() as i64;
            let descent = (ef.font.descent() as f64 * s).round() as i64;
            let cap_height = (ef.font.cap_height() as f64 * s).round() as i64;
            let italic_angle = ef.font.italic_angle();
            let bbox_scaled: [i64; 4] = [
                (bbox[0] as f64 * s).round() as i64,
                (bbox[1] as f64 * s).round() as i64,
                (bbox[2] as f64 * s).round() as i64,
                (bbox[3] as f64 * s).round() as i64,
            ];

            // W 配列: 使用 GID の advance 幅
            let mut w_array: Vec<Object> = Vec::new();
            for &gid in ef.used.keys() {
                let aw = ef.font.advance_width(gid);
                let aw_scaled = (aw as f64 * s).round() as i64;
                w_array.push(Object::Integer(gid as i64));
                w_array.push(Object::Array(vec![Object::Integer(aw_scaled)]));
            }

            // ToUnicode CMap の内容を生成
            let tounicode_bytes = build_tounicode_cmap(&ef.used);

            // オブジェクト ID を収集
            let type0_id = ef.type0_id;
            let descendant_id = ef.descendant_id;
            let descriptor_id = ef.descriptor_id;
            let fontfile_id = ef.fontfile_id;
            let tounicode_id = ef.tounicode_id;

            // FontFile2 ストリーム
            let mut ff_dict = Dictionary::new();
            ff_dict.set("Length1", subset.len() as i64);
            let fontfile_stream = Object::Stream(Stream::new_compressed(ff_dict, &subset));
            self.objects.insert(fontfile_id, fontfile_stream);

            // FontDescriptor
            let mut desc = Dictionary::new();
            desc.set("Type", Object::name("FontDescriptor"));
            desc.set("FontName", Object::name(base.clone()));
            desc.set("Flags", Object::Integer(4));
            desc.set(
                "FontBBox",
                Object::Array(vec![
                    Object::Integer(bbox_scaled[0]),
                    Object::Integer(bbox_scaled[1]),
                    Object::Integer(bbox_scaled[2]),
                    Object::Integer(bbox_scaled[3]),
                ]),
            );
            desc.set("ItalicAngle", Object::Real(italic_angle));
            desc.set("Ascent", Object::Integer(ascent));
            desc.set("Descent", Object::Integer(descent));
            desc.set("CapHeight", Object::Integer(cap_height));
            desc.set("StemV", Object::Integer(80));
            desc.set("FontFile2", Object::Reference(fontfile_id));
            self.objects.insert(descriptor_id, Object::Dictionary(desc));

            // CIDSystemInfo 辞書
            let mut cid_system = Dictionary::new();
            cid_system.set(
                "Registry",
                Object::String(b"Adobe".to_vec(), StringFormat::Literal),
            );
            cid_system.set(
                "Ordering",
                Object::String(b"Identity".to_vec(), StringFormat::Literal),
            );
            cid_system.set("Supplement", Object::Integer(0));

            // Descendant CIDFont
            let mut cid_font = Dictionary::new();
            cid_font.set("Type", Object::name("Font"));
            cid_font.set("Subtype", Object::name("CIDFontType2"));
            cid_font.set("BaseFont", Object::name(base.clone()));
            cid_font.set("CIDSystemInfo", cid_system);
            cid_font.set("FontDescriptor", Object::Reference(descriptor_id));
            cid_font.set("DW", Object::Integer(1000));
            cid_font.set("CIDToGIDMap", Object::name("Identity"));
            if !w_array.is_empty() {
                cid_font.set("W", Object::Array(w_array));
            }
            self.objects
                .insert(descendant_id, Object::Dictionary(cid_font));

            // ToUnicode CMap ストリーム
            let mut tu_dict = Dictionary::new();
            tu_dict.set("Length", tounicode_bytes.len() as i64);
            let tounicode_stream = Object::Stream(Stream::new(tu_dict, tounicode_bytes));
            self.objects.insert(tounicode_id, tounicode_stream);

            // Type0 フォント
            let mut type0 = Dictionary::new();
            type0.set("Type", Object::name("Font"));
            type0.set("Subtype", Object::name("Type0"));
            type0.set("BaseFont", Object::name(base));
            type0.set("Encoding", Object::name("Identity-H"));
            type0.set(
                "DescendantFonts",
                Object::Array(vec![Object::Reference(descendant_id)]),
            );
            type0.set("ToUnicode", Object::Reference(tounicode_id));
            self.objects.insert(type0_id, Object::Dictionary(type0));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// フォント埋め込みの補助関数
// ---------------------------------------------------------------------------

/// 決定論的なサブセットタグ（6 文字の大文字 ASCII）を生成する。
///
/// FNV-1a ハッシュで PostScript 名と使用 GID から 6 文字を導く。
fn make_subset_tag(ps_name: &str, gids: &BTreeSet<u16>) -> String {
    const FNV_PRIME: u64 = 0x100000001b3;
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;

    let mut h = FNV_OFFSET;
    for b in ps_name.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    for &gid in gids {
        h ^= (gid >> 8) as u64;
        h = h.wrapping_mul(FNV_PRIME);
        h ^= (gid & 0xFF) as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }

    let mut tag = String::with_capacity(6);
    for i in 0..6u64 {
        let idx = ((h >> (i * 10)) % 26) as u8;
        tag.push((b'A' + idx) as char);
    }
    tag
}

/// ToUnicode CMap のバイト列を生成する。
fn build_tounicode_cmap(used: &BTreeMap<u16, char>) -> Vec<u8> {
    let mut out = Vec::new();
    let header = b"/CIDInit /ProcSet findresource begin\n\
                   12 dict begin\n\
                   begincmap\n\
                   /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
                   /CMapName /Adobe-Identity-UCS def\n\
                   /CMapType 2 def\n\
                   1 begincodespacerange\n\
                   <0000> <FFFF>\n\
                   endcodespacerange\n";
    out.extend_from_slice(header);

    // 使用グリフを最大 100 エントリずつチャンクで書く
    let entries: Vec<(u16, char)> = used.iter().map(|(&g, &c)| (g, c)).collect();
    for chunk in entries.chunks(100) {
        let line = format!("{} beginbfchar\n", chunk.len());
        out.extend_from_slice(line.as_bytes());
        for (gid, c) in chunk {
            // GID を 4 桁 16 進で書く
            let gid_hex = format!("<{:04X}>", gid);
            // 文字を UTF-16BE（サロゲートペア考慮）で書く
            let mut utf16 = [0u16; 2];
            let units = c.encode_utf16(&mut utf16);
            let char_hex: String = units
                .iter()
                .map(|u| format!("{:04X}", u))
                .collect::<String>();
            let line = format!("{gid_hex} <{char_hex}>\n");
            out.extend_from_slice(line.as_bytes());
        }
        out.extend_from_slice(b"endbfchar\n");
    }

    let footer = b"endcmap\n\
                   CMapName currentdict /CMap defineresource pop\n\
                   end\n\
                   end\n";
    out.extend_from_slice(footer);
    out
}

// ---------------------------------------------------------------------------
// テキスト文字列（§7.9.2.2）のエンコード/デコード
// ---------------------------------------------------------------------------

/// PDF テキスト文字列をデコードする。
///
/// UTF-16BE（BOM `FE FF`）、UTF-8（BOM `EF BB BF`, PDF 2.0）、
/// それ以外は PDFDocEncoding（おおむね Latin-1）として解釈する。
pub fn decode_text_string(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xFE, 0xFF]) {
        // UTF-16BE
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        String::from_utf8_lossy(&bytes[3..]).into_owned()
    } else {
        bytes
            .iter()
            .map(|&b| crate::font::winansi_to_char(b))
            .collect()
    }
}

/// 文字列を PDF テキスト文字列にエンコードする。
///
/// ASCII のみなら素のバイト列、それ以外は BOM 付き UTF-16BE。
pub fn encode_text_string(s: &str) -> Vec<u8> {
    if s.is_ascii() {
        s.as_bytes().to_vec()
    } else {
        let mut out = vec![0xFE, 0xFF];
        for unit in s.encode_utf16() {
            out.extend_from_slice(&unit.to_be_bytes());
        }
        out
    }
}

fn parse_header_version(data: &[u8]) -> Result<String> {
    let head = &data[..data.len().min(1024)];
    let pos = head
        .windows(5)
        .position(|w| w == b"%PDF-")
        .ok_or(PdfError::NotAPdf)?;
    let rest = &data[pos + 5..];
    let end = rest
        .iter()
        .position(|&b| !(b.is_ascii_digit() || b == b'.'))
        .unwrap_or(rest.len());
    let v = String::from_utf8_lossy(&rest[..end]).into_owned();
    if v.is_empty() {
        return Err(PdfError::NotAPdf);
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_string_roundtrip() {
        assert_eq!(decode_text_string(&encode_text_string("Hello")), "Hello");
        assert_eq!(
            decode_text_string(&encode_text_string("こんにちは")),
            "こんにちは"
        );
        // UTF-16BE BOM
        assert_eq!(decode_text_string(&[0xFE, 0xFF, 0x30, 0x42]), "あ");
    }

    #[test]
    fn new_document_has_catalog() {
        let doc = Document::new();
        let cat = doc.catalog().unwrap();
        assert_eq!(cat.get("Type").unwrap().as_name().unwrap(), "Catalog");
        assert_eq!(doc.page_count(), 0);
    }

    #[test]
    fn add_and_remove_pages() {
        let mut doc = Document::new();
        doc.add_page(595.0, 842.0).unwrap();
        doc.add_page(612.0, 792.0).unwrap();
        assert_eq!(doc.page_count(), 2);
        let mb = doc.page_media_box(doc.page_id(0).unwrap());
        assert_eq!(mb, [0.0, 0.0, 595.0, 842.0]);
        doc.remove_page(0).unwrap();
        assert_eq!(doc.page_count(), 1);
        let mb = doc.page_media_box(doc.page_id(0).unwrap());
        assert_eq!(mb, [0.0, 0.0, 612.0, 792.0]);
    }

    #[test]
    fn rotate_accumulates() {
        let mut doc = Document::new();
        doc.add_page(595.0, 842.0).unwrap();
        doc.rotate_page(0, 90).unwrap();
        doc.rotate_page(0, -180).unwrap();
        let page = doc
            .get_object(doc.page_id(0).unwrap())
            .unwrap()
            .as_dict()
            .unwrap();
        assert_eq!(page.get("Rotate").unwrap().as_int().unwrap(), 270);
        assert!(doc.rotate_page(0, 45).is_err());
    }
}
