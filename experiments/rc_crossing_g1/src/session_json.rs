// SPDX-License-Identifier: Apache-2.0
//! Bounded metadata parsing with duplicate-key rejection before typed decoding.
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};
use std::fmt;

#[derive(Debug, Default)]
struct Bounds {
    nodes: usize,
    text: usize,
}
struct Seed<'a> {
    bounds: &'a mut Bounds,
    depth: usize,
}
struct MetadataVisitor<'a> {
    bounds: &'a mut Bounds,
    depth: usize,
}
fn diagnostic<E: de::Error>(s: &'static str) -> E {
    E::custom(s)
}
impl<'de> DeserializeSeed<'de> for Seed<'_> {
    type Value = Value;
    fn deserialize<D: de::Deserializer<'de>>(self, input: D) -> Result<Value, D::Error> {
        self.bounds.nodes += 1;
        if self.bounds.nodes > 4096 || self.depth > 32 {
            return Err(diagnostic("JSON_NODE_DEPTH_BUDGET"));
        }
        input.deserialize_any(MetadataVisitor {
            bounds: self.bounds,
            depth: self.depth,
        })
    }
}
impl MetadataVisitor<'_> {
    fn text<E: de::Error>(&mut self, n: usize) -> Result<(), E> {
        self.bounds.text = self
            .bounds
            .text
            .checked_add(n)
            .ok_or_else(|| diagnostic("JSON_TEXT_OVERFLOW"))?;
        if self.bounds.text > 65536 {
            return Err(diagnostic("JSON_TEXT_BUDGET"));
        }
        Ok(())
    }
}
impl<'de> Visitor<'de> for MetadataVisitor<'_> {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("bounded unique-key metadata")
    }
    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }
    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Value, E> {
        Ok(Value::Number(Number::from(value)))
    }
    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Value, E> {
        Ok(Value::Number(Number::from(value)))
    }
    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
        Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| diagnostic("NONFINITE_JSON"))
    }
    fn visit_str<E: de::Error>(self, value: &str) -> Result<Value, E> {
        self.visit_string(value.to_owned())
    }
    fn visit_string<E: de::Error>(mut self, value: String) -> Result<Value, E> {
        self.text(value.len())?;
        Ok(Value::String(value))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut input: A) -> Result<Value, A::Error> {
        let mut out = Vec::new();
        while let Some(value) = input.next_element_seed(Seed {
            bounds: &mut *self.bounds,
            depth: self.depth + 1,
        })? {
            out.try_reserve(1)
                .map_err(|_| diagnostic("JSON_ALLOCATION"))?;
            out.push(value);
        }
        Ok(Value::Array(out))
    }
    fn visit_map<A: MapAccess<'de>>(mut self, mut input: A) -> Result<Value, A::Error> {
        let mut out = Map::new();
        while let Some(key) = input.next_key::<String>()? {
            self.text(key.len())?;
            if out.contains_key(&key) {
                return Err(diagnostic("DUPLICATE_JSON_KEY"));
            }
            let value = input.next_value_seed(Seed {
                bounds: &mut *self.bounds,
                depth: self.depth + 1,
            })?;
            out.insert(key, value);
        }
        Ok(Value::Object(out))
    }
}
pub fn parse(raw: &[u8]) -> Result<Value, String> {
    if raw.len() > 65536 {
        return Err("JSON_BYTE_CAP".into());
    }
    let mut bounds = Bounds::default();
    let mut input = serde_json::Deserializer::from_slice(raw);
    let value = Seed {
        bounds: &mut bounds,
        depth: 0,
    }
    .deserialize(&mut input)
    .map_err(|e| e.to_string())?;
    input.end().map_err(|e| e.to_string())?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn rejects_duplicate_keys_even_inside_opaque_provenance() {
        assert!(
            parse(br#"{"provenance":{"x":1,"x":2}}"#)
                .unwrap_err()
                .contains("DUPLICATE_JSON_KEY")
        );
        assert!(parse(br#"{"x":1e999}"#).is_err());
    }

    #[test]
    fn metadata_depth_nodes_and_bytes_have_separate_limits() {
        let deep = format!("{}0{}", "[".repeat(33), "]".repeat(33));
        assert!(
            parse(deep.as_bytes())
                .unwrap_err()
                .contains("JSON_NODE_DEPTH_BUDGET")
        );
        let nodes = format!(
            "[{}]",
            std::iter::repeat_n("0", 4096).collect::<Vec<_>>().join(",")
        );
        assert!(
            parse(nodes.as_bytes())
                .unwrap_err()
                .contains("JSON_NODE_DEPTH_BUDGET")
        );
        assert!(
            parse(&vec![b' '; 65537])
                .unwrap_err()
                .contains("JSON_BYTE_CAP")
        );
        assert!(parse(br#"{"valid":[1,2,{"x":true}]}"#).is_ok());
    }
}
