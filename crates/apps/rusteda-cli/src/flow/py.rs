// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 RustEDA contributors

//! Value rules of the reference helpers that the frontend commands share: JSON documents as
//! Python's `json` module reads and writes them (objects keep their order, integers and floats
//! stay apart), truth values, and the typed readers of large documents.

use std::fmt;
use std::marker::PhantomData;

use rustc_hash::FxHashMap as HashMap;
use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

use crate::estimate::{Json, dump, put};

impl Json {
    /// `bool(value)`.
    pub(crate) fn truthy(&self) -> bool {
        match self {
            Json::Null => false,
            Json::Bool(b) => *b,
            Json::Int(i) => *i != 0,
            Json::Float(f) => *f != 0.0,
            Json::Str(s) => !s.is_empty(),
            Json::List(v) => !v.is_empty(),
            Json::Object(v) => !v.is_empty(),
        }
    }
    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }
    pub(crate) fn as_list(&self) -> Option<&[Json]> {
        match self {
            Json::List(v) => Some(v),
            _ => None,
        }
    }
    pub(crate) fn as_object(&self) -> Option<&[(String, Json)]> {
        match self {
            Json::Object(v) => Some(v),
            _ => None,
        }
    }
    /// An `int` or `float` that is not a `bool`, as `float(value)`.
    pub(crate) fn number(&self) -> Option<f64> {
        match self {
            Json::Int(i) => Some(*i as f64),
            Json::Float(f) => Some(*f),
            _ => None,
        }
    }
    /// An `int` that is not a `bool`.
    pub(crate) fn as_int(&self) -> Option<i64> {
        match self {
            Json::Int(i) => i64::try_from(*i).ok(),
            _ => None,
        }
    }
    pub(crate) fn str(s: impl Into<String>) -> Json {
        Json::Str(s.into())
    }
    /// `None` as `null`, a float otherwise.
    pub(crate) fn opt_float(v: Option<f64>) -> Json {
        v.map_or(Json::Null, Json::Float)
    }
    /// `json.dumps(value, indent=indent)`.
    pub(crate) fn dumps(&self, indent: Option<usize>) -> String {
        let mut out = String::new();
        dump(&mut out, self, indent, 0);
        out
    }
    /// `a == b` of two decoded values: dicts compare without their order, and an integer
    /// equals the float of the same value.
    pub(crate) fn py_eq(&self, other: &Json) -> bool {
        match (self, other) {
            (Json::Object(a), Json::Object(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .all(|(k, v)| other.get(k).is_some_and(|w| v.py_eq(w)))
            }
            (Json::List(a), Json::List(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(v, w)| v.py_eq(w))
            }
            (Json::Int(a), Json::Float(b)) | (Json::Float(b), Json::Int(a)) => *a as f64 == *b,
            (Json::Bool(a), Json::Int(b)) | (Json::Int(b), Json::Bool(a)) => i128::from(*a) == *b,
            (a, b) => a == b,
        }
    }
    /// `d[k] = v` on an object (the key keeps its position when present).
    pub(crate) fn set(&mut self, k: &str, v: Json) {
        if let Json::Object(items) = self {
            if let Some(slot) = items.iter_mut().find(|(key, _)| key == k) {
                slot.1 = v;
            } else {
                items.push((k.to_string(), v));
            }
        }
    }
}

struct JsonVisitor;

impl<'de> Visitor<'de> for JsonVisitor {
    type Value = Json;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }
    fn visit_unit<E>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }
    fn visit_bool<E>(self, v: bool) -> Result<Json, E> {
        Ok(Json::Bool(v))
    }
    fn visit_i64<E>(self, v: i64) -> Result<Json, E> {
        Ok(Json::Int(v.into()))
    }
    fn visit_u64<E>(self, v: u64) -> Result<Json, E> {
        Ok(Json::Int(v.into()))
    }
    fn visit_f64<E>(self, v: f64) -> Result<Json, E> {
        Ok(Json::Float(v))
    }
    fn visit_str<E>(self, v: &str) -> Result<Json, E> {
        Ok(Json::Str(v.to_string()))
    }
    fn visit_string<E>(self, v: String) -> Result<Json, E> {
        Ok(Json::Str(v))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Json, A::Error> {
        let mut items = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(4096));
        while let Some(v) = seq.next_element()? {
            items.push(v);
        }
        Ok(Json::List(items))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Json, A::Error> {
        let mut items: Vec<(String, Json)> = Vec::new();
        let mut index = None;
        while let Some((k, v)) = map.next_entry::<String, Json>()? {
            put(&mut items, &mut index, k, v);
        }
        Ok(Json::Object(items))
    }
}

impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Json, D::Error> {
        d.deserialize_any(JsonVisitor)
    }
}

/// A JSON object read as a dict: its items in the order of the text; a repeated key keeps its
/// first position and takes the last value.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Dict<V>(pub(crate) Vec<(String, V)>);

impl<V> Default for Dict<V> {
    fn default() -> Self {
        Dict(Vec::new())
    }
}

