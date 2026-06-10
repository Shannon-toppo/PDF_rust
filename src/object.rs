//! PDF オブジェクトモデル。
//!
//! PDF 32000-1:2008 §7.3 で定義される 8 種の基本オブジェクト
//! （ブール・数値・文字列・名前・配列・辞書・ストリーム・null）と
//! 間接参照を [`Object`] 列挙型として表現する。

use crate::error::{PdfError, Result};

/// 間接オブジェクトの識別子。`(オブジェクト番号, 世代番号)`。
pub type ObjectId = (u32, u16);

/// PDF オブジェクト。
#[derive(Debug, Clone, PartialEq)]
pub enum Object {
    /// `null`
    Null,
    /// `true` / `false`
    Boolean(bool),
    /// 整数（PDF では 32bit だが余裕を持って i64）
    Integer(i64),
    /// 実数
    Real(f64),
    /// 文字列。PDF の文字列はバイト列（テキストとは限らない）。
    String(Vec<u8>, StringFormat),
    /// 名前オブジェクト（`/Name`）。先頭の `/` は含まない。
    Name(String),
    /// 配列
    Array(Vec<Object>),
    /// 辞書
    Dictionary(Dictionary),
    /// ストリーム（辞書 + バイナリデータ）
    Stream(Stream),
    /// 間接参照（`12 0 R`）
    Reference(ObjectId),
}

/// 文字列の書き出し形式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StringFormat {
    /// `(literal)` 形式
    Literal,
    /// `<686578>` 16 進形式
    Hexadecimal,
}

impl Object {
    /// 人間向けの型名（エラーメッセージ用）。
    pub fn type_name(&self) -> &'static str {
        match self {
            Object::Null => "null",
            Object::Boolean(_) => "boolean",
            Object::Integer(_) => "integer",
            Object::Real(_) => "real",
            Object::String(..) => "string",
            Object::Name(_) => "name",
            Object::Array(_) => "array",
            Object::Dictionary(_) => "dictionary",
            Object::Stream(_) => "stream",
            Object::Reference(_) => "reference",
        }
    }

    /// 整数として取り出す。
    pub fn as_int(&self) -> Result<i64> {
        match self {
            Object::Integer(i) => Ok(*i),
            _ => Err(self.type_error("integer")),
        }
    }

    /// 数値（整数・実数とも）を f64 として取り出す。
    pub fn as_number(&self) -> Result<f64> {
        match self {
            Object::Integer(i) => Ok(*i as f64),
            Object::Real(r) => Ok(*r),
            _ => Err(self.type_error("number")),
        }
    }

    /// 名前として取り出す。
    pub fn as_name(&self) -> Result<&str> {
        match self {
            Object::Name(n) => Ok(n),
            _ => Err(self.type_error("name")),
        }
    }

    /// 文字列（バイト列）として取り出す。
    pub fn as_string(&self) -> Result<&[u8]> {
        match self {
            Object::String(s, _) => Ok(s),
            _ => Err(self.type_error("string")),
        }
    }

    /// 配列として取り出す。
    pub fn as_array(&self) -> Result<&Vec<Object>> {
        match self {
            Object::Array(a) => Ok(a),
            _ => Err(self.type_error("array")),
        }
    }

    /// 配列として可変で取り出す。
    pub fn as_array_mut(&mut self) -> Result<&mut Vec<Object>> {
        match self {
            Object::Array(a) => Ok(a),
            _ => Err(PdfError::TypeMismatch {
                expected: "array",
                found: "other",
            }),
        }
    }

    /// 辞書として取り出す。ストリームの場合はそのストリーム辞書を返す。
    pub fn as_dict(&self) -> Result<&Dictionary> {
        match self {
            Object::Dictionary(d) => Ok(d),
            Object::Stream(s) => Ok(&s.dict),
            _ => Err(self.type_error("dictionary")),
        }
    }

    /// 辞書として可変で取り出す。
    pub fn as_dict_mut(&mut self) -> Result<&mut Dictionary> {
        match self {
            Object::Dictionary(d) => Ok(d),
            Object::Stream(s) => Ok(&mut s.dict),
            _ => Err(PdfError::TypeMismatch {
                expected: "dictionary",
                found: "other",
            }),
        }
    }

    /// ストリームとして取り出す。
    pub fn as_stream(&self) -> Result<&Stream> {
        match self {
            Object::Stream(s) => Ok(s),
            _ => Err(self.type_error("stream")),
        }
    }

    /// 間接参照として取り出す。
    pub fn as_reference(&self) -> Result<ObjectId> {
        match self {
            Object::Reference(id) => Ok(*id),
            _ => Err(self.type_error("reference")),
        }
    }

    fn type_error(&self, expected: &'static str) -> PdfError {
        PdfError::TypeMismatch {
            expected,
            found: self.type_name(),
        }
    }

    /// リテラル文字列オブジェクトを作る補助関数。
    pub fn string_literal(s: impl Into<Vec<u8>>) -> Object {
        Object::String(s.into(), StringFormat::Literal)
    }

    /// 名前オブジェクトを作る補助関数。
    pub fn name(s: impl Into<String>) -> Object {
        Object::Name(s.into())
    }
}

impl From<i64> for Object {
    fn from(v: i64) -> Self {
        Object::Integer(v)
    }
}
impl From<i32> for Object {
    fn from(v: i32) -> Self {
        Object::Integer(v as i64)
    }
}
impl From<f64> for Object {
    fn from(v: f64) -> Self {
        Object::Real(v)
    }
}
impl From<bool> for Object {
    fn from(v: bool) -> Self {
        Object::Boolean(v)
    }
}
impl From<Dictionary> for Object {
    fn from(v: Dictionary) -> Self {
        Object::Dictionary(v)
    }
}
impl From<Vec<Object>> for Object {
    fn from(v: Vec<Object>) -> Self {
        Object::Array(v)
    }
}

/// PDF 辞書。挿入順を保持する連想配列。
///
/// キーは名前オブジェクト（`/` を除いた文字列）。PDF の辞書は本来順序を
/// 持たないが、書き出しの安定性のために挿入順を保持する。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Dictionary {
    entries: Vec<(String, Object)>,
}

impl Dictionary {
    /// 空の辞書を作る。
    pub fn new() -> Self {
        Self::default()
    }

    /// キーに対応する値を取得する（間接参照の解決はしない）。
    pub fn get(&self, key: &str) -> Option<&Object> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// キーに対応する値を可変で取得する。
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Object> {
        self.entries
            .iter_mut()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    /// 値を設定する。既存のキーは上書きされる。
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<Object>) {
        let key = key.into();
        let value = value.into();
        if let Some(slot) = self.get_mut(&key) {
            *slot = value;
        } else {
            self.entries.push((key, value));
        }
    }

    /// キーを削除する。存在した場合は値を返す。
    pub fn remove(&mut self, key: &str) -> Option<Object> {
        let pos = self.entries.iter().position(|(k, _)| k == key)?;
        Some(self.entries.remove(pos).1)
    }

    /// キーが存在するか。
    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// エントリ数。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 空かどうか。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// `(キー, 値)` のイテレータ。
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Object)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// 必須キーを取得する。なければ [`PdfError::MissingKey`]。
    pub fn require(&self, key: &'static str) -> Result<&Object> {
        self.get(key).ok_or(PdfError::MissingKey(key))
    }
}

impl FromIterator<(String, Object)> for Dictionary {
    fn from_iter<T: IntoIterator<Item = (String, Object)>>(iter: T) -> Self {
        let mut d = Dictionary::new();
        for (k, v) in iter {
            d.set(k, v);
        }
        d
    }
}

/// ストリームオブジェクト。辞書とバイナリデータの組。
///
/// `data` には**ファイル上に格納されたままの（エンコード済みの）**バイト列を
/// 保持する。伸長済みデータが必要な場合は [`Stream::decoded_data`] を使う。
#[derive(Debug, Clone, PartialEq)]
pub struct Stream {
    /// ストリーム辞書（`/Length` `/Filter` など）。
    pub dict: Dictionary,
    /// エンコード済み（ファイル格納形式のまま）のデータ。
    pub data: Vec<u8>,
}

impl Stream {
    /// フィルタなしの生ストリームを作る。`/Length` は自動設定される。
    pub fn new(mut dict: Dictionary, data: Vec<u8>) -> Self {
        dict.set("Length", data.len() as i64);
        dict.remove("Filter");
        dict.remove("DecodeParms");
        Stream { dict, data }
    }

    /// データを zlib（FlateDecode）で圧縮して格納したストリームを作る。
    pub fn new_compressed(mut dict: Dictionary, data: &[u8]) -> Self {
        let compressed = crate::filters::flate::compress(data);
        dict.set("Length", compressed.len() as i64);
        dict.set("Filter", Object::Name("FlateDecode".into()));
        dict.remove("DecodeParms");
        Stream {
            dict,
            data: compressed,
        }
    }

    /// `/Filter` チェーンを適用してデータを伸長する。
    ///
    /// 間接参照を含む `/Filter` / `/DecodeParms` には対応しない
    /// （その場合は [`crate::document::Document::get_stream_data`] を使う）。
    pub fn decoded_data(&self) -> Result<Vec<u8>> {
        crate::filters::decode_stream(&self.dict, &self.data, None)
    }

    /// データを差し替える（フィルタなし・`/Length` 更新）。
    pub fn set_plain_data(&mut self, data: Vec<u8>) {
        self.dict.set("Length", data.len() as i64);
        self.dict.remove("Filter");
        self.dict.remove("DecodeParms");
        self.data = data;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dictionary_preserves_insertion_order_and_overwrites() {
        let mut d = Dictionary::new();
        d.set("B", 1);
        d.set("A", 2);
        d.set("B", 3);
        let keys: Vec<&str> = d.iter().map(|(k, _)| k).collect();
        assert_eq!(keys, vec!["B", "A"]);
        assert_eq!(d.get("B").unwrap().as_int().unwrap(), 3);
    }

    #[test]
    fn object_accessors() {
        assert_eq!(Object::Integer(5).as_number().unwrap(), 5.0);
        assert_eq!(Object::Real(1.5).as_number().unwrap(), 1.5);
        assert!(Object::Null.as_int().is_err());
        let r = Object::Reference((3, 0));
        assert_eq!(r.as_reference().unwrap(), (3, 0));
    }
}