impl<V> Dict<V> {
    pub(crate) fn get(&self, k: &str) -> Option<&V> {
        self.0.iter().find(|(key, _)| key == k).map(|(_, v)| v)
    }
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, &V)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v))
    }
}

struct DictVisitor<V>(PhantomData<V>);

impl<'de, V: Deserialize<'de>> Visitor<'de> for DictVisitor<V> {
    type Value = Dict<V>;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON object")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Dict<V>, A::Error> {
        let mut items: Vec<(String, V)> = Vec::new();
        let mut index: Option<HashMap<String, usize>> = None;
        while let Some((k, v)) = map.next_entry::<String, V>()? {
            if items.len() < 16 {
                if let Some(slot) = items.iter_mut().find(|(key, _)| *key == k) {
                    slot.1 = v;
                } else {
                    items.push((k, v));
                }
                continue;
            }
            let index = index.get_or_insert_with(|| {
                items
                    .iter()
                    .enumerate()
                    .map(|(i, (key, _))| (key.clone(), i))
                    .collect()
            });
            match index.get(&k) {
                Some(&i) => items[i].1 = v,
                None => {
                    index.insert(k.clone(), items.len());
                    items.push((k, v));
                }
            }
        }
        Ok(Dict(items))
    }
}

impl<'de, V: Deserialize<'de>> Deserialize<'de> for Dict<V> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Dict<V>, D::Error> {
        d.deserialize_map(DictVisitor(PhantomData))
    }
}

/// A net bit of a connectivity graph: an integer, or anything else (a constant such as `"x"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Bit {
    Net(i64),
    Other,
}

impl Bit {
    pub(crate) fn net(self) -> Option<i64> {
        match self {
            Bit::Net(b) => Some(b),
            Bit::Other => None,
        }
    }
}

struct BitVisitor;

impl<'de> Visitor<'de> for BitVisitor {
    type Value = Bit;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a net bit")
    }
    fn visit_i64<E>(self, v: i64) -> Result<Bit, E> {
        Ok(Bit::Net(v))
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Bit, E> {
        i64::try_from(v)
            .map(Bit::Net)
            .map_err(|_| E::custom("net bit out of range"))
    }
    // `isinstance(True, int)` holds in the reference: a boolean bit is the integer 0 or 1.
    fn visit_bool<E>(self, v: bool) -> Result<Bit, E> {
        Ok(Bit::Net(i64::from(v)))
    }
    fn visit_f64<E>(self, _: f64) -> Result<Bit, E> {
        Ok(Bit::Other)
    }
    fn visit_str<E>(self, _: &str) -> Result<Bit, E> {
        Ok(Bit::Other)
    }
    fn visit_unit<E>(self) -> Result<Bit, E> {
        Ok(Bit::Other)
    }
}

impl<'de> Deserialize<'de> for Bit {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Bit, D::Error> {
        d.deserialize_any(BitVisitor)
    }
}

/// `f'{value:04}'` and the other zero-padded decimal fields of the reference.
pub(crate) fn zero_padded(value: usize, width: usize) -> String {
    format!("{value:0width$}")
}

/// `math.ceil(x)` and `math.floor(x)` of a finite float as an integer.
pub(crate) fn py_ceil(x: f64) -> Option<i64> {
    x.is_finite().then(|| x.ceil() as i64)
}

pub(crate) fn py_floor(x: f64) -> Option<i64> {
    x.is_finite().then(|| x.floor() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_reads_json_as_python_does() {
        let text = r#"{"b": 1, "a": 1.0, "b": [true, null, "x\u00e9", 1e-05, -0.0, 12345678901234567890]}"#;
        let v: Json = serde_json::from_str(text).unwrap();
        // The repeated key keeps its first position and takes the last value; text that is not
        // ASCII is written escaped.
        assert_eq!(
            v.dumps(None),
            r#"{"b": [true, null, "x\u00e9", 1e-05, -0.0, 12345678901234567890], "a": 1.0}"#
        );
        assert_eq!(crate::estimate::parse_json(text).unwrap(), v);
        let d: Dict<Vec<Bit>> = serde_json::from_str(r#"{"Y": [3, "x"], "A": [4]}"#).unwrap();
        assert_eq!(d.0[0], ("Y".to_string(), vec![Bit::Net(3), Bit::Other]));
        assert_eq!(d.get("A"), Some(&vec![Bit::Net(4)]));
    }

    #[test]
    fn large_objects_keep_python_key_rules() {
        let mut text = String::from("{");
        for i in 0..40 {
            text.push_str(&format!("\"k{i}\": {i}, "));
        }
        text.push_str("\"k3\": -1}");
        let v: Json = serde_json::from_str(&text).unwrap();
        let items = v.as_object().unwrap();
        assert_eq!(items.len(), 40);
        assert_eq!(items[3], ("k3".to_string(), Json::Int(-1)));
        assert_eq!(crate::estimate::parse_json(&text).unwrap(), v);
    }
}
